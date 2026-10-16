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
//! - 检测到「有工具调用」：块内内容不再外泄，一直扣住到**整块闭合**（收到 `</｜｜DSML｜｜ calls>`），
//!   这时才把这一块的调用一次性当增量发出——客户端要等工具全部解析完才能拿到，
//!   边解析边发会让它拿到半块。
//!   每条调用只发一次，`index` 是它在整轮里的全局序号（重发会让客户端把参数拼两遍）。
//!
//! 一轮里只要出现工具调用，这条回答就**只**输出工具调用：调用之后的正文不再转发
//! （调用之前已经发出的正文不再收回）——正文与工具调用不混在同一条回答里。
//!
//! 不认工具时（`recognize_tools == false`）正文原样透传，DSML 标记也当普通文字。

use anyhow::Result;
use serde_json::{Map, Value, json};
use tracing::{info, warn};

use crate::apis::base::DeltaChunk;
use crate::models::deepseek_web::{
    CALLS_CLOSE, CALLS_OPEN, INVOKE_CLOSE, INVOKE_OPEN, PARAMETER_CLOSE, PARAMETER_OPEN,
};
use crate::models::defs::ChunkSink;
use crate::utils::{TextRecognizerResult, text_recognizer};

#[derive(Debug, Clone, PartialEq, Eq)]
enum FunctionTag {
    CallsOpen,
    CallOpen,
    CallName(String),
    ParamOpen,
    ParamName(String),
    ParamString(String),
    ParamValue(String),
    ParamClose,
    CallClose,
    CallsClose,
}

/// 片段增量 → API 中间增量的转换器。
///
/// 一路从 [`SseFrameProcessor`] 的 `(片段类型, 内容)` 增量进来，一路把中间增量交给 `sink`；
/// 同时把正文 / 思考 / 工具调用各自累计下来，供非流式调用方直接读取。
pub struct ChatStreamProcessor {
    /// 中间增量出口。
    sink: ChunkSink,
    /// 是否在正文里找工具调用；false 时正文（含 DSML 标记）原样透传。
    recognize_tools: bool,
    /// 已放出的正文（不含思考与工具调用标记）。
    text: String,
    /// 是否已经送出过非空白正文。为 false 时纯空白正文不发：模型在标记块前后及块之间的换行、
    /// 缩进只是排版，落到客户端会变成一条只有空白的 assistant 消息，再被当成历史回传。
    body_started: bool,
    /// 本轮是否已经发出过工具调用。发出之后不再输出正文：工具调用与正文不混在同一条回答里。
    calls_emitted: bool,
    /// 已放出的思考。
    reasoning: String,
    /// 已发出去的工具调用（对应哪些已经发过：见 [`Self::emit_new_calls`]）。
    tool_calls: Vec<Value>,
    /// 请求里的工具参数类型表：模型把类型标错时按它矫正。
    schemas: ToolSchemas,
    /// 原始正文流（全部 RESPONSE 增量的拼接，从不裁剪）：坏块原文从它截取。
    row_response_buf: String,
    cache_response_buf: String,
    /// 当前 DSML 块在 [`Self::row_response_buf`] 里的起点：开块时记下、整块闭合即清。
    block_start: Option<usize>,
    /// 解析失败的坏块原文（`finish` 时落定）：上层据此发起一次规范化修复。
    parse_failure: Option<String>,
    /// 已组装好、还没出站的调用（OpenAI 形状的 `tool_calls[]` 元素）。
    cache_function_buf: Vec<Value>,
    cache_tag_stack: Vec<FunctionTag>,
}

impl ChatStreamProcessor {
    /// `recognize_tools` 为 false 时正文原样透传，不解析 DSML、不发工具调用。
    pub fn new(sink: ChunkSink, recognize_tools: bool) -> Self {
        Self {
            sink,
            recognize_tools,
            text: String::new(),
            body_started: false,
            calls_emitted: false,
            reasoning: String::new(),
            tool_calls: Vec::new(),
            schemas: ToolSchemas::default(),
            row_response_buf: String::new(),    
            cache_response_buf: String::new(),
            cache_function_buf: Vec::new(),
            cache_tag_stack: Vec::new(),
            block_start: None,
            parse_failure: None,
        }
    }

    /// 带上请求里的工具参数类型表：模型把参数类型标错（数字写成字符串等）时按它矫正。
    pub fn with_tool_schemas(mut self, schemas: ToolSchemas) -> Self {
        self.schemas = schemas;
        self
    }

    /// 送入一批片段增量（`SseFrameProcessor::feed` / `finish` 的产出）。
    pub fn feed(&mut self, deltas: impl IntoIterator<Item = (String, String)>) {
        for (ftype, delta) in deltas {
            self.feed_fragment(&ftype, &delta);
        }
    }

    /// 流结束：放出残留正文与工具调用。
    /// 结果经 [`Self::text`] / [`Self::reasoning`] / [`Self::tool_calls`] / [`Self::parse_failure`] 读取。
    pub fn finish(&mut self) {
        if self.recognize_tools {
            // 块没闭合（栈会被 `collect_calls` 取走，先记下来）：块内没写完的标记不是正文，整块丢掉
            let block_open = !self.cache_tag_stack.is_empty();
            let open_stack = block_open.then(|| format!("{:?}", self.cache_tag_stack));
            let calls_before = self.tool_calls.len();
            // 收尾：已闭合的调用落进缓冲，整块没等到 `</｜｜DSML｜｜ calls>` 的也在这里补发
            self.collect_calls();
            self.emit_new_calls();
            match open_stack {
                // 块内的标记不当正文；一个调用都没补出来才算工具调用失败
                Some(stack) => {
                    self.cache_response_buf.clear();
                    if self.tool_calls.len() == calls_before {
                        // 失败时打出接收原文与解析停在哪一步，便于对照模型原文排查
                        warn!("工具调用解析失败，接收到的原始内容：{}", self.row_response_buf);
                        warn!("工具调用解析失败，解析停在：{}", stack);
                        // 坏块原文记下来交给上层：用规范化器提示词再问一轮修复
                        let start = self
                            .block_start
                            .take()
                            .unwrap_or(self.row_response_buf.len());
                        self.parse_failure = Some(self.row_response_buf[start..].to_string());
                    }
                }
                None => {
                    // 块外残留（流末的普通文字）仍算正文
                    let tail = std::mem::take(&mut self.cache_response_buf);
                    self.push_text(&tail);
                }
            }
        }
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

    /// 解析失败的坏块原文（DSML 块开了却没写出任何可解析的调用）；None 表示这轮没有失败。
    pub fn parse_failure(&self) -> Option<&str> {
        self.parse_failure.as_deref()
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
            // 未知片段类型：丢弃
            _ => {}
        }
    }

    /// 正文增量：先过工具检测，再按检测结果决定这段内容怎么走。
    fn push_response(&mut self, delta: &str) {
        self.row_response_buf.push_str(delta);
        if !self.recognize_tools {
            // 不认工具：整段就是正文，不做任何切分
            self.push_text(delta);
            return;
        }
        self.cache_response_buf.push_str(delta);
        self.scan();
    }

    /// 标签栈里记下的调用落进 `cache_function_buf`（OpenAI 形状）：只有 `</｜｜DSML｜｜ invoke>` 已到的调用
    /// 才算完整，写了一半的调用丢掉——不产出半个工具调用。
    fn collect_calls(&mut self) {
        let mut name: Option<String> = None;
        let mut arguments = Map::new();
        let mut param: Option<(String, bool)> = None;
        for tag in std::mem::take(&mut self.cache_tag_stack) {
            match tag {
                FunctionTag::CallName(call_name) => {
                    name = Some(call_name);
                    arguments = Map::new();
                    param = None;
                }
                // 参数没写 `string` 属性时按字符串处理（缺省即字符串）
                FunctionTag::ParamName(key) => param = Some((key, true)),
                FunctionTag::ParamString(flag) => {
                    if let Some((_, as_string)) = param.as_mut() {
                        *as_string = flag != "false";
                    }
                }
                FunctionTag::ParamValue(raw) => {
                    if let Some((key, as_string)) = param.take() {
                        arguments.insert(key, param_value(&raw, as_string));
                    }
                }
                FunctionTag::CallClose => {
                    if let Some(call_name) = name.take() {
                        let call = tool_call_json(&call_name, &arguments, &self.schemas);
                        self.cache_function_buf.push(call);
                    }
                }
                _ => {}
            }
        }
    }

    /// 按标签栈顶逐个消费标记，直到一步都推不动（等下一段增量）。
    ///
    /// 一步要么吃掉一段标记、要么原地不动；栈空时找整块开始标记，栈顶为 `CallsClose`
    /// 说明本块已结束，复位后重新找正文与下一块。
    fn scan(&mut self) {
        loop {
            let before = self.cache_response_buf.len();
            self.scan_step();
            if self.cache_response_buf.len() == before {
                break;
            }
        }
    }

    /// 一步状态推进：栈顶决定这一步该找哪个标记。
    fn scan_step(&mut self) {
        match self.cache_tag_stack.last() {
            None => self.scan_calls_open(),
            Some(&FunctionTag::CallsOpen) => self.scan_call_open(),
            Some(&FunctionTag::CallOpen) => self.scan_call_name(),
            Some(&FunctionTag::CallName(_)) => {
                // 调用名之后：要么是参数，要么这次调用直接闭合（无参调用）
                self.scan_param_open();
                self.scan_call_close();
            }
            Some(&FunctionTag::ParamOpen) => self.scan_param_name(),
            Some(&FunctionTag::ParamName(_)) => self.scan_param_string(),
            Some(&FunctionTag::ParamString(_)) => self.scan_param_close(),
            Some(&FunctionTag::ParamClose) => {
                // 参数闭合后：要么还有下一个参数，要么这次调用闭合
                self.scan_param_open();
                self.scan_call_close();
            }
            Some(&FunctionTag::CallClose) => {
                // 调用闭合后：要么还有下一次调用，要么整块闭合
                self.scan_call_open();
                self.scan_calls_close();
            }
            // 整块闭合：调用落定即发，标签栈复位，正文与下一块重新开始
            Some(&FunctionTag::CallsClose) => {
                self.collect_calls();
                self.emit_new_calls();
                self.cache_tag_stack.clear();
                self.block_start = None;
            }
            Some(&FunctionTag::ParamValue(_)) => {}
        }
    }

    /// 栈空时找整块开始标记 `<｜｜DSML｜｜ calls>`：标记之前是正文，标记本身消费掉，
    /// 之后进块内状态；整段都没有标记时整段就是正文。
    fn scan_calls_open(&mut self) {
        match text_recognizer(CALLS_OPEN, &self.cache_response_buf) {
            Result::Ok(TextRecognizerResult::Finded(start, end)) => {
                // 块从这里开始：记下它在原始正文流里的起点，收尾失败时据此截出坏块原文
                self.block_start =
                    Some(self.row_response_buf.len() - self.cache_response_buf.len() + start);
                self.cache_tag_stack.push(FunctionTag::CallsOpen);
                let prefix = self.cache_response_buf[..start].to_string();
                self.push_text(&prefix);
                self.cache_response_buf = self.cache_response_buf[end..].to_string();
            }
            Result::Ok(TextRecognizerResult::Maybe(_start, _end)) => {
                // 尾巴是半个标记：扣住，等下一段增量补齐
            }
            Result::Ok(TextRecognizerResult::NotFinded) => {
                // `Maybe` 已保证尾部不是半个标记：没有整块开始标记时整段都是正文
                let text = std::mem::take(&mut self.cache_response_buf);
                self.push_text(&text);
            }
            Err(_) => {}
        }
    }

    fn scan_call_open(&mut self) {
        match text_recognizer(INVOKE_OPEN, &self.cache_response_buf) {
            // 前面只允许块内排版：还有别的标记（下一次调用之类）时交给那个标记先走
            Result::Ok(TextRecognizerResult::Finded(start, end))
                if self.cache_response_buf[..start].trim().is_empty() =>
            {
                self.cache_tag_stack.push(FunctionTag::CallOpen);
                self.cache_response_buf = self.cache_response_buf[end..].to_string();
            }
            Result::Ok(TextRecognizerResult::Finded(_start, _end)) => {}
            Result::Ok(TextRecognizerResult::Maybe(_start, _end)) => {
                // 尾巴是半个标记：扣住，等下一段增量补齐
            }
            Result::Ok(TextRecognizerResult::NotFinded) => {}
            Err(_) => {}
        }
    }
    /// 先由 [`text_recognizer`] 找到 `name="` 标记，再在标记之后定位收尾的 `>`，
    /// 回看它与标记之间那段里的引号，取两引号之间的名字：名字连同 `>` 一起消费，
    /// 中间的空白一并丢掉；标记、`>` 或收尾引号还没到齐就先不动缓冲区。
    fn scan_call_name(&mut self) {
        match text_recognizer("name=\"", &self.cache_response_buf) {
            // 前面只允许块内排版（标记与 `name` 之间的空白）
            Result::Ok(TextRecognizerResult::Finded(start, end))
                if self.cache_response_buf[..start].trim().is_empty() =>
            {
                let Some(gt) = self.cache_response_buf[end..].find('>').map(|i| end + i) else {
                    return;
                };
                let attr = &self.cache_response_buf[end..gt];
                let Some(close) = attr.find('"') else {
                    return;
                };
                let name = attr[..close].to_string();
                self.cache_tag_stack.push(FunctionTag::CallName(name));
                self.cache_response_buf = self.cache_response_buf[gt + 1..].to_string();
            }
            Result::Ok(TextRecognizerResult::Finded(_start, _end)) => {}
            Result::Ok(TextRecognizerResult::Maybe(_start, _end)) => {
                // 尾巴是半个标记：扣住，等下一段增量补齐
            }
            Result::Ok(TextRecognizerResult::NotFinded) => {}
            Err(_) => {}
        }
    }
    /// 命中参数标记 `<｜｜DSML｜｜ parameter`：消费标记并压入 `ParamOpen`，
    /// 标记之前的块内语法一并丢掉；标记还没到就先不动缓冲区，等下一段增量补齐。
    fn scan_param_open(&mut self) {
        match text_recognizer(PARAMETER_OPEN, &self.cache_response_buf) {
            // 前面只允许块内排版：还有别的标记（`</｜｜DSML｜｜ invoke>`、下一次调用之类）时交给那个标记先走
            Result::Ok(TextRecognizerResult::Finded(start, end))
                if self.cache_response_buf[..start].trim().is_empty() =>
            {
                self.cache_tag_stack.push(FunctionTag::ParamOpen);
                self.cache_response_buf = self.cache_response_buf[end..].to_string();
            }
            Result::Ok(TextRecognizerResult::Finded(_start, _end)) => {}
            Result::Ok(TextRecognizerResult::Maybe(_start, _end)) => {
                // 尾巴是半个标记：扣住，等下一段增量补齐
            }
            Result::Ok(TextRecognizerResult::NotFinded) => {}
            Err(_) => {}
        }
    }
    /// 命中 `name="` 后取引号之间的参数名，连同收尾引号一起消费并压入 `ParamName`，
    /// 标记之前的块内语法一并丢掉；标记、名字或收尾引号还没到就先不动缓冲区，等下一段增量补齐。
    fn scan_param_name(&mut self) {
        match text_recognizer("name=\"", &self.cache_response_buf) {
            // 前面只允许块内排版（标记与 `name` 之间的空白）
            Result::Ok(TextRecognizerResult::Finded(start, end))
                if self.cache_response_buf[..start].trim().is_empty() =>
            {
                let quoted = &self.cache_response_buf[end..];
                let Some(close) = quoted.find('"') else {
                    return;
                };
                let name = quoted[..close].to_string();
                let consumed = end + close + 1;
                self.cache_tag_stack.push(FunctionTag::ParamName(name));
                self.cache_response_buf = self.cache_response_buf[consumed..].to_string();
            }
            Result::Ok(TextRecognizerResult::Finded(_start, _end)) => {}
            Result::Ok(TextRecognizerResult::Maybe(_start, _end)) => {
                // 尾巴是半个标记：扣住，等下一段增量补齐
            }
            Result::Ok(TextRecognizerResult::NotFinded) => {}
            Err(_) => {}
        }
    }
    /// 命中 `string="` 后取引号之间的标记，再找参数标签收尾的 `>`：标记、内容与 `>` 一并消费，
    /// 压入 `ParamString`，缓冲区停在参数值开头；标记、收尾引号或 `>` 还没到就先不动缓冲区，
    /// 等下一段增量补齐。
    fn scan_param_string(&mut self) {
        match text_recognizer("string=\"", &self.cache_response_buf) {
            // 前面只允许块内排版（参数名引号与 `string` 之间的空白）
            Result::Ok(TextRecognizerResult::Finded(start, end))
                if self.cache_response_buf[..start].trim().is_empty() =>
            {
                let quoted = &self.cache_response_buf[end..];
                let Some(close) = quoted.find('"') else {
                    return;
                };
                let flag = quoted[..close].to_string();
                let Some(gt) = self.cache_response_buf[end + close + 1..].find('>') else {
                    return;
                };
                let consumed = end + close + 1 + gt + 1;
                self.cache_tag_stack.push(FunctionTag::ParamString(flag));
                self.cache_response_buf = self.cache_response_buf[consumed..].to_string();
            }
            Result::Ok(TextRecognizerResult::Finded(_start, _end)) => {}
            Result::Ok(TextRecognizerResult::Maybe(_start, _end)) => {
                // 尾巴是半个标记：扣住，等下一段增量补齐
            }
            Result::Ok(TextRecognizerResult::NotFinded) => {}
            Err(_) => {}
        }
    }
    /// 命中参数闭合标志 `</｜｜DSML｜｜ parameter>`：标志之前的内容就是参数值，
    /// 值与标志一起消费，压入 `ParamValue` 后紧跟一个 `ParamClose`（参数已闭合，
    /// 下一个该找 `<｜｜DSML｜｜ parameter` 或 `</｜｜DSML｜｜ invoke>`）；
    /// 闭合标志还没到就先扣住，等下一段增量补齐。
    fn scan_param_close(&mut self) {
        match text_recognizer(PARAMETER_CLOSE, &self.cache_response_buf) {
            Result::Ok(TextRecognizerResult::Finded(start, end)) => {
                let value = self.cache_response_buf[..start].to_string();
                self.cache_response_buf = self.cache_response_buf[end..].to_string();
                self.cache_tag_stack.push(FunctionTag::ParamValue(value));
                self.cache_tag_stack.push(FunctionTag::ParamClose);
            }
            Result::Ok(TextRecognizerResult::Maybe(_start, _end)) => {
                // 尾巴是半个标记：扣住，等下一段增量补齐
            }
            Result::Ok(TextRecognizerResult::NotFinded) => {}
            Err(_) => {}
        }
    }
    /// 命中调用闭合标志 `</｜｜DSML｜｜ invoke>`，且它确实是下一个标记时：标志与它之前的块内排版
    /// 一并消费，压入 `CallClose`（下一个该找 `<｜｜DSML｜｜ invoke` 或 `</｜｜DSML｜｜ calls>`）；
    /// 标志还没到就先扣住，等下一段增量补齐。
    fn scan_call_close(&mut self) {
        match text_recognizer(INVOKE_CLOSE, &self.cache_response_buf) {
            Result::Ok(TextRecognizerResult::Finded(start, end))
                if self.cache_response_buf[..start].trim().is_empty() =>
            {
                self.cache_tag_stack.push(FunctionTag::CallClose);
                self.cache_response_buf = self.cache_response_buf[end..].to_string();
            }
            Result::Ok(TextRecognizerResult::Finded(_start, _end)) => {
                // 前面还有别的标记（参数标签之类）：交给它先走，别把中间的内容一起吃掉
            }
            Result::Ok(TextRecognizerResult::Maybe(_start, _end)) => {
                // 尾巴是半个标记：扣住，等下一段增量补齐
            }
            Result::Ok(TextRecognizerResult::NotFinded) => {}
            Err(_) => {}
        }
    }
    /// 命中整块闭合标志 `</｜｜DSML｜｜ calls>`，且它确实是下一个标记时：标志与它之前的块内排版
    /// 一并消费，压入 `CallsClose`（本块结束）；标志还没到就先扣住，等下一段增量补齐。
    fn scan_calls_close(&mut self) {
        match text_recognizer(CALLS_CLOSE, &self.cache_response_buf) {
            Result::Ok(TextRecognizerResult::Finded(start, end))
                if self.cache_response_buf[..start].trim().is_empty() =>
            {
                self.cache_tag_stack.push(FunctionTag::CallsClose);
                self.cache_response_buf = self.cache_response_buf[end..].to_string();
            }
            Result::Ok(TextRecognizerResult::Finded(_start, _end)) => {
                // 前面还有别的标记（下一次调用之类）：交给它先走，别把中间的内容一起吃掉
            }
            Result::Ok(TextRecognizerResult::Maybe(_start, _end)) => {
                // 尾巴是半个标记：扣住，等下一段增量补齐
            }
            Result::Ok(TextRecognizerResult::NotFinded) => {}
            Err(_) => {}
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
            if self.cache_function_buf.len() <= known {
                return;
            }
            self.cache_function_buf[known..]
                .iter()
                .map(|call| {
                    // 工具调用是链路里最容易「静默消失」的一环，落一行日志便于与客户端收到的对照
                    info!(
                        tool = %call["function"]["name"].as_str().unwrap_or_default(),
                        arguments = %call["function"]["arguments"].as_str().unwrap_or_default(),
                        "检测到工具调用"
                    );
                    call.clone()
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
        // 本轮已经是工具调用：正文到此为止
        self.calls_emitted = true;
        self.emit_delta(DeltaChunk::FUNCTIONS, Value::Array(indexed));
    }

    /// 标记外的正文：累计进结果并实时交给调用方。两条抑制规则：
    ///
    /// 1. 第一段非空白正文之前的纯空白不发——那是模型写在标记前后的排版（空行、缩进），
    ///    发给客户端就成了一条只有空白的回答，还会被它当成历史回传回来；
    /// 2. 本轮一旦发出工具调用就不再输出正文——工具调用与正文不混在同一条回答里。
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
        self.emit_delta(DeltaChunk::RESPONSE, json!(text));
    }

    /// 一条中间增量出站：固定单键 `{键: 值}`，键取 [`DeltaChunk`] 的常量。
    fn emit_delta(&mut self, key: &str, value: Value) {
        let mut frame = Map::with_capacity(1);
        frame.insert(key.to_string(), value);
        (self.sink)(Value::Object(frame).to_string());
    }
}

/// 一次调用（工具名 + 参数）→ OpenAI 形状的 `tool_calls[]` 元素（`arguments` 是 JSON 字符串）。
fn tool_call_json(name: &str, arguments: &Map<String, Value>, schemas: &ToolSchemas) -> Value {
    // 参数类型以请求里的 JSON Schema 为准：模型常把数字、数组也标成 `string="true"`，
    // 照它的标注落值，客户端会因「期望数字、收到字符串」把整次调用判为非法。
    let mut coerced = Map::with_capacity(arguments.len());
    for (key, value) in arguments {
        coerced.insert(key.clone(), coerce(value.clone(), schemas.type_of(name, key)));
    }
    let arguments = serde_json::to_string(&coerced).unwrap_or_else(|_| "{}".to_string());
    json!({
        "id": format!("call_{}", uuid::Uuid::new_v4().simple()),
        "type": "function",
        "function": { "name": name, "arguments": arguments },
    })
}

/// 参数原文 → JSON 值：`as_string` 为真时只做传输层反转义，否则先按 JSON 解析**原文**
/// （JSON 的转义本就是给 JSON 用的），解析不了再退化成反转义后的字符串，保证不丢内容。
///
/// 两种情形都先去掉「值独占多行」布局带来的首尾换行（`<parameter …>\n值\n</…parameter>`）。
fn param_value(raw: &str, as_string: bool) -> Value {
    let raw = raw
        .strip_prefix("\r\n")
        .or_else(|| raw.strip_prefix('\n'))
        .unwrap_or(raw);
    let raw = raw
        .strip_suffix("\r\n")
        .or_else(|| raw.strip_suffix('\n'))
        .unwrap_or(raw);
    if as_string {
        return Value::String(unescape(raw));
    }
    let trimmed = raw.trim();
    match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => value,
        Err(_) => Value::String(unescape(trimmed)),
    }
}

/// 反转义传输层写法：只有 `\"` → `"` 与 `\\` → `\` 两种，其余 `\x` 原样保留。
///
/// 参数值里绝大多数是 Windows 路径，`\t`、`\n`、`\r` 是路径分隔符而不是转义
/// （`\tests`、`\node_modules`、`\registry.rs`）；按转义处理会把分隔符吃掉一层并塞进控制字符。
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// 请求里带的工具参数类型表：`工具名 → 参数名 → 期望类型`，取自 `tools[].function.parameters`。
///
/// 只用来矫正「字符串 ↔ 结构化值」的标注错误，不做别的猜测：类型表里没有的工具或参数保持原样。
#[derive(Debug, Clone, Default)]
pub struct ToolSchemas {
    tools: std::collections::HashMap<String, std::collections::HashMap<String, ParamKind>>,
}

/// 参数在 JSON Schema 里的类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParamKind {
    String,
    Number,
    Integer,
    Boolean,
    Array,
    Object,
}

impl ToolSchemas {
    /// 从请求的 `tools`（OpenAI 形状）建表；没有工具或形状不认识时得到空表。
    pub fn from_tools(tools: Option<&Value>) -> Self {
        let mut table: std::collections::HashMap<
            String,
            std::collections::HashMap<String, ParamKind>,
        > = std::collections::HashMap::new();
        let Some(list) = tools.and_then(Value::as_array) else {
            return Self { tools: table };
        };
        for tool in list {
            let function = tool.get("function").unwrap_or(tool);
            let Some(name) = function.get("name").and_then(Value::as_str) else {
                continue;
            };
            let Some(properties) = function
                .get("parameters")
                .and_then(|parameters| parameters.get("properties"))
                .and_then(Value::as_object)
            else {
                continue;
            };
            let entry = table.entry(name.to_string()).or_default();
            for (param, schema) in properties {
                if let Some(kind) = schema_kind(schema) {
                    entry.insert(param.clone(), kind);
                }
            }
        }
        Self { tools: table }
    }

    /// 某个工具某个参数的期望类型；不知道时为 None（不做矫正）。
    fn type_of(&self, tool: &str, param: &str) -> Option<ParamKind> {
        self.tools.get(tool)?.get(param).copied()
    }
}

/// JSON Schema 的 `type` → 参数类型；认不出（`anyOf`、只认不出的类型名）返回 None。
fn schema_kind(schema: &Value) -> Option<ParamKind> {
    match schema.get("type")? {
        Value::String(kind) => kind_of(kind),
        // 联合类型（`["number","null"]`）取第一个认得的，`null` 只是可选标记
        Value::Array(kinds) => kinds.iter().filter_map(Value::as_str).find_map(kind_of),
        _ => None,
    }
}

/// JSON Schema 类型名 → 参数类型。
fn kind_of(kind: &str) -> Option<ParamKind> {
    match kind {
        "string" => Some(ParamKind::String),
        "number" => Some(ParamKind::Number),
        "integer" => Some(ParamKind::Integer),
        "boolean" => Some(ParamKind::Boolean),
        "array" => Some(ParamKind::Array),
        "object" => Some(ParamKind::Object),
        _ => None,
    }
}

/// 按期望类型矫正一个参数值：只做「字符串 ↔ 结构化值」的互转，转不成就原样留着。
fn coerce(value: Value, kind: Option<ParamKind>) -> Value {
    let Some(kind) = kind else {
        return value;
    };
    match (kind, value) {
        // 期望字符串、模型给了 JSON 标量：补成字符串
        (ParamKind::String, Value::Number(number)) => Value::String(number.to_string()),
        (ParamKind::String, Value::Bool(flag)) => Value::String(flag.to_string()),
        // 期望数字、模型标成了字符串
        (ParamKind::Number, Value::String(text)) => {
            parse_number(&text).unwrap_or(Value::String(text))
        }
        (ParamKind::Integer, Value::String(text)) => {
            parse_integer(&text).unwrap_or(Value::String(text))
        }
        (ParamKind::Boolean, Value::String(text)) => match text.trim() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::String(text),
        },
        // 期望数组 / 对象、模型把 JSON 写成了字符串
        (ParamKind::Array, Value::String(text)) => match parse_json(&text) {
            Some(parsed @ Value::Array(_)) => parsed,
            _ => Value::String(text),
        },
        (ParamKind::Object, Value::String(text)) => match parse_json(&text) {
            Some(parsed @ Value::Object(_)) => parsed,
            _ => Value::String(text),
        },
        (_, other) => other,
    }
}

/// 字符串转数字：整数优先（`120` 不能变成 `120.0`），小数按浮点。
fn parse_number(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if let Ok(int) = trimmed.parse::<i64>() {
        return Some(Value::Number(int.into()));
    }
    trimmed
        .parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map(Value::Number)
}

/// 字符串转整数：整数值才认（`120`、`120.0`），小数原样留着让客户端报更贴切的错。
fn parse_integer(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if let Ok(int) = trimmed.parse::<i64>() {
        return Some(Value::Number(int.into()));
    }
    let float = trimmed.parse::<f64>().ok()?;
    if float.fract() == 0.0 && float.abs() <= i64::MAX as f64 {
        return Some(Value::Number((float as i64).into()));
    }
    None
}

/// 字符串按 JSON 解析；解析不出返回 None。
fn parse_json(text: &str) -> Option<Value> {
    serde_json::from_str::<Value>(text.trim()).ok()
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

    /// 出站增量里的工具调用（`{"FUNCTIONS":[…]}`）拼成一串。
    fn calls_of(deltas: &[String]) -> Vec<Value> {
        deltas
            .iter()
            .filter_map(|delta| parse_delta(delta).functions)
            .flat_map(|functions| functions.as_array().cloned().unwrap_or_default())
            .collect()
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

    /// 标记被切散在多个增量里：调用只落定一次，调用之前的正文照常发、之后的正文不发。
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
        assert_eq!(content, "前", "调用之前的正文照常发、之后的正文不发：{:?}", content);
        assert_eq!(processor.text(), "前");
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
        assert_eq!(content, "读一下", "调用之前的正文照常发、之后的正文不发");
        assert_eq!(processor.tool_calls().len(), 1);
        assert_eq!(processor.text(), "读一下");
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

    /// 流被截断（`</｜｜DSML｜｜ invoke>` 到了、`</｜｜DSML｜｜ calls>` 没来）：调用在收尾时补发，块内剩下的不当正文。
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

    /// 工具调用写了一半断流也要落日志：打出解析停在哪一步，便于对照模型原文排查。
    #[test]
    fn unclosed_call_logs_parse_position() {
        let logs = log_capture();
        let start = logs.lock().unwrap().len();

        let (_, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        processor.feed(vec![(
            DeltaChunk::RESPONSE.to_string(),
            "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"read_file\">".to_string(),
        )]);
        processor.finish();

        // 只认这一轮之后新写进来的日志，别的用例（并行）的日志不算数
        let logged = String::from_utf8(logs.lock().unwrap()[start..].to_vec()).unwrap();
        assert!(logged.contains("工具调用未写完"), "{}", logged);
        assert!(
            logged.contains("CallName(\"read_file\")"),
            "应打出停在哪一步：{}",
            logged
        );
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

    /// 调用工具之前写的正文照常发出；调用之后的正文不再发：正文与工具调用不混在同一条回答里。
    #[test]
    fn preamble_before_calls_is_streamed() {
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
        assert_eq!(content, "我先看看目录", "调用之前的正文照常发：{:?}", deltas);
        assert_eq!(processor.text(), "我先看看目录");
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

    /// 纯文字回答：正文边到边发，不必等到流结束。
    #[test]
    fn long_body_streams_before_finish() {
        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        let body = "字".repeat(200);
        processor.feed(vec![(DeltaChunk::RESPONSE.to_string(), body.clone())]);

        let early: String = seen
            .lock()
            .unwrap()
            .iter()
            .map(|delta| parse_delta(delta).content)
            .collect();
        assert_eq!(early, body, "正文该即时流出：{:?}", early);
        assert_eq!(processor.text(), body);
        assert!(processor.tool_calls().is_empty());
    }

    /// 未闭合的块（模型写了一半就断了）：块内没写完的标记不当正文外泄，也不产出半个调用；
    /// 坏块原文记录在案，交给上层的规范化修复处理。
    #[test]
    fn unclosed_call_is_dropped() {
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
        assert_eq!(content, "", "写坏的标记不当回答：{:?}", content);
        assert!(processor.tool_calls().is_empty());
        assert!(processor.text().is_empty(), "被截断的块不进正文");
        assert_eq!(
            processor.parse_failure(),
            Some(half),
            "坏块原文要留给修复链路"
        );
    }

    /// 坏块原文从块的开边截起：块前的正文不算失败输入；标记被切成多段也能拼全。
    #[test]
    fn parse_failure_starts_at_block_open_and_survives_split() {
        let (_seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        let broken = "先看看<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"a\">";
        let mid = broken
            .char_indices()
            .nth(broken.chars().count() / 2)
            .map(|(i, _)| i)
            .unwrap_or(broken.len());
        processor.feed(vec![
            (DeltaChunk::RESPONSE.to_string(), broken[..mid].to_string()),
            (DeltaChunk::RESPONSE.to_string(), broken[mid..].to_string()),
        ]);
        processor.finish();

        assert_eq!(processor.text(), "先看看", "块前正文照常发");
        assert_eq!(
            processor.parse_failure(),
            Some(&broken["先看看".len()..]),
            "失败输入从块的开边截起"
        );
    }

    /// 参数类型按请求里的 schema 矫正：模型标成字符串的数字 / 数组 / 布尔要落成 JSON 类型，
    /// 否则客户端按 schema 校验会判整次调用非法。
    #[test]
    fn argument_types_follow_request_schema() {
        let tools = json!([{
            "type": "function",
            "function": {
                "name": "powershell",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" },
                        "initial_wait": { "type": "number" },
                        "shellId": { "type": "string" },
                        "exit_code": { "type": "integer" },
                        "background": { "type": "boolean" },
                        "view_range": { "type": "array" },
                        "note": {}
                    }
                }
            }
        }]);
        let schemas = ToolSchemas::from_tools(Some(&tools));

        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true).with_tool_schemas(schemas);
        let markup = concat!(
            "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"powershell\">",
            "<｜｜DSML｜｜ parameter name=\"command\" string=\"true\">cargo test",
            "</｜｜DSML｜｜ parameter>",
            "<｜｜DSML｜｜ parameter name=\"initial_wait\" string=\"true\">120",
            "</｜｜DSML｜｜ parameter>",
            "<｜｜DSML｜｜ parameter name=\"exit_code\" string=\"true\">7",
            "</｜｜DSML｜｜ parameter>",
            "<｜｜DSML｜｜ parameter name=\"background\" string=\"true\">true",
            "</｜｜DSML｜｜ parameter>",
            "<｜｜DSML｜｜ parameter name=\"view_range\" string=\"true\">[1,60]",
            "</｜｜DSML｜｜ parameter>",
            "<｜｜DSML｜｜ parameter name=\"note\" string=\"true\">120",
            "</｜｜DSML｜｜ parameter>",
            "</｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>",
        );
        processor.feed(vec![(DeltaChunk::RESPONSE.to_string(), markup.to_string())]);
        processor.finish();

        let calls = calls_of(&seen.lock().unwrap().clone());
        assert_eq!(calls.len(), 1);
        let arguments: Value =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(arguments["command"], json!("cargo test"));
        assert_eq!(arguments["initial_wait"], json!(120), "数字要落成数字");
        assert_eq!(arguments["exit_code"], json!(7));
        assert_eq!(arguments["background"], json!(true));
        assert_eq!(arguments["view_range"], json!([1, 60]));
        assert_eq!(arguments["note"], json!("120"), "schema 没写类型就不动它");
    }

    /// 请求里没带的工具（类型表里查不到）不矫正：别拿别的工具的类型乱改值。
    #[test]
    fn unknown_tool_arguments_are_left_alone() {
        let tools = json!([{
            "type": "function",
            "function": {
                "name": "view",
                "parameters": { "properties": { "view_range": { "type": "array" } } }
            }
        }]);
        let schemas = ToolSchemas::from_tools(Some(&tools));

        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true).with_tool_schemas(schemas);
        processor.feed(vec![(
            DeltaChunk::RESPONSE.to_string(),
            dsml_call("unknown_tool", "view_range", "[1,60]"),
        )]);
        processor.finish();

        let calls = calls_of(&seen.lock().unwrap().clone());
        let arguments: Value =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(
            arguments["view_range"],
            json!("[1,60]"),
            "类型表里没有就保持原样"
        );
    }
}
