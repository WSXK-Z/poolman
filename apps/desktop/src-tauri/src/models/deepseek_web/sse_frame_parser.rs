use serde_json::Value;

/// 正文片段类型。
const FRAGMENT_RESPONSE: &str = "RESPONSE";

/// 思考片段类型。
const FRAGMENT_THINK: &str = "THINK";

/// 追加片段帧的路径。
const FRAGMENTS_PATH: &str = "response/fragments";

/// 追加片段内容帧的路径前缀。
const FRAGMENT_CONTENT_PREFIX: &str = "response/fragments/";

/// 简写正文帧的路径。
const CONTENT_PATH: &str = "response/content";
/// 网页回答的一个片段：类型 + 累积内容。
struct Fragment {
    ftype: String,
    content: String,
}
// DeepSeek 流式帧解析：`data:` 行里的片段按索引累积，给出各自的新增内容。
///
/// 网页协议的增量有四种形态：
/// 1. 整体快照 `{"v":{"response":{"fragments":[…]}}}`；
/// 2. 追加片段 `{"p":"response/fragments","o":"APPEND","v":[…]}`；
/// 3. 追加片段内容 `{"p":"response/fragments/<idx>/content","o":"APPEND"|"SET","v":"…"}`（`idx` 为 -1 表示最后一个片段）；
/// 4. 无路径续帧 `{"v":"…"}`：续写当前（最后一个）片段，保留其类型——真实流量里正文与思考都主要靠它逐字送达。
///
/// 带路径但不是上面这些的帧（`response/status`、`…/elapsed_secs`、`response` 批量帧等）一律忽略。
pub struct SseFrameParser {
    fragments: Vec<Fragment>,
    /// 各片段已放出的字节数：只把新增部分当作增量。
    sent_length: Vec<usize>,
}

impl SseFrameParser {
    pub fn new() -> Self {
        Self {
            fragments: Vec::new(),
            sent_length: Vec::new(),
        }
    }

    /// 处理一行 SSE，返回本次新增的（片段类型, 内容）。
    fn process_line(&mut self, line: &str) -> Vec<(String, String)> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }
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
        if !value.is_object() {
            return;
        }
        let path = value.get("p").and_then(Value::as_str);
        // 1. 整体快照：已存在的片段按下标覆盖，新增的追加。
        if let Some(fragments) = value
            .get("v")
            .and_then(|v| v.get("response"))
            .and_then(|response| response.get("fragments"))
            .and_then(Value::as_array)
        {
            for (index, fragment) in fragments.iter().enumerate() {
                let (ftype, content) = Self::read_fragment(fragment);
                if index >= self.fragments.len() {
                    self.fragments.push(Fragment { ftype, content });
                } else {
                    self.fragments[index] = Fragment { ftype, content };
                }
            }
            return;
        }
        let op = value
            .get("o")
            .and_then(Value::as_str)
            .unwrap_or("APPEND")
            .to_uppercase();
        // 2. 追加片段（只认 APPEND：整表覆盖形态尚未在真实流量里出现，不猜）。
        if path == Some(FRAGMENTS_PATH) {
            if op == "APPEND" {
                if let Some(items) = value.get("v").and_then(Value::as_array) {
                    for item in items.iter().filter(|item| item.is_object()) {
                        let (ftype, content) = Self::read_fragment(item);
                        self.fragments.push(Fragment { ftype, content });
                    }
                }
            }
            return;
        }
        // 3. 追加 / 覆盖片段内容。
        if let Some(index) = path.and_then(Self::fragment_index) {
            let Some(text) = value.get("v").and_then(Value::as_str) else {
                return;
            };
            let index = if index < 0 {
                self.fragments.len().checked_sub(1)
            } else {
                Some(index as usize)
            };
            let Some(index) = index else {
                return;
            };
            if index >= self.fragments.len() {
                self.fragments.push(Fragment {
                    ftype: String::new(),
                    content: String::new(),
                });
            }
            if self.sent_length.len() < self.fragments.len() {
                self.sent_length.resize(self.fragments.len(), 0);
            }
            if op == "SET" {
                self.fragments[index].content = text.to_string();
                // 覆盖后已放出的长度不能超过新内容，否则后续增量会一直为空
                self.sent_length[index] = self.sent_length[index].min(text.len());
            } else {
                self.fragments[index].content.push_str(text);
            }
            return;
        }
        // 4. 简写正文帧：并入最后一个片段。
        if path == Some(CONTENT_PATH) {
            if let Some(text) = value.get("v").and_then(Value::as_str) {
                self.append_to_last(text);
            }
            return;
        }
        // 5. 无路径的纯文本帧：网页用它续写当前片段（思考片段的续写同样走这里，
        //    所以必须保留原片段类型，不能当成正文）。带路径但不认识的帧一律忽略
        //    （如 `response/status`、`…/elapsed_secs`、`response` 批量帧）。
        if path.is_none() {
            if let Some(text) = value.get("v").and_then(Value::as_str) {
                self.append_to_last(text);
            }
        }
    }

    /// 取自上次以来各片段的新增内容（保片段顺序）；无新增返回空。
    fn take_deltas(&mut self) -> Vec<(String, String)> {
        let mut deltas: Vec<(String, String)> = Vec::new();
        for (index, fragment) in self.fragments.iter().enumerate() {
            if index >= self.sent_length.len() {
                self.sent_length.push(0);
            }
            let sent = self.sent_length[index];
            if fragment.content.len() <= sent {
                continue;
            }
            let delta = fragment.content[sent..].to_string();
            self.sent_length[index] = fragment.content.len();
            let ftype = if fragment.ftype.is_empty() {
                FRAGMENT_RESPONSE.to_string()
            } else {
                fragment.ftype.clone()
            };
            deltas.push((ftype, delta));
        }
        deltas
    }

    /// 追加内容到最后一个片段（保留其类型）；还没有片段时新建正文片段。
    fn append_to_last(&mut self, text: &str) {
        match self.fragments.last_mut() {
            Some(last) => last.content.push_str(text),
            None => self.fragments.push(Fragment {
                ftype: FRAGMENT_RESPONSE.to_string(),
                content: text.to_string(),
            }),
        }
    }

    /// 读一个片段对象 → （类型, 内容）。
    fn read_fragment(value: &Value) -> (String, String) {
        (
            value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            value
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        )
    }

    /// 从 `response/fragments/<idx>/content` 取片段下标（-1 表示最后一个片段）。
    fn fragment_index(path: &str) -> Option<i64> {
        let rest = path.split_once(FRAGMENT_CONTENT_PREFIX)?.1;
        let (index, tail) = rest.split_once('/')?;
        if tail != "content" {
            return None;
        }
        index.parse().ok()
    }
}

