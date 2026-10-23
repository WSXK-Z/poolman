//! 会话标题生成：先用启发式从用户首条消息提炼，必要时交给本地模型或网页渠道。
//!
//! 标题请求的正文通常是一小段 JSON（如 `[{"seq":9,"text":"讲讲当前的项目"}]`），
//! 或客户端直接给的一句话。这个任务的本质是「把第一句人话压成十几个字」，
//! 不需要生成模型：启发式零依赖、确定性、可单测。
//!
//! 生成策略由 [`TitleSettings`] 决定：
//! - [`TitleSource::Heuristic`]（默认）：只做启发式，失败就回落到网页渠道；
//! - [`TitleSource::Local`]：启发式失败后交给本地 ONNX 模型，模型不可用再回落网页渠道；
//! - [`TitleSource::Web`]：直接交给网页渠道（保持旧行为）。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

/// 标题内容的宽度上限：汉字记 1，ASCII 记 0.5，约等于 14 个汉字或 28 个英文字符。
const MAX_TITLE_WIDTH: f32 = 14.0;

/// 设置文件名（数据目录下，与对话频率共用一份）。
const SETTINGS_FILE: &str = "settings.json";

/// 标题设置项键名。
const TITLE_KEY: &str = "title";

/// 本地模型默认目录（相对数据目录）。
pub const DEFAULT_MODEL_DIR: &str = "models/qwen3-0.6b";

/// 标题来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleSource {
    /// 只用启发式；失败时回落网页渠道。
    Heuristic,
    /// 启发式失败后交给本地模型，模型不可用再回落网页渠道。
    Local,
    /// 直接走网页渠道。
    Web,
}

impl TitleSource {
    /// 持久化用的字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Heuristic => "heuristic",
            Self::Local => "local",
            Self::Web => "web",
        }
    }

    /// 从持久化字符串解析；无法识别时回落启发式（失败安全的默认值）。
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "local" => Self::Local,
            "web" => Self::Web,
            _ => Self::Heuristic,
        }
    }
}

/// 标题生成设置：来源策略 + 本地模型目录。
pub struct TitleSettings {
    /// 来源策略。
    source: std::sync::Mutex<TitleSource>,
    /// 本地模型目录；空表示未配置。
    model_dir: std::sync::Mutex<String>,
    /// 持久化文件；None 表示只在内存里（测试）。
    path: Option<PathBuf>,
}

impl TitleSettings {
    /// 从数据目录加载；文件缺失或损坏时用默认值。
    pub fn load(data_dir: &str) -> Arc<Self> {
        let path = Path::new(data_dir).join(SETTINGS_FILE);
        let (source, model_dir) = read_title(&path).unwrap_or((TitleSource::Heuristic, String::new()));
        Arc::new(Self {
            source: std::sync::Mutex::new(source),
            model_dir: std::sync::Mutex::new(model_dir),
            path: Some(path),
        })
    }

    /// 只存在内存里的实例：测试用。
    pub fn in_memory(source: TitleSource, model_dir: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            source: std::sync::Mutex::new(source),
            model_dir: std::sync::Mutex::new(model_dir.into()),
            path: None,
        })
    }

    /// 当前来源策略。
    pub fn source(&self) -> TitleSource {
        *self.source.lock().expect("标题设置锁不可用")
    }

    /// 当前本地模型目录（未配置时为空串）。
    pub fn model_dir(&self) -> String {
        self.model_dir.lock().expect("标题设置锁不可用").clone()
    }

    /// 本地模型目录的绝对路径；未配置时返回 None。
    ///
    /// 相对路径按数据目录解析，方便用户只填 `models/qwen3-0.6b`。
    pub fn model_path(&self) -> Option<PathBuf> {
        let raw = self.model_dir();
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        let path = PathBuf::from(raw);
        if path.is_absolute() {
            return Some(path);
        }
        self.path
            .as_ref()
            .and_then(|settings| settings.parent())
            .map(|dir| dir.join(path))
    }

    /// 是否配置了本地模型目录。
    pub fn has_local_model(&self) -> bool {
        self.model_path().is_some()
    }

    /// 保存设置并持久化，返回实际生效的值。
    pub fn save(&self, source: TitleSource, model_dir: &str) -> Result<(), String> {
        let model_dir = model_dir.trim().to_string();
        if let Some(path) = &self.path {
            write_title(path, source, &model_dir)
                .map_err(|error| format!("保存标题设置失败（{}）：{}", path.display(), error))?;
        }
        *self.source.lock().expect("标题设置锁不可用") = source;
        *self.model_dir.lock().expect("标题设置锁不可用") = model_dir;
        Ok(())
    }
}

/// 读取标题设置；文件不存在、损坏或没有该键时返回 None。
fn read_title(path: &Path) -> Option<(TitleSource, String)> {
    let data = fs::read(path).ok()?;
    let value: Value = serde_json::from_slice(&data).ok()?;
    let title = value.get(TITLE_KEY)?;
    let source = title
        .get("source")
        .and_then(Value::as_str)
        .map(TitleSource::parse)
        .unwrap_or(TitleSource::Heuristic);
    let model_dir = title
        .get("model_dir")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some((source, model_dir))
}

/// 写入标题设置：只改本项，其余键原样保留（文件坏掉时重建）。
fn write_title(path: &Path, source: TitleSource, model_dir: &str) -> std::io::Result<()> {
    let mut settings = fs::read(path)
        .ok()
        .and_then(|data| serde_json::from_slice::<Value>(&data).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    settings.insert(
        TITLE_KEY.to_string(),
        serde_json::json!({
            "source": source.as_str(),
            "model_dir": model_dir,
        }),
    );
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(&Value::Object(settings))
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    fs::write(path, text)
}

/// 从标题请求的消息里提炼标题；拿不出可用内容时返回 None。
pub fn heuristic_title(messages: &[Value]) -> Option<String> {
    for text in candidate_texts(messages) {
        if let Some(title) = title_from_text(&text) {
            return Some(title);
        }
    }
    None
}

/// 交给本地模型的对话正文：把非噪音的人话拼起来，过长时只留前段。
///
/// 标题只需要"这段对话在聊什么"，不需要完整历史，因此截断到
/// [`MAX_MODEL_INPUT_CHARS`] 即可，也顺带把推理前的分词量压下来。
pub fn conversation_for_model(messages: &[Value]) -> String {
    const MAX_MODEL_INPUT_CHARS: usize = 800;
    let mut joined = String::new();
    for text in candidate_texts(messages) {
        if is_noise(&text) {
            continue;
        }
        let cleaned = clean(&text);
        if cleaned.is_empty() {
            continue;
        }
        if !joined.is_empty() {
            joined.push('\n');
        }
        joined.push_str(&cleaned);
        if joined.chars().count() >= MAX_MODEL_INPUT_CHARS {
            break;
        }
    }
    joined.chars().take(MAX_MODEL_INPUT_CHARS).collect()
}

/// 清洗模型输出：去掉引号、书名号、前后标点与多余换行，再按标题宽度截断。
///
/// 小模型经常把指令原样带出来（"标题："）或多写一整句，这里只留第一行。
pub fn sanitize_generated_title(raw: &str) -> Option<String> {
    let first_line = raw
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let stripped = first_line
        .trim_start_matches(|ch: char| {
            matches!(ch, '"' | '\'' | '「' | '『' | '“' | '《' | '#' | '*' | '-' | ' ' | '：' | ':')
        })
        .trim_end_matches(|ch: char| {
            matches!(ch, '"' | '\'' | '」' | '』' | '”' | '》' | '。' | '.' | ' ' | '：' | ':')
        });
    let stripped = stripped.strip_prefix("标题：").unwrap_or(stripped);
    let stripped = stripped.strip_prefix("标题:").unwrap_or(stripped);
    let title = trim_title(stripped.trim());
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

/// 按顺序取出可能是「人话」的文本片段。
fn candidate_texts(messages: &[Value]) -> Vec<String> {
    let mut out = Vec::new();
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let Some(content) = message.get("content").and_then(Value::as_str) else {
            continue;
        };
        out.extend(expand_content(content));
    }
    out
}

/// 一条 user 正文可能是 JSON 数组（客户端把人类消息打包进来），也可能就是一句话。
fn expand_content(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    if trimmed.starts_with('[') || trimmed.starts_with('{') {
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            let mut texts = Vec::new();
            collect_texts(&value, &mut texts);
            if !texts.is_empty() {
                return texts;
            }
        }
    }
    vec![raw.to_string()]
}

/// 递归收集 JSON 里的 `text` / `content` 字段。
fn collect_texts(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_texts(item, out);
            }
        }
        Value::Object(map) => {
            for key in ["text", "content"] {
                if let Some(Value::String(text)) = map.get(key) {
                    if !text.trim().is_empty() {
                        out.push(text.clone());
                    }
                }
            }
            for key in ["messages", "items", "data"] {
                if let Some(child) = map.get(key) {
                    collect_texts(child, out);
                }
            }
        }
        Value::String(text) => {
            if !text.trim().is_empty() {
                out.push(text.clone());
            }
        }
        _ => {}
    }
}

/// 单条文本 → 标题；噪音或无内容时返回 None。
fn title_from_text(text: &str) -> Option<String> {
    if is_noise(text) {
        return None;
    }
    let cleaned = clean(text);
    if cleaned.is_empty() {
        return None;
    }
    let head = first_clause(&cleaned);
    let head = strip_leading_politeness(&head);
    let head = trim_title(&head);
    if head.is_empty() {
        None
    } else {
        Some(head)
    }
}

/// 客户端塞进来的运行时上下文、技能清单、时间戳等，都不是人话。
fn is_noise(text: &str) -> bool {
    let trimmed = text.trim_start();
    if trimmed.is_empty() {
        return true;
    }
    const NOISE_PREFIXES: &[&str] = &[
        "Current runtime context",
        "This snapshot supersedes",
        "<system-reminder>",
        "Time sampled while preparing",
        "Browser time zone",
        "Elapsed since the preceding",
        "Tokens prefixed with @",
        "You are an AI agent",
        "{",
        "[",
    ];
    if NOISE_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
    {
        return true;
    }
    // 只有标点或空白的片段没有信息量。
    trimmed.chars().all(|ch| !ch.is_alphanumeric() && !is_cjk(ch))
}

/// 去掉代码块、行内代码、链接与多余空白。
fn clean(text: &str) -> String {
    let without_fences = strip_fenced_code(text);
    let without_links = strip_links(&without_fences);
    let without_inline = strip_inline_code(&without_links);
    let collapsed = without_inline
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    collapsed.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 去掉 ``` 围起来的整段代码。
fn strip_fenced_code(text: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            inside = !inside;
            continue;
        }
        if !inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// 去掉行内 `code`。
fn strip_inline_code(text: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for ch in text.chars() {
        if ch == '`' {
            inside = !inside;
            continue;
        }
        if !inside {
            out.push(ch);
        }
    }
    out
}

/// Markdown 链接留文字、裸 URL 丢掉。
fn strip_links(text: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        // [text](url) → text
        if chars[index] == '[' {
            if let Some(close) = find_char(&chars, index + 1, ']') {
                if chars.get(close + 1) == Some(&'(') {
                    if let Some(paren) = find_char(&chars, close + 2, ')') {
                        out.extend(&chars[index + 1..close]);
                        index = paren + 1;
                        continue;
                    }
                }
            }
        }
        // http(s):// 开头的裸 URL 直接跳过
        if chars[index] == 'h' && starts_with_at(&chars, index, "http") {
            let mut end = index;
            while end < chars.len() && !chars[end].is_whitespace() {
                end += 1;
            }
            index = end;
            continue;
        }
        out.push(chars[index]);
        index += 1;
    }
    out
}

fn find_char(chars: &[char], from: usize, target: char) -> Option<usize> {
    (from..chars.len()).find(|&i| chars[i] == target)
}

fn starts_with_at(chars: &[char], index: usize, needle: &str) -> bool {
    let mut offset = 0;
    for ch in needle.chars() {
        match chars.get(index + offset) {
            Some(actual) if *actual == ch => offset += 1,
            _ => return false,
        }
    }
    true
}

/// 取第一句（中英文句末标点或换行处切）。
fn first_clause(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if matches!(ch, '。' | '！' | '？' | '!' | '?' | '\n' | ';' | '；') {
            break;
        }
        out.push(ch);
    }
    if out.trim().is_empty() {
        text.to_string()
    } else {
        out
    }
}

/// 去掉「帮我/请/麻烦」这类开场客套。
fn strip_leading_politeness(text: &str) -> String {
    const PREFIXES: &[&str] = &[
        "帮我", "帮忙", "请", "麻烦", "能不能", "可不可以", "可以", "我想", "想要", "给我",
        "Please ", "please ", "Can you ", "can you ", "Could you ", "could you ",
    ];
    let mut current = text.trim().to_string();
    loop {
        let mut stripped = false;
        for prefix in PREFIXES {
            if let Some(rest) = current.strip_prefix(prefix) {
                current = rest.trim_start().to_string();
                stripped = true;
                break;
            }
        }
        if !stripped {
            break;
        }
    }
    current
}

/// 首尾去标点，并按宽度截断。
fn trim_title(text: &str) -> String {
    let trimmed = text.trim_matches(|ch: char| {
        ch.is_whitespace() || matches!(ch, '，' | ',' | '：' | ':' | '、' | '-' | '—' | '"' | '\'')
    });
    let mut out = String::new();
    let mut width = 0.0f32;
    for ch in trimmed.chars() {
        let step = if is_cjk(ch) || ch.is_whitespace() { 1.0 } else { 0.5 };
        if width + step > MAX_TITLE_WIDTH && !out.is_empty() {
            break;
        }
        width += step;
        out.push(ch);
    }
    out.trim()
        .trim_end_matches(|ch: char| matches!(ch, '的' | '了' | '和' | '与' | '，' | ','))
        .trim()
        .to_string()
}

/// 是否为 CJK 字符。
fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x3040..=0x30FF | 0xAC00..=0xD7AF
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn messages(items: Vec<Value>) -> Vec<Value> {
        items
    }

    /// 客户端把人类消息打包成 JSON 数组：取第一条人话。
    #[test]
    fn extracts_first_human_message_from_json_array() {
        let msgs = messages(vec![json!({
            "role": "user",
            "content": r#"[{"seq":9,"text":"讲讲当前的项目"}]"#
        })]);
        assert_eq!(heuristic_title(&msgs).as_deref(), Some("讲讲当前的项目"));
    }

    /// 运行时上下文、技能清单、时间戳都要跳过。
    #[test]
    fn skips_client_runtime_context_noise() {
        let msgs = messages(vec![
            json!({ "role": "user", "content": "帮我看看工作流" }),
            json!({ "role": "user", "content": "Current runtime context. This snapshot supersedes earlier runtime-context snapshots." }),
            json!({ "role": "user", "content": "<system-reminder> A skill is reusable…" }),
            json!({ "role": "user", "content": "Time sampled while preparing turn 1, step 1: 2026-10-23" }),
        ]);
        assert_eq!(heuristic_title(&msgs).as_deref(), Some("看看工作流"));
    }

    /// 只有噪音时返回 None，交给下一级策略。
    #[test]
    fn returns_none_when_only_noise() {
        let msgs = messages(vec![json!({
            "role": "user",
            "content": "Current runtime context. This snapshot supersedes earlier ones."
        })]);
        assert!(heuristic_title(&msgs).is_none());
    }

    /// 代码块不进标题，只取第一句。
    #[test]
    fn strips_code_blocks_and_takes_first_clause() {
        let msgs = messages(vec![json!({
            "role": "user",
            "content": "修复元宝改名接口。顺便看看 ```rust\nfn a() {}\n``` 这段"
        })]);
        assert_eq!(heuristic_title(&msgs).as_deref(), Some("修复元宝改名接口"));
    }

    /// 客套话被去掉。
    #[test]
    fn strips_leading_politeness() {
        let msgs = messages(vec![json!({
            "role": "user",
            "content": "帮我实现标题生成功能"
        })]);
        assert_eq!(heuristic_title(&msgs).as_deref(), Some("实现标题生成功能"));
    }

    /// 超长标题按宽度截断。
    #[test]
    fn truncates_long_title() {
        let msgs = messages(vec![json!({
            "role": "user",
            "content": "把整个网关的请求链路从头到尾重新梳理一遍并写一份详细文档"
        })]);
        let title = heuristic_title(&msgs).expect("应有标题");
        assert!(title.chars().count() <= 15, "标题过长：{title}");
        assert!(title.starts_with("把整个网关"), "{title}");
    }

    /// 英文长句也截断到可读长度。
    #[test]
    fn truncates_long_english_title() {
        let msgs = messages(vec![json!({
            "role": "user",
            "content": "Refactor the entire gateway request pipeline and write a long document about it"
        })]);
        let title = heuristic_title(&msgs).expect("应有标题");
        assert!(title.chars().count() <= 30, "标题过长：{title}");
    }

    /// Markdown 链接留文字、裸链接丢掉。
    #[test]
    fn strips_markdown_links_and_urls() {
        let msgs = messages(vec![json!({
            "role": "user",
            "content": "看看 [官方文档](https://example.com/a) 和 https://example.com/b 的区别"
        })]);
        let title = heuristic_title(&msgs).expect("应有标题");
        assert!(!title.contains("http"), "{title}");
        assert!(title.contains("官方文档"), "{title}");
    }

    /// 设置持久化：只改 title 键，保留 chat_interval_ms。
    #[test]
    fn settings_round_trip_keeps_other_keys() {
        let dir = std::env::temp_dir().join(format!("poolman-title-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"chat_interval_ms":3000}"#).unwrap();

        let settings = TitleSettings::load(&dir.to_string_lossy());
        assert_eq!(settings.source(), TitleSource::Heuristic);
        settings
            .save(TitleSource::Local, "models/qwen3-0.6b")
            .expect("保存应成功");

        let raw: Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["chat_interval_ms"], json!(3000));
        assert_eq!(raw["title"]["source"], json!("local"));
        assert_eq!(raw["title"]["model_dir"], json!("models/qwen3-0.6b"));

        let reloaded = TitleSettings::load(&dir.to_string_lossy());
        assert_eq!(reloaded.source(), TitleSource::Local);
        assert!(reloaded.has_local_model());

        std::fs::remove_dir_all(dir).ok();
    }

    /// 未知来源回落启发式（失败安全的默认值）。
    #[test]
    fn unknown_source_falls_back_to_heuristic() {
        assert_eq!(TitleSource::parse("bogus"), TitleSource::Heuristic);
        assert_eq!(TitleSource::parse(" WEB "), TitleSource::Web);
        assert_eq!(TitleSource::parse("local"), TitleSource::Local);
    }
}
