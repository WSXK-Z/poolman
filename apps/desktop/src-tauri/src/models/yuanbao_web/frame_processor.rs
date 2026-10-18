//! 元宝网页流式帧处理器：把 `data:` 行按负载 `type` 分派成正文增量与收尾信息。
//!
//! 网页协议的帧形态（取自真实抓包）：
//! 1. `{"type":"text","msg":"…"}`：正文增量；没有 `msg` 时是流开始标记，忽略；
//! 2. `{"type":"meta",…}`：收尾信息，带 `messageId`、`stopReason`、`tokenUsageInfo`；
//! 3. `{"type":"searchGuid"|"replace"|"hint_v2",…}`：检索引用 / 多媒体 / 追问建议，与回答无关；
//! 4. `data: [DONE]`：流结束；
//! 5. `data: [plugin: …]`、`[MSGINDEX:n]`、`[TRACEID:…]`、`status`：非 JSON 的诊断标记，忽略。
//!
//! `event:` 行只标注后续 `data:` 的语义，不承载回答内容，忽略。
//!
//! 分行与 `data:` 前缀剥离在这里完成：调用方只管把原始流增量交进来，行被切在任意字节边界上都不用管。

use serde_json::{Map, Value};

/// `data:` 行的前缀。
const DATA_PREFIX: &str = "data:";

/// `event:` 行的前缀。
const EVENT_PREFIX: &str = "event:";

/// 流结束标记。
const DONE_MARKER: &str = "[DONE]";

/// 帧类型键。
const FIELD_TYPE: &str = "type";

/// 正文增量帧里承载正文的键。
const FIELD_MSG: &str = "msg";

/// 正文增量帧的类型名。
const TYPE_TEXT: &str = "text";

/// 收尾信息帧的类型名。
const TYPE_META: &str = "meta";

/// 上游报错帧的类型名。
const TYPE_ERROR: &str = "error";

/// 收尾帧里的消息 id 键。
const FIELD_MESSAGE_ID: &str = "messageId";

/// 收尾帧里的结束原因键。
const FIELD_STOP_REASON: &str = "stopReason";

/// 收尾帧里的用量键。
const FIELD_TOKEN_USAGE: &str = "tokenUsageInfo";

/// 用量里的输入 token 键。
const FIELD_PROMPT_TOKENS: &str = "promptTokens";

/// 用量里的输出 token 键。
const FIELD_COMPLETION_TOKENS: &str = "completionTokens";

/// 用量里的总量 token 键。
const FIELD_TOTAL_TOKENS: &str = "totalTokens";

/// 行分隔符。
const NEWLINE: char = '\n';

/// 行尾可能带回车。
const CARRIAGE_RETURN: char = '\r';

/// 网页端报告的用量。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    /// 输入 token。
    pub prompt_tokens: u64,
    /// 输出 token。
    pub completion_tokens: u64,
    /// 总量 token。
    pub total_tokens: u64,
}

/// 元宝网页流式帧处理器。
#[derive(Default)]
pub struct YuanbaoFrameProcessor {
    /// 未成行的残片：chunk 边界不可预期，一行可能被切在多个 chunk 之间。
    pending: String,
    /// 已累积的正文。
    text: String,
    /// 收尾帧里的消息 id（下轮请求的父消息标识）。
    message_id: Option<String>,
    /// 收尾帧里的结束原因。
    stop_reason: Option<String>,
    /// 收尾帧里的用量；网页端未给出时为 None。
    token_usage: Option<TokenUsage>,
    /// 上游报错帧里的提示（`type: "error"`）。
    upstream_error: Option<String>,
    /// 是否已收到流结束标记。
    finished: bool,
}

impl YuanbaoFrameProcessor {
    pub fn new() -> Self {
        Self::default()
    }

    /// 已累积的正文。
    pub fn text(&self) -> &str {
        &self.text
    }

    /// 收尾帧里的消息 id（流里没出现该字段时为 None）。
    pub fn message_id(&self) -> Option<&str> {
        self.message_id.as_deref()
    }

    /// 收尾帧里的结束原因（流里没出现该字段时为 None）。
    pub fn stop_reason(&self) -> Option<&str> {
        self.stop_reason.as_deref()
    }

    /// 收尾帧里的用量（网页端未给出或全零时为 None）。
    pub fn token_usage(&self) -> Option<TokenUsage> {
        self.token_usage
    }

    /// 上游在流里报的错（没有则为 None）：这轮为什么没有回答。
    pub fn upstream_error(&self) -> Option<&str> {
        self.upstream_error.as_deref()
    }

    /// 是否已收到流结束标记。
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// 送入一段原始流增量，返回其中的正文增量。
    ///
    /// 没有换行符的尾巴留在缓冲区里等下一个增量；流结束用 [`Self::finish`] 收尾。
    pub fn feed(&mut self, raw: &str) -> String {
        self.pending.push_str(raw);
        let mut deltas = String::new();
        while let Some(index) = self.pending.find(NEWLINE) {
            let line = self.pending[..index]
                .trim_end_matches(CARRIAGE_RETURN)
                .to_string();
            self.pending.drain(..=index);
            if let Some(delta) = self.process_line(&line) {
                deltas.push_str(&delta);
            }
        }
        deltas
    }

    /// 流结束：缓冲区里最后一行可能没等到换行符，补处理一次；返回其正文增量。
    pub fn finish(&mut self) -> String {
        let pending = std::mem::take(&mut self.pending);
        let line = pending.trim_end_matches(CARRIAGE_RETURN);
        if line.trim().is_empty() {
            return String::new();
        }
        self.process_line(line).unwrap_or_default()
    }

    /// 处理一行，返回其中的正文增量。
    fn process_line(&mut self, line: &str) -> Option<String> {
        let trimmed = line.trim();
        if trimmed.starts_with(EVENT_PREFIX) {
            return None;
        }
        let payload = trimmed.strip_prefix(DATA_PREFIX)?.trim();
        if payload.is_empty() {
            return None;
        }
        if payload == DONE_MARKER {
            self.finished = true;
            return None;
        }
        let Ok(value) = serde_json::from_str::<Value>(payload) else {
            return None;
        };
        let frame = value.as_object()?;
        match frame.get(FIELD_TYPE).and_then(Value::as_str)? {
            TYPE_TEXT => frame
                .get(FIELD_MSG)
                .and_then(Value::as_str)
                .filter(|msg| !msg.is_empty())
                .map(|msg| {
                    self.text.push_str(msg);
                    msg.to_string()
                }),
            TYPE_META => {
                self.note_meta(frame);
                None
            }
            TYPE_ERROR => {
                if let Some(msg) = frame
                    .get(FIELD_MSG)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|msg| !msg.is_empty())
                {
                    self.upstream_error = Some(msg.to_string());
                }
                None
            }
            _ => None,
        }
    }

    /// 记下收尾帧里的消息 id、结束原因与用量。
    fn note_meta(&mut self, frame: &Map<String, Value>) {
        if let Some(id) = frame.get(FIELD_MESSAGE_ID).and_then(Value::as_str) {
            if !id.is_empty() {
                self.message_id = Some(id.to_string());
            }
        }
        if let Some(reason) = frame.get(FIELD_STOP_REASON).and_then(Value::as_str) {
            if !reason.is_empty() {
                self.stop_reason = Some(reason.to_string());
            }
        }
        if let Some(usage) = frame.get(FIELD_TOKEN_USAGE).and_then(Value::as_object) {
            let read = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
            let usage = TokenUsage {
                prompt_tokens: read(FIELD_PROMPT_TOKENS),
                completion_tokens: read(FIELD_COMPLETION_TOKENS),
                total_tokens: read(FIELD_TOTAL_TOKENS),
            };
            if usage.total_tokens > 0 {
                self.token_usage = Some(usage);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 一帧 `data:` 行（带换行）。
    fn line(value: Value) -> String {
        format!("data: {}\n", value)
    }

    /// 逐字送达的正文帧累积成完整回答。
    #[test]
    fn text_frames_accumulate() {
        let mut processor = YuanbaoFrameProcessor::new();
        assert_eq!(processor.feed(&line(json!({"type": "text", "msg": "你"}))), "你");
        assert_eq!(processor.feed(&line(json!({"type": "text", "msg": "好"}))), "好");
        assert_eq!(processor.text(), "你好");
    }

    /// 上游报错帧记录错误提示，不产生正文。
    #[test]
    fn error_frame_records_message() {
        let mut processor = YuanbaoFrameProcessor::new();
        assert_eq!(
            processor.feed(&line(json!({
                "type": "error",
                "msg": "服务繁忙，请稍后再试。",
                "error": {}
            }))),
            ""
        );
        assert_eq!(processor.upstream_error(), Some("服务繁忙，请稍后再试。"));
    }

    /// 无 `msg` 的开始标记不产生正文。
    #[test]
    fn start_marker_has_no_text() {
        let mut processor = YuanbaoFrameProcessor::new();
        assert_eq!(processor.feed(&line(json!({"type": "text"}))), "");
        assert_eq!(processor.text(), "");
    }

    /// 检索引用与多媒体帧不进入正文。
    #[test]
    fn non_answer_frames_are_ignored() {
        let mut processor = YuanbaoFrameProcessor::new();
        let raw = [
            line(json!({"type": "searchGuid", "title": "引用"})),
            line(json!({"type": "replace", "replace": {}})),
            line(json!({"type": "hint_v2", "hints": []})),
        ]
        .concat();
        assert_eq!(processor.feed(&raw), "");
        assert_eq!(processor.text(), "");
    }

    /// 收尾帧给出消息 id、结束原因与用量。
    #[test]
    fn meta_frame_carries_message_id_and_usage() {
        let mut processor = YuanbaoFrameProcessor::new();
        processor.feed(&line(json!({
            "type": "meta",
            "messageId": "m1",
            "stopReason": "stop",
            "tokenUsageInfo": {
                "promptTokens": 10,
                "completionTokens": 20,
                "totalTokens": 30
            }
        })));
        assert_eq!(processor.message_id(), Some("m1"));
        assert_eq!(processor.stop_reason(), Some("stop"));
        assert_eq!(
            processor.token_usage(),
            Some(TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
            })
        );
    }

    /// 全零用量视为未给出，避免把 0 当成真实计数。
    #[test]
    fn zero_usage_is_treated_as_absent() {
        let mut processor = YuanbaoFrameProcessor::new();
        processor.feed(&line(json!({
            "type": "meta",
            "tokenUsageInfo": {
                "promptTokens": 0,
                "completionTokens": 0,
                "totalTokens": 0
            }
        })));
        assert_eq!(processor.token_usage(), None);
    }

    /// `[DONE]` 置结束标记；非 JSON 诊断标记被跳过。
    #[test]
    fn done_and_diagnostics() {
        let mut processor = YuanbaoFrameProcessor::new();
        processor.feed("data: [plugin: ]\n");
        processor.feed("data: [MSGINDEX:2]\n");
        processor.feed("data: [TRACEID:abc]\n");
        assert!(!processor.is_finished());
        processor.feed("data: [DONE]\n");
        assert!(processor.is_finished());
    }

    /// 行被切在任意字节边界上也能正确拼接。
    #[test]
    fn split_across_chunks() {
        let mut processor = YuanbaoFrameProcessor::new();
        let first = line(json!({ "type": "text", "msg": "好" }));
        let (head, tail) = first.split_at(first.len() / 2);
        assert_eq!(processor.feed(head), "");
        let rest = format!("{tail}data: [DONE]\n");
        assert_eq!(processor.feed(&rest), "好");
        assert!(processor.is_finished());
    }

    /// 没有换行符的最后一行由 `finish` 补处理。
    #[test]
    fn finish_handles_trailing_line() {
        let mut processor = YuanbaoFrameProcessor::new();
        let frame = line(json!({ "type": "text", "msg": "尾" }));
        assert_eq!(processor.feed(frame.trim_end()), "");
        assert_eq!(processor.finish(), "尾");
        assert_eq!(processor.text(), "尾");
    }
}
