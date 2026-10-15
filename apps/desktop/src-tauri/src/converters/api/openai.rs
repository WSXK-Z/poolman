//! OpenAI 兼容格式的 API 转换器。
//!
//! 入站：`messages` / `tools` / 采样参数 → [`AgentInput`]（系统原始提示词、历史对话、
//! 用户最新提问、最新工具结果四类内容分开摆放）。
//! 出站：由 `apis::openai` 负责编码成 `chat.completion` 或 SSE（见该模块）。

use serde_json::Value;

use crate::agent::input::{AgentInput, Params, Turn};

/// OpenAI 兼容请求 → 内部输入参数格式。
///
/// 拆分规则（与上游客户端的习惯一致）：
/// - `system` 消息按出现顺序拼接成 [`AgentInput::system_prompt`]（保留原文，不在这里拆标签）
/// - 末尾连续的 `tool` 消息 → [`AgentInput::tool_results`]（上一轮工具调用的回执）
/// - 最后一条 `user` 消息 → [`AgentInput::question`]
/// - 其余 `user` / `assistant` 消息按顺序进 [`AgentInput::history`]
pub fn to_agent_input(messages: &[Value], tools: Option<Value>, params: Params) -> AgentInput {
    let mut system_parts: Vec<String> = Vec::new();
    let mut turns: Vec<Turn> = Vec::new();

    for message in messages {
        let role = message
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let content = content_text(message);
        match role {
            "system" => {
                if !content.is_empty() {
                    system_parts.push(content);
                }
            }
            "user" | "assistant" | "tool" => turns.push(Turn {
                role: role.to_string(),
                content,
                tool_call_id: message
                    .get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .map(String::from),
            }),
            _ => {}
        }
    }

    // 末尾连续的 tool 结果属于「上一轮工具调用」，单独拎出来
    let mut tool_results: Vec<Turn> = Vec::new();
    while turns.last().map_or(false, |t| t.role == "tool") {
        if let Some(turn) = turns.pop() {
            tool_results.push(turn);
        }
    }
    tool_results.reverse();

    // 最后一条 user 消息是本次提问（历史重试产生的更早 user 留在 history 里）
    let mut question = String::new();
    if let Some(pos) = turns.iter().rposition(|t| t.role == "user") {
        question = turns.remove(pos).content;
    }

    AgentInput {
        system_prompt: system_parts.join("\n"),
        history: turns,
        question,
        tool_results,
        tools,
        params,
    }
}

/// 消息正文归一为纯文本：多模态 `content` 数组只取其中的 text 项。
fn content_text(message: &Value) -> String {
    let Some(content) = message.get("content") else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    if let Some(parts) = content.as_array() {
        let joined = parts
            .iter()
            .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        return joined;
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::to_agent_input;
    use crate::agent::input::Params;
    use serde_json::json;

    #[test]
    fn splits_openai_messages_into_four_parts() {
        let messages = vec![
            json!({"role": "system", "content": "你是助手"}),
            json!({"role": "system", "content": "第二条系统提示"}),
            json!({"role": "user", "content": "第一个问题"}),
            json!({"role": "assistant", "content": "第一个回答"}),
            json!({"role": "user", "content": "上一个问题"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "c1"}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "工具结果 A"}),
            json!({"role": "user", "content": "现在的问题是这个"}),
        ];
        let input = to_agent_input(&messages, Some(json!([{"type": "function"}])), Params::default());

        assert_eq!(input.system_prompt, "你是助手\n第二条系统提示");
        assert_eq!(input.question, "现在的问题是这个");
        assert!(input.tool_results.is_empty(), "提问在 tool 之后，本轮没有待回执的工具结果");
        assert!(input.has_tools());
        // 历史里保留除「最后一条 user」之外的对话（含中间那条工具结果）
        let history: Vec<(&str, &str)> = input
            .history
            .iter()
            .map(|t| (t.role.as_str(), t.content.as_str()))
            .collect();
        assert_eq!(
            history,
            vec![
                ("user", "第一个问题"),
                ("assistant", "第一个回答"),
                ("user", "上一个问题"),
                ("assistant", ""),
                ("tool", "工具结果 A"),
            ]
        );
    }

    #[test]
    fn trailing_tool_messages_become_tool_results() {
        let messages = vec![
            json!({"role": "user", "content": "请读取文件"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "c1"}]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "文件内容 ABC"}),
            json!({"role": "tool", "tool_call_id": "c2", "content": "文件内容 DEF"}),
        ];
        let input = to_agent_input(&messages, None, Params::default());

        assert_eq!(input.tool_results.len(), 2);
        assert_eq!(input.tool_results[0].content, "文件内容 ABC", "工具结果应保持原顺序");
        assert_eq!(input.tool_results[1].content, "文件内容 DEF");
        // 提问是那条 user，历史里不再重复它
        assert_eq!(input.question, "请读取文件");
        assert!(!input.is_first_turn(), "历史里有 assistant，不是首轮");
    }

    #[test]
    fn multimodal_content_is_reduced_to_text() {
        let messages = vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "看图"},
                {"type": "image_url", "image_url": {"url": "http://example.test/x.png"}},
            ],
        })];
        let input = to_agent_input(&messages, None, Params::default());
        assert_eq!(input.question, "看图");
    }
}
