//! Trae Work(SOLO)平台转换器
//!
//! 把 OpenAI 兼容格式的 messages(来自 Trae Work 客户端)转换为 DeepSeek 提示词结构。
//! 按三部分抽象实现：工具转换（convert_tools）、系统提示词转换（convert_system）、
//! 用户提示词转换（convert_user，含上下文提醒/真实提问/工具结果）。
//!
//! 实测 Trae 消息结构（2026-09-01）:
//! - [0] system:SOLO 身份系统提示词,顶层嵌套标签:
//!   application_details / todo_write_tool / task_tool / citation_requirements /
//!   computer_use(内嵌 skills、file_handling_rules 等) / scheduled_tasks /
//!   mcp_file_system(内嵌 mcp_file_system_servers、env)
//! - [1+] user:<system-reminder> 上下文提醒块(可多个) + <user_input> 真实提问

use serde_json::{json, Value};

use crate::agent::input::AgentInput;
use crate::prompts::Converter;

pub const PLATFORM: &str = "trae_work";

/// 按 <tag>内容</tag> 拆分文本，去掉 <xxx> 标签，返回有序段列表。
/// 每段为 (标签名, 内容)：Some 表示来自 <tag> 块，None 表示标签外的普通文本段。
/// 嵌套标签（如 <computer_use> 内嵌 <skills>）整体作为父标签段保留（不递归拆分）。
fn split_tagged_sections(content: &str) -> Vec<(Option<String>, String)> {
    let mut sections: Vec<(Option<String>, String)> = Vec::new();
    let mut plain = String::new();
    let mut pos = 0usize;
    while pos < content.len() {
        let Some(rel) = content[pos..].find('<') else {
            plain.push_str(&content[pos..]);
            break;
        };
        let abs = pos + rel;
        // 标签前的普通文本
        plain.push_str(&content[pos..abs]);
        let after_open = &content[abs + 1..];
        let Some(gt) = after_open.find('>') else {
            plain.push_str(&content[abs..]);
            break;
        };
        let tag = &after_open[..gt];
        // 跳过非 <name> 形式的标签（闭合 / 指令 ! ? 带属性 等）
        if tag.is_empty()
            || tag.starts_with('/')
            || tag.starts_with('!')
            || tag.starts_with('?')
            || !tag
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        {
            plain.push_str(&content[abs..abs + 1 + gt + 1]);
            pos = abs + 1 + gt + 1;
            continue;
        }
        let body_start = abs + 1 + gt + 1;
        let close_tag = format!("</{}>", tag);
        let Some(close_rel) = content[body_start..].find(&close_tag) else {
            plain.push_str(&content[abs..]);
            break;
        };
        let body = content[body_start..body_start + close_rel].trim();
        // 提交已累积的普通文本段
        if !plain.trim().is_empty() {
            sections.push((None, plain.trim().to_string()));
            plain.clear();
        }
        if !body.is_empty() {
            sections.push((Some(tag.to_string()), body.to_string()));
        }
        pos = body_start + close_rel + close_tag.len();
    }
    if !plain.trim().is_empty() {
        sections.push((None, plain.trim().to_string()));
    }
    sections
}

/// 将 Trae(SOLO)实际上下文中的标签名转换为中文（未知标签保留原名）
fn tag_cn(tag: &str) -> String {
    match tag {
        // system 身份提示词内的标签
        "application_details" => "应用详情".to_string(),
        "todo_write_tool" => "待办清单工具".to_string(),
        "task_tool" => "任务工具".to_string(),
        "citation_requirements" => "引用要求".to_string(),
        "computer_use" => "计算机使用".to_string(),
        "scheduled_tasks" => "定时任务".to_string(),
        "mcp_file_system" => "MCP 文件系统".to_string(),
        "mcp_file_system_servers" => "MCP 服务列表".to_string(),
        "env" => "运行环境".to_string(),
        // user 上下文内的标签
        "system-reminder" => "系统提醒".to_string(),
        "user_input" => "用户输入".to_string(),
        _ => tag.to_string(),
    }
}

/// 工具列表渲染（含名称/描述/必填参数/JSON Schema）
fn render_tools(tools: &Value) -> String {
    let mut lines = Vec::new();
    if let Some(arr) = tools.as_array() {
        for (i, t) in arr.iter().enumerate() {
            let name = t
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let desc = t
                .get("function")
                .and_then(|f| f.get("description"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let params = t
                .get("function")
                .and_then(|f| f.get("parameters"))
                .cloned()
                .unwrap_or(json!({}));
            let required: Vec<String> = params
                .get("required")
                .and_then(|r| r.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let req_str = if required.is_empty() {
                "（全部可选）".to_string()
            } else {
                required.join(", ")
            };
            lines.push(format!(
                "{}. 名称: {}\n   描述: {}\n   必填参数: {}\n   参数(JSON Schema): {}",
                i + 1,
                name,
                desc,
                req_str,
                serde_json::to_string(&params).unwrap_or_else(|_| "{}".into())
            ));
        }
    }
    lines.join("\n")
}

/// 追问用户段：阶段引导 + 工具结果 + 最新提问的规则段(<system-reminder>)/提问(<user_input>)
fn build_follow_up_user(input: &AgentInput) -> String {
    let mut parts: Vec<String> = Vec::new();

    // 工具结果：内部输入里已按原顺序取出（带 tool_call_id）
    if !input.tool_results.is_empty() {
        let rendered: Vec<String> = input
            .tool_results
            .iter()
            .map(|turn| {
                let label = match &turn.tool_call_id {
                    Some(id) => format!("工具({})返回的结果：", id),
                    None => "工具返回的结果：".to_string(),
                };
                format!("{}\n{}", label, turn.content)
            })
            .collect();
        parts.push(rendered.join("\n\n"));
    }

    // 最新提问：按标签拆分，<user_input> → 提问，其余(<system-reminder> 等) → 规则段
    let mut question: Option<String> = None;
    let sections = split_tagged_sections(&input.question);
    if sections.is_empty() {
        if !input.question.is_empty() {
            question = Some(input.question.clone());
        }
    } else {
        for (tag, body) in sections {
            match tag.as_deref() {
                Some("user_input") => question = Some(body),
                Some(t) => parts.push(format!("以下是 {} 内容：\n{}", tag_cn(t), body)),
                None => parts.push(format!("以下是用户的环境信息：\n{}", body)),
            }
        }
    }
    if let Some(q) = question {
        parts.push(format!("用户：\n{}", q));
    }

    // 阶段引导插到最前
    if input.has_tools() {
        if input.tool_results.is_empty() {
            parts.insert(
                0,
                "根据用户问题，结合已有工具，判断是否要使用工具来完成任务，并且你应该优先使用工具完成任务，只有纯问答才直接回答。"
                    .to_string(),
            );
        } else {
            parts.insert(
                0,
                "结合工具结果，以及历史对话，判断任务是否已经可以完成：如果还需要调用工具才能完成就继续调用工具；只有任务已全部完成才给出最终答案；不要编造工具执行结果。"
                    .to_string(),
            );
        }
    }
    parts.join("\n\n")
}

pub struct TraeWorkConverter;

impl Converter for TraeWorkConverter {
    fn platform(&self) -> &'static str {
        PLATFORM
    }

    /// 工具转换：工具定义 → 平台工具「记住」段（与 vscode_copilot 一致的 YAML 工具调用格式）
    fn convert_tools(&self, tools: &Value) -> String {
        // 工具调用格式要求由 poolman 系统段统一给出（见 Converter::assemble），
        // 这里只声明可用工具，避免同一段规则在提示词里重复出现。
        format!("以下是你可以调用的工具：\n{}", render_tools(tools))
    }

    /// 追问轮次：再次强调工具调用格式（放在提示词末尾，用具体示例而非占位符）
    fn tool_format_reminder(&self) -> String {
        crate::prompts::poolman::tool_format_reminder().to_string()
    }

    /// 系统提示词转换：SOLO 身份 system 按顶层 <xxx> 标签拆分，每块以中文标签名独立包装成规则段
    fn convert_system(&self, input: &AgentInput) -> String {
        let mut parts: Vec<String> = Vec::new();
        if input.system_prompt.is_empty() {
            return String::new();
        }
        for (tag, body) in split_tagged_sections(&input.system_prompt) {
            // 与 poolman 系统段重复的规则段丢弃：工具/输出格式统一由 poolman 段给出
            if tag
                .as_deref()
                .map_or(false, |t| crate::prompts::POOLMAN_OWNED_TAGS.contains(&t))
            {
                continue;
            }
            match tag {
                Some(t) => parts.push(format!(
                    "以下是 {} 内容，不管用户问题是什么，你都得结合{}回答：\n{}",
                    tag_cn(&t),
                    tag_cn(&t),
                    body
                )),
                None => parts.insert(0, format!("以下是你必须牢记的身份：\n{}", body)),
            }
        }
        parts.join("\n\n")
    }

    /// 用户提示词转换：首轮 = 上下文提醒(<system-reminder>)规则段 + <user_input> 真实提问；
    /// 追问 = 阶段引导 + 工具结果 + 提问
    fn convert_user(&self, input: &AgentInput) -> String {
        if input.is_first_turn() {
            let mut parts: Vec<String> = Vec::new();
            let mut question: Option<String> = None;
            // 历史对话里的 user 段 + 用户最新提问，按 <xxx> 标签拆分：
            // - <user_input> 块 → 真实提问（单独段，保留最后一条）
            // - <system-reminder> 等标签块 → 独立规则段
            let contents = input
                .history
                .iter()
                .filter(|turn| turn.role == "user")
                .map(|turn| turn.content.as_str())
                .chain(std::iter::once(input.question.as_str()));
            for content in contents {
                if content.is_empty() {
                    continue;
                }
                let sections = split_tagged_sections(content);
                if sections.is_empty() {
                    // 无标签的整条 user：作为环境规则段
                    parts.push(format!("以下是用户的环境信息：\n{}", content));
                    continue;
                }
                for (tag, body) in sections {
                    match tag.as_deref() {
                        Some("user_input") => {
                            if !body.is_empty() {
                                question = Some(body);
                            }
                        }
                        Some(t) => parts.push(format!("以下是 {} 内容：\n{}", tag_cn(t), body)),
                        None => parts.push(format!("以下是用户的环境信息：\n{}", body)),
                    }
                }
            }
            if let Some(q) = question {
                parts.push(format!("用户的问题：\n{}", q));
            }
            parts.join("\n\n")
        } else {
            build_follow_up_user(input)
        }
    }

    // 不覆写 convert：完整内容透传，不截断
}

#[cfg(test)]
mod tests {
    use super::{split_tagged_sections, TraeWorkConverter};
    use crate::prompts::Converter;
    use serde_json::json;

    /// 工具段只列可用工具：格式硬性要求由 poolman 系统段统一给出，不在工具段重复。
    #[test]
    fn tool_prompt_lists_tools_without_repeating_format() {
        let tools = json!([{ "type": "function", "function": { "name": "read_file", "parameters": json!({ "type": "object" }) } }]);
        let text = TraeWorkConverter.convert_tools(&tools);
        assert!(text.contains("以下是你可以调用的工具"), "工具段应声明可用工具: {}", text);
        assert!(text.contains("read_file"), "工具段应列出工具名: {}", text);
        assert!(!text.contains("<tool_call>"), "工具段不应重复围栏示例: {}", text);
        assert!(!text.contains("输出前自检"), "工具段不应重复格式要求: {}", text);
    }

    /// 追问轮提醒：仍要用具体示例强调围栏格式，且不得出现错误写法/占位符字面量
    /// （在提示词里演示错误写法会把错误格式“教”给模型）
    #[test]
    fn follow_up_reminder_states_fence_rules() {
        let reminder = TraeWorkConverter.tool_format_reminder();
        assert!(reminder.contains("缩进"), "提醒段应强调参数缩进: {}", reminder);
        assert!(reminder.contains("逐字符写成 tool_call"), "提醒段应强调围栏名: {}", reminder);
        assert!(reminder.contains("\"name\": \"read_file\""), "提醒段应给出具体示例: {}", reminder);
        assert!(reminder.contains("\"filePath\":"), "提醒段示例应含参数行: {}", reminder);
        for bad in ["<toolcall>", "call_call", "<工具名>", "<参数名>"] {
            assert!(!reminder.contains(bad), "提醒段不应出现错误写法/占位符: {}", bad);
        }
    }

    #[test]
    fn split_real_trae_system_tags() {
        // 真实 Trae system 顶层标签结构（截取代表性内容）
        let s = "<application_details>\nYou are SOLO, a powerful assistant.\n</application_details>\n<todo_write_tool>\nTodoWrite tracks steps.\n</todo_write_tool>\n<task_tool>\nTask tool spawns subagents.\n</task_tool>\n<computer_use>\n<skills>\nskill instructions\n</skills>\n<file_creation_advice>\ncreate files\n</file_creation_advice>\n</computer_use>\n<scheduled_tasks>\nSchedule on cron.\n</scheduled_tasks>\n<mcp_file_system>\n<mcp_file_system_servers>\nserver list\n</mcp_file_system_servers>\n<env>\nWindows\n</env>\n</mcp_file_system>";
        let out = split_tagged_sections(s);
        assert_eq!(out.len(), 6);
        assert_eq!(out[0].0.as_deref(), Some("application_details"));
        assert!(out[0].1.contains("SOLO"));
        assert_eq!(out[1].0.as_deref(), Some("todo_write_tool"));
        assert_eq!(out[2].0.as_deref(), Some("task_tool"));
        // 嵌套标签整体保留在父段内
        assert_eq!(out[3].0.as_deref(), Some("computer_use"));
        assert!(out[3].1.contains("<skills>"));
        assert!(out[3].1.contains("<file_creation_advice>"));
        assert_eq!(out[4].0.as_deref(), Some("scheduled_tasks"));
        assert_eq!(out[5].0.as_deref(), Some("mcp_file_system"));
        assert!(out[5].1.contains("<env>"));
    }

    #[test]
    fn convert_system_real_trae() {
        let messages = vec![json!({
            "role": "system",
            "content": "<application_details>\nYou are SOLO.\n</application_details>\n<todo_write_tool>\nUse TodoWrite for steps.\n</todo_write_tool>"
        })];
        let input =
            crate::converters::api::openai::to_agent_input(&messages, None, Default::default());
        let c = TraeWorkConverter.convert_system(&input);
        assert!(c.contains("以下是 应用详情 内容，不管用户问题是什么，你都得结合应用详情回答：\nYou are SOLO."));
        assert!(c.contains("以下是 待办清单工具 内容，不管用户问题是什么，你都得结合待办清单工具回答：\nUse TodoWrite for steps."));
    }

    #[test]
    fn convert_user_real_trae_first_turn() {
        // 真实 Trae user 结构：多个 <system-reminder> + <user_input>
        let messages = vec![
            json!({"role": "system", "content": "<application_details>身份</application_details>"}),
            json!({"role": "user", "content": "<system-reminder>\nIMPORTANT: context may be relevant.\n</system-reminder>\n\n<system-reminder>\n# Response Language Settings\nAlways use same language.\n</system-reminder>\n\n<user_input>\n你好\n</user_input>\n"}),
        ];
        let input =
            crate::converters::api::openai::to_agent_input(&messages, None, Default::default());
        let c = TraeWorkConverter.convert_user(&input);
        // 两个 system-reminder 都作为规则段，user_input 作为提问
        assert!(c.contains("以下是 系统提醒 内容：\nIMPORTANT: context may be relevant."));
        assert!(c.contains("以下是 系统提醒 内容：\n# Response Language Settings"));
        assert!(c.contains("用户的问题：\n你好"));
    }

    #[test]
    fn convert_user_real_trae_follow_up() {
        // 追问(工具结果轮)：消息末尾是连续 tool 结果，前面是原始提问（user_input）
        let messages = vec![
            json!({"role": "system", "content": "<application_details>身份</application_details>"}),
            json!({"role": "user", "content": "<user_input>请读取文件</user_input>"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "read_file", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "文件内容 ABC"}),
        ];
        let input =
            crate::converters::api::openai::to_agent_input(&messages, None, Default::default());
        let c = TraeWorkConverter.convert_user(&input);
        // 末尾 tool 结果被收集，原始提问从 user_input 提取
        assert!(c.contains("工具(call_1)返回的结果：\n文件内容 ABC"));
        assert!(c.contains("用户：\n请读取文件"));
    }
}
