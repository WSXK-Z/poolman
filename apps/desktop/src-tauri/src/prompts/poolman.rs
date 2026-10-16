//! poolman 提示词包装器
//!
//! 提示词正文放在同目录 `system.md`（用标题/列表分段，不用尖括号包段），
//! 编译期通过 `include_str!` 嵌入，本文件只保留加载常量与共享辅助函数。
//! 各平台转换器引用此处常量与函数。
//!
//! 尖括号只用于「约定输入/输出格式」的场合：工具调用围栏 `<tool_call>`，
//! 以及上下文里注入的记忆块名 `<userMemory>` / `<sessionMemory>` / `<repoMemory>`。

use serde_json::{json, Value};

// ============================== 提示词（src/prompts/system.md） ==============================

/// poolman 系统提示词全文（Markdown 分段，编译期嵌入 `system.md`）。
/// 外部一律通过 [`system_prompt`] 取用，不要直接引用本常量。
const SYSTEM_PROMPT: &str = include_str!("system.md").trim_ascii_end();

/// 首轮注入的 poolman 系统段（人设 + 工作方式 + 工具调用格式 + 输出围栏）。
///
/// 唯一来源是 `prompts/system.md`：转换器首轮把它拼进发给网页平台的提示词。
/// 不含「追问轮格式提醒」一节——那是追问轮单独注入的素材（见 [`tool_format_reminder`]），
/// 若混进首轮就会与工具段/追问轮的提醒重复。
pub(crate) fn system_prompt() -> &'static str {
    match SYSTEM_PROMPT.find("\n## 追问轮格式提醒") {
        Some(pos) => SYSTEM_PROMPT[..pos].trim_ascii_end(),
        None => SYSTEM_PROMPT,
    }
}

/// 取 Markdown 二级标题 `## <title>` 小节的正文（不含标题行，到下一个二级标题或文末为止）。
/// 标题不存在时返回空串。
fn md_section(doc: &'static str, title: &str) -> &'static str {
    let heading = format!("## {}", title);
    let Some(start) = doc.find(heading.as_str()) else {
        return "";
    };
    let rest = &doc[start + heading.len()..];
    let end = rest.find("\n## ").unwrap_or(rest.len());
    rest[..end].trim()
}

/// 系统提示词「工具调用格式」一节：首轮工具段附带的完整围栏格式要求
pub(crate) fn tool_format_instruction() -> &'static str {
    md_section(SYSTEM_PROMPT, "工具调用格式")
}

/// 系统提示词「追问轮格式提醒」一节：追问轮末尾的简短格式强调
pub(crate) fn tool_format_reminder() -> &'static str {
    md_section(SYSTEM_PROMPT, "追问轮格式提醒")
}

// ============================== 围栏规格（唯一来源） ==============================

/// 工具调用围栏的标签名——围栏的"唯一真相"。
///
/// 提示词正文在 `prompts/system.md`（编译期 `include_str!` 嵌入，不能插值常量），
/// 与常量只能各写一份字面量，靠单元测试锁死一致：
/// 本常量、`system.md` 里出现的围栏标签（见 `fence_spec_is_single_source`）。
/// 解析侧按这个名字识别围栏。
pub(crate) const TOOL_CALL_TAG: &str = "tool_call";

// ============================== 共享辅助 ==============================

/// 按 `<tag>内容</tag>` 拆分文本，返回有序段列表。
/// 每段为 (标签名, 内容)：Some 表示来自 `<tag>` 块，None 表示标签外的普通文本段。
/// 嵌套标签整体作为父标签段保留（不递归拆分）。
pub(crate) fn split_tagged_sections(content: &str) -> Vec<(Option<String>, String)> {
    let mut sections: Vec<(Option<String>, String)> = Vec::new();
    let mut plain = String::new();
    let mut pos = 0usize;
    while pos < content.len() {
        let Some(rel) = content[pos..].find('<') else {
            plain.push_str(&content[pos..]);
            break;
        };
        let abs = pos + rel;
        plain.push_str(&content[pos..abs]);
        let after_open = &content[abs + 1..];
        let Some(gt) = after_open.find('>') else {
            plain.push_str(&content[abs..]);
            break;
        };
        let tag = &after_open[..gt];
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
pub(crate) fn last_user_content(messages: &[Value]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("user"))
        .and_then(|m| m.get("content").and_then(|v| v.as_str()).map(String::from))
}

/// 工具列表渲染（含名称/描述/必填参数/JSON Schema）
pub(crate) fn render_tools(tools: &Value) -> String {
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
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
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

/// 追问用户段：阶段引导 + 工具结果 + 最后一条 user 的规则段/提问
pub(crate) fn build_follow_up_user(
    messages: &[Value],
    tools: Option<&Value>,
    question_tag: &str,
    tag_cn: impl Fn(&str) -> String,
) -> String {
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
    if let Some(c) = last_user_content(messages) {
        let sections = split_tagged_sections(&c);
        let mut question: Option<String> = None;
        if sections.is_empty() {
            question = Some(c);
        } else {
            for (tag, body) in sections {
                match tag.as_deref() {
                    Some(t) if t == question_tag => question = Some(body),
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

/// 标签名 → 中文
pub(crate) fn tag_cn(tag: &str) -> String {
    match tag {
        "identity" => "身份".to_string(),
        "instructions" => "操作指令".to_string(),
        "tool_instructions" => "工具使用指令".to_string(),
        "todo_tool" => "待办清单工具".to_string(),
        "task_tool" => "任务工具".to_string(),
        "capabilities" => "能力配置".to_string(),
        "file_handling" => "文件操作规则".to_string(),
        "skills" => "技能库".to_string(),
        "sub_agents" => "子代理列表".to_string(),
        "mcp" => "MCP 服务".to_string(),
        "memory" => "记忆".to_string(),
        "output_format" => "输出格式要求".to_string(),
        "citation" => "引用要求".to_string(),
        "scheduled_tasks" => "定时任务".to_string(),
        "environment" | "environment_info" => "环境信息".to_string(),
        "workspace" | "workspace_info" => "工作区信息".to_string(),
        "context" => "上下文信息".to_string(),
        "editor_state" | "editorContext" => "编辑器上下文".to_string(),
        "reminders" | "reminderInstructions" | "system-reminder" => "提醒指令".to_string(),
        "attachments" => "附件信息".to_string(),
        "user_request" | "userRequest" | "user_input" => "用户请求".to_string(),
        "userMemory" => "用户记忆".to_string(),
        "sessionMemory" => "会话记忆".to_string(),
        "repoMemory" => "仓库记忆".to_string(),
        _ => tag.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{md_section, tool_format_instruction, tool_format_reminder, SYSTEM_PROMPT, TOOL_CALL_TAG};

    /// 节提取：取正文不含标题行，到下一个二级标题为止；标题缺失返回空串
    #[test]
    fn md_section_extracts_body_without_heading() {
        let doc = "## A\n甲\n\n## B\n乙\n";
        assert_eq!(md_section(doc, "A"), "甲");
        assert_eq!(md_section(doc, "B"), "乙");
        assert_eq!(md_section(doc, "C"), "");
    }

    /// 工具调用方式的说明取自系统提示词，供工具段/提醒段直接使用
    #[test]
    fn tool_format_sections_are_usable() {
        let instruction = tool_format_instruction();
        for needle in [
            "<tool_call>",
            "\"name\": \"read_file\"",
            "缩进两个空格",
            "逐字符写成 tool_call",
            "输出前自检",
        ] {
            assert!(instruction.contains(needle), "工具调用格式节缺少: {}", needle);
        }
        let reminder = tool_format_reminder();
        for needle in ["<tool_call>", "</tool_call>", "\"name\": \"read_file\"", "\"filePath\":"] {
            assert!(reminder.contains(needle), "追问轮格式提醒节缺少: {}", needle);
        }
        assert!(
            reminder.ends_with("只能使用这一种围栏。"),
            "提醒节应以要点句结尾: {}",
            reminder
        );
    }

    /// 系统提示词规定：只有工具调用使用围栏，普通回答不用围栏
    #[test]
    fn system_prompt_requires_tool_call_fence() {
        let prompt = SYSTEM_PROMPT;
        assert!(prompt.contains("## 输出围栏"), "缺少「输出围栏」一节");
        for needle in ["<tool_call>", "</tool_call>", "普通回答不用围栏"] {
            assert!(prompt.contains(needle), "输出围栏要求缺少: {}", needle);
        }
        assert!(
            !prompt.contains("<context>"),
            "普通回答不应要求围栏"
        );
    }

    /// 围栏同源（提示词侧）：常量与 `system.md` 正文必须一致；
    /// 该标签名同时是提示词要求模型输出的围栏名，解析侧必须按它识别。
    #[test]
    fn fence_spec_is_single_source() {
        assert_eq!(TOOL_CALL_TAG, "tool_call", "围栏标签名被改动，需同步提示词与解析器");
        let prompt = SYSTEM_PROMPT;
        let open = format!("<{}>", TOOL_CALL_TAG);
        let close = format!("</{}>", TOOL_CALL_TAG);
        assert!(prompt.contains(&open), "提示词缺少开围栏 {}", open);
        assert!(prompt.contains(&close), "提示词缺少闭围栏 {}", close);
        // 提示词里不该再出现前缀写法或已被废弃的内容围栏、错写变体
        for bad in ["t||t", "<context>", "</context>", "<toolcall>"] {
            assert!(!prompt.contains(bad), "提示词仍含不该出现的围栏写法: {}", bad);
        }
        // 格式硬性要求集中在系统段（工具段不再重复，去重由 converter 负责）
        for needle in ["缩进两个空格", "逐字符写成 tool_call", "输出前自检"] {
            assert!(prompt.contains(needle), "系统段缺少格式要求: {}", needle);
        }
    }
}
