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
//! ## 标记解析：按上下文推断元素，不依赖标签名逐字写对
//!
//! 网页模型写 DSML 时的错法高度集中（实测 83 个坏块里 81 个属于这几类）：
//! 闭标签丢名字（`</｜｜DSML｜｜>`）、把「闭上一个参数 + 开下一个参数」合并成一个闭标签
//! （`</｜｜DSML｜｜ parameter name="path" …>`）、开标签丢掉 `parameter` 这个词
//! （`<｜｜DSML｜｜ description" string="true">`）、多写一个闭合标签、标记少一根竖线。
//! 按标签名精确匹配时，这些错法要么让整块作废，要么把标记吞进参数值（静默错误，更危险）。
//!
//! 因此这里只依赖 DSML 的**结构性信息**：块内位置 + `name=` / `string=` 属性。元素类型由上下文
//! 推断，多余的闭合忽略、合并的标签拆开、缺失的标签名补回。解析出来的仍是标准 DSML 调用，
//! 对外形态不变。
//!
//! 每段正文增量都先过一遍标签扫描，再决定它怎么走：
//! - 扫描不到标签：这段内容当正文发出去（`{"RESPONSE":…}`）；第一段非空白正文之前的
//!   纯空白不发（标记前后的空行/缩进只是排版，发出去会变成一条只有空白的回答）；
//! - 缓冲区尾巴正是被 chunk 切开的半个标签：只扣住这一小段，等下一段增量来凑齐，
//!   前面已经确定不是标签的部分照常当正文发出；
//! - 进入块内：块内内容不再外泄，一直扣住到**整块闭合**，这时才把这一块的调用一次性当增量
//!   发出——客户端要等工具全部解析完才能拿到，边解析边发会让它拿到半块。
//!   每条调用只发一次，`index` 是它在整轮里的全局序号（重发会让客户端把参数拼两遍）。
//!
//! 一轮里只要出现工具调用，这条回答就**只**输出工具调用：调用之后的正文不再转发
//! （调用之前已经发出的正文不再收回）——正文与工具调用不混在同一条回答里。
//!
//! 不认工具时（`recognize_tools == false`）正文原样透传，DSML 标记也当普通文字。

use serde_json::{Map, Value, json};
use tracing::{info, warn};

use crate::apis::base::DeltaChunk;
use crate::models::defs::ChunkSink;

/// DSML 标签的元素类型。缺省（标签名写丢/写错）时为 [`ElementKind::Other`]，由上下文推断。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ElementKind {
    Calls,
    Invoke,
    Parameter,
    Other,
}

/// 一个解析出来的 DSML 标签。
struct Tag {
    /// 标签在缓冲区里的字节长度（含 `<` 与 `>`）。
    len: usize,
    /// 是否闭合标签（`<` 之后紧跟 `/`）。
    closing: bool,
    /// 标签名推断出的元素类型。
    kind: ElementKind,
    /// `name=` 属性的值；缺省时取标签名后误写的裸标识符（`<… description" string="true">`）。
    name: Option<String>,
    /// `string=` 属性的值；缺省即按字符串处理。
    string_flag: Option<String>,
}

/// 在缓冲区里找标签的结果。
enum TagScan {
    /// 找到完整标签，`pos` 是它的起始下标。
    Found { pos: usize, tag: Tag },
    /// 还没找到，但 `pos` 之后可能是被切开的标签，要留着等下一段增量。
    Incomplete { pos: usize },
    /// 缓冲区里没有标签。
    None,
}

/// 解析一个完整标签；不是标签或不完整时返回 None。
fn parse_tag(s: &str) -> Option<Tag> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'<') {
        return None;
    }
    let mut i = skip_ws(s, 1);
    let closing = bytes.get(i) == Some(&b'/');
    if closing {
        i = skip_ws(s, i + 1);
    }
    // 标记：pipe* DSML pipe*，两侧竖线至少一根（全角 `｜` 或半角 `|` 都认）。
    // 放宽到「少一根竖线」的形态：模型写残的 `</｜DSML｜｜ parameter>` 同样要认。
    let mut pipes = 0;
    while let Some(width) = pipe_len(&s[i..]) {
        i += width;
        pipes += 1;
    }
    if !s[i..].starts_with("DSML") {
        return None;
    }
    i += "DSML".len();
    while let Some(width) = pipe_len(&s[i..]) {
        i += width;
        pipes += 1;
    }
    if pipes == 0 {
        return None;
    }
    i = skip_ws(s, i);
    // 标签名（可能被写丢）
    let name_start = i;
    while let Some(byte) = bytes.get(i) {
        if byte.is_ascii_alphanumeric() || *byte == b'_' {
            i += 1;
        } else {
            break;
        }
    }
    let tag_word = &s[name_start..i];
    let kind = match tag_word.to_ascii_lowercase().as_str() {
        "calls" => ElementKind::Calls,
        "invoke" => ElementKind::Invoke,
        "parameter" => ElementKind::Parameter,
        _ => ElementKind::Other,
    };
    // 属性区到 `>` 结束；中途再出现 `<` 说明这不是一个标签
    let attrs_start = i;
    let mut attr_end = None;
    while let Some(byte) = bytes.get(i) {
        match byte {
            b'>' => {
                attr_end = Some(i);
                break;
            }
            b'<' => return None,
            _ => i += 1,
        }
    }
    let attr_end = attr_end?;
    let (mut name, string_flag) = parse_attrs(&s[attrs_start..attr_end]);
    // 标签名位置写成了参数名（`<｜｜DSML｜｜ description" string="true">`）：把它当参数名用
    if name.is_none() && kind == ElementKind::Other && !tag_word.is_empty() {
        name = Some(tag_word.to_string());
    }
    Some(Tag {
        len: attr_end + 1,
        closing,
        kind,
        name,
        string_flag,
    })
}

/// 解析属性区：`name="x"` / `string="true"`，容忍引号缺失、`name=` 整个写丢的形态。
fn parse_attrs(attrs: &str) -> (Option<String>, Option<String>) {
    let bytes = attrs.as_bytes();
    let mut i = 0;
    let mut name = None;
    let mut string_flag = None;
    let mut implicit_name = None;
    loop {
        i = skip_ws(attrs, i);
        if i >= bytes.len() {
            break;
        }
        let key_start = i;
        while let Some(byte) = bytes.get(i) {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.') {
                i += 1;
            } else {
                break;
            }
        }
        if i == key_start {
            i += 1;
            continue;
        }
        let key = &attrs[key_start..i];
        let after_key = skip_ws(attrs, i);
        if bytes.get(after_key) == Some(&b'=') {
            i = skip_ws(attrs, after_key + 1);
            let (value, next) = read_attr_value(attrs, i);
            i = next;
            match key {
                "name" => name = Some(value),
                "string" => string_flag = Some(value),
                _ => {}
            }
        } else {
            // 没有 `=`：这段标识符就是被写丢的属性名，模型把它写成了 `<… description" string="true">`
            if implicit_name.is_none() {
                implicit_name = Some(key.to_string());
            }
            i = after_key;
            if matches!(bytes.get(i), Some(b'"') | Some(b'\'')) {
                i += 1;
            }
        }
    }
    (name.or(implicit_name), string_flag)
}

/// 读一个属性值：带引号读到对应引号，不带引号读到空白或 `>`。
fn read_attr_value(s: &str, i: usize) -> (String, usize) {
    let bytes = s.as_bytes();
    match bytes.get(i) {
        Some(&quote) if quote == b'"' || quote == b'\'' => {
            let mut j = i + 1;
            let mut value = String::new();
            while let Some(&byte) = bytes.get(j) {
                if byte == b'\\' {
                    if let Some(&escaped) = bytes.get(j + 1) {
                        if escaped.is_ascii() {
                            value.push(escaped as char);
                        }
                        j += 2;
                        continue;
                    }
                }
                if byte == quote {
                    j += 1;
                    break;
                }
                let ch = s[j..].chars().next().unwrap_or('\u{0}');
                value.push(ch);
                j += ch.len_utf8();
            }
            (value, j)
        }
        _ => {
            let mut j = i;
            while let Some(&byte) = bytes.get(j) {
                if byte.is_ascii_whitespace() || byte == b'>' {
                    break;
                }
                j += 1;
            }
            (s[i..j].to_string(), j)
        }
    }
}

/// 竖线标记的字节宽度：全角 `｜` 三个字节，半角 `|` 一个字节。
fn pipe_len(s: &str) -> Option<usize> {
    if s.starts_with('｜') {
        Some('｜'.len_utf8())
    } else if s.starts_with('|') {
        Some(1)
    } else {
        None
    }
}

/// 跳过 ASCII 空白，返回首个非空白位置。
fn skip_ws(s: &str, mut i: usize) -> usize {
    let bytes = s.as_bytes();
    while let Some(byte) = bytes.get(i) {
        if byte.is_ascii_whitespace() {
            i += 1;
        } else {
            break;
        }
    }
    i
}

/// 在缓冲区里找第一个标签；`pos` 之前的部分可以安全当正文/参数值消费。
fn find_tag(buf: &str) -> TagScan {
    let mut from = 0;
    while let Some(relative) = buf[from..].find('<') {
        let pos = from + relative;
        let rest = &buf[pos..];
        if let Some(tag) = parse_tag(rest) {
            return TagScan::Found { pos, tag };
        }
        // 可能是被切开的标签：只有「像 DSML 开头且还没出现 `>`」才留着等
        if could_be_tag(rest) && !rest.contains('>') {
            return TagScan::Incomplete { pos };
        }
        from = pos + 1;
    }
    TagScan::None
}

/// 这段文本（以 `<` 开头）是否可能是一个还没写完的 DSML 标签。
fn could_be_tag(s: &str) -> bool {
    if !s.starts_with('<') {
        return false;
    }
    let bytes = s.as_bytes();
    let mut i = skip_ws(s, 1);
    if bytes.get(i) == Some(&b'/') {
        i = skip_ws(s, i + 1);
    }
    let mut j = i;
    while let Some(width) = pipe_len(&s[j..]) {
        j += width;
    }
    let rest = &s[j..];
    if rest.len() < "DSML".len() {
        return "DSML".starts_with(rest);
    }
    rest.starts_with("DSML")
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
    /// 请求里的工具参数类型表：模型把类型标错时按它矫正，并按它校验必填参数。
    schemas: ToolSchemas,
    /// 原始正文流（全部 RESPONSE 增量的拼接，从不裁剪）：坏块原文从它截取。
    row_response_buf: String,
    /// 跨 chunk 未消费完的尾数据。
    cache_response_buf: String,
    /// 当前 DSML 块在 [`Self::row_response_buf`] 里的起点：开块时记下、整块闭合即清。
    block_start: Option<usize>,
    /// 解析失败的坏块原文（`finish` 时落定）：上层据此发起一次规范化修复。
    parse_failure: Option<String>,
    /// 解析失败的原因（给修复提示词用）。
    failure_reason: Option<String>,
    /// 已组装好、还没出站的调用（OpenAI 形状的 `tool_calls[]` 元素）。
    cache_function_buf: Vec<Value>,
    /// 当前块里已闭合、还没出站的调用。
    block_calls: Vec<Value>,
    /// 解析位置：块外 / 块内 / 调用内 / 参数值内。
    state: ParserState,
    /// 正在收集的调用。
    call: Option<CallAcc>,
    /// 正在收集的参数。
    param: Option<ParamAcc>,
    /// 遇到无法判定的结构（例如调用没闭合又出现新的 invoke）：整块交给修复链路，不猜。
    broken: bool,
}

/// 解析位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParserState {
    Outside,
    InCalls,
    InCall,
    InParam,
}

/// 正在累积的调用。
struct CallAcc {
    name: String,
    arguments: Map<String, Value>,
}

/// 正在收集的参数。
struct ParamAcc {
    name: String,
    as_string: bool,
    value: String,
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
            block_start: None,
            parse_failure: None,
            failure_reason: None,
            cache_function_buf: Vec::new(),
            block_calls: Vec::new(),
            state: ParserState::Outside,
            call: None,
            param: None,
            broken: false,
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
        if !self.recognize_tools {
            let tail = std::mem::take(&mut self.cache_response_buf);
            self.push_text(&tail);
            return;
        }
        if self.state != ParserState::Outside || self.broken {
            // 收尾：已闭合的调用照样发，半个调用丢掉（不产出半块）。
            if !self.broken && !self.block_calls.is_empty() {
                self.cache_function_buf
                    .extend(std::mem::take(&mut self.block_calls));
                self.emit_new_calls();
            } else {
                // 块内没写出任何可解析的调用：整块当坏块，交给修复链路
                self.record_failure();
            }
            self.state = ParserState::Outside;
            self.call = None;
            self.param = None;
            self.block_start = None;
            self.broken = false;
        } else {
            let tail = std::mem::take(&mut self.cache_response_buf);
            self.push_text(&tail);
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

    /// 解析失败的原因（校验不通过 / 结构无法判定）；给修复提示词用。
    pub fn failure_reason(&self) -> Option<&str> {
        self.failure_reason.as_deref()
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

    /// 正文增量：先过标签扫描，再按结果决定这段内容怎么走。
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

    /// 反复取下一个标签，直到缓冲区推不动（等下一段增量）。
    fn scan(&mut self) {
        loop {
            if self.broken {
                break;
            }
            match find_tag(&self.cache_response_buf) {
                TagScan::Found { pos, tag } => {
                    self.take_prefix(pos);
                    let absolute = self.row_response_buf.len() - self.cache_response_buf.len();
                    let raw = self.cache_response_buf[..tag.len].to_string();
                    self.cache_response_buf.drain(..tag.len);
                    self.dispatch(&tag, &raw, absolute);
                }
                TagScan::Incomplete { pos } => {
                    self.take_prefix(pos);
                    break;
                }
                TagScan::None => {
                    let rest = std::mem::take(&mut self.cache_response_buf);
                    self.consume_text(rest);
                    break;
                }
            }
        }
    }

    /// 把标签之前的那一段按当前状态消费掉（块外是正文，参数值内并入值，块内排版丢弃）。
    fn take_prefix(&mut self, pos: usize) {
        if pos == 0 {
            return;
        }
        let prefix = self.cache_response_buf[..pos].to_string();
        self.cache_response_buf.drain(..pos);
        self.consume_text(prefix);
    }

    /// 一段不含标签的原文该往哪走。
    fn consume_text(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        match self.state {
            ParserState::Outside => self.push_text(&text),
            ParserState::InParam => {
                if let Some(param) = self.param.as_mut() {
                    param.value.push_str(&text);
                }
            }
            // 块内没有标签的换行、缩进只是排版
            _ => {}
        }
    }

    /// 认领一个标签：按当前状态推进，必要时补齐被模型写丢的结构。
    fn dispatch(&mut self, tag: &Tag, raw: &str, absolute: usize) {
        match self.state {
            ParserState::Outside => {
                // 块外只认「无属性的整块开标记」：正文里普通尖括号不当工具调用
                let opens_block = !tag.closing
                    && tag.name.is_none()
                    && matches!(tag.kind, ElementKind::Calls | ElementKind::Other);
                if opens_block {
                    self.block_start = Some(absolute);
                    self.state = ParserState::InCalls;
                    self.broken = false;
                    self.failure_reason = None;
                } else {
                    self.push_text(raw);
                }
            }
            ParserState::InCalls => {
                if tag.closing {
                    // 整块闭合；其它多余的闭合标签（参数写完后多补的一行）直接忽略
                    if tag.kind == ElementKind::Calls
                        || (tag.kind == ElementKind::Other && tag.name.is_none())
                    {
                        self.close_block();
                    }
                } else if let Some(name) = tag.name.clone() {
                    // 块内出现带 name 的开标签就是一次调用：`invoke` 或被写丢的 `<… name="x">`。
                    // 孤立 `parameter`（写丢了 invoke）不当工具名，直接当排版忽略。
                    if tag.kind != ElementKind::Parameter {
                        self.start_call(name);
                    }
                }
            }
            ParserState::InCall => {
                if tag.closing {
                    match tag.kind {
                        ElementKind::Invoke => self.close_call(),
                        ElementKind::Calls => {
                            self.close_call();
                            self.close_block();
                        }
                        // 合并标签落在「上一个参数已闭合」之后（实测形态）：
                        // `</… parameter name="workdir" …>` 的 name 就是下一个参数名，按开标签处理
                        ElementKind::Parameter if tag.name.is_some() => {
                            self.start_param(tag.name.clone(), tag.string_flag.clone())
                        }
                        // `</｜｜DSML｜｜>`：写丢名字的调用闭合
                        ElementKind::Other if tag.name.is_none() => self.close_call(),
                        // 参数闭合写了两遍之类：多余，忽略
                        _ => {}
                    }
                } else {
                    match tag.kind {
                        ElementKind::Parameter => {
                            self.start_param(tag.name.clone(), tag.string_flag.clone())
                        }
                        // 开标签写丢 `parameter`：只剩属性区
                        ElementKind::Other if tag.name.is_some() => {
                            self.start_param(tag.name.clone(), tag.string_flag.clone())
                        }
                        // 闭合标签漏写 `/`
                        ElementKind::Invoke if tag.name.is_none() => self.close_call(),
                        // 调用还没闭合又出现新的 invoke：结构无法判定，整块交给修复
                        ElementKind::Invoke => self.break_block("调用未闭合就出现了新的 invoke"),
                        _ => {}
                    }
                }
            }
            ParserState::InParam => {
                if tag.closing {
                    match tag.kind {
                        ElementKind::Parameter => {
                            if tag.name.is_some() {
                                // 合并标签：先把当前参数落定，再用它的属性开下一个参数
                                self.finish_param();
                                self.start_param(tag.name.clone(), tag.string_flag.clone());
                            } else {
                                self.finish_param();
                            }
                        }
                        ElementKind::Calls => {
                            self.finish_param();
                            self.close_call();
                            self.close_block();
                        }
                        ElementKind::Invoke => {
                            self.finish_param();
                            self.close_call();
                        }
                        // `</｜｜DSML｜｜>`：写丢名字的参数闭合
                        ElementKind::Other if tag.name.is_some() => {
                            self.finish_param();
                            self.start_param(tag.name.clone(), tag.string_flag.clone());
                        }
                        ElementKind::Other => self.finish_param(),
                    }
                } else {
                    match tag.kind {
                        // 漏写参数闭合标签：下一个参数开标签视为当前参数到此为止
                        ElementKind::Parameter => {
                            self.finish_param();
                            self.start_param(tag.name.clone(), tag.string_flag.clone());
                        }
                        ElementKind::Other if tag.name.is_some() => {
                            self.finish_param();
                            self.start_param(tag.name.clone(), tag.string_flag.clone());
                        }
                        // 参数没闭合就出现新的 invoke：结构无法判定
                        ElementKind::Invoke if tag.name.is_some() => {
                            self.break_block("参数未闭合就出现了新的 invoke")
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// 开始收集一次调用。
    fn start_call(&mut self, name: String) {
        self.call = Some(CallAcc {
            name,
            arguments: Map::new(),
        });
        self.param = None;
        self.state = ParserState::InCall;
    }

    /// 开始收集一个参数；`string` 属性缺省按字符串处理。
    fn start_param(&mut self, name: Option<String>, string_flag: Option<String>) {
        let name = name.unwrap_or_default();
        self.param = Some(ParamAcc {
            name,
            as_string: string_flag.as_deref() != Some("false"),
            value: String::new(),
        });
        self.state = ParserState::InParam;
    }

    /// 落定当前参数。
    fn finish_param(&mut self) {
        if let Some(param) = self.param.take() {
            if let Some(call) = self.call.as_mut() {
                call.arguments
                    .insert(param.name, param_value(&param.value, param.as_string));
            }
        }
        if self.state == ParserState::InParam {
            self.state = ParserState::InCall;
        }
    }

    /// 一次调用闭合：按 schema 校验后落进当前块；不合格就整块判失败，绝不把坏调用发出去。
    fn close_call(&mut self) {
        self.finish_param();
        if let Some(call) = self.call.take() {
            match self.build_call(&call.name, &call.arguments) {
                Ok(value) => self.block_calls.push(value),
                Err(reason) => {
                    self.break_block(&reason);
                    return;
                }
            }
        }
        self.state = ParserState::InCalls;
    }

    /// 组装并校验一次调用。
    fn build_call(
        &self,
        name: &str,
        arguments: &Map<String, Value>,
    ) -> std::result::Result<Value, String> {
        if name.trim().is_empty() {
            return Err("调用没有工具名".to_string());
        }
        // 参数值里混进标记残片：模型把标签写进了值里（实测占成功调用的 10%），必须重写而不是照发
        for (key, value) in arguments {
            let text = match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            if contains_markup_residue(&text) {
                return Err(format!("参数 {key} 的值里混进了 DSML 标记"));
            }
        }
        self.schemas.check(name, arguments)?;
        Ok(tool_call_json(name, arguments, &self.schemas))
    }

    /// 整块闭合：本块已校验的调用一次性发出，位置复位。
    fn close_block(&mut self) {
        self.finish_param();
        if !self.block_calls.is_empty() {
            self.cache_function_buf
                .extend(std::mem::take(&mut self.block_calls));
            self.emit_new_calls();
        }
        self.call = None;
        self.param = None;
        self.block_start = None;
        self.state = ParserState::Outside;
    }

    /// 结构无法判定：本块作废，交给修复链路（不猜着拼调用）。
    fn break_block(&mut self, reason: &str) {
        self.broken = true;
        self.failure_reason = Some(reason.to_string());
    }

    /// 落定失败：记下坏块原文与解析位置，供上层修复。
    fn record_failure(&mut self) {
        let start = self
            .block_start
            .take()
            .unwrap_or(self.row_response_buf.len());
        if start <= self.row_response_buf.len() {
            self.parse_failure = Some(self.row_response_buf[start..].to_string());
        }
        // 失败时打出接收原文与解析停在哪一步，便于对照模型原文排查
        warn!(
            "工具调用解析失败，接收到的原始内容：{}",
            self.row_response_buf
        );
        warn!("工具调用解析失败，解析停在：{}", self.state_label());
        if let Some(reason) = &self.failure_reason {
            warn!("工具调用解析失败，原因：{}", reason);
        }
    }

    /// 解析位置的可读描述（日志与排查用；`CallName("x")` 与历史日志格式一致）。
    fn state_label(&self) -> String {
        match self.state {
            ParserState::Outside => "Outside".to_string(),
            ParserState::InCalls => "CallsOpen".to_string(),
            ParserState::InCall => format!(
                "CallName({:?})",
                self.call
                    .as_ref()
                    .map(|call| call.name.as_str())
                    .unwrap_or("")
            ),
            ParserState::InParam => format!(
                "ParamName({:?})",
                self.param
                    .as_ref()
                    .map(|param| param.name.as_str())
                    .unwrap_or("")
            ),
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

/// 参数值里是否混进了 DSML 标记残片。
///
/// 只看标记形态（`…DSML｜｜` / `｜｜DSML` / `||DSML` / `<…DSML`），不按裸 `DSML` 判断：
/// 文件名、文档里出现 `DSML.md` 这种字样不该被判成坏值。
fn contains_markup_residue(text: &str) -> bool {
    const NEEDLES: [&str; 5] = ["｜｜DSML", "DSML｜｜", "||DSML", "DSML||", "<｜｜DSML"];
    NEEDLES.iter().any(|needle| text.contains(needle))
}

/// 一次调用（工具名 + 参数）→ OpenAI 形状的 `tool_calls[]` 元素（`arguments` 是 JSON 字符串）。
fn tool_call_json(name: &str, arguments: &Map<String, Value>, schemas: &ToolSchemas) -> Value {
    // 参数类型以请求里的 JSON Schema 为准：模型常把数字、数组也标成 `string="true"`，
    // 照它的标注落值，客户端会因「期望数字、收到字符串」把整次调用判为非法。
    let mut coerced = Map::with_capacity(arguments.len());
    for (key, value) in arguments {
        coerced.insert(
            key.clone(),
            coerce(value.clone(), schemas.type_of(name, key)),
        );
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

/// 请求里带的工具参数表：取自 `tools[].function.parameters`。
///
/// 用途有两层：矫正「字符串 ↔ 结构化值」的标注错误；在调用出站前校验「工具已声明、必填参数齐全」，
/// 不合格的调用绝不发给客户端（客户端一定会判它非法，还会静默改错文件）。
#[derive(Debug, Clone, Default)]
pub struct ToolSchemas {
    tools: std::collections::HashMap<String, ToolSchema>,
}

/// 单个工具的参数表：参数类型 + 必填项。
#[derive(Debug, Clone, Default)]
struct ToolSchema {
    params: std::collections::HashMap<String, ParamKind>,
    required: Vec<String>,
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
        let mut table: std::collections::HashMap<String, ToolSchema> =
            std::collections::HashMap::new();
        let Some(list) = tools.and_then(Value::as_array) else {
            return Self { tools: table };
        };
        for tool in list {
            let function = tool.get("function").unwrap_or(tool);
            let Some(name) = function.get("name").and_then(Value::as_str) else {
                continue;
            };
            // 没有 parameters 的工具也要登记：否则「工具已声明但无参数」会被当成未声明而误判
            let entry = table.entry(name.to_string()).or_default();
            let Some(parameters) = function.get("parameters") else {
                continue;
            };
            if let Some(properties) = parameters.get("properties").and_then(Value::as_object) {
                for (param, schema) in properties {
                    if let Some(kind) = schema_kind(schema) {
                        entry.params.insert(param.clone(), kind);
                    }
                }
            }
            if let Some(required) = parameters.get("required").and_then(Value::as_array) {
                entry.required = required
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
            }
        }
        Self { tools: table }
    }

    /// 某个工具某个参数的期望类型；不知道时为 None（不做矫正）。
    fn type_of(&self, tool: &str, param: &str) -> Option<ParamKind> {
        self.tools.get(tool)?.params.get(param).copied()
    }

    /// 校验一次调用：只拦「客户端一定会判非法」的两种情形——工具未声明、缺必填参数。
    ///
    /// 参数类型不在这里拒绝：`coerce` 已按 schema 矫正过，矫不过来是模型给的值本身有问题，
    /// 交给客户端报更贴切的错（这里拒绝反而会把它升级成「整轮失败」）。
    /// 空表（请求没带可用工具）时不做校验。
    fn check(&self, name: &str, arguments: &Map<String, Value>) -> std::result::Result<(), String> {
        if self.tools.is_empty() {
            return Ok(());
        }
        let schema = self
            .tools
            .get(name)
            .ok_or_else(|| format!("工具 {name} 未在请求的 tools 里声明"))?;
        let missing = schema
            .required
            .iter()
            .filter(|key| !arguments.contains_key(*key))
            .map(String::as_str)
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(format!("工具 {name} 缺必填参数：{}", missing.join("、")));
        }
        Ok(())
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
        assert_eq!(
            content, "前",
            "调用之前的正文照常发、之后的正文不发：{:?}",
            content
        );
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

    /// 工具调用写了一半断流也要落日志：打出接收原文与解析停在哪一步，便于对照模型原文排查。
    #[test]
    fn unclosed_call_logs_parse_position() {
        let logs = log_capture();
        let start = logs.lock().unwrap().len();

        let (_, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        let half = "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"read_file\">";
        processor.feed(vec![(DeltaChunk::RESPONSE.to_string(), half.to_string())]);
        processor.finish();

        // 只认这一轮之后新写进来的日志，别的用例（并行）的日志不算数
        let logged = String::from_utf8(logs.lock().unwrap()[start..].to_vec()).unwrap();
        assert!(logged.contains("工具调用解析失败"), "{}", logged);
        assert!(logged.contains(half), "应打出接收到的原文：{}", logged);
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
        assert_eq!(
            content, "我先看看目录",
            "调用之前的正文照常发：{:?}",
            deltas
        );
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

    /// 闭标签漏写 `/`（写成 `<｜｜DSML｜｜ invoke>`）：按闭合处理，已收好的参数照常落进这次调用，
    /// 不因为少一个字符把整块作废。
    #[test]
    fn slash_less_call_close_still_completes_the_call() {
        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        // 与真实输出同形：块内多行、两个参数，最后的闭标签漏了斜杠
        let markup = concat!(
            "<｜｜DSML｜｜ calls>\n",
            "<｜｜DSML｜｜ invoke name=\"powershell\">\n",
            "<｜｜DSML｜｜ parameter name=\"command\" string=\"true\">Get-Item 'd:\\\\a.exe' | Format-List</｜｜DSML｜｜ parameter>\n",
            "<｜｜DSML｜｜ parameter name=\"description\" string=\"true\">Verify build artifacts exist</｜｜DSML｜｜ parameter>\n",
            "<｜｜DSML｜｜ invoke>\n",
            "</｜｜DSML｜｜ calls>\n",
        );
        processor.feed(vec![(DeltaChunk::RESPONSE.to_string(), markup.to_string())]);
        processor.finish();

        let calls = calls_of(&seen.lock().unwrap());
        assert_eq!(calls.len(), 1, "漏写斜杠不该让整块作废：{:?}", calls);
        assert_eq!(calls[0]["function"]["name"], "powershell");
        let arguments: Value =
            parse_json(calls[0]["function"]["arguments"].as_str().unwrap_or("{}"))
                .expect("参数应是 JSON 对象");
        assert_eq!(arguments["command"], "Get-Item 'd:\\a.exe' | Format-List");
        assert_eq!(arguments["description"], "Verify build artifacts exist");
        assert!(processor.parse_failure().is_none(), "不该判成解析失败");
    }

    /// 带 `name` 的同形标签是真正的开标签：不当闭合处理。
    /// （这里前一次调用确实还差一个闭合标签，整块交给修复链路，不猜成一次调用。）
    #[test]
    fn named_invoke_open_is_not_taken_as_close() {
        let (_seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        let markup = concat!(
            "<｜｜DSML｜｜ calls>",
            "<｜｜DSML｜｜ invoke name=\"a\">",
            "<｜｜DSML｜｜ parameter name=\"x\" string=\"true\">1</｜｜DSML｜｜ parameter>",
            "<｜｜DSML｜｜ invoke name=\"b\">",
            "<｜｜DSML｜｜ parameter name=\"y\" string=\"true\">2</｜｜DSML｜｜ parameter>",
            "</｜｜DSML｜｜ invoke>",
            "</｜｜DSML｜｜ calls>",
        );
        processor.feed(vec![(DeltaChunk::RESPONSE.to_string(), markup.to_string())]);
        processor.finish();

        assert!(processor.tool_calls().is_empty(), "不产出拼在一起的调用");
        assert!(processor.parse_failure().is_some(), "整块交给修复链路");
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

    /// 请求里没带的工具直接判失败，不发给客户端：客户端一定会因「未声明的工具」拒绝整次调用。
    #[test]
    fn unknown_tool_is_rejected() {
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

        assert!(
            calls_of(&seen.lock().unwrap().clone()).is_empty(),
            "未声明的工具不该外发"
        );
        assert!(processor.tool_calls().is_empty());
        assert!(processor.parse_failure().is_some(), "应交给修复链路");
        assert!(
            processor
                .failure_reason()
                .is_some_and(|reason| reason.contains("unknown_tool")),
            "失败原因要指出未声明的工具：{:?}",
            processor.failure_reason()
        );
    }

    /// 模型把「闭上一个参数 + 开下一个参数」合成一个闭标签（实测占失败块 33/83、静默腐蚀 44/49）：
    /// 按上下文拆开，参数值里绝不能混进标记。
    #[test]
    fn merged_close_tag_is_split_instead_of_swallowed() {
        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        let markup = concat!(
            "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"edit\">",
            "<｜｜DSML｜｜ parameter name=\"file_path\" string=\"true\">a.rs",
            "</｜｜DSML｜｜ parameter name=\"old_string\" string=\"true\">fn a() {}",
            "</｜｜DSML｜｜ parameter>",
            "<｜｜DSML｜｜ parameter name=\"new_string\" string=\"true\">fn b() {}",
            "</｜｜DSML｜｜ parameter>",
            "</｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>",
        );
        processor.feed(vec![(DeltaChunk::RESPONSE.to_string(), markup.to_string())]);
        processor.finish();

        let calls = calls_of(&seen.lock().unwrap().clone());
        assert_eq!(calls.len(), 1, "应解析成一次调用：{calls:?}");
        let arguments: Value =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(arguments["file_path"], json!("a.rs"), "路径里不能混进标记");
        assert_eq!(arguments["old_string"], json!("fn a() {}"));
        assert_eq!(arguments["new_string"], json!("fn b() {}"));
        assert!(processor.parse_failure().is_none());
    }

    /// 开标签丢掉 `parameter` 这个词（`<｜｜DSML｜｜ description" string="true">`）：
    /// 参数名从标签名位置与属性区推断，不整块作废。
    #[test]
    fn missing_parameter_word_is_inferred() {
        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        let markup = concat!(
            "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"pwsh\">",
            "<｜｜DSML｜｜ parameter name=\"command\" string=\"true\">git status",
            "</｜｜DSML｜｜ parameter>",
            "<｜｜DSML｜｜ description\" string=\"true\">看工作区状态",
            "</｜｜DSML｜｜ parameter>",
            "</｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>",
        );
        processor.feed(vec![(DeltaChunk::RESPONSE.to_string(), markup.to_string())]);
        processor.finish();

        let calls = calls_of(&seen.lock().unwrap().clone());
        assert_eq!(calls.len(), 1, "{calls:?}");
        let arguments: Value =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(arguments["command"], json!("git status"));
        assert_eq!(
            arguments["description"],
            json!("看工作区状态"),
            "写丢 `parameter` 的参数要按属性名还原"
        );
    }

    /// 闭标签只剩标记、名字写丢（`</｜｜DSML｜｜>`）：按栈顶推断是收参数还是收调用。
    #[test]
    fn nameless_close_tags_are_inferred() {
        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        let markup = concat!(
            "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"read_file\">",
            "<｜｜DSML｜｜ parameter name=\"path\" string=\"true\">a.rs</｜｜DSML｜｜>",
            "</｜｜DSML｜｜></｜｜DSML｜｜ calls>",
        );
        processor.feed(vec![(DeltaChunk::RESPONSE.to_string(), markup.to_string())]);
        processor.finish();

        let calls = calls_of(&seen.lock().unwrap().clone());
        assert_eq!(calls.len(), 1, "参数与调用都要按上下文闭合：{calls:?}");
        assert_eq!(calls[0]["function"]["arguments"], "{\"path\":\"a.rs\"}");
        assert!(processor.parse_failure().is_none());
    }

    /// 多写的闭合标签（参数写完后补一行 `</parameter>`、尾部重复 `</calls>`）直接忽略。
    #[test]
    fn redundant_close_tags_are_ignored() {
        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        let markup = concat!(
            "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"read_file\">",
            "<｜｜DSML｜｜ parameter name=\"path\" string=\"true\">a.rs</｜｜DSML｜｜ parameter>",
            "</｜｜DSML｜｜ parameter>",
            "</｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls></｜｜DSML｜｜ calls>",
        );
        processor.feed(vec![(DeltaChunk::RESPONSE.to_string(), markup.to_string())]);
        processor.finish();

        let calls = calls_of(&seen.lock().unwrap().clone());
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0]["function"]["arguments"], "{\"path\":\"a.rs\"}");
        assert!(processor.parse_failure().is_none());
    }

    /// 标记少一根竖线（`</DSML｜｜ parameter>`）同样能认出来。
    #[test]
    fn broken_pipe_marker_still_parses() {
        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        let markup = concat!(
            "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"read_file\">",
            "<｜｜DSML｜｜ parameter name=\"path\" string=\"true\">a.rs</DSML｜｜ parameter>",
            "</｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>",
        );
        processor.feed(vec![(DeltaChunk::RESPONSE.to_string(), markup.to_string())]);
        processor.finish();

        let calls = calls_of(&seen.lock().unwrap().clone());
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0]["function"]["arguments"], "{\"path\":\"a.rs\"}");
    }

    /// 参数值里混进标记残片（历史腐蚀形态）：判失败交给修复，绝不照原样发给客户端。
    #[test]
    fn markup_residue_in_argument_is_rejected() {
        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        let markup = concat!(
            "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"read_file\">",
            "<｜｜DSML｜｜ parameter name=\"path\" string=\"true\">a.rs DSML｜｜ tail",
            "</｜｜DSML｜｜ parameter>",
            "</｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>",
        );
        processor.feed(vec![(DeltaChunk::RESPONSE.to_string(), markup.to_string())]);
        processor.finish();

        assert!(
            calls_of(&seen.lock().unwrap().clone()).is_empty(),
            "坏值不该外发"
        );
        assert!(processor.tool_calls().is_empty());
        assert!(processor.parse_failure().is_some());
        assert!(
            processor
                .failure_reason()
                .is_some_and(|reason| reason.contains("DSML 标记")),
            "失败原因要说明值里混进了标记：{:?}",
            processor.failure_reason()
        );
    }

    /// 合并标签出现在「上一个参数已闭合」之后（真实日志里的形态）：
    /// `</｜｜DSML｜｜ parameter name="workdir" string="true">…` 的 name 是下一个参数，不能丢。
    #[test]
    fn merged_close_after_closed_param_opens_next_param() {
        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true);
        let markup = concat!(
            "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"pwsh\">",
            "<｜｜DSML｜｜ parameter name=\"command\" string=\"true\">cargo test",
            "</｜｜DSML｜｜ parameter>",
            "<｜｜DSML｜｜ parameter name=\"timeoutMs\" string=\"false\">120000",
            "</｜｜DSML｜｜ parameter>",
            "</｜｜DSML｜｜ parameter name=\"workdir\" string=\"true\">D:\\\\work",
            "</｜｜DSML｜｜ parameter>",
            "</｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>",
        );
        processor.feed(vec![(
            DeltaChunk::RESPONSE.to_string(),
            markup.to_string(),
        )]);
        processor.finish();

        let calls = calls_of(&seen.lock().unwrap().clone());
        assert_eq!(calls.len(), 1, "{calls:?}");
        let arguments: Value =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(arguments["command"], json!("cargo test"));
        assert_eq!(arguments["timeoutMs"], json!(120000));
        assert_eq!(
            arguments["workdir"],
            json!("D:\\work"),
            "合并标签带来的 workdir 参数不能丢"
        );
    }

    /// 缺必填参数：按请求里的 schema 判失败，不让客户端拿到一次注定被拒的调用。
    #[test]
    fn missing_required_argument_is_rejected() {
        let tools = json!([{
            "type": "function",
            "function": {
                "name": "read_file",
                "parameters": {
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }
            }
        }]);
        let schemas = ToolSchemas::from_tools(Some(&tools));

        let (seen, sink) = collector();
        let mut processor = ChatStreamProcessor::new(sink, true).with_tool_schemas(schemas);
        processor.feed(vec![(
            DeltaChunk::RESPONSE.to_string(),
            dsml_call("read_file", "limit", "20"),
        )]);
        processor.finish();

        assert!(calls_of(&seen.lock().unwrap().clone()).is_empty());
        assert!(processor.parse_failure().is_some());
        assert!(
            processor
                .failure_reason()
                .is_some_and(|reason| reason.contains("必填")),
            "{:?}",
            processor.failure_reason()
        );
    }
}
