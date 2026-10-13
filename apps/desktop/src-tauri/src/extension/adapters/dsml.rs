use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// DSML 标记的两种形态：全角 `｜｜DSML｜｜` 与半角 `||DSML||`。
/// 标记前后允许空白，`<` `>` 与标记之间也允许空白。
const MARKERS: [&str; 2] = ["｜｜DSML｜｜", "||DSML||"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// OpenAI 约定：arguments 是 JSON 字符串
    pub arguments: String,
}

#[derive(Debug, Default)]
pub struct ParsedOutput {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
}

/// 跳过从 `pos` 开始的所有 ASCII 空白，返回新位置。
fn skip_ws(bytes: &[u8], mut pos: usize) -> usize {
    while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
        pos += 1;
    }
    pos
}

/// 若 `text[pos..]` 以 `<` + 任意 DSML 标记开头，返回 (标记结束位置, 命中标记)。
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

/// 若 `text[pos..]` 以 `</` + 任意 DSML 标记开头，返回结束位置。
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

/// 在 `pos` 之后找到匹配的 `</...DSML... 结束标记`，返回 (内容, 结束标记之后的索引)。
/// `tag` 为 `calls` / `invoke` / `parameter`。
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

/// 在 `pos` 之后找到 `>`，返回其后索引。
fn find_gt(text: &str, pos: usize) -> Option<usize> {
    text[pos..].find('>').map(|i| pos + i + 1)
}

/// 解析 `<...DSML... invoke name="...">` 的开标签，返回 (name, 内容起始)。
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

/// 解析 `<...DSML... parameter name="..." string="...">`，返回 (name, is_string, 内容起始)。
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

/// 从 `pos` 开始解析 `key="value"`，返回属性值。
fn extract_attr(text: &str, mut pos: usize, key: &str) -> Option<String> {
    let bytes = text.as_bytes();
    loop {
        pos = skip_ws(bytes, pos);
        if pos >= bytes.len() || bytes[pos] == b'>' {
            return None;
        }
        let key_start = pos;
        while pos < bytes.len() && bytes[pos] != b'=' && !bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        let attr_key = &text[key_start..pos];
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

/// 按 OpenAI 约定把参数值转为 JSON。
fn value_to_json(raw: &str, as_string: bool) -> serde_json::Value {
    if as_string {
        return serde_json::Value::String(raw.to_string());
    }
    let trimmed = raw.trim();
    serde_json::from_str(trimmed)
        .unwrap_or_else(|_| serde_json::Value::String(trimmed.to_string()))
}

/// 解析文本中的全部 DSML 工具调用，并返回剥离 DSML 后的正文。
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

/// 快速判断文本是否包含 calls 块开头，流式场景可用于决定是否缓冲。
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