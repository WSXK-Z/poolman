//! 聊天流处理器：把 [`SseFrameProcessor`] 的片段增量转成 API 层要的中间增量。
//!
//! 出站格式就是 [`crate::apis::base::parse_delta`] 认的那三种 JSON 包裹（无内容时一个都不发）：
//! - `{"RESPONSE":"…"}` 正文增量；
//! - `{"THINK":"…"}` 思考增量；
//! - `{"FUNCTIONS":[…]}` 工具调用（OpenAI 标准 tool_calls 数组）。
//!
//! 键名取自 [`DeltaChunk`] 的常量：网页协议里 `RESPONSE` / `THINK` 这两种片段名与中间增量的键
//! 本就是同一套词，只在 `apis::base` 定义一次，渠道侧不另立副本。
//!
//! 每段正文增量都先过一遍 [`DsmlStreamParser`] 的**工具检测**，再决定它怎么走：
//! - 检测到「没有工具调用」：这段内容当正文发出去（`{"RESPONSE":…}`）；第一段非空白正文之前的
//!   纯空白不发（标记前后的空行/缩进只是排版，发出去会变成一条只有空白的回答）；
//! - 检测到「可能有工具调用」（缓冲区尾巴正是被 chunk 切开的半个标记）：只扣住这一小段，
//!   等下一段增量来凑齐，前面已经确定不是标记的部分照常当正文发出；
//! - 检测到「有工具调用」：块内内容不再外泄，一直扣住到**整块闭合**（收到 `</calls>`），
//!   这时才把这一块的调用一次性当增量发出——客户端要等工具全部解析完才能拿到，
//!   边解析边发会让它拿到半块。
//!   每条调用只发一次，`index` 是它在整轮里的全局序号（重发会让客户端把参数拼两遍）。
//!
//! 一轮里只要出现工具调用，这条回答就**只**输出工具调用：调用前扣住的正文（见 [`BODY_HOLD_BACK`]）
//! 丢掉，调用之后模型再写的正文也不再转发——正文与工具调用不混在同一条回答里。
//!
//! 不认工具时（`recognize_tools == false`）正文原样透传，DSML 标记也当普通文字。

use serde_json::{json, Map, Value};
use tracing::info;

use crate::apis::base::DeltaChunk;
use crate::models::deepseek_web::dsml_stream_parser::{DsmlCall, DsmlEvent, DsmlStreamParser};
use crate::models::defs::ChunkSink;

/// 片段增量 → API 中间增量的转换器。
///
/// 一路从 [`SseFrameProcessor`] 的 `(片段类型, 内容)` 增量进来，一路把中间增量交给 `sink`；
/// 同时把正文 / 思考 / 工具调用各自累计下来，供非流式调用方直接读取。
pub struct ChatStreamProcessor {
    /// 中间增量出口。
    sink: ChunkSink,
    /// 正文里的工具调用探测器。
    parser: DsmlStreamParser,
    /// 是否在正文里找工具调用；false 时正文（含 DSML 标记）原样透传。
    recognize_tools: bool,
    /// 已放出的正文（不含思考与工具调用标记）。
    text: String,
    /// 是否已经送出过非空白正文。为 false 时纯空白正文不发：模型在标记块前后及块之间的换行、
    /// 缩进只是排版，落到客户端会变成一条只有空白的 assistant 消息，再被当成历史回传。
    body_started: bool,
    /// 是否已经往客户端流过正文：true 之后新到的正文直接流式转发，不再扣在 [`Self::held`] 里。
    body_released: bool,
    /// 扣住还没发出的正文（见 [`BODY_HOLD_BACK`]）。
    held: String,
    /// 本轮是否已经发出过工具调用。发出之后不再输出正文：工具调用与正文不混在同一条回答里。
    calls_emitted: bool,
    /// 已放出的思考。
    reasoning: String,
    /// 已发出去的工具调用（对应哪些已经发过：见 [`Self::emit_new_calls`]）。
    tool_calls: Vec<Value>,
}

/// 扣住多少字还没等到工具调用就把正文放出去。
///
/// 调用工具前模型偶尔会写一句话（实测不超过 81 字），先扣住它，真出现调用就整段丢掉，
/// 让这条回答只留工具调用；扣满这个字数还没等到调用，就按纯文字回答照常往下流。
const BODY_HOLD_BACK: usize = 120;

impl ChatStreamProcessor {
    /// `recognize_tools` 为 false 时正文原样透传，不解析 DSML、不发工具调用。
    pub fn new(sink: ChunkSink, recognize_tools: bool) -> Self {
        Self {
            sink,
            parser: DsmlStreamParser::new(),
            recognize_tools,
            text: String::new(),
            body_started: false,
            body_released: false,
            held: String::new(),
            calls_emitted: false,
            reasoning: String::new(),
            tool_calls: Vec::new(),
        }
    }

    /// 送入一批片段增量（`SseFrameProcessor::feed` / `finish` 的产出）。
    pub fn feed(&mut self, deltas: impl IntoIterator<Item = (String, String)>) {
        for (ftype, delta) in deltas {
            self.feed_fragment(&ftype, &delta);
        }
    }

    /// 流结束：放出残留正文与工具调用。
    /// 结果经 [`Self::text`] / [`Self::reasoning`] / [`Self::tool_calls`] 读取。
    pub fn finish(&mut self) {
        if self.recognize_tools {
            let mut events = Vec::new();
            self.parser.finish(&mut events);
            self.drain_events(events);
            // 兜底：`invoke` 已闭合但整块没等到 `</calls>`（流被截断）时，调用一次都还没发过
            self.emit_new_calls();
        }
        // 没有工具调用：扣住的正文这时整段放出，它就是这条回答的正文
        self.release_held_body();
    }

    /// 正文（不含思考与工具调用标记）。
    pub fn text(&self) -> &str {
        &self.text
    }

    /// 思考原文（不含正文）。
    pub fn reasoning(&self) -> &str {
        &self.reasoning
    }

    /// 解析出的工具调用（无调用时为空）。
    pub fn tool_calls(&self) -> &[Value] {
        &self.tool_calls
    }

    /// 检测器吃掉的工具调用原文（DSML 标记逐字保留），供落盘对照。
    pub fn tool_call_raw(&self) -> &str {
        self.parser.markup()
    }

    /// 按片段类型分流：正文进 DSML 探测器，思考直接回吐，未知类型丢弃。
    fn feed_fragment(&mut self, ftype: &str, delta: &str) {
        match ftype {
            DeltaChunk::RESPONSE => self.push_response(delta),
            // 思考不计入正文，按思考增量回吐（调用方会映射成 reasoning_content）；
            // 思考里的 DSML 不当工具调用，原样留在思考里
            DeltaChunk::THINK => {
                self.reasoning.push_str(delta);
                self.emit_delta(DeltaChunk::THINK, json!(delta));
            }
            _ => {}
        }
    }

    /// 正文增量：先过工具检测，再按检测结果决定这段内容怎么走。
    fn push_response(&mut self, delta: &str) {
        if !self.recognize_tools {
            // 不认工具：整段就是正文，不做任何切分
            self.push_text(delta);
            return;
        }
        let mut events = Vec::new();
        self.parser.feed(delta, &mut events);
        self.drain_events(events);
    }

    /// 探测器事件：正文要转发、整块闭合要放调用；块内其余内容只用于检测，不外泄。
    fn drain_events(&mut self, events: Vec<DsmlEvent>) {
        for event in events {
            match event {
                DsmlEvent::Text(text) => self.push_text(&text),
                // 一块收全就发：客户端要按 index 拼参数，半块发过去它没法用。
                // 就地发还能让出站顺序保持「块前正文 → 调用 → 块后正文」。
                DsmlEvent::CallsEnd => self.emit_new_calls(),
                _ => {}
            }
        }
    }

    /// 把这一块的调用当增量一次性发出：**只发还没发过的调用**，发过的绝不重发。
    ///
    /// OpenAI 流式契约里客户端会把同一个 `index` 的 `function.arguments` 逐片**拼接**，
    /// 所以重发已发过的调用会把参数拼成两遍（客户端拿到 `{…}{…}`，JSON 解析失败、工具调用报错）。
    /// `index` 取调用在整轮里的全局序号，由本层给出；`apis` 层只在缺省时按位置补位。
    fn emit_new_calls(&mut self) {
        let known = self.tool_calls.len();
        let fresh = {
            let calls = self.parser.calls();
            if calls.len() <= known {
                return;
            }
            calls[known..]
                .iter()
                .map(|call| {
                    // 工具调用是链路里最容易「静默消失」的一环，落一行日志便于与客户端收到的对照
                    info!(
                        tool = %call.name,
                        arguments = %serde_json::to_string(&call.arguments).unwrap_or_default(),
                        "检测到工具调用"
                    );
                    tool_call_json(call)
                })
                .collect::<Vec<Value>>()
        };
        self.tool_calls.extend(fresh.iter().cloned());
        let indexed = fresh
            .into_iter()
            .enumerate()
            .map(|(offset, mut call)| {
                call["index"] = json!(known + offset);
                call
            })
            .collect::<Vec<Value>>();
        // 本轮已经是工具调用：正文到此为止，调用前扣住的那段一并丢掉
        self.calls_emitted = true;
        self.drop_held_body();
        self.emit_delta(DeltaChunk::FUNCTIONS, Value::Array(indexed));
    }

    /// 标记外的正文：累计进结果并实时交给调用方。两条抑制规则：
    ///
    /// 1. 第一段非空白正文之前的纯空白不发——那是模型写在标记前后的排版（空行、缩进），
    ///    发给客户端就成了一条只有空白的回答，还会被它当成历史回传回来；
    /// 2. 本轮一旦发出工具调用就不再输出正文——工具调用与正文不混在同一条回答里。
    ///    调用前那一段也先扣住（见 [`BODY_HOLD_BACK`]），真出现调用就整段丢掉。
    fn push_text(&mut self, text: &str) {
        if text.is_empty() || self.calls_emitted {
            return;
        }
        if !self.body_started {
            if text.trim().is_empty() {
                return;
            }
            self.body_started = true;
        }
        self.text.push_str(text);
        if self.body_released {
            self.emit_delta(DeltaChunk::RESPONSE, json!(text));
            return;
        }
        self.held.push_str(text);
        if self.held.chars().count() >= BODY_HOLD_BACK {
            self.release_held_body();
        }
    }

    /// 把扣住的正文放出去（还没放过时）。
    fn release_held_body(&mut self) {
        if self.held.is_empty() {
            return;
        }
        self.body_released = true;
        let held = std::mem::take(&mut self.held);
        self.emit_delta(DeltaChunk::RESPONSE, json!(held));
    }

    /// 丢掉扣住的正文：本轮已经有工具调用，这条回答只留工具调用。
    fn drop_held_body(&mut self) {
        if self.held.is_empty() {
            return;
        }
        let held = std::mem::take(&mut self.held);
        self.text.truncate(self.text.len() - held.len());
    }

    /// 一条中间增量出站：固定单键 `{键: 值}`，键取 [`DeltaChunk`] 的常量。
    fn emit_delta(&mut self, key: &str, value: Value) {
        let mut frame = Map::with_capacity(1);
        frame.insert(key.to_string(), value);
        (self.sink)(Value::Object(frame).to_string());
    }
}

/// DSML 语法树里的调用 → OpenAI 形状的 `tool_calls[]` 元素（`arguments` 是 JSON 字符串）。
fn tool_call_json(call: &DsmlCall) -> Value {
    let arguments = serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".to_string());
    json!({
        "id": format!("call_{}", uuid::Uuid::new_v4().simple()),
        "type": "function",
        "function": { "name": call.name, "arguments": arguments },
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::apis::base::parse_delta;

    /// 收集出站增量。
    fn collector() -> (Arc<Mutex<Vec<String>>>, ChunkSink) {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let writer = seen.clone();
        (
            seen,
            Box::new(move |delta| writer.lock().unwrap().push(delta)),
        )
    }

    /// 一段 DSML 工具调用（全角标记，真实输出的主流形态）。
    fn dsml_call(name: &str, param: &str, value: &str) -> String {
        format!(
            "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"{name}\">\
             <｜｜DSML｜｜ parameter name=\"{param}\" string=\"true\">{value}</｜｜DSML｜｜ parameter>\
             </｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>"
        )
    }

    /// 出站增量必须是 API 层 `parse_delta` 认的格式：喂进去能原样还原思考 / 工具调用。
    #[test]
    fn deltas_round_trip_through_parse_delta() {
        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        processor.feed(vec![
            (DeltaChunk::THINK.to_string(), "想一下".to_string()),
            (
                DeltaChunk::RESPONSE.to_string(),
                dsml_call("read_file", "path", "a.rs"),
            ),
            // 未知片段类型：丢弃，不产出任何增量
            ("TOOL".to_string(), "忽略我".to_string()),
        ]);
        processor.finish();

        let deltas = seen.lock().unwrap().clone();
        assert!(
            deltas.iter().all(|delta| delta.starts_with('{')),
            "每条增量都应是单键 JSON 包裹：{:?}",
            deltas
        );

        let mut content = String::new();
        let mut reasoning = String::new();
        let mut calls: Vec<Value> = Vec::new();
        for delta in &deltas {
            let chunk = parse_delta(delta);
            content.push_str(&chunk.content);
            reasoning.push_str(&chunk.reasoning);
            if let Some(functions) = chunk.functions {
                calls.extend(functions.as_array().cloned().unwrap_or_default());
            }
        }
        assert_eq!(content, "", "DSML 标记不应作为正文流出：{:?}", deltas);
        assert_eq!(reasoning, "想一下");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "read_file");
        assert_eq!(calls[0]["function"]["arguments"], "{\"path\":\"a.rs\"}");

        // 累计结果与非流式调用方读到的保持一致
        assert_eq!(processor.text(), "");
        assert_eq!(processor.reasoning(), "想一下");
        assert_eq!(processor.tool_calls().len(), 1);
    }

    /// 标记被切散在多个增量里：调用只落定一次，混在两侧的正文不发（这条回答只留工具调用）。
    #[test]
    fn dsml_split_across_fragments_still_parses() {
        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        let markup = dsml_call("grep", "pattern", "a|b");
        let mid = markup.len() / 2;
        processor.feed(vec![
            (DeltaChunk::RESPONSE.to_string(), "前".to_string()),
            (DeltaChunk::RESPONSE.to_string(), markup[..mid].to_string()),
            (DeltaChunk::RESPONSE.to_string(), markup[mid..].to_string()),
            (DeltaChunk::RESPONSE.to_string(), "后".to_string()),
        ]);
        processor.finish();

        let content: String = seen
            .lock()
            .unwrap()
            .iter()
            .map(|delta| parse_delta(delta).content)
            .collect();
        assert_eq!(content, "", "有工具调用时正文不发：{:?}", content);
        assert_eq!(processor.text(), "");
        assert_eq!(processor.tool_calls().len(), 1);
        assert_eq!(
            processor.tool_calls()[0]["function"]["name"],
            Value::String("grep".to_string())
        );
        assert_eq!(
            processor.tool_calls()[0]["function"]["arguments"],
            Value::String("{\"pattern\":\"a|b\"}".to_string())
        );
    }

    /// 客户端视角：把 `function.arguments` 解回来，Windows 路径是单层反斜杠
    /// （模型照原样写一遍、按规则转义写两遍，结果都一样）。
    #[test]
    fn client_sees_single_backslash_path() {
        for raw in [r"d:\projects\a.rs", r"d:\\projects\\a.rs"] {
            let (seen, sink) = collector();
            let mut processor = ChatStreamProcessor::new(sink, true);
            processor.feed(vec![(
                DeltaChunk::RESPONSE.to_string(),
                dsml_call("read_file", "path", raw),
            )]);
            processor.finish();

            let deltas = seen.lock().unwrap().clone();
            let calls: Vec<Value> = deltas
                .iter()
                .filter_map(|delta| parse_delta(delta).functions)
                .flat_map(|calls| calls.as_array().cloned().unwrap_or_default())
                .collect();
            let arguments: Value =
                serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
            assert_eq!(
                arguments["path"],
                json!(r"d:\projects\a.rs"),
                "模型原文：{raw}"
            );
        }
    }

    /// 思考与正文各走各的键：同一次 feed 里交错送达也不串味。
    #[test]
    fn think_and_response_use_separate_keys() {
        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, false);
        processor.feed(vec![
            (DeltaChunk::THINK.to_string(), "思".to_string()),
            (DeltaChunk::RESPONSE.to_string(), "答".to_string()),
        ]);
        processor.finish();

        let keys: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|delta| {
                let chunk = parse_delta(delta);
                format!("{}/{}", chunk.content, chunk.reasoning)
            })
            .collect();
        assert_eq!(keys, vec!["/思".to_string(), "答/".to_string()]);
    }

    /// 不认工具时正文原样透传：DSML 标记也当普通文字，不吞内容。
    #[test]
    fn markup_stays_plain_text_without_tool_recognition() {
        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, false);
        let markup = dsml_call("a", "n", "1");
        processor.feed(vec![(DeltaChunk::RESPONSE.to_string(), markup.clone())]);
        processor.finish();

        let content: String = seen
            .lock()
            .unwrap()
            .iter()
            .map(|delta| parse_delta(delta).content)
            .collect();
        assert_eq!(content, markup);
        assert!(processor.tool_calls().is_empty());
    }

    /// 整块闭合前一个调用都不发：客户端要等工具都解析完才拿到。
    #[test]
    fn call_is_emitted_only_after_block_closes() {
        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        let markup = dsml_call("read_file", "path", "a.rs");
        let split = markup.rfind("</｜｜DSML｜｜ calls>").unwrap();
        processor.feed(vec![
            (DeltaChunk::RESPONSE.to_string(), "读一下".to_string()),
            (
                DeltaChunk::RESPONSE.to_string(),
                markup[..split].to_string(),
            ),
        ]);
        // 调用已收全、整块还没闭合：这时不能发
        let before = seen.lock().unwrap().clone();
        assert!(
            before
                .iter()
                .all(|delta| parse_delta(delta).functions.is_none()),
            "整块闭合前不该发工具调用：{:?}",
            before
        );
        assert!(processor.tool_calls().is_empty());

        processor.feed(vec![
            (
                DeltaChunk::RESPONSE.to_string(),
                markup[split..].to_string(),
            ),
            (DeltaChunk::RESPONSE.to_string(), "接着写".to_string()),
        ]);
        processor.finish();

        let deltas = seen.lock().unwrap().clone();
        let functions: Vec<Value> = deltas
            .iter()
            .filter_map(|delta| parse_delta(delta).functions)
            .collect();
        assert_eq!(functions.len(), 1, "一块只发一次：{:?}", deltas);
        assert_eq!(functions[0].as_array().unwrap().len(), 1);
        assert_eq!(functions[0][0]["index"], 0);
        let content: String = deltas
            .iter()
            .map(|delta| parse_delta(delta).content)
            .collect();
        assert_eq!(content, "", "有工具调用时正文不发、标记不外泄");
        assert_eq!(processor.tool_calls().len(), 1);
        assert_eq!(processor.text(), "");
    }

    /// 一块里有多个调用：整块闭合后一次性发出，`index` 依次是 0、1。
    #[test]
    fn whole_block_is_emitted_as_one_delta() {
        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        let block = concat!(
            "<｜｜DSML｜｜ calls>",
            "<｜｜DSML｜｜ invoke name=\"first\"><｜｜DSML｜｜ parameter name=\"a\" string=\"true\">1</｜｜DSML｜｜ parameter></｜｜DSML｜｜ invoke>",
            "<｜｜DSML｜｜ invoke name=\"second\"><｜｜DSML｜｜ parameter name=\"b\" string=\"true\">2</｜｜DSML｜｜ parameter></｜｜DSML｜｜ invoke>",
            "</｜｜DSML｜｜ calls>",
        );
        processor.feed(vec![(DeltaChunk::RESPONSE.to_string(), block.to_string())]);
        processor.finish();

        let functions: Vec<Value> = seen
            .lock()
            .unwrap()
            .iter()
            .filter_map(|delta| parse_delta(delta).functions)
            .collect();
        assert_eq!(functions.len(), 1, "整块应一次发完：{:?}", functions);
        let calls = functions[0].as_array().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["index"], 0);
        assert_eq!(calls[0]["function"]["name"], "first");
        assert_eq!(calls[1]["index"], 1);
        assert_eq!(calls[1]["function"]["name"], "second");
        assert_eq!(processor.tool_calls().len(), 2);
    }

    /// 流被截断（`invoke` 闭合了、`</calls>` 没来）：调用在收尾时补发，块内原文不混进正文。
    #[test]
    fn call_survives_missing_block_close() {
        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        let markup = dsml_call("read_file", "path", "a.rs");
        let without_close = markup.trim_end_matches("</｜｜DSML｜｜ calls>");
        processor.feed(vec![(
            DeltaChunk::RESPONSE.to_string(),
            without_close.to_string(),
        )]);
        assert!(processor.tool_calls().is_empty());

        processor.finish();

        let content: String = seen
            .lock()
            .unwrap()
            .iter()
            .map(|delta| parse_delta(delta).content)
            .collect();
        assert_eq!(content, "", "块内原文不该当正文流出：{:?}", content);
        assert_eq!(processor.tool_calls().len(), 1);
        assert_eq!(
            processor.tool_calls()[0]["function"]["name"],
            Value::String("read_file".to_string())
        );
    }

    /// 两块各自闭合、各发一次：每次只发这块新落定的调用，`index` 是整轮全局序号。
    ///
    /// 客户端按 `index` 把 `function.arguments` 拼接起来，重发已发过的调用会把参数拼成两遍。
    #[test]
    fn later_calls_are_emitted_once_with_global_index() {
        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        processor.feed(vec![(
            DeltaChunk::RESPONSE.to_string(),
            dsml_call("first", "a", "1"),
        )]);
        processor.feed(vec![(
            DeltaChunk::RESPONSE.to_string(),
            "中间还有正文".to_string(),
        )]);
        processor.feed(vec![(
            DeltaChunk::RESPONSE.to_string(),
            dsml_call("second", "b", "2"),
        )]);
        processor.finish();

        let deltas: Vec<Value> = seen
            .lock()
            .unwrap()
            .iter()
            .filter_map(|delta| parse_delta(delta).functions)
            .collect();
        assert_eq!(deltas.len(), 2, "一条调用发一次：{:?}", deltas);
        assert_eq!(deltas[0].as_array().unwrap().len(), 1);
        assert_eq!(deltas[0][0]["index"], 0);
        assert_eq!(deltas[0][0]["function"]["name"], "first");
        assert_eq!(deltas[1].as_array().unwrap().len(), 1);
        assert_eq!(deltas[1][0]["index"], 1);
        assert_eq!(deltas[1][0]["function"]["name"], "second");
        assert_eq!(processor.tool_calls().len(), 2);

        // 客户端视角：按 index 把各增量的 arguments 拼起来，每条都必须还是合法 JSON
        let mut assembled: BTreeMap<u64, (String, String)> = BTreeMap::new();
        for delta in seen.lock().unwrap().iter() {
            let Some(functions) = parse_delta(delta).functions else {
                continue;
            };
            for call in functions.as_array().unwrap() {
                let entry = assembled
                    .entry(call["index"].as_u64().unwrap())
                    .or_insert_with(|| {
                        (
                            call["function"]["name"].as_str().unwrap().to_string(),
                            String::new(),
                        )
                    });
                entry
                    .1
                    .push_str(call["function"]["arguments"].as_str().unwrap());
            }
        }
        let looked: Vec<(u64, String, Value)> = assembled
            .into_iter()
            .map(|(index, (name, arguments))| {
                let parsed = serde_json::from_str(&arguments).unwrap_or_else(|error| {
                    panic!("index {index} 的参数被拼坏了：{}（{:?}）", error, arguments)
                });
                (index, name, parsed)
            })
            .collect();
        assert_eq!(
            looked,
            vec![
                (0, "first".to_string(), json!({"a": "1"})),
                (1, "second".to_string(), json!({"b": "2"})),
            ]
        );
    }

    /// 原文留给落盘：与喂进去的标记逐字一致，块外正文不混进来。
    #[test]
    fn tool_call_raw_is_kept_for_dump() {
        let (_, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        let markup = dsml_call("read_file", "path", "a.rs");
        processor.feed(vec![
            (DeltaChunk::RESPONSE.to_string(), "先说明".to_string()),
            (DeltaChunk::RESPONSE.to_string(), markup.clone()),
            (DeltaChunk::RESPONSE.to_string(), "再补充".to_string()),
        ]);
        processor.finish();

        assert_eq!(processor.tool_call_raw(), markup);
    }

    /// 检测到工具调用要落日志：这是链路里最容易「静默消失」的一环，排查时得看得见名字与参数。
    #[test]
    fn detected_call_is_logged() {
        let logs = log_capture();
        let start = logs.lock().unwrap().len();

        let (_, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        processor.feed(vec![(
            DeltaChunk::RESPONSE.to_string(),
            dsml_call("read_file", "path", "a.rs"),
        )]);
        processor.finish();

        // 只认这一轮之后新写进来的日志，别的用例（并行）的日志不算数
        let logged = String::from_utf8(logs.lock().unwrap()[start..].to_vec()).unwrap();
        assert!(logged.contains("检测到工具调用"), "{}", logged);
        assert!(logged.contains("read_file"), "应打出工具名：{}", logged);
        assert!(logged.contains("a.rs"), "应打出参数：{}", logged);
    }

    /// 断言日志用的全局订阅者。
    ///
    /// callsite 的「是否感兴趣」缓存按进程算，而 `set_default` 只改当前线程：一旦缓存被判成
    /// 不感兴趣，事件在到达线程订阅者之前就被丢掉，日志断言会时好时坏。全局装一次才稳定。
    fn log_capture() -> Arc<Mutex<Vec<u8>>> {
        static CAPTURE: std::sync::OnceLock<Arc<Mutex<Vec<u8>>>> = std::sync::OnceLock::new();
        CAPTURE
            .get_or_init(|| {
                let logs = Arc::new(Mutex::new(Vec::new()));
                let writer = logs.clone();
                let subscriber = tracing_subscriber::fmt()
                    .with_writer(move || LogSink(writer.clone()))
                    .with_ansi(false)
                    .finish();
                let _ = tracing::subscriber::set_global_default(subscriber);
                logs
            })
            .clone()
    }

    /// 日志写入内存缓冲（用例断言日志内容用）。
    struct LogSink(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 第一段非空白正文之前的纯空白不发：模型写在标记前后的排版不该变成空回答。
    #[test]
    fn leading_whitespace_is_not_emitted() {
        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        processor.feed(vec![
            (DeltaChunk::RESPONSE.to_string(), "\n\n  \n".to_string()),
            (
                DeltaChunk::RESPONSE.to_string(),
                dsml_call("view", "path", "a.rs"),
            ),
            (DeltaChunk::RESPONSE.to_string(), "\n \n".to_string()),
        ]);
        processor.finish();

        let deltas = seen.lock().unwrap().clone();
        let content: String = deltas
            .iter()
            .map(|delta| parse_delta(delta).content)
            .collect();
        assert_eq!(content, "", "只有空白时不该发正文：{:?}", deltas);
        assert_eq!(processor.text(), "");
        assert_eq!(processor.tool_calls().len(), 1, "工具调用照常发出");
    }

    /// 正文开始之后的空行原样保留（markdown 靠它分段）。
    #[test]
    fn whitespace_after_body_starts_is_kept() {
        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        processor.feed(vec![
            (DeltaChunk::RESPONSE.to_string(), "第一段".to_string()),
            (DeltaChunk::RESPONSE.to_string(), "\n\n".to_string()),
            (DeltaChunk::RESPONSE.to_string(), "第二段".to_string()),
        ]);
        processor.finish();

        let content: String = seen
            .lock()
            .unwrap()
            .iter()
            .map(|delta| parse_delta(delta).content)
            .collect();
        assert_eq!(content, "第一段\n\n第二段");
        assert_eq!(processor.text(), "第一段\n\n第二段");
    }

    /// 调用工具之前写的短正文会被丢掉：这条回答只输出工具调用。
    #[test]
    fn preamble_before_calls_is_dropped() {
        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        processor.feed(vec![
            (DeltaChunk::RESPONSE.to_string(), "我先看看目录".to_string()),
            (
                DeltaChunk::RESPONSE.to_string(),
                dsml_call("view", "path", "commons/db"),
            ),
        ]);
        processor.finish();

        let deltas = seen.lock().unwrap().clone();
        let content: String = deltas
            .iter()
            .map(|delta| parse_delta(delta).content)
            .collect();
        assert_eq!(content, "", "有工具调用时不发正文：{:?}", deltas);
        assert_eq!(processor.text(), "");
        assert_eq!(processor.tool_calls().len(), 1);
    }

    /// 调用工具之后写的正文同样不发：正文与工具调用不混在同一条回答里。
    #[test]
    fn text_after_calls_is_dropped() {
        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        processor.feed(vec![
            (
                DeltaChunk::RESPONSE.to_string(),
                dsml_call("view", "path", "commons/db"),
            ),
            (
                DeltaChunk::RESPONSE.to_string(),
                "看完了，接下来…".to_string(),
            ),
        ]);
        processor.finish();

        let deltas = seen.lock().unwrap().clone();
        let content: String = deltas
            .iter()
            .map(|delta| parse_delta(delta).content)
            .collect();
        assert_eq!(content, "", "调用之后的正文不发：{:?}", deltas);
        assert_eq!(processor.text(), "");
        assert_eq!(processor.tool_calls().len(), 1);
    }

    /// 纯文字回答：扣到上限就照常往下流，不必等到流结束。
    #[test]
    fn long_body_streams_before_finish() {
        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        let body = "字".repeat(BODY_HOLD_BACK + 10);
        processor.feed(vec![(DeltaChunk::RESPONSE.to_string(), body.clone())]);

        let early: String = seen
            .lock()
            .unwrap()
            .iter()
            .map(|delta| parse_delta(delta).content)
            .collect();
        assert_eq!(early, body, "扣满上限就该放出：{:?}", early);
        assert_eq!(processor.text(), body);
        assert!(processor.tool_calls().is_empty());
    }

    /// 未闭合的块（模型写了一半就断了）：原文当正文回吐，不静默丢弃。
    #[test]
    fn unclosed_call_is_rolled_back_into_text() {
        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        let half = "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"a\">";
        processor.feed(vec![(DeltaChunk::RESPONSE.to_string(), half.to_string())]);
        processor.finish();

        let content: String = seen
            .lock()
            .unwrap()
            .iter()
            .map(|delta| parse_delta(delta).content)
            .collect();
        assert_eq!(content, half);
        assert!(processor.tool_calls().is_empty());
    }
}
