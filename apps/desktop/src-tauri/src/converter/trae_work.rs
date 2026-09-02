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

use crate::converter::{ConvertContext, Converter};

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

/// 取最后一条 user 消息内容
fn last_user_content(messages: &[Value]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("user"))
        .and_then(|m| m.get("content").and_then(|v| v.as_str()).map(String::from))
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

/// 追问用户段：阶段引导 + 工具结果 + 最后一条 user 的规则段(<system-reminder>)/提问(<user_input>)
fn build_follow_up_user(messages: &[Value], tools: Option<&Value>) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut tool_results: Vec<String> = Vec::new();
    for m in messages.iter().rev() {
        if m.get("role").and_then(|v| v.as_str()) == Some("tool") {
            if let Some(c) = m.get("content").and_then(|v| v.as_str()) {
                let label = m
                    .get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .map(|id| format!("工具({})返回的结果：", id))
                    .unwrap_or_else(|| "工具返回的结果：".to_string());
                tool_results.push(format!("{}\n{}", label, c));
            }
        } else {
            break;
        }
    }
    if !tool_results.is_empty() {
        tool_results.reverse();
        parts.push(tool_results.join("\n\n"));
    }
    // 最后一条 user：按标签拆分，<user_input> → 提问，其余(<system-reminder> 等) → 规则段
    if let Some(c) = last_user_content(messages) {
        let sections = split_tagged_sections(&c);
        let mut question: Option<String> = None;
        if sections.is_empty() {
            question = Some(c);
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
    }
    if let Some(tools) = tools {
        if tools.as_array().map_or(false, |a| !a.is_empty()) {
            let has_tool_results = !tool_results.is_empty();
            if has_tool_results {
                parts.insert(
                    0,
                    "结合工具结果，以及历史对话，判断任务是否已经可以完成：如果还需要调用工具才能完成就继续调用工具；只有任务已全部完成才给出最终答案；不要编造工具执行结果。"
                        .to_string(),
                );
            } else {
                parts.insert(
                    0,
                    "根据用户问题，结合已有工具，判断是否要使用工具来完成任务，并且你应该优先使用工具完成任务，只有纯问答才直接回答。"
                        .to_string(),
                );
            }
        }
    }
    parts.join("\n\n")
}

pub struct TraeWorkConverter;

impl Converter for TraeWorkConverter {
    /// 工具转换：工具定义 → 平台工具「记住」段（与 vscode_copilot 一致的 YAML 工具调用格式）
    fn convert_tools(&self, tools: &Value) -> String {
        let instruction = r#"
        1. 你可以在一次回答中调用多个工具，但必须一个工具对应一个完整的 tool_call 块。
        2. 调用工具时，必须把每个工具输出成一个独立的 <tool_call> ... </tool_call> 围栏 YAML 块，且这是【唯一合法格式】，格式如下：
            <tool_call>
            name: <工具名>
            arguments:
              <参数名>: <值>
            </tool_call>
        3. 格式硬性要求，违反任何一条都会导致你的工具调用无法被解析、任务失败：
           - 每个块必须以开围栏 <tool_call> 开始、以闭围栏 </tool_call> 结束，开闭标签必须成对；不能漏下划线、不能改写成 <toolcall> 或其他任何变体；
           - 块内第一行必须是 name: <工具名>，第二行必须是 arguments:，所有参数必须放在 arguments 键下面；禁止把工具名裸写在首行、禁止省略 name 或 arguments 键；
           - 每个工具块独占一个完整的开/闭围栏；多个工具块之间用换行分隔；
           - 多行参数（代码内容等）用块标量 |- 原样直写，不要转义引号/反斜杠/换行；参数里出现三反引号 ```（如 markdown 代码块）不影响围栏，正常书写即可；
           - 路径用正斜杠或原样反斜杠均可；只能调用已定义工具；编辑类工具先读取文件最新内容再改。
        4. 普通文本与工具调用不可同时出现：一旦决定调用工具，就【只】输出工具调用块（可多个），不要夹带任何普通文本、解释、前言或总结；反之，若你在输出普通回答，就不要再输出工具调用块。"#;
        format!(
            "以下是你可以调用的工具：\n{}\n{}",
            render_tools(tools),
            instruction
        )
    }

    /// 追问轮次：再次强调工具调用格式（简短提醒，不重复完整工具列表）
    fn tool_format_reminder(&self) -> String {
        r#"再次提醒：只要需要调用工具，就必须严格按下面的格式输出（唯一合法格式）：
            <tool_call>
            name: <工具名>
            arguments:
              <参数名>: <值>
            </tool_call>
        要求：开围栏 <tool_call> 与闭围栏 </tool_call> 必须成对，不能漏下划线、不能改写成 <toolcall> 或其他变体；第一行必须 name:、第二行必须 arguments:，参数放在 arguments 键下；不要省略 name/arguments 键、不要把工具名裸写在首行；每个工具块独占完整开/闭围栏，多个工具块用换行分隔；多行参数用 |- 块标量原样直写。调用工具时只输出 <tool_call> 块，不要夹带任何普通文本；普通文本与工具调用不可同时出现。"#
            .to_string()
    }

    /// 系统提示词转换：SOLO 身份 system 按顶层 <xxx> 标签拆分，每块以中文标签名独立包装成规则段
    fn convert_system(&self, messages: &[Value]) -> String {
        let mut parts: Vec<String> = Vec::new();
        for m in messages {
            if m.get("role").and_then(|v| v.as_str()) == Some("system") {
                if let Some(c) = m.get("content").and_then(|v| v.as_str()) {
                    for (tag, body) in split_tagged_sections(c) {
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
                }
            }
        }
        parts.join("\n\n")
    }

    /// 用户提示词转换：首轮 = 上下文提醒(<system-reminder>)规则段 + <user_input> 真实提问；
    /// 追问 = 阶段引导 + 工具结果 + 提问
    fn convert_user(&self, ctx: &ConvertContext) -> String {
        if ctx.is_first_turn {
            let mut parts: Vec<String> = Vec::new();
            let mut question: Option<String> = None;
            // 遍历所有 user 消息，按 <xxx> 标签拆分：
            // - <user_input> 块 → 真实提问（单独段，保留最后一条）
            // - <system-reminder> 等标签块 → 独立规则段
            for m in ctx.messages {
                if m.get("role").and_then(|v| v.as_str()) != Some("user") {
                    continue;
                }
                let Some(c) = m.get("content").and_then(|v| v.as_str()) else {
                    continue;
                };
                let sections = split_tagged_sections(c);
                if sections.is_empty() {
                    // 无标签的整条 user：作为环境规则段
                    parts.push(format!("以下是用户的环境信息：\n{}", c));
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
            build_follow_up_user(ctx.messages, ctx.tools)
        }
    }

    // 不覆写 convert：完整内容透传，不截断
}

#[cfg(test)]
mod tests {
    use super::{split_tagged_sections, TraeWorkConverter};
    use crate::converter::{ConvertContext, Converter};
    use serde_json::json;

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
        let c = TraeWorkConverter.convert_system(&messages);
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
        let ctx = ConvertContext {
            messages: &messages,
            tools: None,
            is_first_turn: true,
        };
        let c = TraeWorkConverter.convert_user(&ctx);
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
        let ctx = ConvertContext {
            messages: &messages,
            tools: None,
            is_first_turn: false,
        };
        let c = TraeWorkConverter.convert_user(&ctx);
        // 末尾 tool 结果被收集，原始提问从 user_input 提取
        assert!(c.contains("工具(call_1)返回的结果：\n文件内容 ABC"));
        assert!(c.contains("用户：\n请读取文件"));
    }
}
