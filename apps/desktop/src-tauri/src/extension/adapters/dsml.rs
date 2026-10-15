use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// DSML（DeepSeek Markup Language）标记的两种形态：
/// - 全角竖线 `｜｜DSML｜｜`（U+FF5C）
/// - 半角竖线 `||DSML||`
///
/// 标记出现在标签名之前，构成形如 `<｜｜DSML｜｜ invoke name="x">` 的标签。
/// 标记与 `<`、标记与标签名之间都允许存在任意 ASCII 空白。
const MARKERS: [&str; 2] = ["｜｜DSML｜｜", "||DSML||"];

/// 一次工具调用，字段布局对齐 OpenAI 的 `tool_calls[]` 元素。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// 调用标识，按解析顺序生成（`call_0`、`call_1`…）。
    pub id: String,
    /// 调用类型，固定为 `function`；序列化为 JSON 时键名为 `type`。
    #[serde(rename = "type")]
    pub call_type: String,
    /// 被调用函数的信息。
    pub function: FunctionCall,
}

/// 被调用函数的名称与参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    /// 工具名，取自 `invoke` 标签的 `name` 属性。
    pub name: String,
    /// OpenAI 约定：arguments 是 JSON 字符串，而非 JSON 对象。
    pub arguments: String,
}

/// 单次解析的整体结果。
#[derive(Debug, Default)]
pub struct ParsedOutput {
    /// 剥离全部 DSML 工具调用块后剩余的正文（已去除首尾空白）。
    pub content: String,
    /// 按出现顺序解析出的全部工具调用；无调用时为空向量。
    pub tool_calls: Vec<ToolCall>,
}

/// 跳过从 `pos` 开始的所有 ASCII 空白（空格、制表、换行等），返回首个非空白位置。
/// `pos` 超出长度时原样返回。仅处理 ASCII 空白，不涉及全角空格。
fn skip_ws(bytes: &[u8], mut pos: usize) -> usize {
    while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
        pos += 1;
    }
    pos
}

/// 尝试把 `pos` 处识别为一个「开标记」：`<` 后接（可含空白）DSML 标记。
///
/// 命中时返回 `(标记结束位置, 命中的标记字面量)`，结束位置指向标记之后、
/// 便于调用方继续读取紧随其后的标签名（如 `calls` / `invoke` / `parameter`）。
/// `pos` 处不是 `<`，或 `<` 后不是任一 DSML 标记时返回 `None`。
fn match_open_marker(text: &str, pos: usize) -> Option<(usize, &'static str)> {
    if !text[pos..].starts_with('<') {
        return None;
    }
    let after_lt = skip_ws(text.as_bytes(), pos + 1);
    for m in MARKERS {
        if text[after_lt..].starts_with(m) {
            return Some((after_lt + m.len(), m));
        }
    }
    None
}

/// 尝试把 `pos` 处识别为一个「闭合标记」：`</` 后接（可含空白）DSML 标记。
///
/// 命中时返回标记结束位置（指向标记之后、标签名之前），否则返回 `None`。
/// 与 [`match_open_marker`] 的区别仅在于 `<` 与标记之间多了一个 `/`。
fn match_close_marker(text: &str, pos: usize) -> Option<usize> {
    if !text[pos..].starts_with('<') {
        return None;
    }
    let after_lt = skip_ws(text.as_bytes(), pos + 1);
    if !text[after_lt..].starts_with('/') {
        return None;
    }
    let after_slash = skip_ws(text.as_bytes(), after_lt + 1);
    for m in MARKERS {
        if text[after_slash..].starts_with(m) {
            return Some(after_slash + m.len());
        }
    }
    None
}

/// 从 `pos` 起查找指定标签的闭合标记标签，返回 `(闭合标签起始下标, 闭合标签之后的索引)`。
///
/// `tag` 取 `calls` / `invoke` / `parameter`；标签名与标记之间允许空白，
/// 标签名之后允许空白但必须以 `>` 结束，否则继续向后查找。
/// 找不到合法闭合标签时返回 `None`。
/// 注意：逐字节推进，只有遇到 `<` 才尝试匹配，避免把正文里的普通字符误判为标签。
fn find_close(text: &str, tag: &str, pos: usize) -> Option<(usize, usize)> {
    let mut i = pos;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if let Some(close_start) = match_close_marker(text, i) {
                let after = skip_ws(bytes, close_start);
                if text[after..].starts_with(tag) {
                    let after_tag = after + tag.len();
                    let after_tag = skip_ws(bytes, after_tag);
                    if after_tag < bytes.len() && bytes[after_tag] == b'>' {
                        return Some((i, after_tag + 1));
                    }
                }
            }
        }
        i += 1;
    }
    None
}

/// 在 `pos` 之后查找第一个 `>`，返回其下一个字节的索引（即标签体起始位置）。
/// 未找到 `>`（标签未闭合）时返回 `None`。
fn find_gt(text: &str, pos: usize) -> Option<usize> {
    text[pos..].find('>').map(|i| pos + i + 1)
}

/// 解析 `<...DSML... invoke name="...">` 形式的开标签。
///
/// 返回 `(工具名, 标签体起始索引)`；标签体起始索引即 `>` 之后的位置，
/// 供调用方截取 `invoke` 与闭合标签之间的内容。
/// 缺少 `name` 属性或标签未闭合时返回 `None`。
fn parse_invoke_open(text: &str, pos: usize) -> Option<(String, usize)> {
    let (marker_end, _) = match_open_marker(text, pos)?;
    let after = skip_ws(text.as_bytes(), marker_end);
    if !text[after..].starts_with("invoke") {
        return None;
    }
    let after_tag = skip_ws(text.as_bytes(), after + "invoke".len());
    let name = extract_attr(text, after_tag, "name")?;
    let gt = find_gt(text, after_tag)?;
    Some((name, gt))
}

/// 解析 `<...DSML... parameter name="..." string="...">` 形式的开标签。
///
/// 返回 `(参数名, 是否按字符串处理, 标签体起始索引)`。
/// `string` 属性缺省视为 `true`（按字符串处理）；仅当显式为 `"false"` 时才按非字符串处理。
/// 缺少 `name` 属性或标签未闭合时返回 `None`。
fn parse_param_open(text: &str, pos: usize) -> Option<(String, bool, usize)> {
    let (marker_end, _) = match_open_marker(text, pos)?;
    let after = skip_ws(text.as_bytes(), marker_end);
    if !text[after..].starts_with("parameter") {
        return None;
    }
    let after_tag = skip_ws(text.as_bytes(), after + "parameter".len());
    let name = extract_attr(text, after_tag, "name")?;
    let is_string = extract_attr(text, after_tag, "string")
        .map(|v| v != "false")
        .unwrap_or(true);
    let gt = find_gt(text, after_tag)?;
    Some((name, is_string, gt))
}

/// 从 `pos` 开始扫描标签属性，取出指定 `key` 的字符串值。
///
/// 逐个解析 `key="value"` 形式：属性名后可跟可选空白再接 `=`，值必须以双引号包裹。
/// 命中目标 `key` 时返回值内容（不含引号）；当前属性缺少 `=` 或值未用双引号包裹时跳过、
/// 继续扫描下一个属性。遇到 `>`（标签结束）或文本结尾仍未命中则返回 `None`。
fn extract_attr(text: &str, mut pos: usize, key: &str) -> Option<String> {
    let bytes = text.as_bytes();
    loop {
        pos = skip_ws(bytes, pos);
        if pos >= bytes.len() || bytes[pos] == b'>' {
            return None;
        }
        // 属性名从当前位置开始，读到 '=' 或空白为止
        let key_start = pos;
        while pos < bytes.len() && bytes[pos] != b'=' && !bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }        let attr_key = &text[key_start..pos];
        pos = skip_ws(bytes, pos);
        if pos < bytes.len() && bytes[pos] == b'=' {
            pos = skip_ws(bytes, pos + 1);
            if pos < bytes.len() && bytes[pos] == b'"' {
                let value_start = pos + 1;
                let value_end = text[value_start..].find('"')? + value_start;
                if attr_key == key {
                    return Some(text[value_start..value_end].to_string());
                }
                pos = value_end + 1;
                continue;
            }
        }
        if attr_key == key {
            return None;
        }
    }
}

/// 按 OpenAI 约定把参数原文转为 JSON 值。
///
/// - `as_string == true`：原样作为字符串，不解析（保留内部换行与空白）。
/// - `as_string == false`：去首尾空白后尝试按 JSON 解析，得到数字、布尔、null、
///   数组或对象等结构；解析失败时退化为字符串，保证不丢内容。
fn value_to_json(raw: &str, as_string: bool) -> serde_json::Value {
    if as_string {
        return serde_json::Value::String(raw.to_string());
    }
    let trimmed = raw.trim();
    serde_json::from_str(trimmed)
        .unwrap_or_else(|_| serde_json::Value::String(trimmed.to_string()))
}

/// 解析文本中的全部 DSML 工具调用，并返回剥离 DSML 后的正文。
///
/// 处理流程：
/// 1. 逐字符扫描正文，遇到 `<...DSML... calls>` 开标记则进入块处理，否则原样保留字符。
/// 2. 在 `calls` 块内循环解析每个 `invoke` 开标签，取工具名，再解析其内部全部
///    `parameter` 得到参数集合，序列化为 JSON 字符串后组装成 [`ToolCall`]。
/// 3. 块处理完毕后跳过整块，继续扫描后续正文。
///
/// 边界行为：非 `calls` 的 DSML 标记（如单独的 `invoke`）按普通文本保留；
/// `calls` 块缺少 `>` 或闭合标签时停止解析，剩余文本不再并入正文。
pub fn parse(text: &str) -> ParsedOutput {
    let mut tool_calls = Vec::new();
    let mut i = 0;
    let mut content = String::new();

    while i < text.len() {
        // 寻找 `<...DSML... calls>`
        let Some((marker_end, _)) = match_open_marker(text, i) else {
            // 拷贝一个字符（保持 UTF-8 边界）
            let ch = text[i..].chars().next().unwrap();
            content.push(ch);
            i += ch.len_utf8();
            continue;
        };

        let after = skip_ws(text.as_bytes(), marker_end);
        if !text[after..].starts_with("calls") {
            // 不是 calls 块，按普通文本处理
            let ch = text[i..].chars().next().unwrap();
            content.push(ch);
            i += ch.len_utf8();
            continue;
        }

        let Some(gt) = find_gt(text, after + "calls".len()) else {
            break;
        };
        let Some((body_start, block_end)) = find_close(text, "calls", gt) else {
            break;
        };
        let block = &text[gt..body_start];

        // 在块内逐个解析 invoke
        let mut p = 0;
        while p < block.len() {
            let Some((name, open_end)) = parse_invoke_open(block, p) else {
                p += 1;
                continue;
            };
            let Some((invoke_body_start, invoke_end)) = find_close(block, "invoke", open_end) else {
                break;
            };
            let invoke_body = &block[open_end..invoke_body_start];

            let mut args = BTreeMap::new();
            let mut q = 0;
            while q < invoke_body.len() {
                let Some((pname, is_string, param_open_end)) =
                    parse_param_open(invoke_body, q)
                else {
                    q += 1;
                    continue;
                };
                let Some((param_body_start, param_end)) =
                    find_close(invoke_body, "parameter", param_open_end)
                else {
                    break;
                };
                let raw = &invoke_body[param_open_end..param_body_start];
                args.insert(pname, value_to_json(raw, is_string));
                q = param_end;
            }

            let arguments =
                serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
            tool_calls.push(ToolCall {
                id: format!("call_{}", tool_calls.len()),
                call_type: "function".to_string(),
                function: FunctionCall { name, arguments },
            });

            p = invoke_end;
        }

        i = block_end;
    }

    ParsedOutput {
        content: content.trim().to_string(),
        tool_calls,
    }
}

/// 快速判断文本是否包含 `calls` 块开头（`<...DSML... calls>`）。
///
/// 只检测开标记，不要求块已闭合，因此适合流式场景：一旦返回 `true`，
/// 调用方即可决定开始缓冲后续内容而不必等到完整块到达。
pub fn contains_dsml(text: &str) -> bool {
    let mut i = 0;
    while i < text.len() {
        if let Some((marker_end, _)) = match_open_marker(text, i) {
            let after = skip_ws(text.as_bytes(), marker_end);
            if text[after..].starts_with("calls") {
                return true;
            }
        }
        i += 1;
    }
    false
}
#[cfg(test)]
mod tests {
    use super::*;

    /// 覆盖完整 DSML 块：多个 `invoke`、字符串/数字混合参数、
    /// 正文前后缀保留，以及工具调用与正文的正确切分。
    #[test]
    fn parses_two_invokes() {
        let raw = r#"前缀<｜｜DSML｜｜ calls>
<｜｜DSML｜｜ invoke name="powershell">
<｜｜DSML｜｜ parameter name="command" string="true">Get-ChildItem -Path "d:\x"</｜｜DSML｜｜ parameter>
<｜｜DSML｜｜ parameter name="timeout" string="false">30</｜｜DSML｜｜ parameter>
</｜｜DSML｜｜ invoke>
<｜｜DSML｜｜ invoke name="view">
<｜｜DSML｜｜ parameter name="path" string="true">a/b.rs</｜｜DSML｜｜ parameter>
</｜｜DSML｜｜ invoke>
</｜｜DSML｜｜ calls>后缀"#;

        let out = parse(raw);
        assert_eq!(out.tool_calls.len(), 2);
        assert_eq!(out.tool_calls[0].function.name, "powershell");

        let args: serde_json::Value =
            serde_json::from_str(&out.tool_calls[0].function.arguments).unwrap();
        assert_eq!(args["command"], "Get-ChildItem -Path \"d:\\x\"");
        assert_eq!(args["timeout"], 30);
        assert_eq!(out.content, "前缀后缀");
    }

    #[test]
    fn no_dsml_is_passthrough() {
        let out = parse("just text");
        assert!(out.tool_calls.is_empty());
        assert_eq!(out.content, "just text");
    }
}