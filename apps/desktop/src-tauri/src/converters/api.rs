//! API 转换器：各格式的 API 参数 → 内部输入参数格式。
//!
//! 「API 格式」是可扩展维度（OpenAI 只是其一）。新增格式时在此新建子模块，
//! 实现「请求 → `AgentInput`」与「`AgentAnswer` → 该格式响应」两半。
//! 反方向（`AgentInput` → poolman 提示词）见 crate 根 [`crate::prompts`]。

pub mod openai;
