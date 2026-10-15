//! 提示词工程：poolman 提示词素材、按接入平台组装提示词的转换器、平台解析。
//!
//! - `system.md`：poolman 系统段正文（唯一来源，编译期 `include_str!` 嵌入）
//! - [`poolman`]：系统段加载与共享辅助（工具围栏规格、tag 处理、工具渲染）
//! - [`vscode_copilot`] / [`trae_work`]：各接入平台的三段转换实现
//!
//! 用法：用 [`resolve`] 按平台取转换器，再用 [`Converter::convert`] 把内部输入
//! （`agent::input::AgentInput`）组装成最终提示词。本模块只认「内部输入」与「poolman 提示词」，
//! 既不知道供应商，也不知道插件。

pub mod poolman;
pub mod trae_work;
pub mod vscode_copilot;

use serde_json::Value;

use crate::agent::input::AgentInput;

/// poolman 系统段已覆盖的规则段：接入平台带来的同名/同类段不再重复注入。
///
/// 提示词正文里已经写明工具调用格式与输出格式要求（`prompts/system.md`），
/// 因此接入平台的系统原始提示词中这些段落属于重复内容，注入前丢弃。
pub const POOLMAN_OWNED_TAGS: [&str; 4] = [
    "tool_instructions",
    "toolUseInstructions",
    "output_format",
    "outputFormat",
];

/// 提示词转换器：内部输入参数格式 → poolman 提示词。
///
/// 各平台只需实现三段转换；组装顺序与公共部分（poolman 系统段、格式提醒）在这里统一。
pub trait Converter: Send + Sync {
    /// 平台标识
    fn platform(&self) -> &'static str;

    /// 工具段：只声明可用工具（格式要求由 poolman 系统段统一给出）
    fn convert_tools(&self, tools: &Value) -> String;

    /// 系统段：把内部输入里的「系统原始提示词」加工成平台规则段
    fn convert_system(&self, input: &AgentInput) -> String;

    /// 用户段：历史对话 + 用户最新提问 + 最新工具结果 → 平台用户提示词段
    fn convert_user(&self, input: &AgentInput) -> String;

    /// 工具调用格式提醒：追问轮再次强调（避免模型忘记 <tool_call> 围栏规范）。默认空。
    fn tool_format_reminder(&self) -> String {
        String::new()
    }

    /// 组装完整 poolman 提示词：
    /// - 首轮：poolman 系统段（唯一来源 `prompts/system.md`）+ 平台系统段
    ///   + 工具列表 + 用户段
    /// - 追问：用户段（仍带工具时，末尾再强调一次格式）
    fn assemble(&self, input: &AgentInput) -> String {
        let mut parts: Vec<String> = Vec::new();
        if input.is_first_turn() {
            // poolman 系统段：人设 + 工作方式 + 工具调用格式 + 输出围栏。
            // 工具调用格式只在这里给出，工具段不再重复。
            let poolman = crate::prompts::poolman::system_prompt();
            if !poolman.is_empty() {
                parts.push(poolman.to_string());
            }
            let system = self.convert_system(input);
            if !system.is_empty() {
                parts.push(system);
            }
            if let Some(tools) = &input.tools {
                let tools_segment = self.convert_tools(tools);
                if !tools_segment.is_empty() {
                    parts.push(tools_segment);
                }
            }
        }
        let user = self.convert_user(input);
        if !user.is_empty() {
            parts.push(user);
        }
        // 追问轮且仍带工具：格式提醒放在【最末尾】（贴近生成位置，遵守率高于放前部）
        if !input.is_first_turn() && input.tools.is_some() {
            let reminder = self.tool_format_reminder();
            if !reminder.is_empty() {
                parts.push(reminder);
            }
        }
        parts.join("\n")
    }

    /// 转换入口（默认 = assemble；平台可覆写追加自己的处理，如截断）
    fn convert(&self, input: &AgentInput) -> String {
        self.assemble(input)
    }
}

/// 按接入平台名解析提示词转换器
pub fn resolve(platform: &str) -> Option<&'static dyn Converter> {
    match platform {
        vscode_copilot::PLATFORM => Some(&vscode_copilot::VscodeCopilotConverter),
        trae_work::PLATFORM => Some(&trae_work::TraeWorkConverter),
        _ => None,
    }
}
