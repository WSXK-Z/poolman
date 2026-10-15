//! 转换器族：链路上两类「格式转换」各自独立，互不知道对方存在。
//!
//! - `api`：**API 转换器**——各 API 格式的请求 ⇄ 内部输入参数格式（`agent::input::AgentInput`）
//! - 提示词转换器（内部输入参数格式 → poolman 提示词，按接入平台区分）见 crate 根 [`crate::prompts`]
//!
//! 中间夹着的 `AgentInput` 是唯一契约：加一种 API 格式只动 `api`，
//! 加一个接入平台只动 `prompts`。

pub mod api;
