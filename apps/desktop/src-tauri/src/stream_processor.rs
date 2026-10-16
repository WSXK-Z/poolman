// 统一流式处理器。
//
// 调用方注入 FenceRecognizer，处理器把任意边界切分的流式 chunk 规整为
// 「内容 + 嵌套闭合围栏」结构，工作流程：
// 1. 识别到围栏开边 → 创建闭合围栏并入栈（支持内嵌），记录开边原文与闭合标记；
// 2. 后续数据先匹配当前最内层围栏的闭合标记：命中则闭合围栏；未命中且不构成
//    闭合标记前缀的数据，经识别器确认身份后归入当前最内层围栏（无围栏时为顶层内容）；
// 3. 疑似围栏边界残片、识别器暂未认领的数据留在待定缓存，确认是围栏还是内容后才放出；
// 4. 初始化时通过 ProcessorOptions 开启 Markdown / JSON 围栏的自动识别，
//    内置识别器排在调用方识别器之前。

use std::mem;

/// 闭合标记匹配规则。
///
/// - `Exact`：闭边是固定字符串（如 `</tool_call>`）。
/// - `Repeat`：闭边是至少 `min_count` 个连续 `ch` 字符，且贪婪匹配尽可能多的重复；
///   用于 Markdown 代码围栏（开边几个反引号，闭边至少同样多）。
#[derive(Debug, Clone)]
pub enum CloseSpec {
    Exact(String),
    Repeat { ch: char, min_count: usize },
}

impl CloseSpec {
    pub fn exact(s: impl Into<String>) -> Self {
        CloseSpec::Exact(s.into())
    }
    pub fn repeat(ch: char, min_count: usize) -> Self {
        CloseSpec::Repeat { ch, min_count }
    }
}

impl From<&str> for CloseSpec {
    fn from(s: &str) -> Self {
        CloseSpec::Exact(s.to_string())
    }
}

impl From<String> for CloseSpec {
    fn from(s: String) -> Self {
        CloseSpec::Exact(s)
    }
}

/// 闭边匹配结果。
#[derive(Debug, Clone, Copy)]
enum CloseMatch {
    /// 匹配成功，消费 `n` 字节。
    Matched(usize),
    /// 可能是闭边前缀，等待更多数据。
    Partial,
    /// 确定不是闭边。
    NoMatch,
}

/// 尝试用 `spec` 匹配 `buf` 头部。
///
/// `at_eof` 为 `true` 时表示流已结束，`Repeat` 规则不会因为「全部是重复字符」
/// 而返回 `Partial`——流都结束了就不可能再有更多数据。
fn match_close(spec: &CloseSpec, buf: &str, at_eof: bool) -> CloseMatch {
    match spec {
        CloseSpec::Exact(needle) => {
            if buf.starts_with(needle.as_str()) {
                CloseMatch::Matched(needle.len())
            } else if needle.starts_with(buf) {
                CloseMatch::Partial
            } else {
                CloseMatch::NoMatch
            }
        }
        CloseSpec::Repeat { ch, min_count } => {
            // 统计 buf 开头的连续 `ch` 字符数量（贪婪）。
            let mut count = 0usize;
            let mut byte_end = 0usize;
            for (i, c) in buf.char_indices() {
                if c == *ch {
                    count += 1;
                    byte_end = i + c.len_utf8();
                } else {
                    break;
                }
            }
            if count >= *min_count {
                CloseMatch::Matched(byte_end)
            } else if byte_end == buf.len() && !at_eof {
                // buf 全是 ch，且还没到 min_count；可能后续会有更多
                CloseMatch::Partial
            } else {
                CloseMatch::NoMatch
            }
        }
    }
}

/// 识别器对缓存头部数据的判定结果。
pub enum Recognized {
    /// 确认是围栏开边：consume 为开边字节数，edge 为开边原文；
    /// close 为闭合标记规则，声明后闭边由处理器自动匹配，无需识别器再介入。
    FenceOpen {
        consume: usize,
        edge: String,
        close: Option<CloseSpec>,
    },
    /// 确认是完整围栏（自平衡块，如 JSON 对象）：edge 为开边原文，content 为围栏内容，
    /// 处理器直接产出一个已闭合的围栏。
    FenceBlock {
        consume: usize,
        edge: String,
        content: String,
    },
    /// 确认是围栏闭边（供开边时未声明闭合标记的围栏使用）。
    FenceClose { consume: usize },
    /// 确认是普通内容；consume 不应覆盖后续可能出现围栏边界的区域。
    Content { consume: usize },
    /// 头部疑似围栏边界残片，等待更多数据再判定（不消耗数据）。
    NeedMore,
    /// 不是本识别器的围栏，交给识别链上下一个识别器。
    NotMine,
}

/// 围栏识别器：判定缓存头部数据是围栏边界还是普通内容。
///
/// 约定：每次从缓存起始位置判定；返回 NeedMore / NotMine 时不得消耗数据。
/// 为提升效率，识别器可在 Content 中一次确认多个字节；若整条识别链都返回
/// NotMine，处理器按单字符内容推进，围栏边界在后续位置仍能被识别。
pub trait FenceRecognizer {
    fn recognize(&self, buf: &str) -> Recognized;
}

/// 处理器选项：常见格式围栏的自动识别开关。
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessorOptions {
    /// 自动识别 Markdown 代码围栏（```lang ... ```）。
    pub auto_md: bool,
    /// 自动识别自平衡 JSON 对象（{"key": ...}）为围栏。
    pub auto_json: bool,
}

/// 闭合围栏：开边已记录，内容累积到闭边出现为止；支持内嵌子围栏。
#[derive(Debug, Clone)]
pub struct Fence {
    /// 开边原文。
    pub open_edge: String,
    /// 围栏内已确认的内容。
    pub content: String,
    /// 内嵌子围栏（按闭合顺序）。
    pub children: Vec<Fence>,
    /// 闭边是否已出现（finish 时未闭合的围栏会被强制闭合）。
    pub closed: bool,
}

/// 顶层节点：普通内容或闭合围栏。
#[derive(Debug, Clone)]
pub enum Node {
    Content(String),
    Fence(Fence),
}

/// feed / finish 产出的事件，供调用方实时转发。
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// 顶层（围栏外）内容。
    Content(String),
    /// 围栏开始：开边原文 + 深度（0 为最外层）。
    FenceOpen { edge: String, depth: usize },
    /// 围栏内内容（归属当前最内层开放围栏）。
    FenceContent(String),
    /// 围栏闭合。
    ///
    /// `edge` 为闭边原文；`None` 表示该围栏没有显式闭边——
    /// 要么是自平衡块（FenceBlock），要么是流末被强制闭合的开放围栏。
    FenceClose { depth: usize, edge: Option<String> },
}

/// 开放围栏栈条目。
struct OpenFence {
    fence: Fence,
    /// 闭合标记规则：None 表示闭边由识别器显式给出 FenceClose。
    close: Option<CloseSpec>,
}

/// 统一流式处理器。
pub struct StreamProcessor {
    recognizer: Box<dyn FenceRecognizer + Send>,
    /// 待定缓存：未识别或疑似围栏边界残片的数据。
    pending: String,
    /// 开放围栏栈（支持内嵌），栈顶为最内层。
    stack: Vec<OpenFence>,
    /// 顶层已完成节点。
    done: Vec<Node>,
}

impl StreamProcessor {
    /// 创建处理器：recognizer 为调用方围栏识别器；
    /// options 开启后会在调用方识别器之前自动识别 Markdown / JSON 围栏。
    pub fn new(
        recognizer: impl FenceRecognizer + Send + 'static,
        options: ProcessorOptions,
    ) -> Self {
        let mut chain: Vec<Box<dyn FenceRecognizer + Send>> = Vec::new();
        if options.auto_md {
            chain.push(Box::new(MarkdownFenceRecognizer));
        }
        if options.auto_json {
            chain.push(Box::new(JsonFenceRecognizer));
        }
        chain.push(Box::new(recognizer));
        Self {
            recognizer: Box::new(ChainRecognizer(chain)),
            pending: String::new(),
            stack: Vec::new(),
            done: Vec::new(),
        }
    }

    /// 传入一个流式 chunk，返回本次确认产出的事件。
    pub fn feed(&mut self, chunk: &str) -> Vec<StreamEvent> {
        self.pending.push_str(chunk);
        self.drain(false)
    }

    /// 流结束：缓存残留全部按内容放出，未闭合围栏逐层强制闭合；
    /// 返回收尾事件与全部顶层节点。
    pub fn finish(&mut self) -> (Vec<StreamEvent>, Vec<Node>) {
        let mut events = self.drain(true);
        while !self.stack.is_empty() {
            events.push(self.close_current(None));
        }
        (events, mem::take(&mut self.done))
    }

    /// 扫描待定缓存，把能确认身份的数据全部放出。
    fn drain(&mut self, flush_all: bool) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        loop {
            if self.pending.is_empty() {
                break;
            }

            // 1. 当前最内层围栏的闭合标记优先匹配。
            let close_match = self
                .stack
                .last()
                .and_then(|f| f.close.as_ref())
                .map(|spec| match_close(spec, &self.pending, flush_all));

            match close_match {
                Some(CloseMatch::Matched(n)) => {
                    let edge: String = self.pending.drain(..n).collect();
                    events.push(self.close_current(Some(edge)));
                    continue;
                }
                Some(CloseMatch::Partial) => {
                    // 闭边残片：留在缓存等后续数据；流末（flush_all）时按内容放出。
                    if flush_all {
                        self.flush_pending(&mut events);
                    }
                    break;
                }
                Some(CloseMatch::NoMatch) => {
                    // 「同字符至少 N 个」的闭合规则下，短于 N 的连续同字符属于本围栏内容：
                    // 交给识别链会被当作新的开边，而 Markdown 围栏不允许用更短的同类围栏内嵌。
                    if self.emit_short_repeat(&mut events) {
                        continue;
                    }
                }
                None => {}
            }

            // 2. 识别链判定缓存头部。
            match self.recognizer.recognize(&self.pending) {
                Recognized::FenceOpen {
                    consume,
                    edge,
                    close,
                } => {
                    self.pending.drain(..consume);
                    let depth = self.stack.len();
                    self.stack.push(OpenFence {
                        fence: Fence {
                            open_edge: edge.clone(),
                            content: String::new(),
                            children: Vec::new(),
                            closed: false,
                        },
                        close,
                    });
                    events.push(StreamEvent::FenceOpen { edge, depth });
                }
                Recognized::FenceBlock {
                    consume,
                    edge,
                    content,
                } => {
                    self.pending.drain(..consume);
                    let depth = self.stack.len();
                    let fence = Fence {
                        open_edge: edge.clone(),
                        content: content.clone(),
                        children: Vec::new(),
                        closed: true,
                    };
                    self.attach_finished(fence);
                    events.push(StreamEvent::FenceOpen { edge, depth });
                    events.push(StreamEvent::FenceContent(content));
                    events.push(StreamEvent::FenceClose { depth, edge: None });
                }
                Recognized::FenceClose { consume } => {
                    let edge_text: String = self.pending.drain(..consume).collect();
                    if self.stack.is_empty() {
                        // 无开放围栏的闭边按内容放出，避免丢数据。
                        self.emit_content(edge_text, &mut events);
                    } else {
                        events.push(self.close_current(Some(edge_text)));
                    }
                }
                Recognized::Content { consume } => {
                    if consume == 0 {
                        break;
                    }
                    let text: String = self.pending.drain(..consume).collect();
                    self.emit_content(text, &mut events);
                }
                Recognized::NeedMore => {
                    if flush_all {
                        self.flush_pending(&mut events);
                    }
                    break;
                }
                Recognized::NotMine => {
                    // 整条识别链都不认领：按单字符内容推进。
                    let ch_len = self
                        .pending
                        .chars()
                        .next()
                        .map(|c| c.len_utf8())
                        .unwrap_or(0);
                    if ch_len == 0 {
                        break;
                    }
                    let text: String = self.pending.drain(..ch_len).collect();
                    self.emit_content(text, &mut events);
                }
            }
        }
        events
    }

    /// 「同字符至少 N 个」的闭合规则下，短于 N 的连续同字符不是闭边：按围栏内容放出。
    ///
    /// 这类短串（如 4 个反引号围栏里出现的 3 个反引号）交给识别链会被当成新的开边，
    /// 而同类围栏不允许用更短的围栏内嵌。返回是否消费了缓存。
    fn emit_short_repeat(&mut self, events: &mut Vec<StreamEvent>) -> bool {
        let Some(CloseSpec::Repeat { ch, min_count }) =
            self.stack.last().and_then(|open| open.close.as_ref())
        else {
            return false;
        };
        let (ch, min_count) = (*ch, *min_count);

        let mut count = 0usize;
        let mut byte_end = 0usize;
        for (i, c) in self.pending.char_indices() {
            if c != ch {
                break;
            }
            count += 1;
            byte_end = i + c.len_utf8();
        }
        if count == 0 || count >= min_count {
            return false;
        }

        let text: String = self.pending.drain(..byte_end).collect();
        self.emit_content(text, events);
        true
    }

    /// 确认的内容归入当前最内层围栏，无开放围栏时并入顶层内容；
    /// 与上一事件同类型时合并，避免按字符碎发。
    fn emit_content(&mut self, text: String, events: &mut Vec<StreamEvent>) {
        if text.is_empty() {
            return;
        }
        match self.stack.last_mut() {
            Some(top) => {
                top.fence.content.push_str(&text);
                match events.last_mut() {
                    Some(StreamEvent::FenceContent(prev)) => prev.push_str(&text),
                    _ => events.push(StreamEvent::FenceContent(text)),
                }
            }
            None => {
                match self.done.last_mut() {
                    Some(Node::Content(last)) => last.push_str(&text),
                    _ => self.done.push(Node::Content(text.clone())),
                }
                match events.last_mut() {
                    Some(StreamEvent::Content(prev)) => prev.push_str(&text),
                    _ => events.push(StreamEvent::Content(text)),
                }
            }
        }
    }

    /// 闭合当前最内层围栏：内嵌围栏并入父围栏，最外层归入顶层节点。
    fn close_current(&mut self, edge: Option<String>) -> StreamEvent {
        let mut top = self.stack.pop().expect("close_current 需有开放围栏");
        top.fence.closed = true;
        let depth = self.stack.len();
        self.attach_finished(top.fence);
        StreamEvent::FenceClose { depth, edge }
    }

    /// 已闭合围栏挂到父围栏或顶层节点。
    fn attach_finished(&mut self, fence: Fence) {
        match self.stack.last_mut() {
            Some(parent) => parent.fence.children.push(fence),
            None => self.done.push(Node::Fence(fence)),
        }
    }

    /// 缓存残留全部按内容放出。
    fn flush_pending(&mut self, events: &mut Vec<StreamEvent>) {
        let text = mem::take(&mut self.pending);
        self.emit_content(text, events);
    }
}

/// 有序识别链：依次尝试各识别器，任一给出确定性结果即返回；
/// 全部 NotMine 才返回 NotMine。
struct ChainRecognizer(Vec<Box<dyn FenceRecognizer + Send>>);

impl FenceRecognizer for ChainRecognizer {
    fn recognize(&self, buf: &str) -> Recognized {
        for r in &self.0 {
            match r.recognize(buf) {
                Recognized::NotMine => continue,
                other => return other,
            }
        }
        Recognized::NotMine
    }
}

/// Markdown 代码围栏识别器。
///
/// 开边：至少 3 个反引号 + 语言标识 + 换行；闭边至少包含同样多的反引号。
struct MarkdownFenceRecognizer;

impl FenceRecognizer for MarkdownFenceRecognizer {
    fn recognize(&self, buf: &str) -> Recognized {
        // 统计开头的连续反引号数量。反引号是单字节字符，字节下标 = 个数。
        let first_non_tick = buf.find(|c: char| c != '`');
        let ticks = first_non_tick.unwrap_or(buf.len());

        if ticks < 3 {
            // 不足 3 个反引号：可能是开边前缀，也可能是普通反引号（行内代码）。
            return if first_non_tick.is_none() {
                Recognized::NeedMore
            } else {
                Recognized::NotMine
            };
        }

        // 开边含语言标识，等换行符到达再确认完整开边。
        let Some(line_end) = buf.find('\n') else {
            return Recognized::NeedMore;
        };

        Recognized::FenceOpen {
            consume: line_end + 1,
            edge: buf[..=line_end].to_string(),
            // 闭边至少包含与开边相同数量的反引号；贪婪匹配更多反引号。
            close: Some(CloseSpec::Repeat {
                ch: '`',
                min_count: ticks,
            }),
        }
    }
}

/// JSON 围栏识别器：行内自平衡的 JSON 对象（{"key": ...}）整体作为一个闭合围栏。
struct JsonFenceRecognizer;

impl FenceRecognizer for JsonFenceRecognizer {
    fn recognize(&self, buf: &str) -> Recognized {
        if !buf.starts_with('{') {
            return Recognized::NotMine;
        }
        // 形状预检：{ 后（跳过空白）应是 " 或 }，否则按普通文本处理。
        let trimmed = buf[1..].trim_start();
        if trimmed.is_empty() {
            return Recognized::NeedMore;
        }
        if !trimmed.starts_with('"') && !trimmed.starts_with('}') {
            return Recognized::NotMine;
        }
        match balanced_json_end(buf) {
            Some(end) => {
                debug_assert!(buf.as_bytes()[end - 1] == b'}');
                Recognized::FenceBlock {
                    consume: end,
                    edge: "{".to_string(),
                    content: buf[1..end - 1].to_string(),
                }
            }
            // 数据不足以完成平衡匹配，等待后续 chunk。
            None => Recognized::NeedMore,
        }
    }
}

/// 扫描自平衡 JSON 对象（字符串与转义感知），返回匹配 } 之后一个字节的下标；
/// 缓存内尚未平衡时返回 None。
fn balanced_json_end(buf: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in buf.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 标签围栏识别器：<tool_call> 开，</tool_call> 闭。
    struct TagRecognizer;

    impl FenceRecognizer for TagRecognizer {
        fn recognize(&self, buf: &str) -> Recognized {
            const OPEN: &str = "<tool_call>";
            const CLOSE: &str = "</tool_call>";
            if buf.starts_with(OPEN) {
                return Recognized::FenceOpen {
                    consume: OPEN.len(),
                    edge: OPEN.to_string(),
                    close: Some(CloseSpec::exact(CLOSE)),
                };
            }
            // 开边残片：等待更多数据。
            if OPEN.starts_with(buf) {
                return Recognized::NeedMore;
            }
            Recognized::NotMine
        }
    }

    #[test]
    fn content_and_fence_across_chunks() {
        let mut p = StreamProcessor::new(TagRecognizer, ProcessorOptions::default());
        // 边界残片 "<to" 留在缓存，确认开边后内容归入围栏，闭边残片同样等待。
        assert!(matches!(p.feed("hello <to")[..], [StreamEvent::Content(ref s)] if s == "hello "));
        assert!(matches!(
            p.feed("ol_call>inner data</tool_")[..],
            [StreamEvent::FenceOpen { .. }, StreamEvent::FenceContent(ref s)] if s == "inner data"
        ));
        assert!(matches!(
            p.feed("call> tail")[..],
            [StreamEvent::FenceClose { depth: 0, .. }, StreamEvent::Content(ref s)] if s == " tail"
        ));
        let (_, nodes) = p.finish();
        assert_eq!(nodes.len(), 3);
        match &nodes[1] {
            Node::Fence(f) => {
                assert_eq!(f.open_edge, "<tool_call>");
                assert_eq!(f.content, "inner data");
                assert!(f.closed);
            }
            _ => panic!("应为围栏节点"),
        }
    }

    #[test]
    fn nested_fences() {
        let mut p = StreamProcessor::new(TagRecognizer, ProcessorOptions::default());
        p.feed("<tool_call>outer <tool_call>inner</tool_call> rest</tool_call>");
        let (_, nodes) = p.finish();
        match &nodes[..] {
            [Node::Fence(f)] => {
                assert_eq!(f.content, "outer  rest");
                match &f.children[..] {
                    [child] => assert_eq!(child.content, "inner"),
                    _ => panic!("应有一个内嵌子围栏"),
                }
            }
            _ => panic!("应为一个顶层围栏"),
        }
    }

    #[test]
    fn auto_json_fence() {
        let mut p = StreamProcessor::new(
            TagRecognizer,
            ProcessorOptions {
                auto_md: false,
                auto_json: true,
            },
        );
        // 平衡未完成时等待；普通文本中的 {x} 不触发。
        assert!(p
            .feed("use {x} then ")
            .iter()
            .all(|e| matches!(e, StreamEvent::Content(_))));
        let events = p.feed("{\"a\": {\"b\": 1}} tail");
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::FenceContent(s) if s == "\"a\": {\"b\": 1}")));
    }

    #[test]
    fn auto_md_fence() {
        let mut p = StreamProcessor::new(
            TagRecognizer,
            ProcessorOptions {
                auto_md: true,
                auto_json: false,
            },
        );
        // 开边残片 `` 等待；开边确认后内容进围栏，``` 闭边由处理器匹配。
        assert!(p.feed("``").is_empty());
        let events = p.feed("`rust\ncode\n```after");
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::FenceOpen { edge, .. } if edge == "```rust\n")));
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::FenceContent(s) if s == "code\n")));
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::FenceClose { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::Content(s) if s == "after")));
    }

    #[test]
    fn auto_md_longer_fence() {
        // 4 个反引号的开边需要至少 4 个反引号闭合；内部 ``` 不应关闭围栏。
        let mut p = StreamProcessor::new(
            TagRecognizer,
            ProcessorOptions {
                auto_md: true,
                auto_json: false,
            },
        );
        let events = p.feed("````md\n```\ninner\n```\n````rest");
        // 内部 ``` 不会被当作闭边。
        assert!(events.iter().any(
            |e| matches!(e, StreamEvent::FenceContent(s) if s.contains("```\ninner\n```"))
        ));
        // 只有连续的 4 个反引号才闭合。
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::FenceClose { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::Content(s) if s == "rest")));
    }

    #[test]
    fn finish_closes_unclosed_fence() {
        let mut p = StreamProcessor::new(TagRecognizer, ProcessorOptions::default());
        p.feed("<tool_call>no close");
        let (events, nodes) = p.finish();
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::FenceClose { edge: None, .. })));
        match &nodes[..] {
            [Node::Fence(f)] => {
                assert_eq!(f.content, "no close");
                assert!(f.closed);
            }
            _ => panic!("应为一个顶层围栏"),
        }
    }

    #[test]
    fn close_event_carries_edge() {
        // 正常闭合时，FenceClose 事件应携带真实闭边原文。
        let mut p = StreamProcessor::new(TagRecognizer, ProcessorOptions::default());
        let events = p.feed("<tool_call>x</tool_call>");
        assert!(events.iter().any(
            |e| matches!(e, StreamEvent::FenceClose { edge: Some(s), .. } if s == "</tool_call>")
        ));
    }

    #[test]
    fn dangling_close_becomes_content() {
        // 没有开放围栏时收到的闭边识别结果不应丢数据。
        struct OnlyClose;
        impl FenceRecognizer for OnlyClose {
            fn recognize(&self, buf: &str) -> Recognized {
                const CLOSE: &str = "</x>";
                if buf.starts_with(CLOSE) {
                    Recognized::FenceClose {
                        consume: CLOSE.len(),
                    }
                } else if CLOSE.starts_with(buf) {
                    Recognized::NeedMore
                } else {
                    Recognized::NotMine
                }
            }
        }
        let mut p = StreamProcessor::new(OnlyClose, ProcessorOptions::default());
        let events = p.feed("</x>");
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::Content(s) if s == "</x>")));
    }
}
