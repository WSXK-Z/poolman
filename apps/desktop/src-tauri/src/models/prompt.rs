//! 网页渠道共用的提示词拼装：把 OpenAI 形状的推理请求摊平成渠道所需的文本。
//!
//! 未绑定远端会话时（标题请求、兼容路径）每轮把整段对话拼进提示词；
//! 已绑定时由 bootstrap / delta 分段函数生成首次上下文和后续增量。
//! 分段规则与工具调用标记在渠道间共用；标记常量与规则文本由 [`super::deepseek_web`] 提供。

use std::collections::HashMap;

use serde_json::{json, Value};
use tracing::warn;

use super::deepseek_web::{
    escape, CALLS_CLOSE, CALLS_OPEN, HISTORY_HEADING, INVOKE_CLOSE, INVOKE_OPEN, NORMALIZER_PROMPT,
    PARAMETER_CLOSE, PARAMETER_OPEN, PROMPT_PREFIX, SYSTEM_HEADING, TOOL_RULE, TOOL_RULE_HEADING,
    TOOLS_HEADING,
};
use super::defs::InferRequest;

/// 固定前缀 → 系统提示词 → 历史对话 → 工具调用格式与限定规则 → 可用工具 → 最新一条记录。
///
/// 工具结果的标题带上工具名与调用 id，与助手标记里那次 `invoke` 一一对应。
pub(crate) fn build_prompt(request: &InferRequest) -> String {
    let mut sections: Vec<String> = vec![PROMPT_PREFIX.to_string()];

    let system = render_system(&request.messages);
    if !system.is_empty() {
        sections.push(format!("{}{}{}", SYSTEM_HEADING, NEWLINE, system));
    }

    // system 消息已单独成段，其余消息里末尾那条是最新记录，之前的都算历史对话。
    let turns: Vec<&Value> = request
        .messages
        .iter()
        .filter(|message| !is_system(message))
        .collect();
    let Some((latest, history)) = turns.split_last() else {
        return sections.join(&DOUBLE_NEWLINE);
    };
    let tool_names = tool_names(&request.messages);
    let history = history
        .iter()
        .filter_map(|message| render_message(message, &tool_names))
        .collect::<Vec<_>>();
    if !history.is_empty() {
        sections.push(format!(
            "{}{}{}",
            HISTORY_HEADING,
            NEWLINE,
            history.join(&DOUBLE_NEWLINE)
        ));
    }
    // 工具段只在确实有工具时出现：格式规则与工具清单成对给出。
    if let Some(tools) = render_tools(request.tools.as_ref()) {
        sections.push(format!("{}{}{}", TOOL_RULE_HEADING, NEWLINE, TOOL_RULE));
        sections.push(format!("{}{}{}", TOOLS_HEADING, NEWLINE, tools));
    }
    if let Some(latest) = render_message(latest, &tool_names) {
        sections.push(latest);
    }
    sections.join(&DOUBLE_NEWLINE)
}

/// 首次建立远端会话时的 bootstrap 前缀。
///
/// 两种形态共用这句：是否把本轮提问放进初始化，由 [`build_bootstrap_segments`]
/// （带提问）与 [`build_context_segments`]（只初始化）区分。
pub(crate) const BOOTSTRAP_PREFIX: &str =
    "以下内容用于初始化本次会话的上下文。请先阅读，不要回答，只回复 OK。";

/// 初始化完成后的收尾提示：仍然不回答，等待提问轮。
pub(crate) const BOOTSTRAP_DONE: &str = "【上下文初始化完成｜本轮不要回答，只回复 OK】";

/// 已有远端会话时，只发送新增内容的提示。
pub(crate) const DELTA_PREFIX: &str = "继续上一轮对话，只处理本次新增内容：";

/// 真实提问轮的前缀：紧接其后的就是用户本轮的原话。
pub(crate) const QUESTION_PREFIX: &str = "请回答本轮提问：";

/// 工具调用强调：贴在本轮真实提问之后，让规则落在离提问最近的位置。
///
/// 初始化段里的 `TOOL_RULE` 离提问隔了若干轮，模型（尤其元宝这类非 DeepSeek 模型）
/// 很容易忽略；这一段在每次提问时重申，只保留「调不调、怎么调」两条硬约束。
const TOOL_EMPHASIS_RULE: &str = "## 本轮工具调用要求（与初始化里的规则一致）\n\
- **需要工具**：整条回复只输出一个 DSML 调用块，块外零字符，不要解释、不要 Markdown 围栏；\n\
- **不需要工具**：只输出正文，禁止出现任何 DSML 标记（含 `DSML` 字样）。";

/// 生成会话摘要的提示：只输出摘要，不回答对话。
pub(crate) fn build_summary_prompt(
    request: &InferRequest,
    existing_summary: &str,
    max_messages: usize,
) -> String {
    let mut out = String::from(
        "请把下面的对话压缩成一段中文摘要，供新会话继续工作时使用。\n\
         必须保留：用户目标、已确认的事实/约束、关键代码或文件、已完成的工具调用及结果、未完成事项。\n\
         不要回答问题，不要输出寒暄，只输出摘要正文。\n",
    );
    if !existing_summary.trim().is_empty() {
        out.push_str("\n已有摘要：\n");
        out.push_str(existing_summary.trim());
        out.push('\n');
    }
    let tool_names = tool_names(&request.messages);
    let turns: Vec<&Value> = request
        .messages
        .iter()
        .filter(|message| !is_system(message))
        .collect();
    let omitted = turns.len().saturating_sub(max_messages);
    let turns = if omitted > 0 { &turns[omitted..] } else { &turns[..] };
    if omitted > 0 {
        out.push_str(&format!("\n（更早的 {omitted} 条消息已省略，请结合已有摘要。）\n"));
    }
    out.push_str("\n对话：\n");
    for message in turns {
        if let Some(rendered) = render_message(message, &tool_names) {
            out.push_str(&rendered);
            out.push_str("\n\n");
        }
    }
    out
}

/// bootstrap 最多携带的历史消息条数：更早的内容用占位说明，避免无限拆段。
const MAX_BOOTSTRAP_HISTORY_MESSAGES: usize = 40;

/// 初始化段结尾的形态。
#[derive(Clone, Copy, PartialEq, Eq)]
enum BootstrapMode {
    /// 初始化 + 本轮提问：最后一段要求回答（历史上 DeepSeek 网页端用的形态）。
    WithQuestion,
    /// 只初始化：不回答，本轮提问由 [`build_question_prompt`] 单独一轮发出。
    ContextOnly,
}

/// 初始化段的公共单元：前缀、系统段、工具规则与工具表、历史（或摘要）。
fn context_units(
    request: &InferRequest,
    summary: Option<&str>,
    history: &[&Value],
) -> Vec<String> {
    let mut units: Vec<String> = vec![BOOTSTRAP_PREFIX.to_string()];

    let system = render_system(&request.messages);
    if !system.is_empty() {
        units.push(format!("{}{}{}", SYSTEM_HEADING, NEWLINE, system));
    }

    if let Some(tools) = request.tools.as_ref().and_then(Value::as_array) {
        if !tools.is_empty() {
            units.push(format!("{}{}{}", TOOL_RULE_HEADING, NEWLINE, TOOL_RULE));
            for (index, line) in render_tool_lines(tools).into_iter().enumerate() {
                if index == 0 {
                    units.push(format!("{}{}{}", TOOLS_HEADING, NEWLINE, line));
                } else {
                    units.push(line);
                }
            }
        }
    }

    if let Some(summary) = summary {
        units.push(format!("【历史摘要】{NEWLINE}{}", summary.trim()));
    } else if !history.is_empty() {
        let tool_names = tool_names(&request.messages);
        units.push(HISTORY_HEADING.to_string());
        let omitted = history.len().saturating_sub(MAX_BOOTSTRAP_HISTORY_MESSAGES);
        let history = if omitted > 0 {
            &history[history.len() - MAX_BOOTSTRAP_HISTORY_MESSAGES..]
        } else {
            history
        };
        if omitted > 0 {
            units.push(format!("（更早的 {omitted} 条历史消息已省略）"));
        }
        units.extend(render_history_groups(history, &tool_names));
    }
    units
}

/// 会话迁移：用摘要代替全量历史建立新远端会话（带本轮提问，最后一段要求回答）。
pub(crate) fn build_bootstrap_segments_with_summary(
    request: &InferRequest,
    summary: &str,
    max_chars: usize,
) -> Vec<String> {
    if summary.trim().is_empty() {
        return build_bootstrap_segments(request, max_chars);
    }
    let max_chars = max_chars.max(500);
    let tool_names = tool_names(&request.messages);
    let mut units = context_units(request, Some(summary), &[]);
    if let Some(tail) = question_with_emphasis(request, &tool_names) {
        units.push(tail);
    }
    decorate_bootstrap(pack_units(units, max_chars), BootstrapMode::WithQuestion)
}

/// 会话迁移（只初始化）：摘要代替历史，本轮提问留给提问轮。
pub(crate) fn build_context_segments_with_summary(
    request: &InferRequest,
    summary: &str,
    max_chars: usize,
) -> Vec<String> {
    if summary.trim().is_empty() {
        return build_context_segments(request, max_chars);
    }
    let max_chars = max_chars.max(500);
    let units = context_units(request, Some(summary), &[]);
    decorate_bootstrap(pack_units(units, max_chars), BootstrapMode::ContextOnly)
}

/// 首次建立远端会话：上下文 + 本轮提问，最后一段要求回答。
pub(crate) fn build_bootstrap_segments(request: &InferRequest, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(500);
    let tool_names = tool_names(&request.messages);
    let turns: Vec<&Value> = request
        .messages
        .iter()
        .filter(|message| !is_system(message))
        .collect();
    let Some((_, history)) = turns.split_last() else {
        let units = context_units(request, None, &[]);
        return decorate_bootstrap(pack_units(units, max_chars), BootstrapMode::ContextOnly);
    };
    let mut units = context_units(request, None, history);
    if let Some(tail) = question_with_emphasis(request, &tool_names) {
        units.push(tail);
    }
    decorate_bootstrap(pack_units(units, max_chars), BootstrapMode::WithQuestion)
}

/// 首次建立远端会话（只初始化）：只灌上下文，**本轮提问不进来**。
///
/// 提问由 [`build_question_prompt`] 在初始化完成后单独一轮发出，让工具调用规则
/// 紧贴提问，而不是埋在若干段初始化内容里。
pub(crate) fn build_context_segments(request: &InferRequest, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(500);
    let turns: Vec<&Value> = request
        .messages
        .iter()
        .filter(|message| !is_system(message))
        .collect();
    let start = current_turn_start(&turns);
    let units = context_units(request, None, &turns[..start]);
    decorate_bootstrap(pack_units(units, max_chars), BootstrapMode::ContextOnly)
}

/// 真实提问轮：本轮用户原话 + 工具调用强调。
///
/// 初始化完成后发这一轮。强调段紧贴提问，解决"规则放在初始化里、离提问太远"的问题——
/// 元宝这类非 DeepSeek 模型对远处规则的遵循度明显更差。
pub(crate) fn build_question_prompt(request: &InferRequest) -> String {
    let mut sections = vec![QUESTION_PREFIX.to_string()];
    let current = render_current_turn(request);
    if current.is_empty() {
        sections.push("（没有新增内容）".to_string());
    } else {
        sections.push(current);
    }
    if let Some(emphasis) = tool_emphasis(request.tools.as_ref()) {
        sections.push(emphasis);
    }
    sections.join(&DOUBLE_NEWLINE)
}

/// 当前轮次的起点：最后一条 assistant 之后；还没有 assistant 时整段都是本轮。
fn current_turn_start(turns: &[&Value]) -> usize {
    turns
        .iter()
        .rposition(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
        .map(|index| index + 1)
        .unwrap_or(0)
}

/// 渲染当前轮次的全部消息（用户本轮原话，可能连带客户端塞进来的运行时上下文）。
fn render_current_turn(request: &InferRequest) -> String {
    let tool_names = tool_names(&request.messages);
    let turns: Vec<&Value> = request
        .messages
        .iter()
        .filter(|message| !is_system(message))
        .collect();
    let start = current_turn_start(&turns);
    turns[start..]
        .iter()
        .filter_map(|message| render_message(message, &tool_names))
        .collect::<Vec<_>>()
        .join(&DOUBLE_NEWLINE)
}

/// 工具调用强调段：只在请求确实带工具时出现。
fn tool_emphasis(tools: Option<&Value>) -> Option<String> {
    let items = tools.and_then(Value::as_array)?;
    if items.is_empty() {
        return None;
    }
    Some(TOOL_EMPHASIS_RULE.to_string())
}

/// 本轮提问 + 工具强调，合成一个原子单元：装段时不会被拆到两段里。
fn question_with_emphasis(
    request: &InferRequest,
    tool_names: &HashMap<String, String>,
) -> Option<String> {
    let latest = request
        .messages
        .iter()
        .rev()
        .find(|message| !is_system(message))?;
    let mut tail = render_message(latest, tool_names)?;
    if let Some(emphasis) = tool_emphasis(request.tools.as_ref()) {
        tail.push_str(&DOUBLE_NEWLINE);
        tail.push_str(&emphasis);
    }
    Some(tail)
}

/// 已有远端会话：只取最后一条助手消息之后的新增内容，并重申工具调用要求。
pub(crate) fn build_delta_prompt(request: &InferRequest) -> String {
    let mut sections = vec![DELTA_PREFIX.to_string()];
    let current = render_current_turn(request);
    if current.is_empty() {
        sections.push("（没有新增内容）".to_string());
    } else {
        sections.push(current);
    }
    if let Some(emphasis) = tool_emphasis(request.tools.as_ref()) {
        sections.push(emphasis);
    }
    sections.join(&DOUBLE_NEWLINE)
}

/// 给每段加上"这是初始化"的前缀；只有带提问的形态最后一段才要求回答。
fn decorate_bootstrap(segments: Vec<String>, mode: BootstrapMode) -> Vec<String> {
    if segments.is_empty() {
        return vec![BOOTSTRAP_PREFIX.to_string()];
    }
    let total = segments.len();
    segments
        .into_iter()
        .enumerate()
        .map(|(index, segment)| {
            let last = index + 1 == total;
            let header = match (last, mode) {
                (true, BootstrapMode::WithQuestion) => {
                    format!("【会话初始化完成｜请回答最后一条】{NEWLINE}{NEWLINE}")
                }
                (true, BootstrapMode::ContextOnly) => {
                    format!("{BOOTSTRAP_DONE}{NEWLINE}{NEWLINE}")
                }
                (false, _) => format!(
                    "【会话初始化 {}/{}｜仅用于建立上下文，不要回答，只回复 OK】{NEWLINE}{NEWLINE}",
                    index + 1,
                    total
                ),
            };
            format!("{header}{segment}")
        })
        .collect()
}

fn pack_units(units: Vec<String>, max_chars: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for unit in units {
        for part in split_unit(&unit, max_chars) {
            let current_len = current.chars().count();
            let part_len = part.chars().count();
            if current_len > 0 && current_len + part_len + 2 > max_chars {
                out.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push_str(&DOUBLE_NEWLINE);
            }
            current.push_str(&part);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn split_unit(unit: &str, max_chars: usize) -> Vec<String> {
    if unit.chars().count() <= max_chars {
        return vec![unit.to_string()];
    }
    let mut parts = Vec::new();
    let mut current = String::new();
    for line in unit.split_inclusive('\n') {
        if current.chars().count() + line.chars().count() > max_chars && !current.is_empty() {
            parts.push(std::mem::take(&mut current));
        }
        if line.chars().count() > max_chars {
            // 单行仍超限：按字符切，避免把一整段内容硬塞给网页端。
            for ch in line.chars() {
                if current.chars().count() >= max_chars {
                    parts.push(std::mem::take(&mut current));
                }
                current.push(ch);
            }
        } else {
            current.push_str(line);
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// 组装修复提示词：规范化器提示词填上可用工具清单（含每个参数的 JSON Schema 与必填项），
/// 末尾附上**本轮失败的具体原因**与解析失败的坏块原文——模型据此知道该补哪一处，
/// 而不是笼统地「重写一遍」。
pub(crate) fn repair_prompt(tools: Option<&Value>, broken_markup: &str, reason: &str) -> String {
    let registered = render_tools(tools).unwrap_or_else(|| "（无）".to_string());
    format!(
        "{}{}{}{}本轮失败原因：{}{}以下是用户输入内容：{}{}",
        PROMPT_PREFIX,
        DOUBLE_NEWLINE,
        NORMALIZER_PROMPT.replace("{{REGISTERED_TOOLS}}", &registered),
        DOUBLE_NEWLINE,
        reason,
        DOUBLE_NEWLINE,
        NEWLINE,
        broken_markup
    )
}

/// 单个换行。
const NEWLINE: &str = "\n";

/// 两个换行。
const DOUBLE_NEWLINE: &str = "\n\n";

/// 是否为 system 消息。
fn is_system(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("system")
}

/// 系统提示词段：取全部 system 消息正文，多条之间空行分隔。
fn render_system(messages: &[Value]) -> String {
    messages
        .iter()
        .filter(|message| is_system(message))
        .map(message_content)
        .filter(|content| !content.is_empty())
        .collect::<Vec<_>>()
        .join(&DOUBLE_NEWLINE)
}

/// 取消息正文：字符串，或多模态内容数组里的文本片段；非文本片段只留占位标记，
/// 避免把图片的 base64 塞进提示词。
fn message_content(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(render_part)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(NEWLINE),
        _ => String::new(),
    }
}

/// 单条消息 → `【角色】正文`；助手消息里的历史工具调用按 DSML 标记接在正文之后。
///
/// 正文只有空白时按空处理：模型写在标记前后的排版、客户端存下的空段都不算回答内容，
/// 原样写进提示词只会变成大段空行。
fn render_message(message: &Value, tool_names: &HashMap<String, String>) -> Option<String> {
    let role = message.get("role").and_then(Value::as_str).unwrap_or("");
    let mut content = message_content(message);
    if content.trim().is_empty() {
        content.clear();
    }
    for call in render_tool_calls(message) {
        if !content.is_empty() {
            content.push_str(NEWLINE);
        }
        content.push_str(&call);
    }
    if role.is_empty() && content.is_empty() {
        return None;
    }
    let heading = match tool_result_label(message, tool_names) {
        Some(label) => format!("【{}】{}", role, label),
        None => format!("【{}】", role),
    };
    if content.is_empty() {
        return Some(heading);
    }
    Some(format!("{}{}{}", heading, NEWLINE, content))
}

/// 工具结果消息的来源标签：`name=工具名 id=调用id`，缺哪半就只写另一半，都缺则不标。
///
/// 工具名优先取消息自带的 `name`，没有就按 `tool_call_id` 到本轮全部助手消息的
/// `tool_calls` 里反查——客户端通常只回 `tool_call_id`。
fn tool_result_label(message: &Value, tool_names: &HashMap<String, String>) -> Option<String> {
    if message.get("role").and_then(Value::as_str) != Some("tool") {
        return None;
    }
    let id = message.get("tool_call_id").and_then(Value::as_str);
    let name = message
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| id.and_then(|id| tool_names.get(id).map(String::as_str)));
    match (name, id) {
        (Some(name), Some(id)) => Some(format!("name={name} id={id}")),
        (Some(name), None) => Some(format!("name={name}")),
        (None, Some(id)) => Some(format!("id={id}")),
        (None, None) => None,
    }
}

/// 全部助手消息里的 `tool_calls` → `调用id → 工具名`，供工具结果标注来源。
fn tool_names(messages: &[Value]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for message in messages {
        let Some(calls) = message.get("tool_calls").and_then(Value::as_array) else {
            continue;
        };
        for call in calls {
            let Some(id) = call.get("id").and_then(Value::as_str) else {
                continue;
            };
            let function = call.get("function").unwrap_or(call);
            if let Some(name) = function.get("name").and_then(Value::as_str) {
                names.insert(id.to_string(), name.to_string());
            }
        }
    }
    names
}

/// 助手消息里的 `tool_calls` → DSML 标记，一次调用一层 `invoke`，多个平铺在一个 `calls` 块里。
///
/// 参数是字符串时按 JSON 解析成对象，解析不了就原样保留。渲染出来的标记与解析器认的格式一致：
/// 字符串参数标 `string=true` 原样写，其余（数字 / 布尔 / 数组 / 对象）标 `string=false` 按 JSON 写，
/// 这样同一段标记被解析回来仍是同一个调用。
pub(crate) fn render_tool_calls(message: &Value) -> Vec<String> {
    let Some(calls) = message.get("tool_calls").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    for call in calls {
        let function = call.get("function").unwrap_or(call);
        let Some(name) = function.get("name").and_then(Value::as_str) else {
            continue;
        };
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .map(|id| format!(" id={id:?}"))
            .unwrap_or_default();
        let arguments = match function.get("arguments") {
            Some(Value::String(raw)) => {
                serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.clone()))
            }
            Some(other) => other.clone(),
            None => json!({}),
        };
        let Some(params) = arguments.as_object() else {
            // 参数不是对象（客户端给了坏数据）：只还原工具名，别编造参数
            warn!(
                "历史工具调用的 arguments 不是 JSON 对象，按无参还原：{}",
                name
            );
            lines.push(format!(
                "{INVOKE_OPEN} name={name:?}{id}>{INVOKE_CLOSE}"
            ));
            continue;
        };
        lines.push(format!("{INVOKE_OPEN} name={name:?}{id}>"));
        for (key, value) in params {
            // 字符串参数按解析端的反转义规则转义，保证渲染后再解析回来还是同一个值
            let (flag, literal) = match value {
                Value::String(text) => ("true", escape(text)),
                other => ("false", other.to_string()),
            };
            lines.push(format!(
                "{PARAMETER_OPEN} name={key:?} string={flag:?}>{literal}{PARAMETER_CLOSE}"
            ));
        }
        lines.push(INVOKE_CLOSE.to_string());
    }
    if lines.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(lines.len() + 2);
    out.push(CALLS_OPEN.to_string());
    out.extend(lines);
    out.push(CALLS_CLOSE.to_string());
    out
}

/// 多模态内容片段 → 文本；非文本片段给占位标记（如 `[image_url]`）。
fn render_part(part: &Value) -> String {
    if let Some(text) = part.get("text").and_then(Value::as_str) {
        return text.to_string();
    }
    let kind = part
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("未知片段");
    format!("[{}]", kind)
}

/// 历史消息按「助手工具调用 + 紧随其后的工具结果」分组，避免 bootstrap 把一次调用拆到两段。
fn render_history_groups(history: &[&Value], tool_names: &HashMap<String, String>) -> Vec<String> {
    let mut units = Vec::new();
    let mut index = 0;
    while index < history.len() {
        let message = history[index];
        let has_tool_calls = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| !calls.is_empty());
        if has_tool_calls {
            let mut group = Vec::new();
            if let Some(rendered) = render_message(message, tool_names) {
                group.push(rendered);
            }
            let mut next = index + 1;
            while next < history.len() {
                let role = history[next].get("role").and_then(Value::as_str).unwrap_or("");
                if role != "tool" && role != "function" {
                    break;
                }
                if let Some(rendered) = render_message(history[next], tool_names) {
                    group.push(rendered);
                }
                next += 1;
            }
            if !group.is_empty() {
                units.push(group.join(&DOUBLE_NEWLINE));
            }
            index = next;
            continue;
        }
        if let Some(rendered) = render_message(message, tool_names) {
            units.push(rendered);
        }
        index += 1;
    }
    units
}

/// 工具清单：每项一行 `- 名称：描述`，参数以紧凑 JSON 附在其后；没有可用工具时返回 None。
fn render_tools(tools: Option<&Value>) -> Option<String> {
    let items = tools.and_then(Value::as_array)?;
    let lines = render_tool_lines(items);
    if lines.is_empty() {
        return None;
    }
    Some(lines.join(NEWLINE))
}

/// 单个工具一行；bootstrap 分段时每个工具是一个原子单元。
fn render_tool_lines(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| {
            let function = item.get("function").unwrap_or(item);
            let name = function.get("name").and_then(Value::as_str).unwrap_or("");
            if name.is_empty() {
                return None;
            }
            let mut line = format!("- {}", name);
            if let Some(description) = function.get("description").and_then(Value::as_str) {
                if !description.is_empty() {
                    line.push_str(&format!("：{}", description));
                }
            }
            if let Some(parameters) = function.get("parameters") {
                line.push_str(&format!("{}{}参数：{}", NEWLINE, "  ", parameters));
            }
            Some(line)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        build_bootstrap_segments, build_bootstrap_segments_with_summary, build_context_segments,
        build_context_segments_with_summary, build_delta_prompt, build_question_prompt,
        build_summary_prompt,
    };
    use crate::models::InferRequest;
    use serde_json::json;

    fn request_with_tools() -> InferRequest {
        let mut request = InferRequest::new(
            "deepseek-web",
            vec![
                json!({ "role": "system", "content": "系统提示词".repeat(80) }),
                json!({ "role": "user", "content": "第一问" }),
                json!({ "role": "assistant", "content": "第一答" }),
                json!({ "role": "user", "content": "最新问题" }),
            ],
        );
        request.tools = Some(json!((0..8)
            .map(|index| json!({
                "type": "function",
                "function": {
                    "name": format!("tool_{index}"),
                    "description": "工具描述".repeat(30),
                    "parameters": { "type": "object", "properties": { "path": { "type": "string" } } }
                }
            }))
            .collect::<Vec<_>>()));
        request
    }

    /// 带提问的初始化（DeepSeek 形态）：最后一段给出本轮提问并要求回答。
    #[test]
    fn bootstrap_is_split_and_last_segment_answers() {
        let request = request_with_tools();
        let segments = build_bootstrap_segments(&request, 500);
        assert!(segments.len() > 1, "bootstrap 应拆成多段");
        assert!(segments.last().unwrap().contains("请回答最后一条"));
        assert!(segments.last().unwrap().contains("最新问题"));
        assert!(
            segments.last().unwrap().contains("本轮工具调用要求"),
            "提问所在段应带强调：{}",
            segments.last().unwrap()
        );
        for segment in &segments[..segments.len() - 1] {
            assert!(segment.contains("不要回答"));
            assert!(segment.contains("初始化"));
        }
    }

    /// 只初始化的形态（元宝用它）：不带本轮提问，也不要求任何一段回答。
    #[test]
    fn context_segments_only_prime_context() {
        let request = request_with_tools();
        let segments = build_context_segments(&request, 500);
        let all = segments.join("\n");
        assert!(!all.contains("最新问题"), "本轮提问不应进入初始化：{all}");
        assert!(!all.contains("请回答最后一条"), "初始化段不应要求回答：{all}");
        assert!(all.contains("第一问"), "历史对话应仍在初始化里");
        assert!(all.contains("第一答"));
        assert!(
            segments.last().unwrap().contains("初始化完成"),
            "最后一段应标明初始化完成"
        );
        for segment in &segments {
            assert!(segment.contains("不要回答"), "每段都应说明不回答：{segment}");
        }
    }

    /// 会话首轮没有历史：初始化段只剩系统提示词与工具表。
    #[test]
    fn context_segments_survive_first_turn() {
        let mut request = InferRequest::new(
            "yuanbao-web",
            vec![json!({ "role": "user", "content": "唯一的问题" })],
        );
        request.tools = Some(json!([{
            "type": "function",
            "function": { "name": "read", "parameters": { "type": "object" } }
        }]));
        let segments = build_context_segments(&request, 500);
        let all = segments.join("\n");
        assert!(!all.contains("唯一的问题"), "首轮提问同样不进初始化：{all}");
        assert!(all.contains("read"), "工具表应仍在初始化里");
    }

    /// 提问轮：只带本轮原话 + 工具调用强调，不带历史。
    #[test]
    fn question_prompt_carries_emphasis() {
        let request = request_with_tools();
        let prompt = build_question_prompt(&request);
        assert!(prompt.contains("最新问题"));
        assert!(!prompt.contains("第一问"), "提问轮不应重发历史");
        assert!(prompt.contains("本轮工具调用要求"), "{}", prompt);
        assert!(prompt.contains("DSML"), "{}", prompt);
    }

    /// 不带工具时，提问轮不出现工具强调。
    #[test]
    fn question_prompt_skips_emphasis_without_tools() {
        let mut request = request_with_tools();
        request.tools = None;
        let prompt = build_question_prompt(&request);
        assert!(prompt.contains("最新问题"));
        assert!(!prompt.contains("本轮工具调用要求"), "{}", prompt);
    }

    #[test]
    fn delta_only_contains_new_messages() {
        let request = request_with_tools();
        let delta = build_delta_prompt(&request);
        assert!(delta.contains("最新问题"));
        assert!(!delta.contains("第一问"));
    }

    /// 增量轮同样重申工具要求。
    #[test]
    fn delta_prompt_carries_emphasis() {
        let request = request_with_tools();
        let delta = build_delta_prompt(&request);
        assert!(delta.contains("本轮工具调用要求"), "{}", delta);
    }

    /// 会话迁移：用摘要代替全量历史，提问同样留给提问轮。
    #[test]
    fn context_segments_with_summary_omit_full_history() {
        let request = request_with_tools();
        let segments = build_context_segments_with_summary(
            &request,
            "用户正在重构 Rust 网关，已完成会话绑定。",
            500,
        );
        let all = segments.join("\n");
        assert!(all.contains("历史摘要"));
        assert!(all.contains("已完成会话绑定"));
        assert!(!all.contains("第一问"), "迁移时不应回放完整历史");
        assert!(
            !all.contains("最新问题"),
            "迁移初始化同样不带本轮提问：{all}"
        );
    }

    /// 带提问的迁移形态仍然保持原语义。
    #[test]
    fn bootstrap_with_summary_keeps_question() {
        let request = request_with_tools();
        let segments = build_bootstrap_segments_with_summary(
            &request,
            "用户正在重构 Rust 网关，已完成会话绑定。",
            500,
        );
        let all = segments.join("\n");
        assert!(all.contains("历史摘要"));
        assert!(!all.contains("第一问"), "迁移时不应回放完整历史");
        assert!(all.contains("最新问题"));
        assert!(segments.last().unwrap().contains("请回答最后一条"));
    }

    #[test]
    fn summary_prompt_keeps_existing_summary_and_recent_messages() {
        let request = request_with_tools();
        let prompt = build_summary_prompt(&request, "旧摘要", 4);
        assert!(prompt.contains("旧摘要"));
        assert!(prompt.contains("最新问题"));
        assert!(prompt.contains("摘要"));
    }
}