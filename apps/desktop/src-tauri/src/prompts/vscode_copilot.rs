//! VS Code Copilot 平台转换器
//!
//! 把 OpenAI 兼容格式的 messages（含工具定义）转换为 Copilot 提示词结构。
//! 按三部分抽象实现：工具转换（convert_tools）、系统提示词转换（convert_system）、
//! 用户提示词转换（convert_user，含环境/提问/工具结果）。

use serde_json::{json, Value};

use crate::agent::input::AgentInput;
use crate::prompts::Converter;

pub const PLATFORM: &str = "vscode-copilot";

/// 按 <tag>内容</tag> 拆分文本，去掉 <xxx> 标签，返回有序段列表。
/// 每段为 (标签名, 内容)：标签名为 Some 表示来自 <tag> 块，None 表示标签外的普通文本段。
/// 所有拆分段独立保留（不被合并/丢弃），供上层作为独立规则段发送。
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

/// 将真实 Copilot 上下文中的嵌套标签名转换为中文（未知标签保留原名）
fn tag_cn(tag: &str) -> String {
    match tag {
        // system 提示词内的标签
        "instructions" => "操作指令".to_string(),
        "toolUseInstructions" => "工具使用指令".to_string(),
        "editFileInstructions" => "文件编辑指令".to_string(),
        "notebookInstructions" => "Notebook 指令".to_string(),
        "outputFormatting" => "输出格式要求".to_string(),
        "memoryInstructions" => "记忆使用说明".to_string(),
        "memoryScopes" => "记忆范围说明".to_string(),
        "memoryGuidelines" => "记忆维护指南".to_string(),
        "skills" => "技能库".to_string(),
        "agents" => "子代理列表".to_string(),
        // user 上下文内的标签
        "context" => "上下文信息".to_string(),
        "editorContext" => "编辑器上下文".to_string(),
        "environment_info" => "环境信息".to_string(),
        "workspace_info" => "工作区信息".to_string(),
        "userMemory" => "用户记忆".to_string(),
        "sessionMemory" => "会话记忆".to_string(),
        "repoMemory" => "仓库记忆".to_string(),
        "reminderInstructions" => "提醒指令".to_string(),
        "attachments" => "附件信息".to_string(),
        "userRequest" => "用户请求".to_string(),
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

/// 追问用户段：阶段引导 + 工具结果 + 提问（按标签拆分后的规则段/提问）
fn build_follow_up_user(input: &AgentInput) -> String {
    let mut parts: Vec<String> = Vec::new();
    // 工具结果：上一轮工具调用的回执（调用方已按顺序摆好）
    if !input.tool_results.is_empty() {
        let results: Vec<String> = input
            .tool_results
            .iter()
            .map(|turn| {
                let label = turn
                    .tool_call_id
                    .as_deref()
                    .map(|id| format!("工具({})返回的结果：", id))
                    .unwrap_or_else(|| "工具返回的结果：".to_string());
                format!("{}\n{}", label, turn.content)
            })
            .collect();
        parts.push(results.join("\n\n"));
    }
    // 最新提问：按标签拆分，非 userRequest 块 → 规则段，userRequest → 提问
    if !input.question.is_empty() {
        let sections = split_tagged_sections(&input.question);
        let mut question: Option<String> = None;
        if sections.is_empty() {
            question = Some(input.question.clone());
        } else {
            for (tag, body) in sections {
                match tag.as_deref() {
                    Some("userRequest") => question = Some(body),
                    Some(t) => parts.push(format!("以下是 {} 内容：\n{}", tag_cn(t), body)),
                    None => parts.push(format!("以下是用户的环境信息：\n{}", body)),
                }
            }
        }
        if let Some(q) = question {
            parts.push(format!("用户：\n{}", q));
        }
    }
    if input.has_tools() {
        let has_tool_results = !input.tool_results.is_empty();
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
    parts.join("\n\n")
}

pub struct VscodeCopilotConverter;

impl Converter for VscodeCopilotConverter {
    fn platform(&self) -> &'static str {
        PLATFORM
    }

    /// 工具转换：工具定义 → 平台工具「记住」段
    fn convert_tools(&self, tools: &Value) -> String {
        let instruction = r#"
        1. 你可以在一次回答中调用多个工具，但必须一个工具对应一个完整的 tool_call 块。
        2. 调用工具时，必须把每个工具输出成一个独立的 <tool_call> ... </tool_call> 围栏 YAML 块，且这是【唯一合法格式】，示例如下：
            <tool_call>
            name: read_file
            arguments:
              filePath: d:/project/src/main.rs
              startLine: 1
              endLine: 20
            </tool_call>
        3. 格式硬性要求，违反任何一条都会导致你的工具调用无法被解析、任务失败：
           - 开标签与闭标签必须同名成对、逐字符写成 tool_call；围栏名不能改成其它任何写法，也不能漏写下划线；
           - 块内第一行必须是 name: 加工具名，第二行必须是 arguments:，所有参数都放在 arguments 下面并缩进两个空格；不要把工具名裸写在首行，不要省略 name/arguments 键，不要把参数写成与 arguments: 同一缩进；
           - 每个工具块独占一个完整的开/闭围栏；多个工具块之间用换行分隔；
           - 多行参数（代码内容等）用块标量 |- 原样直写，不要转义引号/反斜杠/换行；参数里出现三反引号 ```（如 markdown 代码块）不影响围栏，正常书写即可；
           - 路径用正斜杠或原样反斜杠均可；只能调用已定义工具；编辑类工具先读取文件最新内容再改；
           - 除上面这一种围栏外，不要使用任何其它形式的工具调用标记。
           - 除上面这一种围栏外，不要使用任何其它形式的工具调用标记：禁止出现 <parameter>、<invoke>、<calls>、|DSML|、||DSML||、｜｜DSML｜｜ 及其任何闭合形式，块内不得混入任何 XML 或 DSML 标签。
           - 开闭标签数量必须严格一一对应：每个 <tool_call> 恰好配一个 </tool_call>，全文 </tool_call> 的数量等于 <tool_call> 的数量；输出结束后不得再补写任何多余的 </tool_call>。
        4. 普通文本与工具调用不可同时出现：一旦决定调用工具，就【只】输出工具调用块（可多个），不要夹带任何普通文本、解释、前言或总结；反之，若你在输出普通回答，就不要再输出工具调用块。
        5. 输出前自检，任一条不符就重写：① 开/闭标签是否都是 tool_call？② 参数是否都缩进在 arguments: 下面？③ 是否夹带了普通文本？④ 工具名与参数名是否与工具定义完全一致、必填参数是否补齐？⑤ 数一遍 <tool_call> 与 </tool_call> 是否数量相等、末尾是否多写了闭合标签？"#;
        format!(
            "\n{}\n以下是你可以调用的工具：\n{}",
            //render_tools(tools),
            instruction,
            tools,
        )
    }

    /// 追问轮次：再次强调工具调用格式（放在提示词末尾，用具体示例而非占位符）
    fn tool_format_reminder(&self) -> String {
        r#"【工具调用格式】只要需要调用工具，就只输出下面这种块（可以有多个），不要夹带任何其它文字：
                <tool_call>
                name: read_file
                arguments:
                  filePath: d:/project/src/main.rs
                  startLine: 1
                  endLine: 20
                </tool_call>
            要点：开标签和闭标签都必须逐字符写成 tool_call；每个参数都要缩进在 arguments: 下面（两个空格）；禁止出现 <parameter>、<invoke>、<calls>、|DSML|、||DSML||、｜｜DSML｜｜ 及其任何闭合形式；开闭标签数量必须一一对应，禁止在末尾补写多余的 </tool_call>；只能使用这一种围栏。"#
            .to_string()
    }

    /// 系统提示词转换：system 原始提示词按 <xxx> 标签拆分，每块去掉标签后以中文标签名独立包装成规则段
    fn convert_system(&self, input: &AgentInput) -> String {
        let mut parts: Vec<String> = Vec::new();
        if input.system_prompt.is_empty() {
            return String::new();
        }
        for (tag, body) in split_tagged_sections(&input.system_prompt) {
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

    /// 用户提示词转换：首轮 = 环境/上下文各标签块(规则段) + 真实提问；追问 = 阶段引导 + 工具结果 + 提问
    fn convert_user(&self, input: &AgentInput) -> String {
        if input.is_first_turn() {
            let mut parts: Vec<String> = Vec::new();
            let mut question: Option<String> = None;
            // 历史里的 user 段 + 用户最新提问，按 <xxx> 标签拆分（真实 Copilot 单条 user 内平铺多标签块）：
            // - <userRequest> 块 → 真实提问（单独段）
            // - 其余标签块(environment/workspace/memory/context/editorContext 等) → 独立规则段
            let contents = input
                .history
                .iter()
                .filter(|turn| turn.role == "user")
                .map(|turn| turn.content.as_str())
                .chain(std::iter::once(input.question.as_str()));
            for c in contents {
                if c.is_empty() {
                    continue;
                }
                let sections = split_tagged_sections(c);
                if sections.is_empty() {
                    // 无标签的整条 user：作为环境规则段
                    parts.push(format!("以下是用户的环境信息：\n{}", c));
                    continue;
                }
                for (tag, body) in sections {
                    match tag.as_deref() {
                        Some("userRequest") => {
                            // 只保留最后一条 userRequest（历史 Try Again 等不覆盖当前提问）
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

    // 不覆写 convert：完整内容透传，不截断（如后续 DeepSeek 端出现输入超限再恢复截断）
}

#[cfg(test)]
mod tests {
    use super::{split_tagged_sections, VscodeCopilotConverter};
    use crate::converters::api::openai::to_agent_input;
    use crate::prompts::Converter;
    use serde_json::json;

    /// 工具提示词：必须有具体示例与硬性要求，且不得出现错误写法/占位符字面量
    /// （在提示词里演示错误写法会把错误格式“教”给模型）
    #[test]
    fn tool_prompt_contains_hard_requirements() {
        let tools = json!([{ "type": "function", "function": { "name": "read_file", "parameters": json!({ "type": "object" }) } }]);
        let text = VscodeCopilotConverter.convert_tools(&tools);
        for needle in [
            "<tool_call>",
            "name: read_file",
            "缩进两个空格",
            "逐字符写成 tool_call",
            "输出前自检",
        ] {
            assert!(text.contains(needle), "工具段缺少内容: {}\n实际: {}", needle, text);
        }
        let reminder = VscodeCopilotConverter.tool_format_reminder();
        assert!(reminder.contains("缩进"), "提醒段应强调参数缩进: {}", reminder);
        assert!(reminder.contains("逐字符写成 tool_call"), "提醒段应强调围栏名: {}", reminder);
        assert!(reminder.contains("name: read_file"), "提醒段应给出具体示例: {}", reminder);
        assert!(reminder.contains("filePath:"), "提醒段示例应含参数行: {}", reminder);
        // 禁止清单必须点名 DSML/XML 标签，否则模型会混写
        assert!(reminder.contains("DSML"), "提醒段应显式禁止 DSML: {}", reminder);
        assert!(reminder.contains("parameter"), "提醒段应显式禁止 parameter 标签: {}", reminder);
        for bad in ["<toolcall>", "call_call", "<工具名>", "<参数名>"] {
            assert!(!reminder.contains(bad), "提醒段不应出现错误写法/占位符: {}", bad);
            assert!(!text.contains(bad), "工具段不应出现错误写法/占位符: {}", bad);
        }
    }

    #[test]
    fn split_basic_tagged() {
        let s = "<environment>这是环境</environment><workspace>这是工作区</workspace>";
        let out = split_tagged_sections(s);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], (Some("environment".into()), "这是环境".into()));
        assert_eq!(out[1], (Some("workspace".into()), "这是工作区".into()));
    }

    #[test]
    fn split_mixed_with_plain_text() {
        let s = "前言 <foo>内容A</foo> 中间 <bar>内容B</bar> 结尾";
        let out = split_tagged_sections(s);
        assert_eq!(out.len(), 5);
        assert_eq!(out[0], (None, "前言".into()));
        assert_eq!(out[1], (Some("foo".into()), "内容A".into()));
        assert_eq!(out[2], (None, "中间".into()));
        assert_eq!(out[3], (Some("bar".into()), "内容B".into()));
        assert_eq!(out[4], (None, "结尾".into()));
    }

    #[test]
    fn split_skips_closing_and_inline_tags() {
        let s = "<userRequest>真实提问</userRequest> 剩下 <b>加粗</b> 内容";
        let out = split_tagged_sections(s);
        // <b>…</b> 会匹配成功（简单标签），此处验证 <userRequest> 与普通文本
        assert_eq!(out[0], (Some("userRequest".into()), "真实提问".into()));
        assert_eq!(out[1], (None, "剩下".into()));
        assert_eq!(out[2], (Some("b".into()), "加粗".into()));
        assert_eq!(out[3], (None, "内容".into()));
    }

    #[test]
    fn split_unclosed_tag_keeps_plain() {
        let s = "<environment>没有闭合的标签";
        let out = split_tagged_sections(s);
        // 未闭合标签整体作为普通文本段保留，不丢信息
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], (None, "<environment>没有闭合的标签".into()));
    }

    #[test]
    fn split_real_copilot_user_context() {
        // 真实 Copilot 单次 user 上下文：一条消息内平铺多个标签块，<userRequest> 为真实提问
        let s = "<environment_info>Windows 系统</environment_info>\n\
                 <workspace_info>工作区结构</workspace_info>\n\
                 <userMemory>用户记忆</userMemory>\n\
                 <repoMemory>仓库记忆</repoMemory>\n\
                 <editorContext>编辑器上下文</editorContext>\n\
                 <reminderInstructions>提醒指令</reminderInstructions>\n\
                 <userRequest>请帮我写个函数</userRequest>";
        let out = split_tagged_sections(s);
        assert_eq!(out.len(), 7);
        assert_eq!(
            out[0],
            (Some("environment_info".into()), "Windows 系统".into())
        );
        assert_eq!(out[1], (Some("workspace_info".into()), "工作区结构".into()));
        assert_eq!(out[2], (Some("userMemory".into()), "用户记忆".into()));
        assert_eq!(out[3], (Some("repoMemory".into()), "仓库记忆".into()));
        assert_eq!(
            out[4],
            (Some("editorContext".into()), "编辑器上下文".into())
        );
        assert_eq!(
            out[5],
            (Some("reminderInstructions".into()), "提醒指令".into())
        );
        assert_eq!(
            out[6],
            (Some("userRequest".into()), "请帮我写个函数".into())
        );
    }

    #[test]
    fn split_real_copilot_system_tags() {
        // 真实 Copilot 系统提示词内的标签块
        let s = "System: You are an expert.\n\
                 <instructions>\n指令内容\n</instructions>\n\
                 <toolUseInstructions>\n工具使用\n</toolUseInstructions>\n\
                 <skills>\n技能\n</skills>";
        let out = split_tagged_sections(s);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0], (None, "System: You are an expert.".into()));
        assert_eq!(out[1], (Some("instructions".into()), "指令内容".into()));
        assert_eq!(
            out[2],
            (Some("toolUseInstructions".into()), "工具使用".into())
        );
        assert_eq!(out[3], (Some("skills".into()), "技能".into()));
    }

    /// 基于真实捕获的 messages_20260901_005758_692.json 结构
    /// [0]system + [1]user(env+workspace合并) + [2]user(userRequest) + 历史重试 + 最后一轮 user
    #[test]
    fn convert_user_real_messages_first_turn() {
        let messages = vec![
            json!({"role": "system", "content": "You are an expert.\n<instructions>规则</instructions>"}),
            json!({"role": "user", "content": "<environment_info>Windows</environment_info>\n<workspace_info>项目结构</workspace_info>"}),
            json!({"role": "user", "content": "<userRequest>你好？</userRequest>"}),
            json!({"role": "user", "content": "<userRequest>Try Again</userRequest>"}),
        ];
        let input = to_agent_input(&messages, None, Default::default());
        assert!(input.is_first_turn(), "无 assistant 历史应算首轮");
        let c = VscodeCopilotConverter.convert_user(&input);
        // 环境/工作区 各成规则段；提问取最后一条 userRequest
        assert!(c.contains("以下是 环境信息 内容：\nWindows"));
        assert!(c.contains("以下是 工作区信息 内容：\n项目结构"));
        assert!(c.contains("用户的问题：\nTry Again"));
        assert!(!c.contains("你好？"));
    }

    #[test]
    fn convert_user_real_messages_follow_up() {
        // 追问：最后一条 user 合并 context + editorContext + reminderInstructions + userRequest
        let messages = vec![
            json!({"role": "assistant", "content": "回答时间: x\n会话: abc\ndef"}),
            json!({"role": "user", "content": "<context>当前日期</context>\n<editorContext>当前文件 api.rs</editorContext>\n<reminderInstructions>请检查</reminderInstructions>\n<userRequest>你好</userRequest>"}),
        ];
        let input = to_agent_input(&messages, None, Default::default());
        assert!(!input.is_first_turn(), "有 assistant 历史应算追问轮");
        let c = VscodeCopilotConverter.convert_user(&input);
        // context/editorContext/reminderInstructions 都作为规则段保留，userRequest 作为提问
        assert!(c.contains("以下是 上下文信息 内容：\n当前日期"));
        assert!(c.contains("以下是 编辑器上下文 内容：\n当前文件 api.rs"));
        assert!(c.contains("以下是 提醒指令 内容：\n请检查"));
        assert!(c.contains("用户：\n你好"));
    }

    /// 追问轮(带工具)应再次强调工具调用格式；首轮不注入提醒(已有完整工具段)
    #[test]
    fn assemble_follow_up_injects_format_reminder() {
        let tools = json!([{"function": {"name": "read_file", "description": "读取文件", "parameters": {"type": "object", "properties": {}}}}]);
        // 追问：注入格式提醒
        let follow_up = vec![
            json!({"role": "assistant", "content": "回答时间: x\n会话: abc\ndef"}),
            json!({"role": "user", "content": "<userRequest>继续</userRequest>"}),
        ];
        let input = to_agent_input(&follow_up, Some(tools.clone()), Default::default());
        assert!(!input.is_first_turn());
        let c = VscodeCopilotConverter.assemble(&input);
        assert!(c.contains("【工具调用格式】"), "追问应再次强调格式, 实际: {}", c);
        assert!(c.contains("<tool_call>"), "提醒应含 <tool_call> 围栏");
        assert!(c.contains("</tool_call>"));
        // 提醒必须放在提示词末尾（紧跟在用户提问之后，贴近生成位置）
        let user_pos = c.find("用户：").expect("应含用户提问");
        let reminder_pos = c.find("【工具调用格式】").unwrap();
        assert!(
            reminder_pos > user_pos,
            "格式提醒应在用户提问之后（末尾），实际: {}",
            c
        );
        assert!(
            c.trim_end().ends_with("只能使用这一种围栏。"),
            "格式提醒应在提示词末尾，实际: {}",
            c
        );
        // 首轮：完整工具段，不注入提醒
        let first_turn = vec![json!({"role": "user", "content": "<userRequest>继续</userRequest>"})];
        let input_first = to_agent_input(&first_turn, Some(tools), Default::default());
        assert!(input_first.is_first_turn());
        let cf = VscodeCopilotConverter.assemble(&input_first);
        assert!(cf.contains("以下是你可以调用的工具"));
        assert!(cf.contains("输出前自检"), "首轮工具段应含完整格式要求, 实际: {}", cf);
        assert!(!cf.contains("【工具调用格式】"), "首轮不应再注入提醒, 实际: {}", cf);
    }
}
