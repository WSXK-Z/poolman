use serde_json::{Map, Value};

use crate::apis::base::DeltaChunk;

/// 追加片段帧的路径。
const FRAGMENTS_PATH: &str = "response/fragments";

/// 片段内容帧的路径前缀。
const FRAGMENT_CONTENT_PREFIX: &str = "response/fragments/";

/// 简写正文帧的路径。
const CONTENT_PATH: &str = "response/content";

/// 网页回答的一个片段。
enum Fragment {
    /// 思考片段。
    Think(Content),
    /// 正文片段，也是类型未知时的归类。
    Response(Content),
    /// 其它类型：类型原值原样透出，调用方自行判断用途。
    Other(String, Content),
}

impl Default for Fragment {
    fn default() -> Self {
        Fragment::Response(Content::default())
    }
}

impl Fragment {
    /// 按片段对象里的 `type` 归类：空值与正文同名，其余按原值透出。
    ///
    /// 片段类型名取自 [`DeltaChunk`] 的键常量：网页协议的 `type` 与中间增量的键本就是同一套词。
    fn new(ftype: &str, content: Content) -> Self {
        match ftype {
            "" | DeltaChunk::RESPONSE => Fragment::Response(content),
            DeltaChunk::THINK => Fragment::Think(content),
            other => Fragment::Other(other.to_string(), content),
        }
    }

    /// 片段类型名。
    fn kind(&self) -> &str {
        match self {
            Fragment::Think(_) => DeltaChunk::THINK,
            Fragment::Response(_) => DeltaChunk::RESPONSE,
            Fragment::Other(kind, _) => kind,
        }
    }

    /// 累积内容与增量游标。
    fn content(&mut self) -> &mut Content {
        match self {
            Fragment::Think(content)
            | Fragment::Response(content)
            | Fragment::Other(_, content) => content,
        }
    }

    /// 用片段对象里的 `type` / `content` 覆盖本片段，游标退回覆盖后的字符边界内。
    fn assign(&mut self, value: &Value) {
        let ftype = value.get("type").and_then(Value::as_str).unwrap_or("");
        let text = value.get("content").and_then(Value::as_str).unwrap_or("");
        let content = std::mem::take(self.content());
        *self = Self::new(ftype, content);
        self.set(text);
    }

    /// 覆盖内容，游标退回新内容的字符边界内。
    fn set(&mut self, text: &str) {
        self.content().set(text);
    }

    /// 追加内容。
    fn append(&mut self, text: &str) {
        self.content().append(text);
    }

    /// 取出游标之后的未放内容（片段类型 + 增量）并推进游标；没有未放内容返回 `None`。
    fn take_delta(&mut self) -> Option<(String, String)> {
        let delta = self.content().take()?;
        Some((self.kind().to_string(), delta))
    }
}

/// 片段的累积内容与增量游标。
#[derive(Default)]
struct Content {
    /// 累积内容。
    text: String,
    /// 已作为增量放出的字节数，始终落在 `text` 的字符边界上。
    sent: usize,
}

impl Content {
    /// 覆盖内容，游标退回新内容的字符边界内。
    fn set(&mut self, text: &str) {
        self.text.clear();
        self.text.push_str(text);
        self.rewind();
    }

    /// 追加内容。
    fn append(&mut self, text: &str) {
        self.text.push_str(text);
    }

    /// 把游标退回到不超过内容长度、且落在字符边界上的位置：覆盖后已放出的部分不能再当增量。
    fn rewind(&mut self) {
        let mut sent = self.sent.min(self.text.len());
        while !self.text.is_char_boundary(sent) {
            sent -= 1;
        }
        self.sent = sent;
    }

    /// 取出游标之后的未放内容并推进游标；没有未放内容返回 `None`。
    fn take(&mut self) -> Option<String> {
        if self.sent >= self.text.len() {
            return None;
        }
        let delta = self.text[self.sent..].to_string();
        self.sent = self.text.len();
        Some(delta)
    }
}

/// DeepSeek 网页流式帧处理器：把 `data:` 行里的片段按索引累积，只放出各自的新增内容。
///
/// 网页协议的增量形态：
/// 1. 整体快照 `{"v":{"response":{"fragments":[…]}}}`：已有片段按下标覆盖，多出来的追加；
/// 2. 追加片段 `{"p":"response/fragments","o":"APPEND","v":[…]}`；
/// 3. 片段内容 `{"p":"response/fragments/<idx>/content","o":"APPEND"|"SET","v":"…"}`：`idx` 为 -1 表示最后一个片段，越界下标补空片段；
/// 4. 简写正文 `{"p":"response/content","v":"…"}`；
/// 5. 无路径续帧 `{"v":"…"}`：续写当前（最后一个）片段，保留其类型——真实流量里正文与思考都主要靠它逐字送达。
///
/// 带路径但不属于以上形态的帧（`response/status`、`…/elapsed_secs`、`response` 批量帧等）一律忽略。
///
/// 分行也在这里完成：调用方只管把原始流增量交进来，行被切在任意字节边界上都不用管。
pub struct SseFrameProcessor {
    fragments: Vec<Fragment>,
    /// 未成行的残片：chunk 边界不可预期，一行可能被切在多个 chunk 之间。
    pending: String,
    /// 最近一次响应的消息 id：下轮请求的 parent_message_id 来源。
    message_id: Option<i64>,
}

impl SseFrameProcessor {
    pub fn new() -> Self {
        Self {
            fragments: Vec::new(),
            pending: String::new(),
            message_id: None,
        }
    }

    /// 最近一次响应的消息 id（流里没出现 id 帧时为 None）。
    pub fn message_id(&self) -> Option<i64> {
        self.message_id
    }

    /// 送入一段原始流增量，返回其中完整行产生的新增（片段类型, 内容）。
    ///
    /// 没有换行符的尾巴留在缓冲区里等下一个增量；流结束用 [`Self::finish`] 收尾。
    pub fn feed(&mut self, raw: &str) -> Vec<(String, String)> {
        self.pending.push_str(raw);
        let mut deltas = Vec::new();
        while let Some(index) = self.pending.find('\n') {
            let line = self.pending[..index].trim_end_matches('\r').to_string();
            self.pending.drain(..=index);
            deltas.extend(self.process_line(&line));
        }
        deltas
    }

    /// 流结束：缓冲区里最后一行可能没等到换行符，补处理一次；返回其新增。
    pub fn finish(&mut self) -> Vec<(String, String)> {
        let pending = std::mem::take(&mut self.pending);
        let line = pending.trim_end_matches('\r');
        if line.trim().is_empty() {
            return Vec::new();
        }
        self.process_line(line)
    }

    /// 处理一行 SSE，返回本次新增的（片段类型, 内容）。
    pub fn process_line(&mut self, line: &str) -> Vec<(String, String)> {
        let trimmed = line.trim();
        let payload = match trimmed.strip_prefix("data:") {
            Some(rest) => rest.trim(),
            None => trimmed,
        };
        if payload.is_empty() || payload == "[DONE]" {
            return Vec::new();
        }
        let Ok(value) = serde_json::from_str::<Value>(payload) else {
            return Vec::new();
        };
        self.apply(&value);
        self.take_deltas()
    }

    /// 把一帧并入片段表。
    fn apply(&mut self, value: &Value) {
        let Some(frame) = value.as_object() else {
            return;
        };
        // 响应消息 id：首帧的顶层 response_message_id，或快照帧的 v.response.message_id。
        if let Some(id) = frame.get("response_message_id").and_then(Value::as_i64) {
            self.message_id = Some(id);
        }
        if let Some(id) = frame
            .get("v")
            .and_then(|v| v.get("response"))
            .and_then(|response| response.get("message_id"))
            .and_then(Value::as_i64)
        {
            self.message_id = Some(id);
        }
        let path = frame.get("p").and_then(Value::as_str);
        // 1. 整体快照：已有片段按下标覆盖，多出来的追加。
        if let Some(items) = frame
            .get("v")
            .and_then(|v| v.get("response"))
            .and_then(|response| response.get("fragments"))
            .and_then(Value::as_array)
        {
            for (index, item) in items.iter().enumerate() {
                match self.fragments.get_mut(index) {
                    Some(fragment) => fragment.assign(item),
                    None => self.push_fragment(item),
                }
            }
            return;
        }
        // 2. 追加片段（只认 APPEND：整表覆盖形态尚未在真实流量里出现，不猜）。
        if path == Some(FRAGMENTS_PATH) && is_append(frame) {
            if let Some(items) = frame.get("v").and_then(Value::as_array) {
                for item in items.iter().filter(|item| item.is_object()) {
                    self.push_fragment(item);
                }
            }
            return;
        }
        // 3. 追加 / 覆盖片段内容。
        if let Some(index) = path.and_then(Self::fragment_index) {
            let Some(text) = frame.get("v").and_then(Value::as_str) else {
                return;
            };
            let target = if index < 0 {
                match self.fragments.len().checked_sub(1) {
                    Some(last) => last,
                    None => return,
                }
            } else {
                index as usize
            };
            while self.fragments.len() <= target {
                self.fragments.push(Fragment::default());
            }
            let fragment = &mut self.fragments[target];
            if is_set(frame) {
                fragment.set(text);
            } else {
                fragment.append(text);
            }
            return;
        }
        // 4. 简写正文帧 / 无路径续帧：并入当前（最后一个）片段，保留其类型。
        if path.is_none() || path == Some(CONTENT_PATH) {
            if let Some(text) = frame.get("v").and_then(Value::as_str) {
                self.append_to_last(text);
            }
        }
    }

    /// 取出各片段自上次以来新增的内容（保持片段顺序）；无新增返回空。
    fn take_deltas(&mut self) -> Vec<(String, String)> {
        let mut deltas: Vec<(String, String)> = Vec::with_capacity(self.fragments.len());
        for fragment in &mut self.fragments {
            if let Some(delta) = fragment.take_delta() {
                deltas.push(delta);
            }
        }
        deltas
    }

    /// 追加内容到最后一个片段（保留其类型）；还没有片段时新建正文片段。
    fn append_to_last(&mut self, text: &str) {
        match self.fragments.last_mut() {
            Some(last) => last.append(text),
            None => {
                let mut fragment = Fragment::default();
                fragment.append(text);
                self.fragments.push(fragment);
            }
        }
    }

    /// 追加一个空片段并按片段对象填充。
    fn push_fragment(&mut self, item: &Value) {
        let mut fragment = Fragment::default();
        fragment.assign(item);
        self.fragments.push(fragment);
    }

    /// 从 `response/fragments/<idx>/content` 取片段下标（-1 表示最后一个片段）。
    fn fragment_index(path: &str) -> Option<i64> {
        let rest = path.strip_prefix(FRAGMENT_CONTENT_PREFIX)?;
        let (index, tail) = rest.split_once('/')?;
        if tail != "content" {
            return None;
        }
        index.parse().ok()
    }
}

impl Default for SseFrameProcessor {
    fn default() -> Self {
        Self::new()
    }
}

/// 帧的操作类型是否为追加：缺 `o` 按追加处理。
fn is_append(frame: &Map<String, Value>) -> bool {
    frame
        .get("o")
        .and_then(Value::as_str)
        .is_none_or(|op| op.eq_ignore_ascii_case("APPEND"))
}

/// 帧的操作类型是否为覆盖。
fn is_set(frame: &Map<String, Value>) -> bool {
    frame
        .get("o")
        .and_then(Value::as_str)
        .is_some_and(|op| op.eq_ignore_ascii_case("SET"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response_delta(text: &str) -> Vec<(String, String)> {
        vec![(DeltaChunk::RESPONSE.to_string(), text.to_string())]
    }

    /// 未知片段类型按原值透出，交由调用方判断用途。
    #[test]
    fn unknown_fragment_type_keeps_its_name() {
        let mut processor = SseFrameProcessor::new();
        assert_eq!(
            processor.process_line(
                "data: {\"p\":\"response/fragments\",\"o\":\"APPEND\",\"v\":[{\"type\":\"TOOL\",\"content\":\"x\"}]}"
            ),
            vec![("TOOL".to_string(), "x".to_string())]
        );
    }

    /// 越界下标补空片段：占位片段不算正文，只有目标片段出增量。
    #[test]
    fn out_of_range_index_fills_placeholders() {
        let mut processor = SseFrameProcessor::new();
        assert_eq!(
            processor.process_line(
                "data: {\"p\":\"response/fragments/2/content\",\"o\":\"APPEND\",\"v\":\"x\"}"
            ),
            response_delta("x")
        );
    }

    /// SET 换成更短的多字节内容时游标退回字符边界，不按字节切开内容。
    #[test]
    fn set_frame_rewinds_cursor_onto_char_boundary() {
        let mut processor = SseFrameProcessor::new();
        assert_eq!(
            processor.process_line(
                "data: {\"p\":\"response/fragments\",\"o\":\"APPEND\",\"v\":[{\"type\":\"RESPONSE\",\"content\":\"ab\"}]}"
            ),
            response_delta("ab")
        );
        assert_eq!(
            processor.process_line(
                "data: {\"p\":\"response/fragments/0/content\",\"o\":\"SET\",\"v\":\"中文\"}"
            ),
            response_delta("中文")
        );
    }

    /// SET 覆盖本身不出增量，游标停在新内容末尾，之后的追加只出追加部分。
    #[test]
    fn set_frame_leaves_cursor_at_content_end() {
        let mut processor = SseFrameProcessor::new();
        assert_eq!(
            processor.process_line(
                "data: {\"p\":\"response/fragments\",\"o\":\"APPEND\",\"v\":[{\"type\":\"RESPONSE\",\"content\":\"你好！呀\"}]}"
            ),
            response_delta("你好！呀")
        );
        assert!(processor
            .process_line(
                "data: {\"p\":\"response/fragments/0/content\",\"o\":\"SET\",\"v\":\"重写\"}"
            )
            .is_empty());
        assert_eq!(
            processor.process_line(
                "data: {\"p\":\"response/fragments/0/content\",\"o\":\"APPEND\",\"v\":\"好\"}"
            ),
            response_delta("好")
        );
    }

    /// 分行在处理器内部完成：一行被切在任意位置都只在凑齐整行后出增量。
    #[test]
    fn feed_splits_lines_across_chunks() {
        let mut processor = SseFrameProcessor::new();
        // 半个行：还没有换行符，什么都不放
        assert!(processor.feed("data: {\"p\":\"response/con").is_empty());
        // 补齐这一行并带上下一行的开头：只有凑齐的那行出增量
        assert_eq!(
            processor.feed("tent\",\"v\":\"甲\"}\ndata: {\"p\":\"response/con"),
            response_delta("甲"),
            "只放出完整行，后一行还没换行先留着"
        );
        // 补齐后一行，并一次送入多行（换行符 \r\n 也能认）
        assert_eq!(
            processor.feed("tent\",\"v\":\"乙\"}\r\ndata: {\"v\":\"丙\"}\n"),
            vec![
                (DeltaChunk::RESPONSE.to_string(), "乙".to_string()),
                (DeltaChunk::RESPONSE.to_string(), "丙".to_string()),
            ]
        );
    }

    /// 收尾：缓冲里没有换行符的最后一行由 `finish` 补处理；空行不收。
    #[test]
    fn finish_processes_trailing_line_without_newline() {
        let mut processor = SseFrameProcessor::new();
        assert!(processor
            .feed("data: {\"p\":\"response/content\",\"v\":\"尾\"}")
            .is_empty());
        assert_eq!(processor.finish(), response_delta("尾"));
        // 已经处理过就再没有残留
        assert!(processor.finish().is_empty());

        // 只有空白残留时不产生增量
        let mut processor = SseFrameProcessor::new();
        processor.feed("data: {\"v\":\"x\"}\n  ");
        assert!(processor.finish().is_empty());
    }
}
