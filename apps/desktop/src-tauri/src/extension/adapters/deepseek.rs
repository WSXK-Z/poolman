//! DeepSeek 网页版服务端直管服务（对齐 apps/server/src/providers/deepseek.ts）。
//! 负责：凭证确保 → 1s 节流 → PoW 求解 → 请求组装（会话/工具段/阶段引导）→
//! SSE 解析（THINK/RESPONSE 实时）→ 会话管理 → 工具调用解析（YAML + JSON 容错）→ 422 自愈 → 回答前缀携带会话 id。

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tracing::{debug, error, info, warn};

use crate::extension::credentials::CredentialStore;
use crate::extension::adapters::pow::{build_pow_header, solve_pow};
use crate::extension::websocket::{ChatResponsePayload, ExecuteRequestPayload, WebSocketBridge};
use crate::session::manager::SessionManager;

const PROVIDER_ID: &str = "deepseek-web";
const DEEPSEEK_API_URL: &str = "https://chat.deepseek.com/api/v0/chat/completion";
const DEEPSEEK_SESSION_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/create";
const DEEPSEEK_HISTORY_URL: &str = "https://chat.deepseek.com/api/v0/chat/history_messages";
const DEEPSEEK_TITLE_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/update_title";
const POW_CHALLENGE_URL: &str = "https://chat.deepseek.com/api/v0/chat/create_pow_challenge";
const DEEPSEEK_MODEL_TYPE: &str = "default";
const DEEPSEEK_THINK: bool = true;
const MAX_ATTEMPTS: usize = 6;
const AUX_TIMEOUT: u64 = 10_000;
/// 工具调用围栏：XML 风格（避免参数内容含 ``` 三反引号时与围栏冲突）。
/// 开围栏变体：<tool_call> / 漏下划线 <toolcall>；兼容属性形式 <tool_call name="Read">
const TOOL_FENCE_PREFIXES: [&str; 2] = ["<tool_call", "<toolcall"];
/// 闭围栏：</tool_call>（与开围栏对称）
const TOOL_FENCE_CLOSE: &str = "</tool_call>";
/// invoke 标签名（工具调用的入口标签）
const MARKER_INVOKE: &str = "invoke";
/// invoke 内部参数字段标签名
const PARAM_TAG_NAME: &str = "parameter";
/// 可独立标识「工具调用开始」的标签名：DSML 外层标记标签（真实输出里外层名即 calls）、
/// 工具标签与自有围栏变体；含模型错写（call / call_call）。parameter 只是内部字段，不算起始标记。
const TOOL_TAG_NAMES: [&str; 7] = [
    "tool_call",
    "toolcall",
    "tool_calls",
    "calls",
    "call",
    "call_call",
    MARKER_INVOKE,
];
const SEND_INTERVAL_MS: u64 = 1_000;

/* ------------------------------ 辅助函数 ------------------------------ */

/// 毫秒级时间戳（保证每次回答前缀唯一）
fn format_ts_ms() -> String {
    let now = chrono::Local::now();
    format!("{}.{:03}", now.format("%Y/%m/%d %H:%M:%S"), now.timestamp_subsec_millis())
}

/// 从回答内容提取「会话: <会话id>」，供追问复用会话。
/// ⚠️ 必须用 find("会话") 精确定位：前缀「回答时间: <ts>\n会话: <id>」中
/// "会话" 位于字节偏移 37（时间戳格式固定），不是 6 的倍数，
/// 若用 i += "会话".len() 字节跳步扫描会漏掉 → 追问解析不到会话 id → 重启后新建会话。
fn extract_session_id(text: &str) -> Option<String> {
    let mut last = None;
    let mut cursor = 0usize;
    while let Some(rel) = text[cursor..].find("会话") {
        let abs = cursor + rel;
        let rest = &text[abs + "会话".len()..];
        let rest = rest.strip_prefix(':').or_else(|| rest.strip_prefix('：'));
        if let Some(rest) = rest {
            let rest = rest.trim_start();
            let id: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            if id.len() >= 10 && id.chars().next().map_or(false, |c| c.is_ascii_alphanumeric()) {
                last = Some(id);
            }
        }
        cursor = abs + "会话".len();
    }
    last
}

/// 从 422 报错里解析缺失字段名，如 "missing field `ref_file_ids`"
fn parse_missing_field(text: &str) -> Option<String> {
    let start = text.find("missing field `")? + "missing field `".len();
    let end = text[start..].find('`')? + start;
    Some(text[start..end].to_string())
}

/// 为缺失字段猜测默认值
fn guess_default(field: &str) -> Value {
    match field {
        "ref_file_ids" => return json!([]),
        "search_enabled" => return json!(false),
        "force_search" => return json!(false),
        "n" => return json!(1),
        "session_version" => return json!(0),
        "prompt" => return json!(""),
        "message_meta" => return json!({}),
        _ => {}
    }
    if field.ends_with("_id")
        || field.ends_with("_ids")
        || field.ends_with("_files")
        || field.ends_with("_list")
        || field.ends_with("ids")
    {
        return json!([]);
    }
    if field.starts_with("is_")
        || field.starts_with("has_")
        || field.starts_with("enable")
        || field.starts_with("disable")
        || field.starts_with("force")
        || field.starts_with("search")
    {
        return json!(false);
    }
    json!("")
}

/// 识别 HTTP 200 下的业务错误 JSON
fn parse_biz_error(body: &str) -> Option<(i64, String)> {
    let t = body.trim();
    if t.is_empty() || !t.starts_with('{') {
        return None;
    }
    if let Ok(j) = serde_json::from_str::<Value>(t) {
        let code = j
            .get("data")
            .and_then(|d| d.get("biz_code"))
            .and_then(|v| v.as_i64())
            .or_else(|| j.get("code").and_then(|v| v.as_i64()))
            .unwrap_or(0);
        let msg = j
            .get("data")
            .and_then(|d| d.get("biz_msg"))
            .and_then(|v| v.as_str())
            .or_else(|| j.get("msg").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        if code != 0 && !msg.is_empty() {
            return Some((code, msg));
        }
    }
    None
}

/// 标签前缀后是否为合法边界（'>' 或空白）；空串表示流式增量尚未到达后续字符。
/// 用于区分 <tool_calls>（前缀后紧随 's'，非边界）与项目自有 <tool_call>。
fn is_tag_boundary(after: &str) -> bool {
    match after.chars().next() {
        None => true,
        Some(c) => c == '>' || c.is_whitespace(),
    }
}

/// 扫描到的标签（'<' … '>'）
struct Tag<'a> {
    /// '<' 的下标
    start: usize,
    /// '>' 之后的下标
    end: usize,
    /// 是否闭合标签（'</' 开头）
    closing: bool,
    /// 标记名（tool_call/toolcall/tool_calls/invoke/parameter）；非标记标签为 None
    marker: Option<&'static str>,
    /// '<' 与 '>' 之间的原文（含属性）
    inner: &'a str,
}

/// 从 `from` 起扫描下一个标签；没有完整标签（缺 '>'）时返回 None
fn next_tag(text: &str, from: usize) -> Option<Tag<'_>> {
    if from >= text.len() {
        return None;
    }
    let lt = text[from..].find('<')? + from;
    let gt = lt + text[lt..].find('>')?;
    let inner = &text[lt + 1..gt];
    Some(Tag {
        start: lt,
        end: gt + 1,
        closing: inner.trim_start().starts_with('/'),
        marker: marker_name(inner),
        inner,
    })
}

/// 收集文本里所有 [A-Za-z0-9_]+ 连续段
fn ident_runs(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in s.char_indices() {
        let is_ident = c.is_ascii_alphanumeric() || c == '_';
        match (is_ident, start) {
            (true, None) => start = Some(i),
            (false, Some(s0)) => {
                out.push(&s[s0..i]);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s0) = start {
        out.push(&s[s0..]);
    }
    out
}

/// 识别标签原文里的标记名：不依赖具体标记前缀，直接看标识符段，
/// 使 <invoke name="x">、<|DSML| invoke>、<||DSML|| invoke>、全角竖线 <｜｜DSML｜｜invoke> 都能识别；
/// 非标记标签（html/div/json 等）返回 None。
fn marker_name(inner: &str) -> Option<&'static str> {
    for token in inner.split_whitespace() {
        // 属性 token（含 '='，如 name="invoke"）只看等号前的部分，避免参数值恰好是标签名时误判
        let head = match token.find('=') {
            Some(i) => &token[..i],
            None => token,
        };
        // 逐个标识符段判断：跳过标记符里的名字（<||DSML|| invoke> 里的 "DSML"），命中标记名即返回
        for ident in ident_runs(head) {
            if let Some(n) = TOOL_TAG_NAMES.iter().find(|n| **n == ident) {
                return Some(*n);
            }
            if ident == PARAM_TAG_NAME {
                return Some(PARAM_TAG_NAME);
            }
        }
    }
    None
}

/// 提取标签属性值（键名须为独立单词；支持 " 与 ' 引号）
fn extract_attr(attr: &str, key: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let needle = format!("{}={}", key, quote);
        let mut from = 0;
        while let Some(rel) = attr[from..].find(&needle) {
            let pos = from + rel;
            let prev = attr[..pos].chars().next_back();
            if prev.map_or(true, |c| !(c.is_alphanumeric() || c == '_' || c == '-')) {
                let rest = &attr[pos + needle.len()..];
                if let Some(end) = rest.find(quote) {
                    return Some(rest[..end].to_string());
                }
            }
            from = pos + needle.len();
        }
    }
    None
}

/// 找 `from` 之后第一个闭合的标记标签（如 </invoke>、</|DSML| parameter>），返回其 '<' 下标
fn find_close_marker(text: &str, from: usize, marker: &str) -> Option<usize> {
    let mut i = from;
    while i < text.len() {
        let tag = next_tag(text, i)?;
        if tag.closing && tag.marker == Some(marker) {
            return Some(tag.start);
        }
        // 只前进一个字节：值里出现 '<'（如代码 "if (a < b)"）会被误当标签，必须继续找真正的闭合标签
        i = tag.start + 1;
    }
    None
}

/// 是否属于「工具调用围栏」标签家族（含模型常见错写：call / call_call / toolcalls / tool_calls）
fn is_fence_family(marker: Option<&str>) -> bool {
    matches!(
        marker,
        Some("tool_call")
            | Some("toolcall")
            | Some("tool_calls")
            | Some("call")
            | Some("call_call")
            | Some("calls")
    )
}

/// 找围栏块内容结束位置：下一个「工具调用围栏」闭合标签的 '<' 下标。
/// 容忍模型错写闭标签（如把闭合标签写成 call_call）；内容里其它闭合标签（html 等）不算。
fn find_fence_close_tag(text: &str) -> Option<usize> {
    let mut i = 0;
    while i < text.len() {
        let tag = next_tag(text, i)?;
        if tag.closing && is_fence_family(tag.marker) {
            return Some(tag.start);
        }
        // 只前进一个字节：内容里出现 '<'（代码/XML 片段）时不能跳过真正的闭合标签
        i = tag.start + 1;
    }
    None
}

/// 是否出现工具调用开标签的【完整】形态（<tool_call> / <tool_calls> / <invoke name="X"> /
/// <|DSML| invoke name="X"> 等）。只认完整标签；被增量拆分的前缀由 tool_fence_remaining 处理。
fn contains_tool_open_tag(buf: &str) -> bool {
    let mut i = 0;
    while let Some(tag) = next_tag(buf, i) {
        // 只认标记标签（参数标签不算起始标记）；普通标签（json/div 等）marker 为 None
        if !tag.closing && tag.marker.is_some() && tag.marker != Some(PARAM_TAG_NAME) {
            return true;
        }
        i = tag.end;
    }
    false
}

/// 尾部（从最后一个 '<' 开始）是否可能是工具调用开标签的未完成前缀。
/// 覆盖 <tool_call / <tool_calls / <invoke 及 DSML 标记形态（半角 | 与全角 ｜ 均支持）。
/// 返回 Some 表示还需继续缓冲；None 表示不是（含标签已闭合、交由 looks_like_tool_call 判定的情况）。
fn xml_fence_remaining(tail: &str) -> Option<usize> {
    if !tail.starts_with('<') || tail.contains('>') {
        return None;
    }
    let body = &tail[1..];
    match body.chars().next() {
        // 只有 '<'，等后续字符
        None => return Some(1),
        // 普通文本里的 '<'（如 "a < b"）不算标签前缀
        Some(c) if !(c.is_ascii_alphabetic() || c == '/' || c == '|' || c == '\u{FF5C}') => {
            return None
        }
        _ => {}
    }
    // 剥掉标记前缀（<|DSML| / </|DSML| / <||DSML|| / <｜｜DSML｜｜ 等）：取最后一个标记符后的部分
    let after_prefix = match body.rfind(|c: char| c == '|' || c == '\u{FF5C}') {
        Some(i) => {
            // 标记符可能是多字节字符（全角竖线占 3 字节）→ 按字符长度跳过，避免切断 UTF-8
            let mark_len = body[i..].chars().next().map_or(1, |c| c.len_utf8());
            &body[i + mark_len..]
        }
        None => body,
    };
    let word: String = after_prefix
        .trim_start_matches('/')
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if word.is_empty() {
        // 标记前缀还没输完
        return Some(1);
    }
    if TOOL_TAG_NAMES.iter().any(|n| n.starts_with(word.as_str())) {
        return Some(1);
    }
    None
}

/// 工具名校验：非空且不含空白/尖括号（挡住畸形标签，避免产生假工具调用）
fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(|c| c.is_whitespace() || c == '<' || c == '>')
}

/// 文本里是否留有「像工具调用但没被解析」的痕迹（用于告警：判断模型是否未按提示词围栏格式输出）
fn looks_like_unparsed_tool_call(text: &str) -> bool {
    text.contains("tool_call") || text.contains("invoke") || text.contains("arguments:")
}

/// XML/DSML 参数值：去掉独占多行布局引入的首尾换行；
/// force_string（对应 string="true"）或多行值原样保留字符串；其余单行值按标量自动转类型
/// （整数/小数/布尔/null）。
fn xml_param_value(raw: &str, force_string: bool) -> Value {
    let mut v = raw;
    if let Some(r) = v.strip_prefix("\r\n").or_else(|| v.strip_prefix('\n')) {
        v = r;
    }
    if let Some(r) = v.strip_suffix("\r\n").or_else(|| v.strip_suffix('\n')) {
        v = r;
    }
    if force_string || v.contains('\n') {
        return Value::String(v.to_string());
    }
    let t = v.trim();
    if t.is_empty() {
        return Value::String(String::new());
    }
    if let Ok(i) = t.parse::<i64>() {
        return json!(i);
    }
    if let Ok(f) = t.parse::<f64>() {
        if let Some(n) = serde_json::Number::from_f64(f) {
            return Value::Number(n);
        }
    }
    if t.eq_ignore_ascii_case("true") {
        return json!(true);
    }
    if t.eq_ignore_ascii_case("false") {
        return json!(false);
    }
    if t == "null" {
        return Value::Null;
    }
    Value::String(t.to_string())
}

/// 解析 invoke 体：<parameter name="k" string="true">v</parameter>（含 <|DSML| parameter> 变体）→ 参数对象
fn parse_xml_parameters(body: &str) -> Value {
    let mut map = serde_json::Map::new();
    let mut i = 0;
    while i < body.len() {
        let Some(tag) = next_tag(body, i) else { break };
        if tag.closing || tag.marker != Some(PARAM_TAG_NAME) {
            i = tag.start + 1;
            continue;
        }
        let Some(key) = extract_attr(tag.inner, "name") else {
            i = tag.start + 1;
            continue;
        };
        let value_end = find_close_marker(body, tag.end, PARAM_TAG_NAME).unwrap_or(body.len());
        let force_string = extract_attr(tag.inner, "string")
            .map_or(false, |v| v.trim().eq_ignore_ascii_case("true"));
        map.insert(key, xml_param_value(&body[tag.end..value_end], force_string));
        i = value_end + 1;
    }
    Value::Object(map)
}

/// 解析 XML/DSML 工具调用：
///   <invoke name="X"><parameter name="k">v</parameter></invoke>
///   <|DSML| tool_calls><|DSML| invoke name="X"><|DSML| parameter name="k" string="true">v</|DSML| parameter></|DSML| invoke></|DSML| tool_calls>
/// （外层 tool_calls 可缺）。返回 (工具名, 参数对象) 列表。
fn parse_xml_tool_call_blocks(text: &str) -> Vec<(String, Value)> {
    let mut out: Vec<(String, Value)> = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let Some(tag) = next_tag(text, i) else { break };
        if tag.closing || tag.marker != Some(MARKER_INVOKE) {
            i = tag.start + 1;
            continue;
        }
        let Some(name) = extract_attr(tag.inner, "name") else {
            i = tag.start + 1;
            continue;
        };
        let body_end = find_close_marker(text, tag.end, MARKER_INVOKE).unwrap_or(text.len());
        if is_valid_tool_name(&name) {
            out.push((name, parse_xml_parameters(&text[tag.end..body_end])));
        }
        i = body_end + 1;
    }
    out
}

/// 解析单个工具块（纯 YAML）：<tool_call> 围栏内/整体均为 YAML 块标量格式。
/// 返回解析出的对象或数组；必须是工具形状（含 name/function）才接受。
fn parse_tool_block_yaml(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let v: Value = serde_yaml::from_str(trimmed).ok()?;
    if (v.is_object() || v.is_array()) && has_tool_shape(&v) {
        Some(v)
    } else {
        None
    }
}

/// 工具调用块解析（<tool_call> 围栏内 YAML，支持多个工具块；无围栏时整体尝试 YAML）。返回 (name, arguments) 列表。
fn parse_tool_call_blocks(text: &str) -> Option<Vec<(String, Value)>> {
    let mut results: Vec<(String, Value)> = Vec::new();

    let push = |results: &mut Vec<(String, Value)>, v: &Value| {
        let items: Vec<&Value> = if v.is_array() {
            v.as_array().unwrap().iter().collect()
        } else {
            vec![v]
        };
        for item in items {
            if !item.is_object() {
                continue;
            }
            let obj = item.as_object().unwrap();
            let name = obj
                .get("name")
                .and_then(|v| v.as_str())
                .or_else(|| obj.get("function").and_then(|f| f.get("name")).and_then(|v| v.as_str()))
                .map(|s| s.to_string());
            if let Some(name) = name {
                let mut args = obj
                    .get("arguments")
                    .or_else(|| obj.get("parameters"))
                    .or_else(|| obj.get("args"))
                    .or_else(|| obj.get("function").and_then(|f| f.get("arguments")))
                    .cloned();
                if let Some(Value::String(s)) = &args {
                    args = serde_json::from_str::<Value>(s).ok();
                }
                let mut args = args.filter(|v| v.is_object()).unwrap_or_else(|| json!({}));
                // arguments 缺失/为空时，把同级其它键当作参数：
                // 模型常把参数与 arguments 写在同一层级（arguments 为空 → 参数挂到外层，参数会丢）
                if args.as_object().map_or(true, |m| m.is_empty()) {
                    let hoisted: serde_json::Map<String, Value> = obj
                        .iter()
                        .filter(|(k, _)| {
                            !matches!(k.as_str(), "name" | "function" | "arguments" | "parameters" | "args")
                        })
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    if !hoisted.is_empty() {
                        args = Value::Object(hoisted);
                    }
                }
                results.push((name, args));
            }
        }
    };

    // 0) XML 工具调用格式（<tool_calls>/<invoke>/<parameter>）：与自有围栏格式并存
    results.extend(parse_xml_tool_call_blocks(text));

    // 1) 围栏块（<tool_call> ... </tool_call>；兼容 <tool_call name="X"> 属性形式；
    //    支持漏下划线变体 <toolcall>；支持多个工具块）
    let mut rest = text;
    loop {
        // 找最近的下一个开围栏（任意变体，可带 name 属性）
        let Some((_pos, fence_end, attr_name)) = find_open_fence(rest) else { break };
        let after = &rest[fence_end..];
        // 闭围栏优先按标准 </tool_call> 精确匹配；模型错写（</call_call> 等）时退回按标签名扫描同一家族
        let end = after
            .find(TOOL_FENCE_CLOSE)
            .or_else(|| find_fence_close_tag(after))
            .unwrap_or(after.len());
        let inner = after[..end].trim();
        if let Some(name) = attr_name {
            // 属性形式：<tool_call name="X"> 内是参数 YAML（无 name 字段）
            results.push((name, parse_args_object(inner)));
        } else if let Some(v) = parse_tool_block_yaml(inner) {
            push(&mut results, &v);
        }
        rest = &after[end.min(after.len())..];
    }
    // 2) 无围栏时整体尝试 YAML
    if results.is_empty() {
        if let Some(v) = parse_tool_block_yaml(text) {
            push(&mut results, &v);
        }
    }
    if results.is_empty() {
        None
    } else {
        Some(results)
    }
}

/// 从文本中找最近的下一个开围栏，返回 (开围栏起始, 开围栏结束含 '>', 可选 name 属性值)。
/// 支持 <tool_call> / <toolcall> / <tool_call name="X"> / <toolcall name='Y'> 等。
fn find_open_fence(text: &str) -> Option<(usize, usize, Option<String>)> {
    let mut best: Option<(usize, usize, Option<String>)> = None;
    for prefix in TOOL_FENCE_PREFIXES {
        let mut from = 0;
        while let Some(rel) = text[from..].find(prefix) {
            let pos = from + rel;
            let after = &text[pos + prefix.len()..];
            // 前缀后必须是标签边界：排除 <tool_calls>（XML 工具调用格式，前缀后紧随 's'）等其它标签
            if !is_tag_boundary(after) {
                from = pos + prefix.len();
                continue;
            }
            if let Some(gt) = after.find('>') {
                let attr = &after[..gt];
                let end = pos + prefix.len() + gt + 1;
                let name = extract_name_attr(attr);
                if best.as_ref().map_or(true, |(p, _, _)| pos < *p) {
                    best = Some((pos, end, name));
                }
                break;
            }
            from = pos + prefix.len();
        }
    }
    best
}

/// 从标签属性串提取 name 值（自有围栏 <tool_call name="Read"> 与 XML 格式共用）
fn extract_name_attr(attr: &str) -> Option<String> {
    extract_attr(attr, "name")
}

/// 属性形式下解析参数 YAML：兼容裸参数（filePath: a）与带 arguments/parameters/args 包装；
/// 解析失败返回空对象。
fn parse_args_object(raw: &str) -> Value {
    let Ok(v) = serde_yaml::from_str::<Value>(raw.trim()) else {
        return json!({});
    };
    if !v.is_object() {
        return json!({});
    }
    for k in ["arguments", "parameters", "args"] {
        if let Some(a) = v.get(k) {
            if a.is_object() {
                return a.clone();
            }
        }
    }
    v
}

fn has_tool_shape(v: &Value) -> bool {
    let obj = if v.is_array() {
        v.as_array().and_then(|a| a.first()).cloned().unwrap_or(json!({}))
    } else {
        v.clone()
    };
    obj.get("name").is_some() || obj.get("function").is_some()
}

/// 从回答内容解析工具调用 → OpenAI 标准 tool_calls 数组
fn parse_tool_calls(content: &str) -> Option<Value> {
    let text = extract_response_text(content);
    let calls = parse_tool_call_blocks(&text)?;
    let base = format!("{:x}", chrono::Utc::now().timestamp_millis());
    let arr: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(i, (name, args))| {
            json!({
                "id": format!("call_{}_{}", base, i),
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": serde_json::to_string(args).unwrap_or_else(|_| "{}".into()),
                }
            })
        })
        .collect();
    Some(Value::Array(arr))
}

/// 从回答内容（JSON 字符串 {THINK, RESPONSE} 或纯字符串）提取 RESPONSE 纯文本
fn extract_response_text(content: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(content) {
        if let Some(s) = v.as_str() {
            return s.to_string();
        }
        if let Some(r) = v.get("RESPONSE").and_then(|x| x.as_str()) {
            return r.to_string();
        }
    }
    content.to_string()
}

/// 返回前加「回答时间 + 会话」前缀
fn attach_answer_prefix(content: &str, ts: &str, session_id: &str) -> String {
    format!("回答时间: {}\n会话: {}\n{}", ts, session_id, content)
}

/// 估算文本对应的 token 数（DeepSeek 网页版不返回真实 usage，只能近似）：
/// 中文等 CJK 字符按 1 字 ≈ 1 token；其它字符按每 4 字符 ≈ 1 token（向上取整）。
fn estimate_tokens(text: &str) -> u32 {
    let mut cjk: u32 = 0;
    let mut other: u32 = 0;
    for c in text.chars() {
        if ('\u{4E00}'..='\u{9FFF}').contains(&c) || ('\u{3400}'..='\u{4DBF}').contains(&c) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + (other + 3) / 4
}

/// 组装 OpenAI 兼容的 usage JSON
fn make_usage(prompt_tokens: u32, completion_tokens: u32) -> Value {
    json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": prompt_tokens + completion_tokens,
    })
}

/// 递归收集 Value 中的所有字符串叶子并拼接（估算模型生成 {THINK,RESPONSE} 的输出 token 用）
fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(value_text).collect(),
        Value::Object(m) => m.values().map(value_text).collect(),
        _ => String::new(),
    }
}

/// 从流式转发增量（{"THINK":..} / {"RESPONSE":..} / {"tool_calls":..}）提取纯文本，用于累计输出 token。
/// 网关自加前缀「回答时间/会话」也是实际输出的 token，故保留不剥。
fn delta_text(d: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(d) {
        return value_text(&v);
    }
    d.to_string()
}

/* ------------------------------ SSE 解析器 ------------------------------ */

struct Fragment {
    ftype: String,
    content: String,
}

/// DeepSeek SSE 片段解析器（2026-08 网站协议）
struct DeepSeekParser {
    fragments: Vec<Fragment>,
    sent_length: Vec<usize>,
    hint_error: Option<String>,
    /// 最近一次响应的消息 id（供下轮 parent_message_id；比 fetch_history 更可靠）
    message_id: Option<i64>,
}

impl DeepSeekParser {
    fn new() -> Self {
        Self {
            fragments: Vec::new(),
            sent_length: Vec::new(),
            hint_error: None,
            message_id: None,
        }
    }

    fn handle_data(&mut self, json_str: &str) {
        let obj: Value = match serde_json::from_str(json_str) {
            Ok(v) => v,
            Err(_) => return,
        };
        if !obj.is_object() {
            return;
        }
        // 0.5) 顶层响应消息 id（首事件：{"request_message_id":7,"response_message_id":8,...}）
        if let Some(mid) = obj.get("response_message_id").and_then(|v| v.as_i64()) {
            self.message_id = Some(mid);
        }
        // 0) hint 错误（rate limit「消息发送过于频繁」等）：{"type":"error","content":"..."}
        if obj.get("type").and_then(|v| v.as_str()) == Some("error") {
            if let Some(c) = obj.get("content").and_then(|v| v.as_str()) {
                if self.hint_error.is_none() {
                    self.hint_error = Some(c.to_string());
                }
            }
            return;
        }
        // 1) 快照：{"v":{"response":{"fragments":[...]}}}（含 message_id / parent_id）
        if let Some(resp) = obj.get("v").and_then(|v| v.get("response")) {
            // 捕获响应消息 id：作为下轮 parent_message_id（工具结果续接上一条 AI 回复）
            if let Some(mid) = resp.get("message_id").and_then(|v| v.as_i64()) {
                self.message_id = Some(mid);
            }
            if let Some(frags) = resp.get("fragments").and_then(|f| f.as_array()) {
                for (i, f) in frags.iter().enumerate() {
                    let ftype = f.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let content = f.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    if i >= self.fragments.len() {
                        self.fragments.push(Fragment { ftype, content });
                    } else {
                        self.fragments[i].ftype = ftype;
                        self.fragments[i].content = content;
                    }
                }
            }
            return;
        }
        // 2) 新片段追加：{"p":"response/fragments","o":"APPEND","v":[...]}
        if obj.get("p").and_then(|v| v.as_str()) == Some("response/fragments")
            && obj.get("o").and_then(|v| v.as_str()) == Some("APPEND")
        {
            if let Some(arr) = obj.get("v").and_then(|v| v.as_array()) {
                for f in arr {
                    if f.is_object() {
                        self.fragments.push(Fragment {
                            ftype: f.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                            content: f.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        });
                    }
                }
            }
            return;
        }
        // 3) 片段内容追加：{"p":"response/fragments/<idx>/content","o":"APPEND"|"SET","v":"..."}
        if let Some(p) = obj.get("p").and_then(|v| v.as_str()) {
            if let Some(v) = obj.get("v").and_then(|v| v.as_str()) {
                if let Some(idx_pos) = p.find("response/fragments/") {
                    let rest = &p[idx_pos + "response/fragments/".len()..];
                    if let Some(content_pos) = rest.find("/content") {
                        let idx_str = &rest[..content_pos];
                        if let Ok(mut idx) = idx_str.parse::<i64>() {
                            if idx == -1 {
                                idx = (self.fragments.len() as i64) - 1;
                            }
                            let op = obj.get("o").and_then(|v| v.as_str()).unwrap_or("APPEND").to_uppercase();
                            if idx >= 0 {
                                let idx = idx as usize;
                                if idx >= self.fragments.len() {
                                    self.fragments.push(Fragment { ftype: String::new(), content: String::new() });
                                }
                                if op == "APPEND" {
                                    self.fragments[idx].content.push_str(v);
                                } else {
                                    self.fragments[idx].content = v.to_string();
                                }
                            }
                        }
                    }
                }
                return;
            }
        }
        // 4) 纯文本 v（无路径）
        if let Some(v) = obj.get("v").and_then(|v| v.as_str()) {
            if let Some(last) = self.fragments.last_mut() {
                last.content.push_str(v);
            } else {
                self.fragments.push(Fragment { ftype: String::new(), content: v.to_string() });
            }
        }
    }

    fn process_line(&mut self, line: &str) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        if let Some(rest) = trimmed.strip_prefix("data:") {
            let json_str = rest.trim();
            if !json_str.is_empty() && json_str != "[DONE]" {
                self.handle_data(json_str);
            }
        }
    }

    /// 返回自上次以来的新增增量（按片段 type 分组）
    fn answer(&mut self) -> Value {
        let has_typed = self.fragments.iter().any(|f| !f.ftype.is_empty());
        if !has_typed {
            let all: String = self.fragments.iter().map(|f| f.content.as_str()).collect();
            if all.is_empty() {
                return Value::Null;
            }
            return Value::String(all);
        }
        let mut chunks: HashMap<String, String> = HashMap::new();
        for (i, f) in self.fragments.iter().enumerate() {
            if i >= self.sent_length.len() {
                self.sent_length.push(0);
            }
            if f.content.len() > self.sent_length[i] {
                let delta = &f.content[self.sent_length[i]..];
                chunks.entry(f.ftype.clone()).or_default().push_str(delta);
                self.sent_length[i] = f.content.len();
            }
        }
        let mut obj = serde_json::Map::new();
        for (k, v) in chunks {
            if !v.is_empty() {
                obj.insert(k, Value::String(v));
            }
        }
        if obj.is_empty() {
            Value::Null
        } else {
            Value::Object(obj)
        }
    }

    /// 完整内容（忽略 sent_length）
    #[allow(dead_code)]
    fn full_answer(&self) -> Value {
        let has_typed = self.fragments.iter().any(|f| !f.ftype.is_empty());
        if !has_typed {
            let all: String = self.fragments.iter().map(|f| f.content.as_str()).collect();
            if all.is_empty() {
                return Value::Null;
            }
            return Value::String(all);
        }
        let mut chunks: HashMap<String, String> = HashMap::new();
        for f in &self.fragments {
            if !f.content.is_empty() {
                chunks.entry(f.ftype.clone()).or_default().push_str(&f.content);
            }
        }
        let mut obj = serde_json::Map::new();
        for (k, v) in chunks {
            obj.insert(k, Value::String(v));
        }
        Value::Object(obj)
    }

    /// 取出 SSE hint 错误（如「消息发送过于频繁」rate_limit）
    fn take_hint_error(&mut self) -> Option<String> {
        self.hint_error.take()
    }

    /// 最近一次响应的消息 id（供下轮 parent_message_id；比 fetch_history 更可靠）
    fn message_id(&self) -> Option<i64> {
        self.message_id
    }
}

/* ------------------------------ DeepSeekService ------------------------------ */

pub struct DeepSeekService {
    bridge: Arc<WebSocketBridge>,
    credentials: Arc<CredentialStore>,
    timeout_ms: u64,
    last_send_at: StdMutex<u64>,
    current_session_id: StdMutex<Option<String>>,
    session_map: StdMutex<HashMap<String, i64>>,
    #[allow(dead_code)]
    last_raw_snippet: StdMutex<String>,
    /// 会话管理（记录原始/转换后对话，可按 key 查看）
    session_manager: Option<Arc<SessionManager>>,
}

/// 构建好的请求结果
struct BuiltRequest {
    url: String,
    headers: Vec<(String, String)>,
    body: Value,
    is_new_session: bool,
    /// 是否首次对话（没有 assistant 消息）：仅在首次回答加「回答时间 + 会话」前缀
    is_first_turn: bool,
    chat_session_id: String,
    /// 转换后的提示词（实际发给网页平台的对话）
    prompt: String,
}

impl DeepSeekService {
    pub fn new(
        bridge: Arc<WebSocketBridge>,
        credentials: Arc<CredentialStore>,
        timeout_ms: u64,
        session_manager: Option<Arc<SessionManager>>,
    ) -> Self {
        Self {
            bridge,
            credentials,
            timeout_ms,
            last_send_at: StdMutex::new(0),
            current_session_id: StdMutex::new(None),
            session_map: StdMutex::new(HashMap::new()),
            last_raw_snippet: StdMutex::new(String::new()),
            session_manager,
        }
    }

    /* ------------------------- 凭证与节流 ------------------------- */

    async fn ensure_token(&self) -> Result<String> {
        if let Some(token) = self.credentials.get_token(PROVIDER_ID) {
            return Ok(token);
        }
        info!("未找到凭证，触发捕获");
        let _ = self.bridge.request_credentials(PROVIDER_ID, 10_000).await;
        self.credentials
            .get_token(PROVIDER_ID)
            .ok_or_else(|| anyhow!("未获取到 DeepSeek 访问凭证：请先在浏览器打开 chat.deepseek.com 并登录，且插件已连接"))
    }

    /// 节流：保证两次请求间隔 ≥ 1s
    async fn throttle_send(&self) {
        let now = chrono::Utc::now().timestamp_millis() as u64;
        // 计算等待时长后立即释放锁（MutexGuard 非 Send，不能跨 await 持有）
        let wait = {
            let mut last = self.last_send_at.lock().unwrap();
            let w = SEND_INTERVAL_MS.saturating_sub(now.saturating_sub(*last));
            *last = chrono::Utc::now().timestamp_millis() as u64;
            w
        };
        if wait > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(wait)).await;
        }
    }

    /* ------------------------- 执行通道 ------------------------- */

    async fn execute_once(
        &self,
        method: &str,
        url: &str,
        headers: Vec<(String, String)>,
        body: Option<String>,
        timeout_ms: u64,
    ) -> Result<crate::extension::websocket::ExecuteResultPayload> {
        self.throttle_send().await;
        let payload = ExecuteRequestPayload {
            provider: PROVIDER_ID.to_string(),
            url: url.to_string(),
            method: method.to_string(),
            headers,
            body: body.unwrap_or_default(),
        };
        let res = self.bridge.send_execute(payload, timeout_ms, None).await?;
        if url != DEEPSEEK_API_URL && !res.ok {
            warn!(
                "辅助接口失败 {} {} -> {} body={}",
                method,
                url,
                res.status,
                res.body.chars().take(200).collect::<String>()
            );
        }
        Ok(res)
    }

    async fn execute_streaming(
        &self,
        method: &str,
        url: &str,
        headers: Vec<(String, String)>,
        body: String,
        on_chunk: Box<dyn FnMut(String) + Send + Sync>,
    ) -> Result<crate::extension::websocket::ExecuteResultPayload> {
        self.throttle_send().await;
        let payload = ExecuteRequestPayload {
            provider: PROVIDER_ID.to_string(),
            url: url.to_string(),
            method: method.to_string(),
            headers,
            body,
        };
        self.bridge.send_execute(payload, self.timeout_ms, Some(on_chunk)).await
    }

    /// DeepSeek 默认请求头
    fn default_headers(&self, token: &str, settings_token: Option<&str>) -> Vec<(String, String)> {
        let mut h = vec![
            ("x-client-bundle-id".to_string(), "com.deepseek.chat".to_string()),
            ("x-client-platform".to_string(), "web".to_string()),
            ("x-client-version".to_string(), "2.4.0".to_string()),
            ("x-client-locale".to_string(), "zh_CN".to_string()),
            ("x-client-timezone-offset".to_string(), "28800".to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Authorization".to_string(), format!("Bearer {}", token)),
        ];
        if let Some(st) = settings_token {
            h.push(("x-settings-token".to_string(), st.to_string()));
        }
        h
    }

    /* ------------------------- PoW ------------------------- */

    async fn get_pow_header(&self, token: &str, settings_token: Option<&str>) -> Result<Option<String>> {
        for attempt in 1..=2u32 {
            let res = self
                .execute_once(
                    "POST",
                    POW_CHALLENGE_URL,
                    self.default_headers(token, settings_token),
                    Some(json!({ "target_path": "/api/v0/chat/completion" }).to_string()),
                    AUX_TIMEOUT,
                )
                .await;
            let res = match res {
                Ok(r) => r,
                Err(e) => {
                    warn!("PoW 流程失败（第 {} 次）: {}", attempt, e);
                    continue;
                }
            };
            if !res.ok || res.body.is_empty() {
                warn!("PoW 挑战获取失败(HTTP {}) 第{}次", res.status, attempt);
                continue;
            }
            let json: Value = match serde_json::from_str(&res.body) {
                Ok(v) => v,
                Err(_) => {
                    warn!("PoW 挑战响应非 JSON");
                    continue;
                }
            };
            let ch = json
                .get("data")
                .and_then(|d| d.get("biz_data"))
                .and_then(|b| b.get("challenge"))
                .or_else(|| json.get("data").and_then(|d| d.get("challenge")))
                .or_else(|| json.get("challenge"))
                .cloned();
            let ch = match ch {
                Some(c) if c.is_object() => c,
                _ => {
                    warn!("PoW 挑战响应缺少 challenge 字段");
                    continue;
                }
            };
            let algorithm = ch.get("algorithm").and_then(|v| v.as_str()).unwrap_or("DeepSeekHashV1").to_string();
            let challenge = ch.get("challenge").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let salt = ch.get("salt").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let expire_at = ch
                .get("expire_at")
                .or_else(|| ch.get("expireAt"))
                .or_else(|| ch.get("expires_at"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let difficulty = ch.get("difficulty").and_then(|v| v.as_u64()).unwrap_or(144000).max(1);
            let signature = ch.get("signature").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let target_path = ch
                .get("target_path")
                .and_then(|v| v.as_str())
                .unwrap_or("/api/v0/chat/completion")
                .to_string();
            let start = std::time::Instant::now();
            let answer = solve_pow(&challenge, &salt, expire_at, difficulty);
            debug!("PoW solved in {:?} difficulty={}", start.elapsed(), difficulty);
            if answer < 0 {
                warn!("PoW 求解失败（difficulty 范围内未找到答案）");
                continue;
            }
            return Ok(Some(build_pow_header(&algorithm, &challenge, &salt, answer, &signature, &target_path)));
        }
        Ok(None)
    }

    /* ------------------------- 会话管理 ------------------------- */
    /// 创建会话（返回会话 id 或 None）
    async fn create_session(&self, token: &str, settings_token: Option<&str>) -> Result<Option<String>> {
        let res = self
            .execute_once(
                "POST",
                DEEPSEEK_SESSION_URL,
                self.default_headers(token, settings_token),
                Some("{}".to_string()),
                AUX_TIMEOUT,
            )
            .await?;
        if !res.ok || res.body.is_empty() {
            return Ok(None);
        }
        let json: Value = match serde_json::from_str(&res.body) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        let data = json.get("data");
        let candidates = [
            data.and_then(|d| d.get("biz_data")).and_then(|b| b.get("chat_session")).and_then(|s| s.get("id")),
            data.and_then(|d| d.get("biz_data")).and_then(|b| b.get("chat_session_id")),
            data.and_then(|d| d.get("biz_data")).and_then(|b| b.get("id")),
            data.and_then(|d| d.get("biz_data")).and_then(|b| b.get("session_id")),
            data.and_then(|d| d.get("chat_session_id")),
            data.and_then(|d| d.get("id")),
            data.and_then(|d| d.get("session_id")),
            json.get("chat_session_id"),
        ];
        for c in candidates {
            if let Some(s) = c.and_then(|v| v.as_str()) {
                if s.len() > 10 {
                    debug!("创建会话 {}", s);
                    return Ok(Some(s.to_string()));
                }
            }
        }
        Ok(None)
    }
    /// 获取会话历史（用于获取 current_message_id，便于后续继续对话）
    async fn fetch_history(&self, token: &str, session_id: &str) {
        let url = format!(
            "{}?chat_session_id={}&cache_version=8",
            DEEPSEEK_HISTORY_URL,
            urlencoding(session_id)
        );
        if let Ok(res) = self
            .execute_once("GET", &url, self.default_headers(token, None), None, AUX_TIMEOUT)
            .await
        {
            if res.ok {
                if let Ok(json) = serde_json::from_str::<Value>(&res.body) {
                    if let Some(id) = json
                        .get("data")
                        .and_then(|d| d.get("biz_data"))
                        .and_then(|b| b.get("chat_session"))
                        .and_then(|s| s.get("current_message_id"))
                        .and_then(|v| v.as_i64())
                    {
                        self.session_map.lock().unwrap().insert(session_id.to_string(), id);
                    }
                }
            }
        }
    }

    async fn update_session_title(&self, token: &str, settings_token: Option<&str>, session_id: &str) {
        for attempt in 1..=2u32 {
            let res = self
                .execute_once(
                    "POST",
                    DEEPSEEK_TITLE_URL,
                    self.default_headers(token, settings_token),
                    Some(json!({ "chat_session_id": session_id, "title": session_id }).to_string()),
                    AUX_TIMEOUT,
                )
                .await;
            match res {
                Ok(r) if r.ok => {
                    let mut biz_ok = false;
                    if let Ok(j) = serde_json::from_str::<Value>(&r.body) {
                        biz_ok = j.get("code").and_then(|v| v.as_i64()) == Some(0)
                            && j.get("data").and_then(|d| d.get("biz_code")).and_then(|v| v.as_i64()) == Some(0);
                    }
                    if biz_ok || r.body.is_empty() {
                        info!("已将会话标题设为会话 id {}", session_id);
                        return;
                    }
                    warn!(
                        "设置会话标题业务失败 session={} body={}",
                        session_id,
                        r.body.chars().take(200).collect::<String>()
                    );
                }
                Ok(r) => {
                    warn!("设置会话标题失败(HTTP {}) session={}", r.status, session_id);
                }
                Err(e) => {
                    warn!("设置会话标题异常（第 {} 次）: {}", attempt, e);
                }
            }
            if attempt < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
        }
    }

    /* ------------------------- 请求组装（提示词转换见 converter 模块） ------------------------- */

    /// 组装请求：会话选择 + 工具段 + body
    async fn build_request(&self, messages: &[Value], options: &Value) -> Result<BuiltRequest> {
        let token = self.ensure_token().await?;
        let settings_token = self.credentials.get_settings_token(PROVIDER_ID);
        let tools = options.get("tools").cloned();
        let _has_tools = tools.as_ref().map_or(false, |t| t.as_array().map_or(false, |a| !a.is_empty()));
        // 是否首次对话（没有 assistant 消息）
        let is_first_turn = !messages
            .iter()
            .any(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"));
        let fixed_session = options.get("sessionId").and_then(|v| v.as_str()).map(String::from);

        let chat_session_id: String;
        let is_new_session: bool;
        if let Some(sid) = fixed_session {
            chat_session_id = sid;
            is_new_session = false;
        } else if let Some(sid) = extract_session_id_from_history(messages) {
            chat_session_id = sid;
            is_new_session = false;
        } else if !is_first_turn {
            let fallback = self.current_session_id.lock().unwrap().clone();
            chat_session_id = match fallback {
                Some(s) => s,
                None => self
                    .create_session(&token, settings_token.as_deref())
                    .await?
                    .unwrap_or_else(|| uuid()),
            };
            is_new_session = false;
        } else {
            chat_session_id = self
                .create_session(&token, settings_token.as_deref())
                .await?
                .unwrap_or_else(|| uuid());
            is_new_session = true;
        }
        self.fetch_history(&token, &chat_session_id).await;
        *self.current_session_id.lock().unwrap() = Some(chat_session_id.clone());

        // 中间转换器：按 API Key 绑定的平台选择（默认 vscode-copilot）
        let platform = options
            .get("converterPlatform")
            .and_then(|v| v.as_str())
            .unwrap_or(crate::converter::vscode_copilot::PLATFORM);
        let converter = crate::converter::resolve(platform)
            .ok_or_else(|| anyhow!("未知转换平台: {}", platform))?;
        let ctx = crate::converter::ConvertContext {
            messages,
            tools: tools.as_ref(),
            is_first_turn,
        };
        // 所有 Key 一律走平台转换器（按平台拆分重建提示词）
        let prompt = converter.convert(&ctx);

        let pow_header = self.get_pow_header(&token, settings_token.as_deref()).await?;

        let mut headers = self.default_headers(&token, settings_token.as_deref());
        if let Some(pow) = pow_header {
            headers.push(("x-ds-pow-response".to_string(), pow));
        }

        let parent_message_id = if is_new_session {
            Value::Null
        } else {
            let id = self.session_map.lock().unwrap().get(&chat_session_id).copied().unwrap_or(0);
            if id == 0 {
                Value::Null
            } else {
                Value::Number(id.into())
            }
        };

        let body = json!({
            "chat_session_id": chat_session_id,
            "parent_message_id": parent_message_id,
            "model_type": DEEPSEEK_MODEL_TYPE,
            "prompt": prompt,
            "ref_file_ids": [],
            "thinking_enabled": DEEPSEEK_THINK,
            "search_enabled": false,
            "action": null,
            "preempt": false,
        });

        Ok(BuiltRequest {
            url: DEEPSEEK_API_URL.to_string(),
            headers,
            body,
            is_new_session,
            is_first_turn,
            chat_session_id,
            prompt,
        })
    }

    /* ------------------------- 会话记录（会话管理模块） ------------------------- */

    /// 记录会话与对话（原始 + 转换后），供会话管理模块按 key 查看
    async fn record_session(&self, built: &BuiltRequest, messages: &[Value], options: &Value) {
        if let Some(sm) = &self.session_manager {
            let api_key = options
                .get("apiKey")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let title =
                Self::session_title(messages).unwrap_or_else(|| built.chat_session_id.clone());
            // 网页平台会话 id 作为 web_session_id；工具自身 id（UUIDv7）由 upsert 分配
            if let Ok(sid) = sm
                .upsert_session(&built.chat_session_id, &title, PROVIDER_ID, api_key)
                .await
            {
                // 原始对话（OpenAI 格式）
                for m in messages {
                    if let (Some(role), Some(content)) = (
                        m.get("role").and_then(|v| v.as_str()),
                        m.get("content").and_then(|v| v.as_str()),
                    ) {
                        let _ = sm.add_raw_message(&sid, role, content).await;
                    }
                }
                // 转换后的实际对话（转换器输出的提示词）
                let _ = sm.add_converted_message(&sid, "user", &built.prompt).await;
            }
        }
    }

    /// 从消息里提取简短标题（最后一条 user 内容前 40 字）
    fn session_title(messages: &[Value]) -> Option<String> {
        for m in messages.iter().rev() {
            if m.get("role").and_then(|v| v.as_str()) == Some("user") {
                if let Some(c) = m.get("content").and_then(|v| v.as_str()) {
                    if !c.is_empty() {
                        let t: String = c.chars().take(40).collect();
                        return Some(t);
                    }
                }
            }
        }
        None
    }

    /* ------------------------- 执行 + 422 自愈 ------------------------- */

    /// 执行非流式请求（含 422 自愈 / 业务错误 / 空响应）
    async fn execute_with_retry(
        &self,
        url: &str,
        headers: Vec<(String, String)>,
        body: &Value,
    ) -> Result<Value> {
        let mut current_body = body.clone();
        for attempt in 1..=MAX_ATTEMPTS {
            let body_str = serde_json::to_string(&current_body)?;
            let res = self
                .execute_once("POST", url, headers.clone(), Some(body_str), self.timeout_ms)
                .await?;

            if res.status == 401 || res.status == 403 {
                return Err(anyhow!("DeepSeek 登录状态失效或未登录：请先在浏览器打开 chat.deepseek.com 并登录"));
            }
            if res.status == 422 {
                if let Some(missing) = parse_missing_field(&res.body) {
                    if attempt < MAX_ATTEMPTS {
                        let val = guess_default(&missing);
                        if let Some(obj) = current_body.as_object_mut() {
                            obj.insert(missing.clone(), val.clone());
                        }
                        warn!("自动补全字段 {}={} 后重试(第{}次)", missing, val, attempt + 1);
                        continue;
                    }
                }
                return Err(anyhow!("DeepSeek API 请求失败 (422): {}", res.body.chars().take(300).collect::<String>()));
            }
            if !res.ok {
                return Err(anyhow!(
                    "DeepSeek API 请求失败 ({}): {}",
                    res.status,
                    res.body.chars().take(200).collect::<String>()
                ));
            }
            if let Some((_, msg)) = parse_biz_error(&res.body) {
                warn!("DeepSeek 业务错误：{}", msg);
                return Err(anyhow!("DeepSeek 业务错误: {}", msg));
            }

            let mut parser = DeepSeekParser::new();
            let mut line_buf = String::new();
            line_buf.push_str(&res.body);
            while let Some(nl) = line_buf.find('\n') {
                let line = line_buf[..nl].to_string();
                parser.process_line(&line);
                line_buf.drain(..nl + 1);
            }
            if !line_buf.trim().is_empty() {
                parser.process_line(&line_buf);
            }

            // hint 错误识别（rate limit 等）→ 明确报错，避免误报空响应
            if let Some(hint) = parser.take_hint_error() {
                return Err(anyhow!("DeepSeek 提示: {}", hint));
            }
            let content = parser.answer();
            if content.is_null()
                || content.as_str().map_or(false, |s| s.is_empty())
                || content.as_object().map_or(false, |o| o.is_empty())
            {
                error!("DeepSeek 解析结果为空（deepseek_empty_response） status={}", res.status);
                return Err(anyhow!(
                    "DeepSeek 未返回内容（status={}，原始响应: {}）",
                    res.status,
                    res.body.chars().take(200).collect::<String>()
                ));
            }
            // 捕获响应消息 id → 更新 session_map，供下轮 parent_message_id 使用
            if let Some(mid) = parser.message_id() {
                if mid > 0 {
                    if let Some(sid) = current_body.get("chat_session_id").and_then(|v| v.as_str()) {
                        self.session_map.lock().unwrap().insert(sid.to_string(), mid);
                    }
                }
            }
            return Ok(content);
        }
        Err(anyhow!("DeepSeek 请求重试次数用尽"))
    }

    /* ------------------------- 对外接口 ------------------------- */

    /// 非流式聊天
    pub async fn chat(&self, messages: Vec<Value>, options: Option<Value>) -> Result<ChatResponsePayload> {
        let options = options.unwrap_or_else(|| json!({}));
        // 是否带工具（用于告警观察模型是否未按提示词围栏格式输出）
        let has_tools = options
            .get("tools")
            .and_then(|t| t.as_array())
            .map_or(false, |a| !a.is_empty());
        let built = self.build_request(&messages, &options).await?;
        self.record_session(&built, &messages, &options).await;
        let ts = format_ts_ms();
        let session_id = built.chat_session_id.clone();

        let content = self
            .execute_with_retry(&built.url, built.headers.clone(), &built.body)
            .await?;
        // DeepSeek 网页版不返回 usage → 估算：输入=发给平台的提示词，输出=模型生成的全部内容
        let usage = {
            let prompt_tokens = estimate_tokens(&built.prompt);
            let completion_tokens = estimate_tokens(&value_text(&content));
            info!(session = %session_id, prompt_tokens, completion_tokens, total = prompt_tokens + completion_tokens, "估算 token 用量（DeepSeek 网页版无真实 usage）");
            Some(make_usage(prompt_tokens, completion_tokens))
        };
        if built.is_new_session {
            let token = self.ensure_token().await?;
            let settings_token = self.credentials.get_settings_token(PROVIDER_ID);
            self.update_session_title(&token, settings_token.as_deref(), &session_id).await;
        }
        let text = extract_response_text(&content.to_string());
        if let Some(tool_calls) = parse_tool_calls(&text) {
            info!("检测到工具调用");
            return Ok(ChatResponsePayload {
                content: String::new(),
                model: PROVIDER_ID.to_string(),
                usage: usage.clone(),
                finished_reason: Some("tool_calls".to_string()),
                tool_calls: Some(tool_calls),
            });
        }
        if has_tools && looks_like_unparsed_tool_call(&text) {
            warn!(
                "本次请求带工具，但未解析出工具调用（模型可能未按提示词围栏格式输出）: {}",
                text.chars().take(300).collect::<String>()
            );
        }
        // 只在首次回答添加「回答时间 + 会话」前缀；追问通过反向查找历史里的前缀定位会话
        let final_content = if built.is_first_turn {
            attach_answer_prefix(&text, &ts, &session_id)
        } else {
            text
        };
        Ok(ChatResponsePayload {
            content: final_content,
            model: PROVIDER_ID.to_string(),
            usage,
            finished_reason: Some("stop".to_string()),
            tool_calls: None,
        })
    }

    /// 流式聊天
    pub async fn chat_stream<F>(
        &self,
        messages: Vec<Value>,
        options: Option<Value>,
        on_chunk: F,
    ) -> Result<ChatResponsePayload>
    where
        F: FnMut(String) + Send + Sync + 'static,
    {
        let options = options.unwrap_or_else(|| json!({}));
        let built = self.build_request(&messages, &options).await?;
        self.record_session(&built, &messages, &options).await;
        let ts = format_ts_ms();
        let session_id = built.chat_session_id.clone();
        // 只在首次回答带前缀；追问不再带（通过反向查找历史前缀定位会话）
        let prefix = if built.is_first_turn {
            format!("回答时间: {}\n会话: {}\n", ts, session_id)
        } else {
            String::new()
        };
        let has_tools = options
            .get("tools")
            .map_or(false, |t| t.as_array().map_or(false, |a| !a.is_empty()));
        // 累计本次流式实际输出的文本，用于估算输出 token（DeepSeek 网页版无真实 usage）
        let completion_buf = Arc::new(StdMutex::new(String::new()));

        let mut current_body = built.body.clone();
        let mut deltas_out: Vec<String> = Vec::new();
        let mut attempt = 0usize;
        // 共享 on_chunk：WS 回调线程实时转发增量（THINK/RESPONSE 逐块流式），
        // 流结束再用同一入口转发收尾增量（工具调用 / 嗅探回退补发），避免重复发送
        let on_chunk = Arc::new(StdMutex::new(on_chunk));

        // 流式执行 + 422 自愈：每次重试重置 driver（避免残留增量）
        loop {
            attempt += 1;
            let driver = Arc::new(StdMutex::new(StreamProcessor::new(has_tools, prefix.clone())));

            let on_chunk_cb: Box<dyn FnMut(String) + Send + Sync> = {
                let driver_cb = driver.clone();
                let on_chunk_cb = on_chunk.clone();
                let completion_cb = Arc::clone(&completion_buf);
                Box::new(move |data: String| {
                    let deltas = {
                        let mut d = driver_cb.lock().unwrap();
                        d.feed(&data)
                    };
                    // 实时转发到客户端（THINK / 普通回答逐字流式），同时累计输出文本用于估算 token
                    if !deltas.is_empty() {
                        let mut oc = on_chunk_cb.lock().unwrap();
                        for d in deltas {
                            completion_cb.lock().unwrap().push_str(&delta_text(&d));
                            oc(d);
                        }
                    }
                })
            };

            let body_str = serde_json::to_string(&current_body)?;
            let res = self
                .execute_streaming("POST", &built.url, built.headers.clone(), body_str, on_chunk_cb)
                .await?;

            if res.status == 401 || res.status == 403 {
                return Err(anyhow!("DeepSeek 登录状态失效或未登录"));
            }
            // 422 自愈：补缺失字段后重试
            if res.status == 422 && attempt < MAX_ATTEMPTS {
                if let Some(missing) = parse_missing_field(&res.body) {
                    let val = guess_default(&missing);
                    if let Some(obj) = current_body.as_object_mut() {
                        obj.insert(missing.clone(), val.clone());
                    }
                    warn!("流式自动补全字段 {}={} 后重试(第{}次)", missing, val, attempt + 1);
                    continue;
                }
            }
            if !res.ok {
                return Err(anyhow!("DeepSeek API 请求失败 ({}): {}", res.status, res.body));
            }

            // 收尾增量（工具模式解析 / 嗅探补发）
            let final_deltas = driver.lock().unwrap().finish();
            // 转发收尾增量（工具调用 → tool_calls delta；嗅探回退 → 补发文本）
            if !final_deltas.is_empty() {
                let mut oc = on_chunk.lock().unwrap();
                for d in &final_deltas {
                    completion_buf.lock().unwrap().push_str(&delta_text(d));
                    oc(d.clone());
                }
            }
            deltas_out.extend(final_deltas);
            // 捕获响应消息 id → 更新 session_map，供下轮 parent_message_id 使用
            if let Some(mid) = driver.lock().unwrap().message_id() {
                if mid > 0 {
                    self.session_map.lock().unwrap().insert(session_id.clone(), mid);
                }
            }
            // hint 错误识别（rate limit 等）→ 明确报错
            if let Some(hint) = driver.lock().unwrap().take_hint_error() {
                return Err(anyhow!("DeepSeek 提示: {}", hint));
            }
            break;
        }

        let mut tool_calls: Option<Value> = None;
        for d in deltas_out {
            if let Ok(v) = serde_json::from_str::<Value>(&d) {
                if let Some(tc) = v.get("tool_calls").cloned() {
                    tool_calls = Some(tc);
                }
            }
        }

        if built.is_new_session {
            let token = self.ensure_token().await?;
            let settings_token = self.credentials.get_settings_token(PROVIDER_ID);
            self.update_session_title(&token, settings_token.as_deref(), &session_id).await;
        }

        // 估算 token：输入=发给平台的提示词，输出=本次流式实际输出的全部文本
        let prompt_tokens = estimate_tokens(&built.prompt);
        let completion_tokens = estimate_tokens(&completion_buf.lock().unwrap());
        info!(session = %session_id, prompt_tokens, completion_tokens, total = prompt_tokens + completion_tokens, "估算 token 用量（DeepSeek 网页版无真实 usage）");
        let usage = Some(make_usage(prompt_tokens, completion_tokens));

        if let Some(tc) = tool_calls {
            return Ok(ChatResponsePayload {
                content: String::new(),
                model: PROVIDER_ID.to_string(),
                usage,
                finished_reason: Some("tool_calls".to_string()),
                tool_calls: Some(tc),
            });
        }
        Ok(ChatResponsePayload {
            content: String::new(),
            model: PROVIDER_ID.to_string(),
            usage,
            finished_reason: Some("stop".to_string()),
            tool_calls: None,
        })
    }
}

/* ------------------------------ 流式驱动 ------------------------------ */

#[derive(PartialEq, Clone, Copy, Debug)]
enum ToolMode {
    Sniff,
    Tool,
    Stream,
}

struct StreamProcessor {
    parser: DeepSeekParser,
    line_buf: String,
    mode: ToolMode,
    sniff_buf: String,
    resp_buf: String,
    prefix: String,
    prefix_sent: bool,
    /// 是否携带工具（决定是否兜底解析思考中的工具调用）
    has_tools: bool,
    /// 累积的 THINK 思考内容（兜底解析工具调用用）
    think_buf: String,
}

impl StreamProcessor {
    fn new(has_tools: bool, prefix: String) -> Self {
        Self {
            parser: DeepSeekParser::new(),
            line_buf: String::new(),
            mode: if has_tools { ToolMode::Sniff } else { ToolMode::Stream },
            sniff_buf: String::new(),
            resp_buf: String::new(),
            prefix,
            prefix_sent: false,
            has_tools,
            think_buf: String::new(),
        }
    }

    /// 喂原始文本块，返回要 on_chunk 的 JSON 增量列表
    fn feed(&mut self, data: &str) -> Vec<String> {
        self.line_buf.push_str(data);
        let mut out = Vec::new();
        while let Some(nl) = self.line_buf.find('\n') {
            let line = self.line_buf[..nl].to_string();
            self.line_buf.drain(..nl + 1);
            self.parser.process_line(&line);
            self.drain(&mut out);
        }
        out
    }

    fn drain(&mut self, out: &mut Vec<String>) {
        let delta = self.parser.answer();
        if delta.is_null() {
            return;
        }
        if let Some(t) = delta.get("THINK").and_then(|v| v.as_str()) {
            if !t.is_empty() {
                // 累积思考内容：供 finish 兜底解析（模型可能在思考里直接写 <tool_call> 而 RESPONSE 为空）
                self.think_buf.push_str(t);
                out.push(json!({ "THINK": t }).to_string());
            }
        }
        let resp = if delta.is_string() {
            delta.as_str().unwrap_or("").to_string()
        } else {
            delta.get("RESPONSE").and_then(|v| v.as_str()).unwrap_or("").to_string()
        };
        if resp.is_empty() {
            return;
        }
        match self.mode {
            ToolMode::Stream => {
                // 普通文本模式：实时输出，但持续检测是否开始工具调用（普通文本可夹杂工具）
                if looks_like_tool_call(&resp) {
                    // 出现完整工具标识 → 切 Tool 模式
                    self.mode = ToolMode::Tool;
                    self.resp_buf.push_str(&resp);
                } else if tool_fence_remaining(&resp).is_some() {
                    // 出现 <tool_call> 开围栏前缀（可能被增量拆分）→ 切 Sniff 缓冲确认
                    self.mode = ToolMode::Sniff;
                    self.sniff_buf.push_str(&resp);
                } else {
                    // 普通文本，正常输出
                    let body = if self.prefix_sent {
                        resp
                    } else {
                        format!("{}{}", self.prefix, resp)
                    };
                    self.prefix_sent = true;
                    out.push(json!({ "RESPONSE": body }).to_string());
                }
            }
            ToolMode::Tool => {
                self.resp_buf.push_str(&resp);
            }
            ToolMode::Sniff => {
                self.sniff_buf.push_str(&resp);
                if looks_like_tool_call(&self.sniff_buf) {
                    // 检测到完整工具标识 → 切到 Tool 模式，等完整工具调用接收完再按工具返回
                    self.mode = ToolMode::Tool;
                    self.resp_buf.push_str(&self.sniff_buf);
                    self.sniff_buf.clear();
                } else if tool_fence_remaining(&self.sniff_buf).is_some() {
                    // 处于 <tool_call> 开围栏前缀阶段（增量拆分中）→ 继续缓冲等待补齐
                } else {
                    // 不是围栏前缀 → 回退为普通文本输出（后续若再出现工具标识，Stream 模式仍会检测）
                    self.mode = ToolMode::Stream;
                    let body = if self.prefix_sent {
                        self.sniff_buf.clone()
                    } else {
                        format!("{}{}", self.prefix, self.sniff_buf)
                    };
                    self.prefix_sent = true;
                    out.push(json!({ "RESPONSE": body }).to_string());
                    self.sniff_buf.clear();
                }
            }
        }
    }

    /// 流结束：返回最终要发的增量
    fn finish(&mut self) -> Vec<String> {
        if !self.line_buf.trim().is_empty() {
            self.parser.process_line(&self.line_buf);
            self.line_buf.clear();
            let mut out = Vec::new();
            self.drain(&mut out);
            if !out.is_empty() {
                return out;
            }
        }
        let mut out = Vec::new();
        match self.mode {
            ToolMode::Tool => {
                let response_text = std::mem::take(&mut self.resp_buf);
                if let Some(calls) = parse_tool_calls(&response_text) {
                    out.push(json!({ "tool_calls": calls }).to_string());
                } else if !response_text.is_empty() {
                    // 已按工具模式缓冲却解析不出工具调用：模型很可能没按提示词的围栏格式输出
                    warn!(
                        "工具模式下未解析出工具调用，按普通文本回退（模型可能未按提示词格式输出）: {}",
                        response_text.chars().take(300).collect::<String>()
                    );
                    let body = if self.prefix_sent {
                        response_text
                    } else {
                        format!("{}{}", self.prefix, response_text)
                    };
                    self.prefix_sent = true;
                    out.push(json!({ "RESPONSE": body }).to_string());
                }
            }
            ToolMode::Sniff => {
                // 流结束时仍处于嗅探：尝试解析工具调用（可能围栏/特征被增量拆分未命中），失败则当普通文本
                let sniff = std::mem::take(&mut self.sniff_buf);
                if !sniff.is_empty() {
                    if let Some(calls) = parse_tool_calls(&sniff) {
                        out.push(json!({ "tool_calls": calls }).to_string());
                    } else {
                        if looks_like_unparsed_tool_call(&sniff) {
                            warn!(
                                "嗅探到像工具调用的内容但未解析成功（模型可能未按提示词格式输出）: {}",
                                sniff.chars().take(300).collect::<String>()
                            );
                        }
                        let body = if self.prefix_sent {
                            sniff
                        } else {
                            format!("{}{}", self.prefix, sniff)
                        };
                        self.prefix_sent = true;
                        out.push(json!({ "RESPONSE": body }).to_string());
                    }
                }
            }
            ToolMode::Stream => {}
        }
        // 兜底：模型可能在「思考(THINK)」里直接写出 <tool_call> 而 RESPONSE 为空，
        // 此时 RESPONSE 侧无任何输出(prefix_sent=false)、也无工具调用 →
        // 从累积的思考内容解析工具调用，避免「思考中有工具调用后就直接停止」。
        // 注意 prefix_sent=true（RESPONSE 已正常输出）时不解析——那是思考中对工具调用的描述，
        // 与 think_content_tool_call_not_converted 测试一致。
        if out.is_empty() && self.has_tools && !self.prefix_sent {
            let think = std::mem::take(&mut self.think_buf);
            if let Some(calls) = parse_tool_calls(&think) {
                out.push(json!({ "tool_calls": calls }).to_string());
            }
        }
        out
    }

    /// 取出 SSE hint 错误（rate limit 等）
    fn take_hint_error(&mut self) -> Option<String> {
        self.parser.take_hint_error()
    }

    /// 最近一次响应的消息 id（供下轮 parent_message_id）
    fn message_id(&self) -> Option<i64> {
        self.parser.message_id()
    }
}

/// 流式嗅探：判断累积内容中是否已出现【完整】工具调用标识（任意位置，普通文本后可夹杂）。
/// 覆盖：<tool_call> 开围栏（含属性形式 <tool_call name="X">）、XML 格式 <tool_calls> / <invoke name="X">、
/// YAML name: 开头（行首）。只用于"确定是工具"→ 切 Tool 模式；不完整的前缀由 tool_fence_remaining 处理。
fn looks_like_tool_call(buf: &str) -> bool {
    // <tool_call> / <toolcall> / <tool_call name="X"> 开围栏：普通文本夹杂时也能命中（不只开头）
    if contains_open_fence(buf) {
        return true;
    }
    // XML/DSML 工具调用格式：完整开标签（<invoke name="X">、<|DSML| invoke name="X"> 等）
    if contains_tool_open_tag(buf) {
        return true;
    }
    // YAML name: 开头（行首，前面的普通文本行不受影响）
    buf.lines()
        .any(|l| l.trim_start().starts_with("name:") || l.trim_start().starts_with("name："))
}

/// 检测是否出现【完整】开围栏（含 <tool_call> / <toolcall> / <tool_call name="X"> / <toolcall name='Y'>）
fn contains_open_fence(buf: &str) -> bool {
    for prefix in TOOL_FENCE_PREFIXES {
        let mut from = 0;
        while let Some(rel) = buf[from..].find(prefix) {
            let pos = from + rel;
            let rest = &buf[pos + prefix.len()..];
            // 要求能读到 '>'（完整开围栏），且 <tool_call> 或 <tool_call name=...>
            if let Some(gt) = rest.find('>') {
                let inner = rest[..gt].trim();
                if inner.is_empty() || inner.starts_with("name=") {
                    return true;
                }
            }
            from = pos + prefix.len();
        }
    }
    false
}

/// 判断累积内容【尾部】是否处于开围栏前缀阶段（自有 <tool_call> 或 XML 格式 <tool_calls>/<invoke>），
/// 并返回还差多少字符补齐完整开围栏（含 '>'）。
/// 例如 `<`、`<t`、`<to`、`<too`、`<tool_`、`<tool_c`、`<tool_call`、`<tool_calls`、`<invo` 等，都可能继续拼成完整开围栏。
/// 返回 None 表示不是开围栏前缀（普通文本或其它）。
/// 开围栏最小形态 = 前缀 + '>'，据此动态计算剩余待匹配长度。
fn tool_fence_remaining(buf: &str) -> Option<usize> {
    let bytes = buf.as_bytes();
    // 从右往左找最后一个 '<'，取其后的后缀作为候选开围栏前缀
    let mut start = None;
    for j in (0..bytes.len()).rev() {
        if bytes[j] == b'<' {
            start = Some(j);
            break;
        }
    }
    let Some(s) = start else {
        return None;
    };
    let tail = &buf[s..];
    // 匹配任意开围栏变体（<tool_call / <toolcall）：
    // - 短前缀：tail 是 prefix 的一部分 → 还差补齐 prefix + '>'
    // - 前缀已完整但属性进行中（如 <tool_call name="Wri）：也处于开围栏阶段 → 继续缓冲
    for prefix in TOOL_FENCE_PREFIXES {
        if prefix.starts_with(tail) && tail.len() < prefix.len() {
            return Some(prefix.len() - tail.len() + 1);
        }
        if tail.len() >= prefix.len() && tail.starts_with(prefix) {
            let after = &tail[prefix.len()..];
            if after.is_empty() || after.starts_with(' ') {
                return Some(1);
            }
            // after 以 '>' 开头 → 完整开围栏，由 looks_like_tool_call 判定
        }
    }
    // XML/DSML 工具调用格式的开标签（<tool_calls / <invoke / <|DSML| invoke 等）前缀：同样需缓冲
    xml_fence_remaining(tail)
}

/* ------------------------------ 其他辅助 ------------------------------ */

fn uuid() -> String {
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        randish(),
        randish() & 0xffff,
        randish() & 0x0fff,
        (randish() & 0x3fff) | 0x8000,
        randish()
    )
}

fn randish() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
    t.wrapping_mul(2654435761).rotate_left(13)
}

/// 从消息历史里提取最近一次 assistant 回答中的会话 id
fn extract_session_id_from_history(messages: &[Value]) -> Option<String> {
    for m in messages.iter().rev() {
        if m.get("role").and_then(|v| v.as_str()) == Some("assistant") {
            if let Some(c) = m.get("content").and_then(|v| v.as_str()) {
                if let Some(id) = extract_session_id(c) {
                    return Some(id);
                }
            }
        }
    }
    None
}

fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{parse_tool_block_yaml, parse_tool_calls, parse_tool_call_blocks};

    /// 新格式：<tool_call> 围栏内 YAML 块标量
    #[test]
    fn parse_yaml_tool_call_block() {
        let text = "我需要读取文件\n<tool_call>\nname: read_file\narguments:\n  filePath: /a/b.txt\n  startLine: 1\n  endLine: 10\n</tool_call>\n结束";
        let calls = parse_tool_call_blocks(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "read_file");
        assert_eq!(calls[0].1["filePath"], "/a/b.txt");
        assert_eq!(calls[0].1["startLine"], 1);
        assert_eq!(calls[0].1["endLine"], 10);
    }

    /// 新格式：多行参数用 |- 块标量原样直写（含代码/引号/反斜杠，不转义）
    #[test]
    fn parse_yaml_tool_call_multiline() {
        let text = "<tool_call>\nname: replace_string_in_file\narguments:\n  filePath: d:/proj/a.rs\n  oldString: |-\n    let x = \"hi\"; // 注释\n    let y = 'q';\n  newString: |-\n    let x = \"hello\";\n</tool_call>";
        let calls = parse_tool_call_blocks(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "replace_string_in_file");
        let old: &str = calls[0].1["oldString"].as_str().unwrap();
        assert!(old.contains("let x = \"hi\"; // 注释"));
        assert!(old.contains("let y = 'q';"));
        assert!(calls[0].1["newString"].as_str().unwrap().contains("let x = \"hello\";"));
    }

    /// 新格式：一次多个工具 → 多个 <tool_call> 块
    #[test]
    fn parse_yaml_multiple_tool_calls() {
        let text = "<tool_call>\nname: read_file\narguments:\n  filePath: a\n</tool_call>\n<tool_call>\nname: search\narguments:\n  query: x\n</tool_call>";
        let calls = parse_tool_call_blocks(text).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "read_file");
        assert_eq!(calls[1].0, "search");
        assert_eq!(calls[1].1["query"], "x");
    }

    /// 参数含三反引号（Markdown/mermaid 代码块）时仍能完整解析出工具调用
    #[test]
    fn parse_tool_call_args_with_code_fence() {
        let text = "<tool_call>\nname: replace_string_in_file\narguments:\n  filePath: d:/a/README.md\n  oldString: \"# AI Gateway\"\n  newString: |-\n    # AI Gateway 使用文档\n    ```mermaid\n    graph TB\n    ```\n    ## 结束\n</tool_call>";
        let calls = parse_tool_call_blocks(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "replace_string_in_file");
        assert_eq!(calls[0].1["filePath"], "d:/a/README.md");
        assert_eq!(calls[0].1["oldString"], "# AI Gateway");
        let ns = calls[0].1["newString"].as_str().unwrap();
        assert!(ns.contains("```mermaid"), "应包含 mermaid 围栏,实际: {:?}", ns);
        assert!(ns.contains("graph TB"));
        assert!(ns.contains("## 结束"), "块标量应完整解析到末尾,实际: {:?}", ns);
    }

    /// 参数含多个代码块(```mermaid + ```bash)也能完整解析出工具调用（XML 围栏不与 ``` 冲突）
    #[test]
    fn parse_tool_call_args_with_nested_code_fences() {
        let text = "<tool_call>\nname: replace_string_in_file\narguments:\n  filePath: d:/a/README.md\n  oldString: \"# AI Gateway\"\n  newString: |-\n    # 标题\n    ```mermaid\n    graph TB\n    ```\n    ```bash\n    echo hi\n    ```\n    ## 结尾\n</tool_call>";
        let calls = parse_tool_call_blocks(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "replace_string_in_file");
        let ns = calls[0].1["newString"].as_str().unwrap();
        assert!(ns.contains("```mermaid"));
        assert!(ns.contains("graph TB"));
        assert!(ns.contains("```bash"));
        assert!(ns.contains("## 结尾"), "块标量应完整解析到末尾,实际: {:?}", ns);
    }

    /// 参数含三反引号时经 parse_tool_calls → OpenAI 标准 tool_calls（newString 完整不截断）
    #[test]
    fn parse_tool_calls_args_with_code_fence_openai() {
        let content = "<tool_call>\nname: replace_string_in_file\narguments:\n  filePath: d:/a/README.md\n  oldString: \"# AI Gateway\"\n  newString: |-\n    # 标题\n    ```bash\n    echo hi\n    ```\n    ## 结尾\n</tool_call>";
        let calls = parse_tool_calls(content).unwrap();
        let arr = calls.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["function"]["name"], "replace_string_in_file");
        let args: serde_json::Value =
            serde_json::from_str(arr[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        let ns = args["newString"].as_str().unwrap();
        assert!(ns.contains("```bash"));
        assert!(ns.contains("## 结尾"));
    }

    /// parse_tool_calls 输出 OpenAI 标准 tool_calls
    #[test]
    fn parse_tool_calls_openai_format() {
        let content = "<tool_call>\nname: read_file\narguments:\n  filePath: /a/b.txt\n</tool_call>";
        let calls = parse_tool_calls(content).unwrap();
        let arr = calls.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "function");
        assert_eq!(arr[0]["function"]["name"], "read_file");
        assert!(arr[0]["id"].as_str().unwrap().starts_with("call_"));
        let args = arr[0]["function"]["arguments"].as_str().unwrap();
        assert!(args.contains("\"filePath\":\"/a/b.txt\""));
    }

    /// 属性形式：<tool_call name="X"> 内是裸参数 YAML（无 name 字段）也能解析
    #[test]
    fn parse_tool_call_attr_name() {
        let text = "<tool_call name=\"Read\">\nfilePath: /a/b.txt\nstartLine: 1\n</tool_call>";
        let calls = parse_tool_call_blocks(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "Read");
        assert_eq!(calls[0].1["filePath"], "/a/b.txt");
        assert_eq!(calls[0].1["startLine"], 1);
    }

    /// 属性形式 + arguments 包装
    #[test]
    fn parse_tool_call_attr_name_with_arguments() {
        let text = "<tool_call name=\"Write\">\narguments:\n  file_path: d:/a.md\n  content: |-\n    hi\n</tool_call>";
        let calls = parse_tool_call_blocks(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "Write");
        assert_eq!(calls[0].1["file_path"], "d:/a.md");
        assert!(calls[0].1["content"].as_str().unwrap().contains("hi"));
    }

    /// 混合：普通 <tool_call> + 属性形式 <tool_call name='X'>（单引号）
    #[test]
    fn parse_tool_call_mixed_attr_and_plain() {
        let text = "<tool_call>\nname: read_file\narguments:\n  filePath: a\n</tool_call>\n<tool_call name='Search'>\nquery: x\n</tool_call>";
        let calls = parse_tool_call_blocks(text).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "read_file");
        assert_eq!(calls[1].0, "Search");
        assert_eq!(calls[1].1["query"], "x");
    }

    /// 复现真实场景：普通文本 + <tool_call name="Write"> + content 块标量(HTML 含大量标签/引号)
    #[test]
    fn parse_tool_call_attr_write_with_html_content() {
        let text = "开始生成登录页面HTML。\n<tool_call name=\"Write\">\narguments:\n  file_path: \"C:\\\\Users\\\\wushi\\\\login.html\"\n  content: |-\n    <!DOCTYPE html>\n    <html lang=\"zh-CN\">\n    <head>\n      <style>\n        body { margin: 0; }\n      </style>\n    </head>\n    <body>\n    </body>\n    </html>\n</tool_call>";
        let calls = parse_tool_call_blocks(text).unwrap();
        assert_eq!(calls.len(), 1, "应解析出 1 个工具, 实际: {:?}", calls);
        assert_eq!(calls[0].0, "Write");
        assert_eq!(calls[0].1["file_path"], "C:\\Users\\wushi\\login.html");
        let content = calls[0].1["content"].as_str().unwrap();
        assert!(content.contains("<!DOCTYPE html>"), "content 应含 DOCTYPE, 实际: {:?}", content);
        assert!(content.contains("</html>"));
        assert!(content.contains("body { margin: 0; }"));
    }

    /// parse_tool_block_yaml：仅接受工具形状的 YAML
    #[test]
    fn parse_tool_block_yaml_only() {
        // YAML 工具形状
        assert!(parse_tool_block_yaml("name: x\narguments:\n  a: 1").is_some());
        // 无工具形状（普通对象/普通文本）→ None
        assert!(parse_tool_block_yaml("hello world").is_none());
        assert!(parse_tool_block_yaml("a: 1").is_none());
        // 空
        assert!(parse_tool_block_yaml("  ").is_none());
    }

    /// 流式嗅探：完整工具标识（任意位置，普通文本后可夹杂）→ 判定为工具
    #[test]
    fn looks_like_tool_call_sniff() {
        // YAML 开头
        assert!(super::looks_like_tool_call("name: read_file"));
        assert!(super::looks_like_tool_call("name：read_file"));
        // <tool_call> 围栏（完整，任意位置）
        assert!(super::looks_like_tool_call("<tool_call>\nname: read_file"));
        // 普通文本夹杂工具调用（不是开头）也能命中
        assert!(super::looks_like_tool_call("好的，我来读取文件\n<tool_call>\nname: read_file"));
        assert!(super::looks_like_tool_call("好的，我来读取文件\n<tool_call>"));
        // 属性形式 <tool_call name="X"> / <toolcall name='Y'> 也命中
        assert!(super::looks_like_tool_call("<tool_call name=\"Read\">\nfilePath: a"));
        assert!(super::looks_like_tool_call("好的，我来读取文件\n<tool_call name='Read'>"));
        // 不完整前缀 / 未闭合属性 / 其它 XML 标签 / 纯文本 → 不算完整标识
        assert!(!super::looks_like_tool_call("<tool_call"));
        assert!(!super::looks_like_tool_call("<tool_call name=\"Read\""));
        assert!(!super::looks_like_tool_call("<tool_c"));
        assert!(!super::looks_like_tool_call("<json>"));
        assert!(!super::looks_like_tool_call("{\"name\":\"read_file\""));
        assert!(!super::looks_like_tool_call("这是一个普通回答"));
        assert!(!super::looks_like_tool_call("    ```long block"));
        assert!(!super::looks_like_tool_call(""));
    }

    /// 流式嗅探：围栏任意前缀（尾部反引号，增量拆分中）→ 返回剩余待匹配长度
    #[test]
    fn is_tool_fence_prefix_sniff() {
        // 前缀阶段：< / <t / <to / <too / <tool / <tool_ / <tool_c 都是 <tool_call> 前缀
        assert_eq!(super::tool_fence_remaining("<").unwrap(), 10);
        assert_eq!(super::tool_fence_remaining("<t").unwrap(), 9);
        assert_eq!(super::tool_fence_remaining("<to").unwrap(), 8);
        assert_eq!(super::tool_fence_remaining("<too").unwrap(), 7);
        assert_eq!(super::tool_fence_remaining("<tool").unwrap(), 6);
        assert_eq!(super::tool_fence_remaining("<tool_").unwrap(), 5);
        assert_eq!(super::tool_fence_remaining("<tool_c").unwrap(), 4);
        assert_eq!(super::tool_fence_remaining("<tool_call").unwrap(), 1);
        // 普通文本夹杂围栏前缀（尾部）→ 也命中
        assert!(super::tool_fence_remaining("好的，我来读取\n<tool_c").is_some());
        // 完整标识不算前缀（由 looks_like_tool_call 判定）
        assert!(super::tool_fence_remaining("<tool_call>").is_none());
        // 非围栏前缀（其它 XML 标签 / 纯文本）
        assert!(super::tool_fence_remaining("<j").is_none());
        assert!(super::tool_fence_remaining("<json>").is_none());
        assert!(super::tool_fence_remaining("这是一个普通回答").is_none());
        assert!(super::tool_fence_remaining("").is_none());
    }

    /// 流式：一次回答中【普通文本 + 工具调用夹杂】，既能输出普通文本又能识别工具
    #[test]
    fn stream_processor_text_then_tool() {
        let mut sp = super::StreamProcessor::new(true, String::new());
        // 快照注册 RESPONSE 片段（content 内换行在 JSON 中是转义 \\n）
        sp.feed("data: {\"v\":{\"response\":{\"fragments\":[{\"type\":\"RESPONSE\",\"content\":\"好的，我来读取文件。\\n<tool\"}]}}}\n");
        // 增量补齐工具调用（<tool + _call> 拼成 <tool_call>）
        sp.feed("data: {\"p\":\"response/fragments/0/content\",\"o\":\"APPEND\",\"v\":\"_call>\\nname: read_file\\narguments:\\n  filePath: a\\n</tool_call>\"}\n");
        let final_deltas = sp.finish();
        assert_eq!(final_deltas.len(), 1);
        let v: serde_json::Value = serde_json::from_str(&final_deltas[0]).unwrap();
        assert!(v.get("tool_calls").is_some());
    }

    /// 真实场景：普通文本（含行内反引号 markdown）较长，后面跟完整 <tool_call> 块，也要能识别工具
    #[test]
    fn stream_processor_long_text_then_tool() {
        let mut sp = super::StreamProcessor::new(true, String::new());
        // 普通文本较长（超过围栏长度），且夹杂行内反引号
        let long_text = "我将读取 `pow.rs` 文件的内容，然后为你解释它的作用和实现细节。";
        sp.feed(&format!("data: {{\"v\":{{\"response\":{{\"fragments\":[{{\"type\":\"RESPONSE\",\"content\":\"{}\"}}]}}}}\n", long_text));
        // 快照后继续：完整工具调用块（含 Windows 路径反斜杠）
        sp.feed("data: {\"p\":\"response/fragments/0/content\",\"o\":\"APPEND\",\"v\":\"\\n<tool_call>\\nname: read_file\\narguments:\\n  filePath: D:\\\\projects\\\\x.rs\\n  startLine: 1\\n  endLine: 9999\\n</tool_call>\"}\n");
        let final_deltas = sp.finish();
        // 应解析出 tool_calls
        let has_tool = final_deltas
            .iter()
            .any(|d| serde_json::from_str::<serde_json::Value>(d).map(|v| v.get("tool_calls").is_some()).unwrap_or(false));
        assert!(has_tool, "普通文本夹杂工具调用应识别出 tool_calls, 实际: {:?}", final_deltas);
    }

    /// 思考内容（THINK）中的 <tool_call> 不转换：直接透传，不产生 tool_calls
    #[test]
    fn think_content_tool_call_not_converted() {
        let mut sp = super::StreamProcessor::new(true, String::new());
        // THINK 片段含 <tool_call>（思考中提到了工具调用）
        let deltas = sp.feed("data: {\"v\":{\"response\":{\"fragments\":[{\"type\":\"THINK\",\"content\":\"我需要调用 read_file 工具，格式是 <tool_call>\\nname: read_file\\narguments:\\n  filePath: a\\n</tool_call>\\n好的，开始调用。\"},{\"type\":\"RESPONSE\",\"content\":\"正在读取文件...\"}]}}}\n");
        let final_deltas = sp.finish();
        // THINK 在 feed 中透传输出
        let has_think = deltas
            .iter()
            .any(|d| serde_json::from_str::<serde_json::Value>(d).map(|v| v.get("THINK").is_some()).unwrap_or(false));
        assert!(has_think, "THINK 应被透传输出");
        // 所有增量（feed + finish）都不产生 tool_calls
        for d in deltas.iter().chain(final_deltas.iter()) {
            let v: serde_json::Value = serde_json::from_str(d).unwrap();
            assert!(v.get("tool_calls").is_none(), "THINK 中的 <tool_call> 不应被转换: {}", d);
        }
    }

    /// 流式：围栏被拆分成多段增量（真实 DeepSeek 协议：快照注册 + APPEND 追加），最终也能进入 Tool 模式解析
    #[test]
    fn stream_processor_split_fence() {
        let mut sp = super::StreamProcessor::new(true, String::new());
        // 快照注册 RESPONSE 片段（初始为空）
        sp.feed("data: {\"v\":{\"response\":{\"fragments\":[{\"type\":\"RESPONSE\",\"content\":\"\"}]}}}\n");
        // 增量 1：围栏只到部分 <tool → 进入 Sniff 缓冲
        sp.feed("data: {\"p\":\"response/fragments/0/content\",\"o\":\"APPEND\",\"v\":\"<tool\"}\n");
        // 增量 2：围栏拼完整（<tool + _call> = <tool_call>）+ YAML 参数
        sp.feed("data: {\"p\":\"response/fragments/0/content\",\"o\":\"APPEND\",\"v\":\"_call>\\nname: read_file\\narguments:\\n  filePath: a\\n</tool_call>\"}\n");
        let final_deltas = sp.finish();
        assert_eq!(final_deltas.len(), 1);
        let v: serde_json::Value = serde_json::from_str(&final_deltas[0]).unwrap();
        assert!(v.get("tool_calls").is_some());
    }

    /// 流式：YAML name: 开头（无围栏）也能进入 Tool 模式并解析
    #[test]
    fn stream_processor_yaml_no_fence() {
        let mut sp = super::StreamProcessor::new(true, String::new());
        sp.feed("data: {\"v\":{\"response\":{\"fragments\":[{\"type\":\"RESPONSE\",\"content\":\"name: read_file\\narguments:\\n  filePath: a\\n\"}]}}}\n");
        let final_deltas = sp.finish();
        assert_eq!(final_deltas.len(), 1);
        let v: serde_json::Value = serde_json::from_str(&final_deltas[0]).unwrap();
        assert!(v.get("tool_calls").is_some());
    }

    /// 变体围栏：AI 输出 <toolcall>（漏下划线）也能解析成工具调用（复现真实场景）
    #[test]
    fn parse_toolcall_variant_fence() {
        let text = "回答时间: 2026/09/01 13:18:13.899\n会话: ebf3aefa-9aee-4a05-bc35-70f7febfb6b0\n<toolcall>\nname: CheckCommandStatus\narguments:\n  command_id: \"call_1\"\n</tool_call>\n\n<toolcall>\nname: Write\narguments:\n  file_path: 'd:/a.md'\n  content: |-\n    import os\n    print('hi')\n</tool_call>";
        let calls = super::parse_tool_call_blocks(text).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "CheckCommandStatus");
        assert_eq!(calls[0].1["command_id"], "call_1");
        assert_eq!(calls[1].0, "Write");
        assert_eq!(calls[1].1["file_path"], "d:/a.md");
        assert!(calls[1].1["content"].as_str().unwrap().contains("import os"));
    }

    /// 流式：变体围栏 <toolcall> 也能被嗅探并进入 Tool 模式解析
    #[test]
    fn stream_processor_toolcall_variant() {
        let mut sp = super::StreamProcessor::new(true, String::new());
        sp.feed("data: {\"v\":{\"response\":{\"fragments\":[{\"type\":\"RESPONSE\",\"content\":\"<toolcall>\\nname: read_file\\narguments:\\n  filePath: a\\n</tool_call>\"}]}}}\n");
        let final_deltas = sp.finish();
        let has_tool = final_deltas
            .iter()
            .any(|d| serde_json::from_str::<serde_json::Value>(d).map(|v| v.get("tool_calls").is_some()).unwrap_or(false));
        assert!(has_tool, "变体围栏应解析出 tool_calls, 实际: {:?}", final_deltas);
    }

    /// 流式：普通文本 + 属性形式 <tool_call name="X"> 完整出现在一个增量 → 识别工具
    #[test]
    fn stream_processor_attr_form_complete() {
        let mut sp = super::StreamProcessor::new(true, String::new());
        sp.feed("data: {\"v\":{\"response\":{\"fragments\":[{\"type\":\"RESPONSE\",\"content\":\"开始生成页面。\\n<tool_call name=\\\"Write\\\">\\narguments:\\n  file_path: a\\n  content: |\\n    hi\\n</tool_call>\"}]}}}\n");
        let final_deltas = sp.finish();
        let has_tool = final_deltas
            .iter()
            .any(|d| serde_json::from_str::<serde_json::Value>(d).map(|v| v.get("tool_calls").is_some()).unwrap_or(false));
        assert!(has_tool, "完整属性形式应识别 tool_calls, 实际: {:?}", final_deltas);
    }

    /// 流式：属性形式开围栏被拆分成增量（<tool_call name="Wri + te">）→ 也应进入 Sniff 识别
    #[test]
    fn stream_processor_attr_form_split() {
        let mut sp = super::StreamProcessor::new(true, String::new());
        sp.feed("data: {\"v\":{\"response\":{\"fragments\":[{\"type\":\"RESPONSE\",\"content\":\"开始生成页面。\\n<tool_call name=\\\"Wri\"}]}}}\n");
        sp.feed("data: {\"p\":\"response/fragments/0/content\",\"o\":\"APPEND\",\"v\":\"te\\\">\\narguments:\\n  file_path: a\\n  content: |\\n    hi\\n</tool_call>\"}\n");
        let final_deltas = sp.finish();
        let has_tool = final_deltas
            .iter()
            .any(|d| serde_json::from_str::<serde_json::Value>(d).map(|v| v.get("tool_calls").is_some()).unwrap_or(false));
        assert!(has_tool, "拆分属性形式应识别 tool_calls, 实际: {:?}", final_deltas);
    }

    /// 思考(THINK)中直接写了 <tool_call>、RESPONSE 为空 → 兜底从思考解析工具调用，
    /// 避免「思考中有工具调用后就直接停止」（模型把工具调用写在思考里）
    #[test]
    fn think_only_tool_call_parsed() {
        let mut sp = super::StreamProcessor::new(true, String::new());
        // 只有 THINK 片段：思考里直接写出完整 <tool_call> 块
        sp.feed("data: {\"v\":{\"response\":{\"fragments\":[{\"type\":\"THINK\",\"content\":\"我需要调用工具。\\n<tool_call>\\nname: read_file\\narguments:\\n  filePath: a\\n</tool_call>\\n好的开始。\"}]}}}\n");
        let final_deltas = sp.finish();
        let has_tool = final_deltas
            .iter()
            .any(|d| serde_json::from_str::<serde_json::Value>(d).map(|v| v.get("tool_calls").is_some()).unwrap_or(false));
        assert!(has_tool, "思考中的工具调用应被兜底解析, 实际: {:?}", final_deltas);
    }

    /// 思考(THINK)中提到工具调用但 RESPONSE 是正常回答 → 不解析思考（保持原有行为）
    #[test]
    fn think_tool_call_desc_with_response_not_parsed() {
        let mut sp = super::StreamProcessor::new(true, String::new());
        // THINK 描述工具调用 + RESPONSE 正常回答
        let deltas = sp.feed("data: {\"v\":{\"response\":{\"fragments\":[{\"type\":\"THINK\",\"content\":\"我想到 <tool_call> 格式可以用。\\n好的开始。\"},{\"type\":\"RESPONSE\",\"content\":\"正在处理...\"}]}}}\n");
        let final_deltas = sp.finish();
        for d in deltas.iter().chain(final_deltas.iter()) {
            let v: serde_json::Value = serde_json::from_str(d).unwrap();
            assert!(v.get("tool_calls").is_none(), "RESPONSE 正常回答时不应解析思考中的描述: {}", d);
        }
        // RESPONSE 仍正常输出
        assert!(deltas.iter().any(|d| serde_json::from_str::<serde_json::Value>(d).unwrap().get("RESPONSE").is_some()));
    }

    /// SSE 响应捕获 message_id（供下轮 parent_message_id，工具结果续接上一条 AI 回复）
    #[test]
    fn parser_captures_message_id() {
        let mut parser = super::DeepSeekParser::new();
        // 首事件含 response_message_id
        parser.process_line("data: {\"request_message_id\":7,\"response_message_id\":8,\"model_type\":\"default\"}");
        assert_eq!(parser.message_id(), Some(8));
        // 快照含 message_id（以快照为准，覆盖首事件值）
        parser.process_line("data: {\"v\":{\"response\":{\"message_id\":9,\"parent_id\":7,\"status\":\"WIP\",\"fragments\":[]}}}");
        assert_eq!(parser.message_id(), Some(9));
        // 无消息 id 的普通数据不改动已捕获值
        parser.process_line("data: {\"v\":\"普通内容\"}");
        assert_eq!(parser.message_id(), Some(9));
    }

    /// extract_session_id：前缀「回答时间: <ts>\n会话: <id>」中"会话"位于非 6 倍数字节偏移，
    /// 旧实现按字节跳步(i += "会话".len())会漏掉 → 追问解析不到会话 id → 重启后新建会话。
    /// 用真实前缀回归验证。
    #[test]
    fn extract_session_id_from_answer_prefix() {
        // 真实前缀（时间戳格式固定，使"会话"落在字节 37，非 6 倍数）
        let text = "回答时间: 2026/09/01 11:22:41.633\n会话: f8441551-4bed-4487-9f69-ee4a42b23209\n具体回答内容";
        assert_eq!(
            super::extract_session_id(text).as_deref(),
            Some("f8441551-4bed-4487-9f69-ee4a42b23209")
        );
        // 全角冒号
        let text2 = "会话：abc12345-def\n回答";
        assert_eq!(super::extract_session_id(text2).as_deref(), Some("abc12345-def"));
        // 回答内容里出现"会话"（非前缀、无 id）不影响取前缀 id
        let text3 = "好的，我开启一个新会话。\n回答时间: 2026/09/01 11:22:41.633\n会话: f8441551-4bed-4487-9f69-ee4a42b23209\n完成";
        assert_eq!(
            super::extract_session_id(text3).as_deref(),
            Some("f8441551-4bed-4487-9f69-ee4a42b23209")
        );
        // 无有效会话 id → None
        assert!(super::extract_session_id("这是一个普通回答").is_none());
        assert!(super::extract_session_id("开启新会话吧").is_none());
    }

    /// token 估算：中文 1 字≈1、其它≈4 字符/1、向上取整
    #[test]
    fn estimate_tokens_basic() {
        assert_eq!(super::estimate_tokens("你好世界"), 4); // 4 个中文 → 4
        assert_eq!(super::estimate_tokens("hello"), 2); // 5 ascii → ceil(5/4)=2
        assert_eq!(super::estimate_tokens("ab"), 1); // 2 ascii → ceil(2/4)=1
        assert_eq!(super::estimate_tokens(""), 0);
        assert!(super::estimate_tokens("你好 hello 世界") > 0);
    }

    /// 增量文本提取：{"THINK":..} / {"RESPONSE":..} 拼接（用于累计输出 token）
    #[test]
    fn delta_text_collects_payload_strings() {
        assert_eq!(super::delta_text(r#"{"RESPONSE":"你好"}"#), "你好");
        let s = super::delta_text(r#"{"THINK":"思考","RESPONSE":"回答"}"#);
        assert!(s.contains("思考") && s.contains("回答"), "应包含所有字符串叶子, 实际: {}", s);
        // 非法 JSON → 原样返回
        assert_eq!(super::delta_text("not json"), "not json");
    }

    /// 对象值收集：{THINK,RESPONSE} → 全部文本（估算输出 token 含思考）；数字不是叶子
    #[test]
    fn value_text_joins_leaves() {
        let v = serde_json::json!({ "THINK": "思考", "RESPONSE": "回答", "n": 1 });
        let s = super::value_text(&v);
        assert!(s.contains("思考") && s.contains("回答"), "实际: {}", s);
        assert!(!s.contains('1'), "数字不应计入文本: {}", s);
        assert_eq!(super::value_text(&serde_json::json!("plain")), "plain");
        assert_eq!(super::value_text(&serde_json::json!(42)), "");
    }

    /// 流式实时性：THINK/RESPONSE 逐块输出，不是等整个流结束一次性给
    #[test]
    fn stream_processor_incremental_deltas() {
        let mut sp = super::StreamProcessor::new(false, String::new());
        // 快照注册 THINK + RESPONSE 片段（初始为空）
        sp.feed("data: {\"v\":{\"response\":{\"fragments\":[{\"type\":\"THINK\",\"content\":\"\"},{\"type\":\"RESPONSE\",\"content\":\"\"}]}}}\n");
        // 增量 1：THINK 出现一部分 → 应即时输出 THINK
        let d1 = sp.feed("data: {\"p\":\"response/fragments/0/content\",\"o\":\"APPEND\",\"v\":\"思考一\"}\n");
        assert!(
            d1.iter()
                .any(|d| serde_json::from_str::<serde_json::Value>(d).unwrap().get("THINK").is_some()),
            "THINK 应即时输出, 实际: {:?}",
            d1
        );
        // 增量 2：RESPONSE 出现一部分 → 应即时输出 RESPONSE
        let d2 = sp.feed("data: {\"p\":\"response/fragments/1/content\",\"o\":\"APPEND\",\"v\":\"回答一\"}\n");
        assert!(
            d2.iter()
                .any(|d| serde_json::from_str::<serde_json::Value>(d).unwrap().get("RESPONSE").is_some()),
            "RESPONSE 应即时输出, 实际: {:?}",
            d2
        );
        // 增量 3：RESPONSE 追加 → 应继续即时输出后续部分（非累积重发）
        let d3 = sp.feed("data: {\"p\":\"response/fragments/1/content\",\"o\":\"APPEND\",\"v\":\"回答二\"}\n");
        let resp3: String = d3
            .iter()
            .filter_map(|d| {
                serde_json::from_str::<serde_json::Value>(d)
                    .ok()
                    .and_then(|v| v.get("RESPONSE").and_then(|x| x.as_str()).map(String::from))
            })
            .collect();
        assert!(resp3.contains("回答二"), "后续增量应只含新内容, 实际: {:?}", d3);
        assert!(!resp3.contains("回答一"), "不应重发已输出的内容: {:?}", d3);
    }

    /// XML 格式工具调用：<tool_calls><invoke name="X"><parameter name="k">v</parameter></invoke></tool_calls>
    #[test]
    fn parse_xml_tool_calls_basic() {
        let text = "<tool_calls>\n<invoke name=\"file_search\">\n<parameter name=\"query\">**/*.csproj</parameter>\n<parameter name=\"maxResults\">50</parameter>\n</invoke>\n</tool_calls>";
        let calls = parse_tool_call_blocks(text).unwrap();
        assert_eq!(calls.len(), 1, "实际: {:?}", calls);
        assert_eq!(calls[0].0, "file_search");
        assert_eq!(calls[0].1["query"], "**/*.csproj");
        assert_eq!(calls[0].1["maxResults"], 50, "单行标量应自动转数字");
    }

    /// XML 格式：一次多个 <invoke> → 多个工具调用
    #[test]
    fn parse_xml_tool_calls_multiple_invokes() {
        let text = "<tool_calls>\n<invoke name=\"file_search\">\n<parameter name=\"query\">**/*.csproj</parameter>\n</invoke>\n<invoke name=\"list_dir\">\n<parameter name=\"path\">f:/games</parameter>\n</invoke>\n</tool_calls>";
        let calls = parse_tool_call_blocks(text).unwrap();
        assert_eq!(calls.len(), 2, "实际: {:?}", calls);
        assert_eq!(calls[0].0, "file_search");
        assert_eq!(calls[0].1["query"], "**/*.csproj");
        assert_eq!(calls[1].0, "list_dir");
        assert_eq!(calls[1].1["path"], "f:/games");
    }

    /// XML 格式：缺少外层 <tool_calls> 包裹、普通文本夹杂时也能解析
    #[test]
    fn parse_xml_invoke_without_wrapper() {
        let text = "好的，我来读取文件。\n<invoke name=\"read_file\">\n<parameter name=\"filePath\">d:/a.rs</parameter>\n<parameter name=\"startLine\">1</parameter>\n<parameter name=\"endLine\">10</parameter>\n</invoke>";
        let calls = parse_tool_call_blocks(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "read_file");
        assert_eq!(calls[0].1["filePath"], "d:/a.rs");
        assert_eq!(calls[0].1["startLine"], 1);
        assert_eq!(calls[0].1["endLine"], 10);
    }

    /// XML 格式：多行参数（代码/围栏）原样保留，只去掉布局引入的首尾换行
    #[test]
    fn parse_xml_tool_calls_multiline_parameter() {
        let text = "<tool_calls>\n<invoke name=\"Write\">\n<parameter name=\"file_path\">d:/a.md</parameter>\n<parameter name=\"content\">\n# 标题\n```mermaid\ngraph TB\n```\n</parameter>\n</invoke>\n</tool_calls>";
        let calls = parse_tool_call_blocks(text).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1["file_path"], "d:/a.md");
        let c = calls[0].1["content"].as_str().unwrap();
        assert!(c.starts_with("# 标题"), "应去掉布局引入的首换行, 实际: {:?}", c);
        assert!(c.contains("```mermaid"));
        assert!(c.ends_with("```"), "应去掉布局引入的尾换行, 实际: {:?}", c);
    }

    /// XML 格式 → OpenAI 标准 tool_calls（参数序列化为 JSON 字符串）
    #[test]
    fn parse_tool_calls_xml_to_openai() {
        let content = "<tool_calls><invoke name=\"read_file\"><parameter name=\"filePath\">d:/a.rs</parameter><parameter name=\"startLine\">1</parameter></invoke></tool_calls>";
        let calls = parse_tool_calls(content).unwrap();
        let arr = calls.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "function");
        assert_eq!(arr[0]["function"]["name"], "read_file");
        assert!(arr[0]["id"].as_str().unwrap().starts_with("call_"));
        let args: serde_json::Value =
            serde_json::from_str(arr[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["filePath"], "d:/a.rs");
        assert_eq!(args["startLine"], 1);
    }

    /// XML 格式与自有 <tool_call> 围栏格式互不干扰（<tool_calls> 不被误当自有围栏）
    #[test]
    fn xml_and_own_fence_coexist() {
        let own = "<tool_call>\nname: read_file\narguments:\n  filePath: a\n</tool_call>";
        let calls = parse_tool_call_blocks(own).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "read_file");
        let xml = "<tool_calls><invoke name=\"read_file\"><parameter name=\"filePath\">a</parameter></invoke></tool_calls>";
        let calls = parse_tool_call_blocks(xml).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "read_file");
        assert_eq!(calls[0].1["filePath"], "a");
        // 畸形 invoke（无 name 属性）不产生工具调用
        assert!(parse_tool_call_blocks(
            "<tool_calls><invoke><parameter name=\"a\">1</parameter></invoke></tool_calls>"
        )
        .is_none());
    }

    /// 闭标签错写（写成 call_call 而非 tool_call）→ 仍能解析出围栏块
    #[test]
    fn parse_misspelled_close_tag() {
        let text = "<tool_call>\nname: read_file\narguments:\n  filePath: a.rs\n  startLine: 1\n</call_call>";
        let calls = parse_tool_call_blocks(text).unwrap();
        assert_eq!(calls.len(), 1, "实际: {:?}", calls);
        assert_eq!(calls[0].0, "read_file");
        assert_eq!(calls[0].1["filePath"], "a.rs");
        assert_eq!(calls[0].1["startLine"], 1);
    }

    /// 参数与 arguments 写在同一层级（arguments 为空）→ 同级键提升为参数
    #[test]
    fn parse_args_siblings_hoisted() {
        let text = "<tool_call>\nname: grep_search\narguments:\nquery: MainButtonDef\nisRegexp: false\n</tool_call>";
        let calls = parse_tool_call_blocks(text).unwrap();
        assert_eq!(calls.len(), 1, "实际: {:?}", calls);
        assert_eq!(calls[0].0, "grep_search");
        assert_eq!(calls[0].1["query"], "MainButtonDef");
        assert_eq!(calls[0].1["isRegexp"], false);
    }

    /// 真实样本：闭合标签错写为 call_call + 参数与 arguments 同级 → 3 个工具全部解析
    #[test]
    fn parse_real_sample_misspelled_close_and_hoisted_args() {
        let text = "<tool_call>\nname: file_search\narguments:\nquery: f:/SteamLibrary/steamapps/common/RimWorld/Data/Core/Defs/**/MainButtonDefs*.xml\n</call_call>\n<tool_call>\nname: grep_search\narguments:\nquery: MainButtonDef\nisRegexp: false\nincludePattern: f:/SteamLibrary/steamapps/workshop/content/294100/**/About/About.xml\n</call_call>\n<tool_call>\nname: file_search\narguments:\nquery: f:/SteamLibrary/steamapps/common/RimWorld/Data/**/MainButtonDefs/**/*.xml\n</call_call>\n</call_call>";
        let calls = parse_tool_call_blocks(text).unwrap();
        assert_eq!(calls.len(), 3, "实际: {:?}", calls);
        assert_eq!(calls[0].0, "file_search");
        assert_eq!(
            calls[0].1["query"],
            "f:/SteamLibrary/steamapps/common/RimWorld/Data/Core/Defs/**/MainButtonDefs*.xml"
        );
        assert_eq!(calls[1].0, "grep_search");
        assert_eq!(calls[1].1["query"], "MainButtonDef");
        assert_eq!(calls[1].1["isRegexp"], false);
        assert_eq!(
            calls[1].1["includePattern"],
            "f:/SteamLibrary/steamapps/workshop/content/294100/**/About/About.xml"
        );
        assert_eq!(calls[2].0, "file_search");
        assert_eq!(
            calls[2].1["query"],
            "f:/SteamLibrary/steamapps/common/RimWorld/Data/**/MainButtonDefs/**/*.xml"
        );
    }

    /// 真实样本走完整链路：parse_tool_calls（非流式）+ StreamProcessor（流式）
    #[test]
    fn user_sample_through_openai_and_stream() {
        let text = "<tool_call>\nname: read_file\narguments:\nquery: f:/a/**/*.xml\n</call_call>";
        let arr = parse_tool_calls(text).unwrap();
        let arr = arr.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["function"]["name"], "read_file");
        let args: serde_json::Value =
            serde_json::from_str(arr[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["query"], "f:/a/**/*.xml");

        let mut sp = super::StreamProcessor::new(true, String::new());
        let content = format!("先搜索一下。\n{}", text);
        let payload = serde_json::json!({
            "v": { "response": { "fragments": [ { "type": "RESPONSE", "content": content } ] } }
        });
        sp.feed(&format!("data: {}\n", payload));
        let deltas = sp.finish();
        let has_tool = deltas
            .iter()
            .any(|d| serde_json::from_str::<serde_json::Value>(d).map(|v| v.get("tool_calls").is_some()).unwrap_or(false));
        assert!(has_tool, "流式应识别出 tool_calls, 实际: {:?}", deltas);
    }

    /// 流式嗅探：XML 开标签（完整形态 / 未闭合不算）
    #[test]
    fn looks_like_xml_tool_open() {
        assert!(super::looks_like_tool_call("<tool_calls>"));
        assert!(super::looks_like_tool_call("好的，我来搜索\n<tool_calls>\n<invoke name=\"x\">"));
        assert!(super::looks_like_tool_call("<invoke name=\"read_file\">"));
        // 未读到 '>' 的不完整开标签不算完整标识
        assert!(!super::looks_like_tool_call("<tool_calls"));
        assert!(!super::looks_like_tool_call("<invoke name=\"read_file\""));
        // 自有 <tool_call> 行为不变
        assert!(super::looks_like_tool_call("<tool_call>\nname: read_file"));
        assert!(!super::looks_like_tool_call("<tool_call"));
    }

    /// 流式嗅探：XML 开标签前缀（尾部、增量拆分中）→ 返回剩余待匹配长度
    #[test]
    fn is_xml_fence_prefix_sniff() {
        assert!(super::tool_fence_remaining("<tool_calls").is_some());
        assert!(super::tool_fence_remaining("<invo").is_some());
        assert!(super::tool_fence_remaining("<invoke").is_some());
        assert!(super::tool_fence_remaining("好的，我来搜索\n<invoke nam").is_some());
        // 完整外层开标签不算前缀（由 looks_like_tool_call 判定）
        assert!(super::tool_fence_remaining("<tool_calls>").is_none());
    }

    /// 流式：XML 工具调用格式 → tool_calls 增量
    #[test]
    fn stream_processor_xml_tool_call() {
        let mut sp = super::StreamProcessor::new(true, String::new());
        sp.feed("data: {\"v\":{\"response\":{\"fragments\":[{\"type\":\"RESPONSE\",\"content\":\"<tool_calls>\\n<invoke name=\\\"file_search\\\">\\n<parameter name=\\\"query\\\">**/*.csproj</parameter>\\n</invoke>\\n</tool_calls>\"}]}}}\n");
        let final_deltas = sp.finish();
        let has_tool = final_deltas
            .iter()
            .any(|d| serde_json::from_str::<serde_json::Value>(d).map(|v| v.get("tool_calls").is_some()).unwrap_or(false));
        assert!(has_tool, "XML 工具调用格式应解析出 tool_calls, 实际: {:?}", final_deltas);
    }

    /// 流式：XML 开标签被增量拆分（<tool_calls + >…）也能识别
    #[test]
    fn stream_processor_xml_split_fence() {
        let mut sp = super::StreamProcessor::new(true, String::new());
        sp.feed("data: {\"v\":{\"response\":{\"fragments\":[{\"type\":\"RESPONSE\",\"content\":\"<tool_calls\"}]}}}\n");
        sp.feed("data: {\"p\":\"response/fragments/0/content\",\"o\":\"APPEND\",\"v\":\">\\n<invoke name=\\\"read_file\\\">\\n<parameter name=\\\"filePath\\\">a</parameter>\\n</invoke>\\n</tool_calls>\"}\n");
        let final_deltas = sp.finish();
        let has_tool = final_deltas
            .iter()
            .any(|d| serde_json::from_str::<serde_json::Value>(d).map(|v| v.get("tool_calls").is_some()).unwrap_or(false));
        assert!(has_tool, "拆分的 XML 开标签应识别 tool_calls, 实际: {:?}", final_deltas);
    }

    /// DeepSeek DSML 格式（真实输出形态）：<|DSML| tool_calls> / <|DSML| invoke name="X"> /
    /// <|DSML| parameter name="k" string="true">v</|DSML| parameter>
    #[test]
    fn parse_dsml_tool_calls() {
        let text = r#"Optimized tool selection
<|DSML| tool_calls>
<|DSML| invoke name="file_search">
<|DSML| parameter name="query" string="true">**/*.csproj</|DSML| parameter>
<|DSML| parameter name="maxResults" string="false">50</|DSML| parameter>
</|DSML| invoke>
<|DSML| invoke name="file_search">
<|DSML| parameter name="query" string="true">MyBuildings/**</|DSML| parameter>
<|DSML| parameter name="maxResults" string="false">100</|DSML| parameter>
</|DSML| invoke>
<|DSML| invoke name="read_file">
<|DSML| parameter name="filePath" string="true">f:\SteamLibrary\steamapps\common\RimWorld\Version.txt</|DSML| parameter>
<|DSML| parameter name="startLine" string="false">1</|DSML| parameter>
<|DSML| parameter name="endLine" string="false">10</|DSML| parameter>
</|DSML| invoke>
<|DSML| invoke name="list_dir">
<|DSML| parameter name="path" string="true">f:\SteamLibrary\steamapps\common\RimWorld\MyBuildings</|DSML| parameter>
</|DSML| invoke>
</|DSML| tool_calls>"#;
        let calls = parse_tool_call_blocks(text).unwrap();
        assert_eq!(calls.len(), 4, "实际: {:?}", calls);
        assert_eq!(calls[0].0, "file_search");
        assert_eq!(calls[0].1["query"], "**/*.csproj");
        assert_eq!(calls[0].1["maxResults"], 50);
        assert_eq!(calls[1].0, "file_search");
        assert_eq!(calls[1].1["query"], "MyBuildings/**");
        assert_eq!(calls[1].1["maxResults"], 100);
        assert_eq!(calls[2].0, "read_file");
        assert_eq!(
            calls[2].1["filePath"].as_str().unwrap(),
            r"f:\SteamLibrary\steamapps\common\RimWorld\Version.txt"
        );
        assert_eq!(calls[2].1["startLine"], 1);
        assert_eq!(calls[2].1["endLine"], 10);
        assert_eq!(calls[3].0, "list_dir");
        assert_eq!(
            calls[3].1["path"].as_str().unwrap(),
            r"f:\SteamLibrary\steamapps\common\RimWorld\MyBuildings"
        );
    }

    /// DSML → OpenAI 标准 tool_calls（一次多个工具）
    #[test]
    fn parse_tool_calls_dsml_to_openai() {
        let content = r#"<|DSML| tool_calls>
<|DSML| invoke name="file_search">
<|DSML| parameter name="query" string="true">**/*.csproj</|DSML| parameter>
</|DSML| invoke>
<|DSML| invoke name="read_file">
<|DSML| parameter name="filePath" string="true">d:/a.rs</|DSML| parameter>
<|DSML| parameter name="startLine" string="false">1</|DSML| parameter>
</|DSML| invoke>
</|DSML| tool_calls>"#;
        let calls = parse_tool_calls(content).unwrap();
        let arr = calls.as_array().unwrap();
        assert_eq!(arr.len(), 2, "实际: {:?}", arr);
        assert_eq!(arr[0]["type"], "function");
        assert_eq!(arr[0]["function"]["name"], "file_search");
        assert_eq!(arr[1]["function"]["name"], "read_file");
        assert!(arr[0]["id"].as_str().unwrap().starts_with("call_"));
        let args: serde_json::Value =
            serde_json::from_str(arr[1]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["filePath"], "d:/a.rs");
        assert_eq!(args["startLine"], 1);
    }

    /// DSML：string 属性决定类型——string="true" 保持字符串，string="false" 按标量转数字
    #[test]
    fn parse_dsml_string_attribute() {
        let text = r#"<|DSML| invoke name="file_search">
<|DSML| parameter name="query" string="true">50</|DSML| parameter>
<|DSML| parameter name="maxResults" string="false">50</|DSML| parameter>
</|DSML| invoke>"#;
        let calls = parse_tool_call_blocks(text).unwrap();
        assert_eq!(calls.len(), 1, "实际: {:?}", calls);
        assert_eq!(calls[0].1["query"], "50");
        assert_eq!(calls[0].1["maxResults"], 50);
    }

    /// DSML：多行值（代码含 '<'、三反引号）原样保留，只去掉布局引入的首尾换行
    #[test]
    fn parse_dsml_multiline_value_with_angle() {
        let text = r#"<|DSML| invoke name="Write">
<|DSML| parameter name="file_path" string="true">d:/a.md</|DSML| parameter>
<|DSML| parameter name="content" string="true">
# 标题
if (a < b) { }
```mermaid
graph TB
```
</|DSML| parameter>
</|DSML| invoke>"#;
        let calls = parse_tool_call_blocks(text).unwrap();
        assert_eq!(calls.len(), 1, "实际: {:?}", calls);
        assert_eq!(calls[0].1["file_path"], "d:/a.md");
        let c = calls[0].1["content"].as_str().unwrap();
        assert!(c.starts_with("# 标题"), "实际: {:?}", c);
        assert!(c.contains("if (a < b) { }"), "实际: {:?}", c);
        assert!(c.contains("```mermaid"), "实际: {:?}", c);
        assert!(c.ends_with("```"), "实际: {:?}", c);
    }

    /// DSML 变体：双管道前缀（部分客户端渲染/输出的形态）同样识别
    #[test]
    fn parse_dsml_double_pipe() {
        let text = r#"<||DSML|| tool_calls>
<||DSML|| invoke name="list_dir">
<||DSML|| parameter name="path" string="true">f:/a</||DSML|| parameter>
</||DSML|| invoke>
</||DSML|| tool_calls>"#;
        let calls = parse_tool_call_blocks(text).unwrap();
        assert_eq!(calls.len(), 1, "实际: {:?}", calls);
        assert_eq!(calls[0].0, "list_dir");
        assert_eq!(calls[0].1["path"], "f:/a");
    }

    /// 双竖线 DSML 完整样本：<||DSML|| tool_calls> / <||DSML|| invoke name="X"> /
    /// <||DSML|| parameter name="k" string="true">v</||DSML|| parameter> / </||DSML|| invoke>
    #[test]
    fn parse_dsml_double_pipe_full_sample() {
        let text = r#"Optimized tool selection<||DSML|| tool_calls>
<||DSML|| invoke name="file_search">
<||DSML|| parameter name="query" string="true">**/*.csproj</||DSML|| parameter>
<||DSML|| parameter name="maxResults" string="false">50</||DSML|| parameter>
</||DSML|| invoke>
<||DSML|| invoke name="file_search">
<||DSML|| parameter name="query" string="true">MyBuildings/**</||DSML|| parameter>
<||DSML|| parameter name="maxResults" string="false">100</||DSML|| parameter>
</||DSML|| invoke>
<||DSML|| invoke name="file_search">
<||DSML|| parameter name="query" string="true">Source/**/*.cs</||DSML|| parameter>
<||DSML|| parameter name="maxResults" string="false">100</||DSML|| parameter>
</||DSML|| invoke>
<||DSML|| invoke name="read_file">
<||DSML|| parameter name="filePath" string="true">f:\SteamLibrary\steamapps\common\RimWorld\Version.txt</||DSML|| parameter>
<||DSML|| parameter name="startLine" string="false">1</||DSML|| parameter>
<||DSML|| parameter name="endLine" string="false">10</||DSML|| parameter>
</||DSML|| invoke>
<||DSML|| invoke name="list_dir">
<||DSML|| parameter name="path" string="true">f:\SteamLibrary\steamapps\common\RimWorld\MyBuildings</||DSML|| parameter>
</||DSML|| invoke>
<||DSML|| invoke name="list_dir">
<||DSML|| parameter name="path" string="true">f:\SteamLibrary\steamapps\common\RimWorld\Source</||DSML|| parameter>
</||DSML|| invoke>
</||DSML|| tool_calls>"#;
        let calls = parse_tool_call_blocks(text).unwrap();
        assert_eq!(calls.len(), 6, "实际: {:?}", calls);
        assert_eq!(calls[0].0, "file_search");
        assert_eq!(calls[0].1["query"], "**/*.csproj");
        assert_eq!(calls[0].1["maxResults"], 50);
        assert_eq!(calls[1].1["query"], "MyBuildings/**");
        assert_eq!(calls[1].1["maxResults"], 100);
        assert_eq!(calls[2].1["query"], "Source/**/*.cs");
        assert_eq!(calls[2].1["maxResults"], 100);
        assert_eq!(calls[3].0, "read_file");
        assert_eq!(
            calls[3].1["filePath"].as_str().unwrap(),
            r"f:\SteamLibrary\steamapps\common\RimWorld\Version.txt"
        );
        assert_eq!(calls[3].1["startLine"], 1);
        assert_eq!(calls[3].1["endLine"], 10);
        assert_eq!(calls[4].0, "list_dir");
        assert_eq!(
            calls[4].1["path"].as_str().unwrap(),
            r"f:\SteamLibrary\steamapps\common\RimWorld\MyBuildings"
        );
        assert_eq!(calls[5].0, "list_dir");
        assert_eq!(
            calls[5].1["path"].as_str().unwrap(),
            r"f:\SteamLibrary\steamapps\common\RimWorld\Source"
        );
    }

    /// 全角竖线 DSML（原始输出形态，标记符为 U+FF5C + DSML）：6 个 invoke 全部解析。
    /// 标记符在运行时用 char 转义拼出，源码里不出现原样标记字符。
    #[test]
    fn parse_dsml_fullwidth_pipe_full_sample() {
        let p = "\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}";
        let o = format!("<{p}"); // 开标签前缀
        let c = format!("</{p}"); // 闭标签前缀
        let text = format!(
            "{o} calls>\n\
             {o} invoke name=\"file_search\">\n\
             {o} parameter name=\"query\" string=\"true\">**/*.csproj{c} parameter>\n\
             {o} parameter name=\"maxResults\" string=\"false\">50{c} parameter>\n\
             {c} invoke>\n\
             {o} invoke name=\"file_search\">\n\
             {o} parameter name=\"query\" string=\"true\">MyBuildings/**{c} parameter>\n\
             {o} parameter name=\"maxResults\" string=\"false\">100{c} parameter>\n\
             {c} invoke>\n\
             {o} invoke name=\"file_search\">\n\
             {o} parameter name=\"query\" string=\"true\">Source/**/*.cs{c} parameter>\n\
             {o} parameter name=\"maxResults\" string=\"false\">100{c} parameter>\n\
             {c} invoke>\n\
             {o} invoke name=\"read_file\">\n\
             {o} parameter name=\"filePath\" string=\"true\">f:\\SteamLibrary\\steamapps\\common\\RimWorld\\Version.txt{c} parameter>\n\
             {o} parameter name=\"startLine\" string=\"false\">1{c} parameter>\n\
             {o} parameter name=\"endLine\" string=\"false\">10{c} parameter>\n\
             {c} invoke>\n\
             {o} invoke name=\"list_dir\">\n\
             {o} parameter name=\"path\" string=\"true\">f:\\SteamLibrary\\steamapps\\common\\RimWorld\\MyBuildings{c} parameter>\n\
             {c} invoke>\n\
             {o} invoke name=\"list_dir\">\n\
             {o} parameter name=\"path\" string=\"true\">f:\\SteamLibrary\\steamapps\\common\\RimWorld\\Source{c} parameter>\n\
             {c} invoke>\n\
             {c} calls>"
        );
        let calls = parse_tool_call_blocks(&text).unwrap();
        assert_eq!(calls.len(), 6, "实际: {:?}", calls);
        assert_eq!(calls[0].0, "file_search");
        assert_eq!(calls[0].1["query"], "**/*.csproj");
        assert_eq!(calls[0].1["maxResults"], 50);
        assert_eq!(calls[1].0, "file_search");
        assert_eq!(calls[1].1["query"], "MyBuildings/**");
        assert_eq!(calls[1].1["maxResults"], 100);
        assert_eq!(calls[2].0, "file_search");
        assert_eq!(calls[2].1["query"], "Source/**/*.cs");
        assert_eq!(calls[2].1["maxResults"], 100);
        assert_eq!(calls[3].0, "read_file");
        assert_eq!(
            calls[3].1["filePath"].as_str().unwrap(),
            r"f:\SteamLibrary\steamapps\common\RimWorld\Version.txt"
        );
        assert_eq!(calls[3].1["startLine"], 1);
        assert_eq!(calls[3].1["endLine"], 10);
        assert_eq!(calls[4].0, "list_dir");
        assert_eq!(
            calls[4].1["path"].as_str().unwrap(),
            r"f:\SteamLibrary\steamapps\common\RimWorld\MyBuildings"
        );
        assert_eq!(calls[5].0, "list_dir");
        assert_eq!(
            calls[5].1["path"].as_str().unwrap(),
            r"f:\SteamLibrary\steamapps\common\RimWorld\Source"
        );
    }

    /// 流式嗅探：DSML 开标签完整形态 / 拆分前缀 / 普通文本 '<'
    #[test]
    fn dsml_fence_sniff() {
        assert!(super::looks_like_tool_call("<||DSML|| tool_calls>"));
        assert!(super::looks_like_tool_call(
            "Optimized tool selection\n<||DSML|| invoke name=\"file_search\">"
        ));
        assert!(super::looks_like_tool_call("<|DSML| tool_calls>"));
        assert!(super::looks_like_tool_call("<invoke name=\"file_search\">"));
        assert!(!super::looks_like_tool_call("<||DSML|| tool_calls"));
        assert!(!super::looks_like_tool_call("<||DSML|| invoke name=\"x\""));
        // 全角竖线（原始输出形态）：标记符在运行时拼出，源码不含原样标记字符
        let p = "\u{FF5C}\u{FF5C}DSML\u{FF5C}\u{FF5C}";
        let open = format!("<{p}");
        assert!(super::looks_like_tool_call(&format!("{open} calls>")));
        assert!(super::looks_like_tool_call(&format!("{open} invoke name=\"file_search\">")));
        assert!(!super::looks_like_tool_call(&format!("{open} calls")));
        assert!(super::tool_fence_remaining(&open).is_some());
        assert!(super::tool_fence_remaining(&format!("{open} invoke")).is_some());
        assert!(super::tool_fence_remaining(&format!("{open} calls>")).is_none());
        // 拆分前缀 → 需缓冲
        assert!(super::tool_fence_remaining("<||DSML||").is_some());
        assert!(super::tool_fence_remaining("<||DSML|| too").is_some());
        assert!(super::tool_fence_remaining("<||DSML|| invoke").is_some());
        assert!(super::tool_fence_remaining("<||DSML|| invoke nam").is_some());
        assert!(super::tool_fence_remaining("<|DSML|").is_some());
        // 完整标签不算前缀（由 looks_like_tool_call 判定）
        assert!(super::tool_fence_remaining("<||DSML|| tool_calls>").is_none());
        // 普通文本里的 '<' / 非标记标签不缓冲
        assert!(super::tool_fence_remaining("a < b").is_none());
        assert!(super::tool_fence_remaining("<div").is_none());
    }

    /// 流式：DSML 工具调用（普通文本 + 开标签被拆分）→ tool_calls 增量
    #[test]
    fn stream_processor_dsml_tool_call() {
        let mut sp = super::StreamProcessor::new(true, String::new());
        // 增量 1：普通文本 + DSML 标记前缀（标签未闭合）
        sp.feed("data: {\"v\":{\"response\":{\"fragments\":[{\"type\":\"RESPONSE\",\"content\":\"Optimized tool selection\\n<|DSML|\"}]}}}\n");
        // 增量 2：补齐完整 DSML 工具调用
        sp.feed("data: {\"p\":\"response/fragments/0/content\",\"o\":\"APPEND\",\"v\":\" tool_calls>\\n<|DSML| invoke name=\\\"file_search\\\">\\n<|DSML| parameter name=\\\"query\\\" string=\\\"true\\\">**/*.csproj</|DSML| parameter>\\n</|DSML| invoke>\\n</|DSML| tool_calls>\"}\n");
        let final_deltas = sp.finish();
        let has_tool = final_deltas
            .iter()
            .any(|d| serde_json::from_str::<serde_json::Value>(d).map(|v| v.get("tool_calls").is_some()).unwrap_or(false));
        assert!(has_tool, "DSML 工具调用应解析出 tool_calls, 实际: {:?}", final_deltas);
    }
}


