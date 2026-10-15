// DeepSeek 流式响应适配器。
//
// SSE 字节文本经 DeepSeekParser 拆分为 THINK/RESPONSE 增量，
// RESPONSE 文本经统一流式处理器识别工具调用围栏（内嵌支持），
// 产出与旧实现兼容的 JSON 增量字符串。

use serde_json::{json, Value};
use tracing::warn;

use super::tool_parsing::{extract_session_id, parse_tool_calls, DeepSeekParser};
use crate::extension::adapters::stream_processor::{
    Fence, FenceRecognizer, Node, ProcessorOptions, Recognized, StreamEvent,
    StreamProcessor as UnifiedStreamProcessor,
};

// 围栏前缀（长前缀在前，避免短前缀误匹配后 NeedMore）
const FENCE_PREFIXES: &[&str] = &["<tool_calls", "<tool_call", "<toolcall", "<invoke"];

/// DeepSeek 工具调用围栏识别器。
///
/// 识别 <tool_call> / <toolcall> / <tool_calls> / <invoke> 开边，
/// 闭合标记由开边标签名自动派生，处理器负责匹配闭边。
struct DeepSeekFenceRecognizer;

impl FenceRecognizer for DeepSeekFenceRecognizer {
    fn recognize(&self, buf: &str) -> Recognized {
        for prefix in FENCE_PREFIXES {
            if buf.starts_with(prefix) {
                // 前缀匹配，需要 '>' 确认完整开边
                match buf.find('>') {
                    Some(gt_pos) => {
                        let open_tag = &buf[..=gt_pos];
                        let close = close_tag_for_open_edge(open_tag);
                        return Recognized::FenceOpen {
                            consume: gt_pos + 1,
                            edge: open_tag.to_string(),
                            close,
                        };
                    }
                    None => return Recognized::NeedMore,
                }
            }
        }
        // buf 可能是某围栏前缀的前缀（如 "<to"、"<inv"）
        if buf.starts_with('<') {
            for prefix in FENCE_PREFIXES {
                if prefix.starts_with(buf) {
                    return Recognized::NeedMore;
                }
            }
        }
        Recognized::NotMine
    }
}

/// 从开边标签派生闭合标记
fn close_tag_for_open_edge(open_edge: &str) -> Option<String> {
    let s = open_edge.strip_prefix('<')?;
    let end = s.find(|c: char| c.is_whitespace() || c == '>' || c == '/')?;
    let tag = &s[..end];
    if tag.is_empty() {
        return None;
    }
    Some(format!("</{}>", tag))
}

/// 重建围栏完整文本（开边 + 内容 + 子围栏 + 闭合标记）
fn reconstruct_fence_text(fence: &Fence) -> String {
    let mut s = fence.open_edge.clone();
    s.push_str(&fence.content);
    for child in &fence.children {
        s.push_str(&reconstruct_fence_text(child));
    }
    if let Some(close) = close_tag_for_open_edge(&fence.open_edge) {
        s.push_str(&close);
    }
    s
}

/// DeepSeek 流式驱动器：SSE 解析 + 围栏识别 -> JSON 增量。
///
/// 内部组合 DeepSeekParser（SSE 行解析）和统一 StreamProcessor（围栏识别），
/// 对外保持与旧 StreamProcessor 相同的接口（feed/finish/take_hint_error/message_id）。
pub(crate) struct DeepSeekStreamDriver {
    parser: DeepSeekParser,
    line_buf: String,
    /// RESPONSE 围栏处理器
    processor: UnifiedStreamProcessor,
    prefix: String,
    prefix_sent: bool,
}

impl DeepSeekStreamDriver {
    pub(crate) fn new(_has_tools: bool, prefix: String) -> Self {
        Self {
            parser: DeepSeekParser::new(),
            line_buf: String::new(),
            processor: UnifiedStreamProcessor::new(
                DeepSeekFenceRecognizer,
                ProcessorOptions {
                    auto_md: false,
                    auto_json: false,
                },
            ),
            prefix,
            prefix_sent: false,
        }
    }

    /// 接收任意边界切分的原始文本；完整 SSE 行送入解析器并产生输出增量。
    pub(crate) fn feed(&mut self, data: &str) -> Vec<String> {
        self.line_buf.push_str(data);
        let mut out = Vec::new();
        while let Some(nl) = self.line_buf.find('\n') {
            let line = self.line_buf[..nl].to_string();
            self.line_buf.drain(..nl + 1);
            self.parser.process_line(&line);
            out.extend(self.drain());
        }
        out
    }

    fn drain(&mut self) -> Vec<String> {
        let delta = self.parser.answer();
        if delta.is_null() {
            return Vec::new();
        }
        let mut out = Vec::new();

        // THINK：直接输出，不经过围栏处理器（思考里的工具调用围栏不当作工具调用）
        if let Some(t) = delta.get("THINK").and_then(|v| v.as_str()) {
            if !t.is_empty() {
                out.push(json!({ "THINK": t }).to_string());
            }
        }

        // RESPONSE：经围栏处理器识别围栏
        let resp = if delta.is_string() {
            delta.as_str().unwrap_or("").to_string()
        } else {
            delta
                .get("RESPONSE")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        if !resp.is_empty() {
            let events = self.processor.feed(&resp);
            out.extend(self.map_events(events));
        }

        out
    }

    fn map_events(&mut self, events: Vec<StreamEvent>) -> Vec<String> {
        let mut out = Vec::new();
        for event in events {
            match event {
                StreamEvent::Content(text) => {
                    let body = if self.prefix_sent {
                        text
                    } else {
                        self.prefix_sent = true;
                        format!("{}{}", self.prefix, text)
                    };
                    out.push(json!({ "RESPONSE": body }).to_string());
                }
                // 围栏事件：内容由处理器累积，finish 时统一解析
                _ => {}
            }
        }
        out
    }

    /// 流结束：处理残留行，闭合未完成围栏，解析工具调用。
    pub(crate) fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();

        // 处理残留行
        if !self.line_buf.trim().is_empty() {
            self.parser.process_line(&self.line_buf);
            self.line_buf.clear();
            out.extend(self.drain());
        }

        // RESPONSE 处理器收尾：强制闭合未完成围栏
        let (events, nodes) = self.processor.finish();
        out.extend(self.map_events(events));

        // 从闭合围栏解析工具调用；失败则回退为普通文本
        for node in &nodes {
            if let Node::Fence(fence) = node {
                let full_text = reconstruct_fence_text(fence);
                if let Some(calls) = parse_tool_calls(&full_text) {
                    out.push(json!({ "tool_calls": calls }).to_string());
                } else if !full_text.is_empty() {
                    warn!(
                        "工具围栏内容未解析出工具调用，按普通文本回退: {}",
                        full_text.chars().take(300).collect::<String>()
                    );
                    let body = if self.prefix_sent {
                        full_text
                    } else {
                        self.prefix_sent = true;
                        format!("{}{}", self.prefix, full_text)
                    };
                    out.push(json!({ "RESPONSE": body }).to_string());
                }
            }
        }

        out
    }

    /// 取出 SSE hint 错误（rate limit 等）
    pub(crate) fn take_hint_error(&mut self) -> Option<String> {
        self.parser.take_hint_error()
    }

    /// 最近一次响应的消息 id（供下轮 parent_message_id）
    pub(crate) fn message_id(&self) -> Option<i64> {
        self.parser.message_id()
    }
}

/* ------------------------------ 其他辅助 ------------------------------ */

pub(crate) fn uuid() -> String {
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        randish(),
        randish() & 0xffff,
        randish() & 0x0fff,
        (randish() & 0x3fff) | 0x8000,
        randish()
    )
}

fn randish() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    t.wrapping_mul(2654435761).rotate_left(13)
}

/// 从消息历史里提取最近一次 assistant 回答中的会话 id
pub(crate) fn extract_session_id_from_history(messages: &[Value]) -> Option<String> {
    for m in messages.iter().rev() {
        if m.get("role").and_then(|v| v.as_str()) == Some("assistant") {
            if let Some(c) = m.get("content").and_then(|v| v.as_str()) {
                if let Some(id) = extract_session_id(c) {
                    return Some(id);
                }
            }
        }
    }
    None
}