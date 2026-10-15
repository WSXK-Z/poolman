//! 内部输入参数格式：四类内容分开承载，是「API 转换器」与「提示词转换器」之间的唯一契约。
//!
//! 上游（各 API 格式）与下游（各接入平台）都各自千差万别，这里把它们压平成一件事：
//!
//! - `system_prompt`：系统原始提示词——上游客户端/平台给的原文（可能仍带 `<tag>` 段）
//! - `history`：历史对话——按时间顺序，**不含**最新提问
//! - `question`：用户最新提问
//! - `tool_results`：最新的工具结果——上一轮工具调用的回执
//!
//! 谁负责填它：`converters::api`（不同 API 格式各自的转换器）。
//! 谁负责读它：crate 根 `prompts`（不同接入平台各自的提示词转换器）。
//!
//! 这样 API 差异与平台差异就彻底解耦：新增一种 API 格式只动 api 转换器，
//! 新增一个接入平台只动提示词转换器，两边都不需要知道对方。

use serde_json::Value;

/// 一条对话消息（历史对话与工具结果的公共形状）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// 角色：user / assistant / tool
    pub role: String,
    pub content: String,
    /// 工具结果对应的调用 id（仅 role = tool 时有值），用于在提示词里标出是哪次调用的回执
    pub tool_call_id: Option<String>,
}

/// 请求参数（与具体 API 格式无关）
#[derive(Debug, Clone, Default)]
pub struct Params {
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_tokens: Option<u32>,
    pub stream: bool,
    /// 客户端指定的会话提示（如固定网页会话 id）
    pub session_hint: Option<String>,
}

/// 内部输入参数格式
#[derive(Debug, Clone, Default)]
pub struct AgentInput {
    /// 系统原始提示词（未拆分的原文）
    pub system_prompt: String,
    /// 历史对话（不含最新提问）
    pub history: Vec<Turn>,
    /// 用户最新提问
    pub question: String,
    /// 最新一轮工具调用的结果
    pub tool_results: Vec<Turn>,
    /// 本轮可用的工具定义（OpenAI 形状，None 表示不带工具）
    pub tools: Option<Value>,
    pub params: Params,
}

impl AgentInput {
    /// 是否首次对话：历史里没有 assistant 回合。首轮才注入系统段与工具段。
    pub fn is_first_turn(&self) -> bool {
        !self.history.iter().any(|turn| turn.role == "assistant")
    }

    /// 本轮是否携带工具
    pub fn has_tools(&self) -> bool {
        self.tools
            .as_ref()
            .and_then(|t| t.as_array())
            .map_or(false, |a| !a.is_empty())
    }

}

#[cfg(test)]
mod tests {
    use super::{AgentInput, Turn};

    fn turn(role: &str, content: &str) -> Turn {
        Turn {
            role: role.to_string(),
            content: content.to_string(),
            tool_call_id: None,
        }
    }

    #[test]
    fn first_turn_detection_is_based_on_assistant_history() {
        let mut input = AgentInput::default();
        assert!(input.is_first_turn(), "空历史应视为首轮");

        input.history = vec![turn("user", "上一问")];
        assert!(input.is_first_turn(), "只有 user 历史仍算首轮");

        input.history.push(turn("assistant", "上一答"));
        assert!(!input.is_first_turn(), "出现 assistant 后不再是首轮");
    }

    #[test]
    fn has_tools_requires_non_empty_array() {
        let mut input = AgentInput::default();
        assert!(!input.has_tools());
        input.tools = Some(serde_json::json!([]));
        assert!(!input.has_tools(), "空数组视为不带工具");
        input.tools = Some(serde_json::json!([{ "type": "function" }]));
        assert!(input.has_tools());
    }
}
