use serde_json::{Map, Value};

/// 正文片段类型。
pub const FRAGMENT_RESPONSE: &str = "RESPONSE";

/// 思考片段类型。
pub const FRAGMENT_THINK: &str = "THINK";

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
    /// 按片段对象里的 `type` 归类：空值与 `RESPONSE` 同属正文，其余按原值透出。
    fn new(ftype: &str, content: Content) -> Self {
        match ftype {
            "" | FRAGMENT_RESPONSE => Fragment::Response(content),
            FRAGMENT_THINK => Fragment::Think(content),
            other => Fragment::Other(other.to_string(), content),
        }
    }

    /// 片段类型名。
    fn kind(&self) -> &str {
        match self {
            Fragment::Think(_) => FRAGMENT_THINK,
            Fragment::Response(_) => FRAGMENT_RESPONSE,
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
pub struct SseFrameProcessor {
    fragments: Vec<Fragment>,
}

impl SseFrameProcessor {
    pub fn new() -> Self {
        Self {
            fragments: Vec::new(),
        }
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
        vec![(FRAGMENT_RESPONSE.to_string(), text.to_string())]
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
}
