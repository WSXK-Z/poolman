//! 中间转换器：OpenAI 兼容格式 → 各平台提示词结构
//!
//! 这是独立的中间转换层，不属于插件端（不负责与浏览器扩展交互），
//! 只负责把 OpenAI 格式 API 中的提示词数据按平台规则转换为
//! 各平台（提示词结构不同）可理解的形式，目前仅有 VS Code Copilot。
//! 由 API Key 绑定的平台决定使用哪个转换器。

pub mod trae_work;
pub mod vscode_copilot;

use serde_json::Value;

/// 转换上下文：OpenAI 格式请求数据
pub struct ConvertContext<'a> {
    pub messages: &'a [Value],
    pub tools: Option<&'a Value>,
    /// 是否首次提问（决定是否拼接系统/工具段）
    pub is_first_turn: bool,
}

/// 转换器抽象：工具转换 / 系统提示词转换 / 用户提示词转换 三部分
pub trait Converter: Send + Sync {
    /// 工具转换：把 OpenAI 工具定义转换为平台工具说明段
    fn convert_tools(&self, tools: &Value) -> String;

    /// 工具调用格式提醒：追问轮次再次强调格式（避免模型忘记 <tool_call> 围栏规范）。默认空。
    fn tool_format_reminder(&self) -> String {
        String::new()
    }

    /// 系统提示词转换：把 OpenAI system 消息转换为平台系统提示词段
    fn convert_system(&self, messages: &[Value]) -> String;

    /// 用户提示词转换：把 OpenAI 用户消息（环境/提问/工具结果）转换为平台用户提示词段
    fn convert_user(&self, ctx: &ConvertContext) -> String;

    /// 组装完整提示词：首轮 = 系统 + 工具 + 用户；追问 = 用户（带工具时再次强调格式）。
    /// 默认实现可被覆写 convert 复用（避免覆写时无法调用默认 convert 导致递归）。
    fn assemble(&self, ctx: &ConvertContext) -> String {
        let mut parts: Vec<String> = Vec::new();
        if ctx.is_first_turn {
            let sys = self.convert_system(ctx.messages);
            if !sys.is_empty() {
                parts.push(sys);
            }
            if let Some(tools) = ctx.tools {
                let t = self.convert_tools(tools);
                if !t.is_empty() {
                    parts.push(t);
                }
            }
        }
        let user = self.convert_user(ctx);
        if !user.is_empty() {
            parts.push(user);
        }
        // 追问轮且仍带工具：格式提醒放在【最末尾】（贴近生成位置，遵守率高于放前部）
        if !ctx.is_first_turn && ctx.tools.is_some() {
            let r = self.tool_format_reminder();
            if !r.is_empty() {
                parts.push(r);
            }
        }
        parts.join("\n")
    }

    /// 转换入口（默认 = assemble；平台可覆写追加自己的处理，如截断）
    fn convert(&self, ctx: &ConvertContext) -> String {
        self.assemble(ctx)
    }
}

/// 按平台名解析转换器
pub fn resolve(platform: &str) -> Option<&'static dyn Converter> {
    match platform {
        vscode_copilot::PLATFORM => Some(&vscode_copilot::VscodeCopilotConverter),
        trae_work::PLATFORM => Some(&trae_work::TraeWorkConverter),
        _ => None,
    }
}


