// DeepSeek 回答文本解析：工具调用围栏（自有 <tool_call> / XML / DSML / YAML）识别与解析、
// OpenAI 兼容 usage 估算、回答前缀提取，以及网页 SSE 片段的增量解析。

use serde_json::{json, Value};
use std::collections::HashMap;

/// 工具调用围栏：XML 风格（避免参数内容含 ``` 三反引号时与围栏冲突）。
/// 开围栏变体：<tool_call> / 漏下划线 <toolcall>；兼容属性形式 <tool_call name="Read">
///
/// 标签名以 `crate::prompts::poolman::TOOL_CALL_TAG` 为唯一真相；
/// 本文件的字面量与提示词正文 `prompts/system.md` 的一致性由单测锁死
/// （见 `prompts::poolman` 与 `tool_parsing` 各自的 fence 同源测试）。
pub(crate) const TOOL_FENCE_PREFIXES: [&str; 2] = ["<tool_call", "<toolcall"];
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

/* ------------------------------ 辅助函数 ------------------------------ */

/// 毫秒级时间戳（保证每次回答前缀唯一）
pub(crate) fn format_ts_ms() -> String {
    let now = chrono::Local::now();
    format!(
        "{}.{:03}",
        now.format("%Y/%m/%d %H:%M:%S"),
        now.timestamp_subsec_millis()
    )
}

/// 从回答内容提取「会话: <会话id>」，供追问复用会话。
/// ⚠️ 必须用 find("会话") 精确定位：前缀「回答时间: <ts>\n会话: <id>」中
/// "会话" 位于字节偏移 37（时间戳格式固定），不是 6 的倍数，
/// 若用 i += "会话".len() 字节跳步扫描会漏掉 → 追问解析不到会话 id → 重启后新建会话。
pub(crate) fn extract_session_id(text: &str) -> Option<String> {
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
            if id.len() >= 10
                && id
                    .chars()
                    .next()
                    .map_or(false, |c| c.is_ascii_alphanumeric())
            {
                last = Some(id);
            }
        }
        cursor = abs + "会话".len();
    }
    last
}

/// 从 422 报错里解析缺失字段名，如 "missing field `ref_file_ids`"
pub(crate) fn parse_missing_field(text: &str) -> Option<String> {
    let start = text.find("missing field `")? + "missing field `".len();
    let end = text[start..].find('`')? + start;
    Some(text[start..end].to_string())
}

/// 为缺失字段猜测默认值
pub(crate) fn guess_default(field: &str) -> Value {
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
pub(crate) fn parse_biz_error(body: &str) -> Option<(i64, String)> {
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
pub(crate) fn contains_tool_open_tag(buf: &str) -> bool {
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
pub(crate) fn xml_fence_remaining(tail: &str) -> Option<usize> {
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
    !name.is_empty()
        && !name
            .chars()
            .any(|c| c.is_whitespace() || c == '<' || c == '>')
}

/// 文本里是否留有「像工具调用但没被解析」的痕迹（用于告警：判断模型是否未按提示词围栏格式输出）
pub(crate) fn looks_like_unparsed_tool_call(text: &str) -> bool {
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
        map.insert(
            key,
            xml_param_value(&body[tag.end..value_end], force_string),
        );
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
                .or_else(|| {
                    obj.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())
                })
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
                            !matches!(
                                k.as_str(),
                                "name" | "function" | "arguments" | "parameters" | "args"
                            )
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
        let Some((_pos, fence_end, attr_name)) = find_open_fence(rest) else {
            break;
        };
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
        v.as_array()
            .and_then(|a| a.first())
            .cloned()
            .unwrap_or(json!({}))
    } else {
        v.clone()
    };
    obj.get("name").is_some() || obj.get("function").is_some()
}

#[cfg(test)]
mod tests {
    use super::{TOOL_FENCE_CLOSE, TOOL_FENCE_PREFIXES, TOOL_TAG_NAMES};
    use crate::prompts::poolman::TOOL_CALL_TAG;

    /// 围栏同源（解析器侧）：本文件接受的字面量必须覆盖 poolman 的围栏标签名。
    /// 提示词侧由 `prompts::poolman::fence_spec_is_single_source` 守住；两侧合起来锁死三方。
    #[test]
    fn fence_constants_match_poolman_tag() {
        assert_eq!(
            TOOL_CALL_TAG, "tool_call",
            "围栏标签名被改动，需同步解析器与提示词"
        );
        let open = format!("<{}", TOOL_CALL_TAG); // <tool_call
        let close = format!("</{}>", TOOL_CALL_TAG); // </tool_call>
        assert!(
            TOOL_FENCE_PREFIXES.contains(&open.as_str()),
            "开围栏前缀未覆盖 {}",
            open
        );
        assert_eq!(TOOL_FENCE_CLOSE, close, "闭围栏常量与标签名不一致");
        assert!(
            TOOL_TAG_NAMES.contains(&TOOL_CALL_TAG),
            "标记名集合未包含 {}",
            TOOL_CALL_TAG
        );
    }
}

/// 从回答内容解析工具调用 → OpenAI 标准 tool_calls 数组
pub(crate) fn parse_tool_calls(content: &str) -> Option<Value> {    let text = extract_response_text(content);
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
pub(crate) fn extract_response_text(content: &str) -> String {
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
pub(crate) fn attach_answer_prefix(content: &str, ts: &str, session_id: &str) -> String {
    format!("回答时间: {}\n会话: {}\n{}", ts, session_id, content)
}

/// 估算文本对应的 token 数（DeepSeek 网页版不返回真实 usage，只能近似）：
/// 中文等 CJK 字符按 1 字 ≈ 1 token；其它字符按每 4 字符 ≈ 1 token（向上取整）。
pub(crate) fn estimate_tokens(text: &str) -> u32 {
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
pub(crate) fn make_usage(prompt_tokens: u32, completion_tokens: u32) -> Value {
    json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": prompt_tokens + completion_tokens,
    })
}

/// 递归收集 Value 中的所有字符串叶子并拼接（估算模型生成 {THINK,RESPONSE} 的输出 token 用）
pub(crate) fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(value_text).collect(),
        Value::Object(m) => m.values().map(value_text).collect(),
        _ => String::new(),
    }
}

/// 从流式转发增量（{"THINK":..} / {"RESPONSE":..} / {"tool_calls":..}）提取纯文本，用于累计输出 token。
/// 网关自加前缀「回答时间/会话」也是实际输出的 token，故保留不剥。
pub(crate) fn delta_text(d: &str) -> String {
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
pub(crate) struct DeepSeekParser {
    fragments: Vec<Fragment>,
    sent_length: Vec<usize>,
    hint_error: Option<String>,
    /// 最近一次响应的消息 id（供下轮 parent_message_id；比 fetch_history 更可靠）
    message_id: Option<i64>,
}

impl DeepSeekParser {
    pub(crate) fn new() -> Self {
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
                    let ftype = f
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let content = f
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
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
                            ftype: f
                                .get("type")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            content: f
                                .get("content")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
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
                            let op = obj
                                .get("o")
                                .and_then(|v| v.as_str())
                                .unwrap_or("APPEND")
                                .to_uppercase();
                            if idx >= 0 {
                                let idx = idx as usize;
                                if idx >= self.fragments.len() {
                                    self.fragments.push(Fragment {
                                        ftype: String::new(),
                                        content: String::new(),
                                    });
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
                self.fragments.push(Fragment {
                    ftype: String::new(),
                    content: v.to_string(),
                });
            }
        }
    }

    pub(crate) fn process_line(&mut self, line: &str) {
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
    pub(crate) fn answer(&mut self) -> Value {
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
                chunks
                    .entry(f.ftype.clone())
                    .or_default()
                    .push_str(&f.content);
            }
        }
        let mut obj = serde_json::Map::new();
        for (k, v) in chunks {
            obj.insert(k, Value::String(v));
        }
        Value::Object(obj)
    }

    /// 取出 SSE hint 错误（如「消息发送过于频繁」rate_limit）
    pub(crate) fn take_hint_error(&mut self) -> Option<String> {
        self.hint_error.take()
    }

    /// 最近一次响应的消息 id（供下轮 parent_message_id；比 fetch_history 更可靠）
    pub(crate) fn message_id(&self) -> Option<i64> {
        self.message_id
    }
}
