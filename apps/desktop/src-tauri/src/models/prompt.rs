//! 网页渠道共用的提示词拼装：把 OpenAI 形状的推理请求摊平成渠道所需的单段文本。
//!
//! 网页会话本身带着历史，因此每轮都把整段对话拼进提示词，并要求模型只依据本次内容作答。
//! 分段规则与工具调用标记在渠道间共用；标记常量与规则文本由 [`super::deepseek_web`] 提供。

use std::collections::HashMap;

use serde_json::{json, Value};
use tracing::warn;

use super::deepseek_web::{
    escape, CALLS_CLOSE, CALLS_OPEN, HISTORY_HEADING, INVOKE_CLOSE, INVOKE_OPEN, NORMALIZER_PROMPT,
    PARAMETER_CLOSE, PARAMETER_OPEN, PROMPT_PREFIX, SYSTEM_HEADING, TOOL_RULE, TOOL_RULE_HEADING,
    TOOLS_HEADING,
};
use super::defs::InferRequest;

/// 把推理请求拼成渠道所需的 prompt，顺序固定为：
/// 固定前缀 → 系统提示词 → 历史对话 → 工具调用格式与限定规则 → 可用工具 → 最新一条记录。
///
/// 工具结果的标题带上工具名与调用 id，与助手标记里那次 `invoke` 一一对应。
pub(crate) fn build_prompt(request: &InferRequest) -> String {
    let mut sections: Vec<String> = vec![PROMPT_PREFIX.to_string()];

    let system = render_system(&request.messages);
    if !system.is_empty() {
        sections.push(format!("{}{}{}", SYSTEM_HEADING, NEWLINE, system));
    }

    // system 消息已单独成段，其余消息里末尾那条是最新记录，之前的都算历史对话。
    let turns: Vec<&Value> = request
        .messages
        .iter()
        .filter(|message| !is_system(message))
        .collect();
    let Some((latest, history)) = turns.split_last() else {
        return sections.join(&DOUBLE_NEWLINE);
    };
    let tool_names = tool_names(&request.messages);
    let history = history
        .iter()
        .filter_map(|message| render_message(message, &tool_names))
        .collect::<Vec<_>>();
    if !history.is_empty() {
        sections.push(format!(
            "{}{}{}",
            HISTORY_HEADING,
            NEWLINE,
            history.join(&DOUBLE_NEWLINE)
        ));
    }
    // 工具段只在确实有工具时出现：格式规则与工具清单成对给出。
    if let Some(tools) = render_tools(request.tools.as_ref()) {
        sections.push(format!("{}{}{}", TOOL_RULE_HEADING, NEWLINE, TOOL_RULE));
        sections.push(format!("{}{}{}", TOOLS_HEADING, NEWLINE, tools));
    }
    if let Some(latest) = render_message(latest, &tool_names) {
        sections.push(latest);
    }
    sections.join(&DOUBLE_NEWLINE)
}

/// 组装修复提示词：规范化器提示词填上可用工具清单（含每个参数的 JSON Schema 与必填项），
/// 末尾附上**本轮失败的具体原因**与解析失败的坏块原文——模型据此知道该补哪一处，
/// 而不是笼统地「重写一遍」。
pub(crate) fn repair_prompt(tools: Option<&Value>, broken_markup: &str, reason: &str) -> String {
    let registered = render_tools(tools).unwrap_or_else(|| "（无）".to_string());
    format!(
        "{}{}{}{}本轮失败原因：{}{}以下是用户输入内容：{}{}",
        PROMPT_PREFIX,
        DOUBLE_NEWLINE,
        NORMALIZER_PROMPT.replace("{{REGISTERED_TOOLS}}", &registered),
        DOUBLE_NEWLINE,
        reason,
        DOUBLE_NEWLINE,
        NEWLINE,
        broken_markup
    )
}

/// 单个换行。
const NEWLINE: &str = "\n";

/// 两个换行。
const DOUBLE_NEWLINE: &str = "\n\n";

/// 是否为 system 消息。
fn is_system(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("system")
}

/// 系统提示词段：取全部 system 消息正文，多条之间空行分隔。
fn render_system(messages: &[Value]) -> String {
    messages
        .iter()
        .filter(|message| is_system(message))
        .map(message_content)
        .filter(|content| !content.is_empty())
        .collect::<Vec<_>>()
        .join(&DOUBLE_NEWLINE)
}

/// 取消息正文：字符串，或多模态内容数组里的文本片段；非文本片段只留占位标记，
/// 避免把图片的 base64 塞进提示词。
fn message_content(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(render_part)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(NEWLINE),
        _ => String::new(),
    }
}

/// 单条消息 → `【角色】正文`；助手消息里的历史工具调用按 DSML 标记接在正文之后。
///
/// 正文只有空白时按空处理：模型写在标记前后的排版、客户端存下的空段都不算回答内容，
/// 原样写进提示词只会变成大段空行。
fn render_message(message: &Value, tool_names: &HashMap<String, String>) -> Option<String> {
    let role = message.get("role").and_then(Value::as_str).unwrap_or("");
    let mut content = message_content(message);
    if content.trim().is_empty() {
        content.clear();
    }
    for call in render_tool_calls(message) {
        if !content.is_empty() {
            content.push_str(NEWLINE);
        }
        content.push_str(&call);
    }
    if role.is_empty() && content.is_empty() {
        return None;
    }
    let heading = match tool_result_label(message, tool_names) {
        Some(label) => format!("【{}】{}", role, label),
        None => format!("【{}】", role),
    };
    if content.is_empty() {
        return Some(heading);
    }
    Some(format!("{}{}{}", heading, NEWLINE, content))
}

/// 工具结果消息的来源标签：`name=工具名 id=调用id`，缺哪半就只写另一半，都缺则不标。
///
/// 工具名优先取消息自带的 `name`，没有就按 `tool_call_id` 到本轮全部助手消息的
/// `tool_calls` 里反查——客户端通常只回 `tool_call_id`。
fn tool_result_label(message: &Value, tool_names: &HashMap<String, String>) -> Option<String> {
    if message.get("role").and_then(Value::as_str) != Some("tool") {
        return None;
    }
    let id = message.get("tool_call_id").and_then(Value::as_str);
    let name = message
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| id.and_then(|id| tool_names.get(id).map(String::as_str)));
    match (name, id) {
        (Some(name), Some(id)) => Some(format!("name={name} id={id}")),
        (Some(name), None) => Some(format!("name={name}")),
        (None, Some(id)) => Some(format!("id={id}")),
        (None, None) => None,
    }
}

/// 全部助手消息里的 `tool_calls` → `调用id → 工具名`，供工具结果标注来源。
fn tool_names(messages: &[Value]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for message in messages {
        let Some(calls) = message.get("tool_calls").and_then(Value::as_array) else {
            continue;
        };
        for call in calls {
            let Some(id) = call.get("id").and_then(Value::as_str) else {
                continue;
            };
            let function = call.get("function").unwrap_or(call);
            if let Some(name) = function.get("name").and_then(Value::as_str) {
                names.insert(id.to_string(), name.to_string());
            }
        }
    }
    names
}

/// 助手消息里的 `tool_calls` → DSML 标记，一次调用一层 `invoke`，多个平铺在一个 `calls` 块里。
///
/// 参数是字符串时按 JSON 解析成对象，解析不了就原样保留。渲染出来的标记与解析器认的格式一致：
/// 字符串参数标 `string=true` 原样写，其余（数字 / 布尔 / 数组 / 对象）标 `string=false` 按 JSON 写，
/// 这样同一段标记被解析回来仍是同一个调用。
pub(crate) fn render_tool_calls(message: &Value) -> Vec<String> {
    let Some(calls) = message.get("tool_calls").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    for call in calls {
        let function = call.get("function").unwrap_or(call);
        let Some(name) = function.get("name").and_then(Value::as_str) else {
            continue;
        };
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .map(|id| format!(" id={id:?}"))
            .unwrap_or_default();
        let arguments = match function.get("arguments") {
            Some(Value::String(raw)) => {
                serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.clone()))
            }
            Some(other) => other.clone(),
            None => json!({}),
        };
        let Some(params) = arguments.as_object() else {
            // 参数不是对象（客户端给了坏数据）：只还原工具名，别编造参数
            warn!(
                "历史工具调用的 arguments 不是 JSON 对象，按无参还原：{}",
                name
            );
            lines.push(format!(
                "{INVOKE_OPEN} name={name:?}{id}>{INVOKE_CLOSE}"
            ));
            continue;
        };
        lines.push(format!("{INVOKE_OPEN} name={name:?}{id}>"));
        for (key, value) in params {
            // 字符串参数按解析端的反转义规则转义，保证渲染后再解析回来还是同一个值
            let (flag, literal) = match value {
                Value::String(text) => ("true", escape(text)),
                other => ("false", other.to_string()),
            };
            lines.push(format!(
                "{PARAMETER_OPEN} name={key:?} string={flag:?}>{literal}{PARAMETER_CLOSE}"
            ));
        }
        lines.push(INVOKE_CLOSE.to_string());
    }
    if lines.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(lines.len() + 2);
    out.push(CALLS_OPEN.to_string());
    out.extend(lines);
    out.push(CALLS_CLOSE.to_string());
    out
}

/// 多模态内容片段 → 文本；非文本片段给占位标记（如 `[image_url]`）。
fn render_part(part: &Value) -> String {
    if let Some(text) = part.get("text").and_then(Value::as_str) {
        return text.to_string();
    }
    let kind = part
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("未知片段");
    format!("[{}]", kind)
}

/// 工具清单：每项一行 `- 名称：描述`，参数以紧凑 JSON 附在其后；没有可用工具时返回 None。
fn render_tools(tools: Option<&Value>) -> Option<String> {
    let items = tools
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())?;
    let lines = items
        .iter()
        .filter_map(|item| {
            let function = item.get("function").unwrap_or(item);
            let name = function.get("name").and_then(Value::as_str).unwrap_or("");
            if name.is_empty() {
                return None;
            }
            let mut line = format!("- {}", name);
            if let Some(description) = function.get("description").and_then(Value::as_str) {
                if !description.is_empty() {
                    line.push_str(&format!("：{}", description));
                }
            }
            if let Some(parameters) = function.get("parameters") {
                line.push_str(&format!("{}{}参数：{}", NEWLINE, "  ", parameters));
            }
            Some(line)
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return None;
    }
    Some(lines.join(NEWLINE))
}