//! DSML 流式解析器：在任意边界的 chunk 流里找 DeepSeek 标记语言写出的工具调用。
//!
//! 设计见仓库根目录 `解析器设计.md`，本文件是它的落地实现。DSML 只有三层
//! `calls` > `invoke` > `parameter`，标签名前可带标记（全角 `｜｜DSML｜｜` 或半角 `||DSML||`）：
//!
//! ```text
//! <｜｜DSML｜｜ calls>
//! <｜｜DSML｜｜ invoke name="view">
//! <｜｜DSML｜｜ parameter name="path" string="true">d:\a.rs</｜｜DSML｜｜ parameter>
//! </｜｜DSML｜｜ invoke>
//! </｜｜DSML｜｜ calls>
//! ```
//!
//! 本模块只负责把文本切成事件（[`DsmlEvent`]）；事件怎么变成对外的增量 / 响应由调用方决定
//! （见 `chat_stream_processor`）。
//!
//! 相对设计文档的取舍，都就近写了原因：
//! 1. 事件集多一个 [`DsmlEvent::Text`]：标记之外的正文必须能原样转发给客户端，事件流里得有这一段；
//! 2. 标记必带，但两种写法都认、标记与标签名之间允许空白；
//!    裸 `<invoke>` 这类没有标记的伪 XML 不算工具调用，免得正文里的普通尖括号被误吃；
//! 3. 参数类型按 `string` 属性定：缺省 / `"true"` 原样当字符串，只有显式 `"false"` 才按 JSON 解析；
//! 4. 一个流里出现多个 `calls` 块时结果累加、序号连续（设计文档按「一块一解析器」写，这里是超集）；
//! 5. 未闭合的块在收尾时把原文当正文吐回——宁可多出文字，也不吞掉模型输出；
//! 6. 参数漏写闭合标签时按「遇到下一个 DSML 标签」截断当前参数值，并剪掉值末尾粘着的
//!    写坏的标签碎片——否则标签会被并进值里，客户端拿它当命令执行。
//!
//! 未实现：设计文档 §1 的 `snapshot_to_json`（`{"type":"tool_calls",...}` 形状）。
//! 本仓库的中间增量协议是 `{"FUNCTIONS":[OpenAI 形状的 tool_calls]}`（见 `apis::base::DeltaChunk`），
//! 没有第二处消费方，故不额外造一套 JSON 形状。

use serde_json::{Map, Value};

/// DSML 标记的两种写法：全角竖线（模型输出的主流形态）与半角竖线。
const MARKERS: [&str; 2] = ["｜｜DSML｜｜", "||DSML||"];

/// 认得的标签名：DSML 只有这三层。
const TAG_NAMES: [&str; 3] = ["calls", "invoke", "parameter"];

/// 未闭合块的缓冲上限：模型幻觉出半个标记时，防止缓冲无限增长（超限即回吐并重置）。
const MAX_PENDING_BYTES: usize = 4 * 1024 * 1024;

/// 解析器吐出的事件。
#[derive(Debug, Clone, PartialEq)]
pub enum DsmlEvent {
    /// 工具调用标记之外的正文。调用方应原样转发（设计文档事件集之外的补充，见模块注释）。
    Text(String),
    /// `<…DSML… calls>`：一块工具调用开始。
    CallsStart,
    /// 一个 `invoke` 开标签出现；`index` 是这个调用在本块内的序号。
    CallStart { index: usize, name: String },
    /// 一个参数收集完成。
    Param { name: String, value: Value },
    /// `</…invoke>`：一个调用闭合，`arguments` 是它的全部参数。
    CallEnd {
        index: usize,
        name: String,
        arguments: Map<String, Value>,
    },
    /// `</…calls>`：一块工具调用结束。
    CallsEnd,
}

/// 语法树里的一次工具调用（DSML 的 `invoke`）。
///
/// 注意与 OpenAI 的 `tool_calls[]` 元素区分：这里 `arguments` 是对象，对外输出时才是 JSON 字符串。
#[derive(Debug, Clone, PartialEq)]
pub struct DsmlCall {
    /// 工具名，取自 `invoke` 的 `name` 属性。
    pub name: String,
    /// 参数集合，保键入顺序无关（serde_json 的 Map）。
    pub arguments: Map<String, Value>,
}

/// 解析位置：标记之外 / `calls` 块内 / `invoke` 内。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum State {
    #[default]
    Outside,
    InCalls,
    InInvoke,
}

/// 正在收集的 `parameter`。
struct PendingParam {
    /// 参数名。
    name: String,
    /// 是否按字符串处理（`string` 属性缺省为真）。
    as_string: bool,
    /// 已收集的原始值。
    value: String,
}

/// 正在累积的 `invoke`。
struct CallAcc {
    /// 调用序号。
    index: usize,
    /// 工具名。
    name: String,
    /// 已收集的参数。
    arguments: Map<String, Value>,
}

/// 一个完整标签的识别结果。
struct Tag {
    /// 标签在缓冲区里的字节长度（含 `<` 与 `>`）。
    len: usize,
    /// 是否闭合标签。
    is_close: bool,
    /// 标签名，取 [`TAG_NAMES`] 里的原值。
    name: &'static str,
    /// 属性区（标签名之后、`>` 之前）。
    attrs: String,
}

/// 在缓冲区里找标签的结果。
enum Scan {
    /// 找到满足条件的完整标签，`pos` 是它的起始下标。
    Found { pos: usize, tag: Tag },
    /// 还没找到，但 `pos` 之前的部分可以安全消费。
    NeedMore { safe: usize },
    /// 整个缓冲区都没有标签。
    None,
}

/// 推进的结果：还能继续处理，还是得等更多数据。
enum Progress {
    Continue,
    Wait,
}

/// DSML 增量解析器。
#[derive(Default)]
pub struct DsmlStreamParser {
    /// 跨 chunk 未消费完的尾数据：标签可能被任意切开，切开的尾巴一直留在这里。
    buf: String,
    /// 当前位置。
    state: State,
    /// 已闭合的调用，保序累加。
    calls: Vec<DsmlCall>,
    /// 正在收集的参数。
    pending: Option<PendingParam>,
    /// 正在累积的调用。
    cur_call: Option<CallAcc>,
    /// 当前未落定的原文：已消费、但还没变成事件的标记，收尾 / 超限时原样回吐用。
    raw: String,
    /// 检测器吃掉的**全部** DSML 原文（标记 + 块内布局 + 参数值，按收到顺序），只增不减：
    /// 落盘时用来对照「模型吐了什么」与「解析成了什么」。
    markup: String,
}

impl DsmlStreamParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入任意 chunk；产生的事件按序追加到 `out`。
    pub fn feed(&mut self, chunk: &str, out: &mut Vec<DsmlEvent>) {
        self.buf.push_str(chunk);
        self.drive(out);
    }

    /// 流结束：把没闭合的残料原样当正文吐出，再清空状态。
    ///
    /// 已经闭合的调用留在 [`Self::calls`] 里；没闭合的（半个块、半个 `invoke`、没收到闭合
    /// 标签的参数值）连同标记一起吐回正文，不吞模型输出。
    pub fn finish(&mut self, out: &mut Vec<DsmlEvent>) {
        let text = self.pending_text();
        if !text.is_empty() {
            out.push(DsmlEvent::Text(text));
        }
        if !self.buf.is_empty() {
            out.push(DsmlEvent::Text(std::mem::take(&mut self.buf)));
        }
        self.state = State::Outside;
        self.cur_call = None;
        self.pending = None;
    }

    /// 已解析出的全部调用（保序；多个 `calls` 块的累加，不因新块清空）。
    pub fn calls(&self) -> &[DsmlCall] {
        &self.calls
    }

    /// 检测器吃掉的全部 DSML 原文（含块内布局与参数值），供落盘 / 排查对照。
    pub fn markup(&self) -> &str {
        &self.markup
    }

    /// 从缓冲区头部取走 `len` 字节，并按序记进落盘原文。
    ///
    /// 只在消费「DSML 标记本身」时用：正文（块外文字）不走这里。
    fn take_markup(&mut self, len: usize) -> String {
        let slice = self.buf[..len].to_string();
        self.buf.drain(..len);
        self.markup.push_str(&slice);
        slice
    }

    /// 把缓冲区里能确定的部分全部推进状态机。
    fn drive(&mut self, out: &mut Vec<DsmlEvent>) {
        loop {
            // 收集参数值时只找 `parameter` 的闭合标签：中间的 `|`、`[`、`]`、换行、`<` 都是原始值。
            if self.pending.is_some() {
                match self.advance_pending(out) {
                    Progress::Continue => continue,
                    Progress::Wait => return,
                }
            }
            let (pos, tag) = match self.scan(&|_| true) {
                Scan::Found { pos, tag } => (pos, tag),
                // 剩下的都不成标签：能定的部分当正文吐出去，尾巴留着等下一块
                Scan::NeedMore { safe } => return self.flush_text(safe, out),
                Scan::None => return self.flush_text(self.buf.len(), out),
            };
            if pos > 0 {
                let text = self.buf[..pos].to_string();
                self.buf.drain(..pos);
                self.push_body(&text, out);
            }
            let raw = self.take_markup(tag.len);
            self.dispatch(tag, &raw, out);
        }
    }

    /// 标记之外的内容：`calls` 块外的照常当正文，块内的只是布局（换行、缩进），
    /// 只记进原文——未闭合时好原样回吐，但既不该当正文发给客户端，也不该进落盘原文之外的地方。
    fn push_body(&mut self, text: &str, out: &mut Vec<DsmlEvent>) {
        if text.is_empty() {
            return;
        }
        match self.state {
            State::Outside => out.push(DsmlEvent::Text(text.to_string())),
            State::InCalls | State::InInvoke => {
                self.raw.push_str(text);
                self.markup.push_str(text);
            }
        }
    }

    /// 参数值收集：找到闭合标签就落定参数，没找齐就把能定的部分先收进值里。
    ///
    /// 值收集期间遇到的**任何** DSML 标签都算当前参数到此为止：正常的 `</parameter>` 直接消费掉，
    /// 其余（下一个 `parameter`、`</invoke>`、`</calls>`）留给 [`Self::drive`] 正常派发。
    /// 模型漏写 `</parameter>` 时，不这么做就会把下一个参数的标签整段吞进值里，
    /// 客户端拿着这串标签当命令去执行（`command` 里出现 `<｜｜DSML｜｜ parameter name=…`）。
    fn advance_pending(&mut self, out: &mut Vec<DsmlEvent>) -> Progress {
        let terminator = self.scan(&|_| true);
        match terminator {
            Scan::Found { pos, tag } => {
                // 值分两处：已经吃进 pending 的部分 + 缓冲区里剩下的这一段
                let tail = self.take_markup(pos);
                // 正常闭合才把闭合标签一起消费掉并标记落定；其余标签留给外层正常派发
                if tag.is_close && tag.name == "parameter" {
                    self.take_markup(tag.len);
                }
                self.finish_pending(&tail, out);
                Progress::Continue
            }
            // 值还没收完：先把能定的部分吃进值里，剩下的等下一块
            Scan::NeedMore { safe } => self.wait_or_rollback(safe, out),
            Scan::None => {
                let len = self.buf.len();
                self.wait_or_rollback(len, out)
            }
        }
    }

    /// 落定当前参数：`tail` 是值里还没并进来的那一段。
    fn finish_pending(&mut self, tail: &str, out: &mut Vec<DsmlEvent>) {
        let Some(mut pending) = self.pending.take() else {
            return;
        };
        pending.value.push_str(tail);
        let cleaned = trim_broken_tag(&pending.value);
        let value = param_value(cleaned, pending.as_string);
        if let Some(call) = self.cur_call.as_mut() {
            call.arguments.insert(pending.name.clone(), value.clone());
        }
        out.push(DsmlEvent::Param {
            name: pending.name,
            value,
        });
    }

    /// 收下 `len` 字节的值；缓冲失控时把已吞的原文吐回正文并重置，好继续往下走。
    fn wait_or_rollback(&mut self, len: usize, out: &mut Vec<DsmlEvent>) -> Progress {
        match self.collect_value(len) {
            Some(text) => {
                out.push(DsmlEvent::Text(text));
                Progress::Continue
            }
            None => Progress::Wait,
        }
    }

    /// 把缓冲区前 `len` 字节并进当前参数值；超过上限则返回需要吐回的原文。
    fn collect_value(&mut self, len: usize) -> Option<String> {
        if len == 0 {
            return None;
        }
        let slice = self.take_markup(len);
        let overflow = match self.pending.as_mut() {
            Some(pending) => {
                pending.value.push_str(&slice);
                pending.value.len() > MAX_PENDING_BYTES
            }
            None => false,
        };
        if !overflow {
            return None;
        }
        // 半个标记吃掉了整段输出：把原文并回正文，状态重置回标记之外，别把用户的内容吞掉
        let text = self.pending_text();
        self.state = State::Outside;
        self.cur_call = None;
        Some(text)
    }

    /// 未落定的原文：已消费的标记 + 正在收集的参数值。
    fn pending_text(&mut self) -> String {
        let mut text = std::mem::take(&mut self.raw);
        if let Some(pending) = self.pending.take() {
            text.push_str(&pending.value);
        }
        text
    }

    /// 缓冲区前 `safe` 字节当正文吐出，其余留着等下一块。
    fn flush_text(&mut self, safe: usize, out: &mut Vec<DsmlEvent>) {
        if safe == 0 {
            return;
        }
        let text = self.buf[..safe].to_string();
        self.buf.drain(..safe);
        out.push(DsmlEvent::Text(text));
    }

    /// 在缓冲区里找第一个满足 `want` 的完整标签。
    fn scan(&self, want: &dyn Fn(&Tag) -> bool) -> Scan {
        let mut from = 0;
        while let Some(pos) = self.buf[from..].find('<').map(|i| i + from) {
            let rest = &self.buf[pos..];
            if let Some(tag) = parse_tag(rest) {
                if want(&tag) {
                    return Scan::Found { pos, tag };
                }
                // 不是想要的标签（例如参数值里的别的标签）：跳过，继续往后找
                from = pos + tag.len;
                continue;
            }
            // 不是完整标签：只有最后一个 `<` 可能是被切开的一半，其余当普通正文
            if self.buf[pos + 1..].find('<').is_none() && is_tag_prefix(rest) {
                return Scan::NeedMore { safe: pos };
            }
            from = pos + 1;
        }
        Scan::None
    }

    /// 认领一个标签：按 `(是否闭合, 标签名)` 推进状态并产出事件；`raw` 是这个标签的原文。
    fn dispatch(&mut self, tag: Tag, raw: &str, out: &mut Vec<DsmlEvent>) {
        match (tag.is_close, tag.name) {
            (false, "calls") => {
                self.state = State::InCalls;
                // 从这一块的开标记开始记原文：块没闭合时整段原样回吐
                self.raw.clear();
                self.raw.push_str(raw);
                out.push(DsmlEvent::CallsStart);
            }
            (true, "calls") => {
                self.state = State::Outside;
                self.raw.clear();
                out.push(DsmlEvent::CallsEnd);
            }
            (false, "invoke") => {
                let name = attr(&tag.attrs, "name").unwrap_or_default();
                let index = self.calls.len();
                self.cur_call = Some(CallAcc {
                    index,
                    name: name.clone(),
                    arguments: Map::new(),
                });
                self.state = State::InInvoke;
                self.raw.push_str(raw);
                out.push(DsmlEvent::CallStart { index, name });
            }
            (true, "invoke") => {
                if let Some(call) = self.cur_call.take() {
                    out.push(DsmlEvent::CallEnd {
                        index: call.index,
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    });
                    self.calls.push(DsmlCall {
                        name: call.name,
                        arguments: call.arguments,
                    });
                    // 这个 invoke 已经落定成事件，原文不必再留着回吐
                    self.raw.clear();
                }
                self.state = State::InCalls;
            }
            (false, "parameter") => {
                let name = attr(&tag.attrs, "name").unwrap_or_default();
                // 缺省按字符串；只有显式 string="false" 才让参数值按 JSON 解析
                let as_string = attr(&tag.attrs, "string").is_none_or(|v| v != "false");
                // 开标签也记进原文：参数值收一半就断了的话，回吐要能对得上
                self.raw.push_str(raw);
                self.pending = Some(PendingParam {
                    name,
                    as_string,
                    value: String::new(),
                });
            }
            // 参数闭合标签正常由参数收集分支消费，走到这里说明是多余的闭合：忽略
            (true, "parameter") => {}
            _ => {}
        }
    }
}

/// 把 `s`（以 `<` 开头）解析成一个完整标签；不是完整标签返回 `None`。
fn parse_tag(s: &str) -> Option<Tag> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'<') {
        return None;
    }
    let mut i = 1;
    let is_close = bytes.get(i) == Some(&b'/');
    if is_close {
        i += 1;
    }
    i = skip_ws(s, i);
    // 标记必带：`<｜｜DSML｜｜ invoke>` / `<||DSML|| invoke>`，标记与标签名之间可有空白
    let matched = MARKERS.iter().find(|marker| s[i..].starts_with(**marker));
    let marker = matched?;
    i += marker.len();
    i = skip_ws(s, i);
    let name_start = i;
    while let Some(byte) = bytes.get(i) {
        if byte.is_ascii_alphabetic() || *byte == b'_' {
            i += 1;
        } else {
            break;
        }
    }
    let name = TAG_NAMES
        .iter()
        .copied()
        .find(|name| *name == &s[name_start..i])?;
    // 属性区到 `>` 结束；中途再出现 `<` 说明这不是一个标签
    let attr_start = i;
    while let Some(byte) = bytes.get(i) {
        match byte {
            b'>' => {
                return Some(Tag {
                    len: i + 1,
                    is_close,
                    name,
                    attrs: s[attr_start..i].to_string(),
                });
            }
            b'<' => return None,
            _ => i += 1,
        }
    }
    // 还没有 `>`：标签被切开了
    None
}

/// `s`（以 `<` 开头）是否可能是某个标签的前缀——是则这部分尾数据要留着等下一块。
fn is_tag_prefix(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'<') {
        return false;
    }
    let mut i = 1;
    if i >= bytes.len() {
        return true; // "<"
    }
    if bytes[i] == b'/' {
        i += 1;
        if i >= bytes.len() {
            return true; // "</"
        }
    }
    i = skip_ws(s, i);
    if i >= bytes.len() {
        return true; // "<   "
    }
    let rest = &s[i..];
    let mut matched_marker = None;
    for marker in MARKERS {
        if rest.starts_with(marker) {
            matched_marker = Some(marker);
            break;
        }
        if marker.starts_with(rest) {
            return true; // 标记被切了一半
        }
    }
    let Some(marker) = matched_marker else {
        return false; // 不是 `<…DSML…`
    };
    i += marker.len();
    i = skip_ws(s, i);
    if i >= bytes.len() {
        return true; // "<｜｜DSML｜｜ "
    }
    let name_start = i;
    while let Some(byte) = bytes.get(i) {
        if byte.is_ascii_alphabetic() || *byte == b'_' {
            i += 1;
        } else {
            break;
        }
    }
    let name = &s[name_start..i];
    if i >= bytes.len() {
        // 标签名可能还没写完
        return TAG_NAMES.iter().any(|tag| tag.starts_with(name));
    }
    if !TAG_NAMES.contains(&name) {
        return false;
    }
    // 标签名已完整：还没到 `>` 且中间没有 `<`，就是被切开了
    !s[i..].contains('<')
}

/// 跳过 ASCII 空白，返回首个非空白位置。
fn skip_ws(s: &str, mut i: usize) -> usize {
    let bytes = s.as_bytes();
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// 取标签属性 `key="value"` 的值（反转义）；没有该属性返回 `None`。
fn attr(attrs: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let start = attrs.find(&needle)? + needle.len();
    let rest = &attrs[start..];
    let end = rest.find('"')?;
    Some(unescape(&rest[..end]))
}

/// 剪掉值末尾粘着的写坏的标签碎片（`</｜｜DSML｜｜`、`</｜｜DSML｜｜ param>` 之类）。
///
/// 模型漏写闭合标签、或闭合标签写错名字时，这些碎片会被当成参数值的一部分；
/// 值本身不可能是我们的标记（参数值里出现 `<｜｜DSML｜｜` 只可能是这种残渣），
/// 所以最后一个 `<` 之后只要带标记就整段丢掉。
fn trim_broken_tag(text: &str) -> &str {
    let Some(pos) = text.rfind('<') else {
        return text;
    };
    let tail = &text[pos..];
    if MARKERS.iter().any(|marker| tail.contains(marker)) {
        &text[..pos]
    } else {
        text
    }
}

/// 反转义传输层写法：只有 `\"` → `"` 与 `\\` → `\` 两种。
///
/// 参数值里绝大多数是 Windows 路径，`\t`、`\n`、`\r` 是路径分隔符而不是转义
/// （`\tests`、`\node_modules`、`\registry.rs`）；按转义处理会把分隔符吃掉一层并塞进控制字符，
/// 客户端拿着这种路径取文件必然失败。其余 `\x` 原样保留，与 [`escape`] 互为逆运算。
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
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

/// 参数原文 → JSON 值。
///
/// - `as_string`：按纯文本处理，只做传输层反转义（`\"` → `"`、`\\` → `\`）；
/// - 否则按 JSON 值处理：**先用原文**解析（JSON 的转义本就是给 JSON 用的），解析不了再退化成
///   反转义后的字符串，保证不丢内容。
fn param_value(raw: &str, as_string: bool) -> Value {
    let raw = trim_layout(raw);
    if as_string {
        return Value::String(unescape(raw));
    }
    let trimmed = raw.trim();
    match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => value,
        Err(_) => Value::String(unescape(trimmed)),
    }
}

/// 反转义 [`param_value`] 用的转义：值里的 `\` 与 `"` 写成转义形态。
///
/// 渲染历史调用（`DeepSeekWebModel::render_tool_calls`）必须用它，否则同一个值渲染后再解析回来
/// 会被反转义吃掉一层反斜杠（如 UNC 路径 `\\server\share` 变成 `\server\share`）。
pub fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// 去掉「值独占多行」布局带来的首尾换行（`<parameter …>\n值\n</…parameter>`）。
/// 只去掉紧邻标签的那一个换行，值内部的换行与空白原样保留。
fn trim_layout(raw: &str) -> &str {
    let raw = raw
        .strip_prefix("\r\n")
        .or_else(|| raw.strip_prefix('\n'))
        .unwrap_or(raw);
    raw.strip_suffix("\r\n")
        .or_else(|| raw.strip_suffix('\n'))
        .unwrap_or(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 一次喂完，收集事件。
    fn run(chunks: &[&str]) -> (Vec<DsmlEvent>, Vec<DsmlCall>) {
        let mut parser = DsmlStreamParser::new();
        let mut events = Vec::new();
        for chunk in chunks {
            parser.feed(chunk, &mut events);
        }
        parser.finish(&mut events);
        (events, parser.calls().to_vec())
    }

    /// 事件流里的正文拼成一整段（正文的切分粒度与喂入粒度一致，天然会分片）。
    fn text_of(events: &[DsmlEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                DsmlEvent::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// 事件流里除正文外的事件（结构化事件不该受切分粒度影响）。
    fn structure_of(events: &[DsmlEvent]) -> Vec<DsmlEvent> {
        events
            .iter()
            .filter(|event| !matches!(event, DsmlEvent::Text(_)))
            .cloned()
            .collect()
    }

    /// 逐字符切分（每个 chunk 都在字符边界上），用来验证「切分粒度不影响结果」。
    fn char_chunks(text: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let mut start = 0;
        for (index, ch) in text.char_indices() {
            out.push(&text[start..index + ch.len_utf8()]);
            start = index + ch.len_utf8();
        }
        out
    }

    /// 反斜杠只剩一层：模型照原样写 `d:\a`、按转义写 `d:\\a`，落值都必须是单个 `d:\a`。
    #[test]
    fn backslashes_are_unescaped_once() {
        for raw in [r#"d:\projects\a.rs"#, r#"d:\\projects\\a.rs"#] {
            let (_, calls) = run(&[&call_with("path", "true", raw)]);
            assert_eq!(
                calls[0].arguments["path"],
                json!(r#"d:\projects\a.rs"#),
                "入参原文 {raw} 应落成单层反斜杠"
            );
        }
        // UNC 路径按规则转义后落值仍是两条起头
        let (_, calls) = run(&[&call_with("path", "true", r#"\\\\server\\share"#)]);
        assert_eq!(calls[0].arguments["path"], json!(r#"\\server\share"#));
    }

    /// 路径分隔符不是转义：`\t`、`\n`、`\r` 出现在 Windows 路径里必须原样保留。
    #[test]
    fn path_separators_are_not_escapes() {
        for raw in [
            r#"d:\projects\app\tests\a.rs"#,
            r#"d:\new\node_modules\b.ts"#,
            r#"d:\projects\db\src\registry.rs"#,
        ] {
            let (_, calls) = run(&[&call_with("path", "true", raw)]);
            assert_eq!(
                calls[0].arguments["path"],
                json!(raw),
                "路径不能被反转义吃掉一层：{raw}"
            );
        }
    }

    /// `string="false"` 的值按 JSON 原文解析：对象不会被反转义弄成非法 JSON，也不会退化成字符串。
    #[test]
    fn json_params_parse_from_raw_text() {
        let (_, calls) = run(&[&call_with("path", "false", r#"{"k":"d:\\a\\b"}"#)]);
        assert_eq!(calls[0].arguments["path"], json!({ "k": r#"d:\a\b"# }));

        // JSON 字符串字面量：解析出来是字符串本体，不带外层引号
        let (_, calls) = run(&[&call_with("path", "false", r#""d:\\a\\b""#)]);
        assert_eq!(calls[0].arguments["path"], json!(r#"d:\a\b"#));

        // 不是合法 JSON 就退化成字符串，别把内容吞掉
        let (_, calls) = run(&[&call_with("path", "false", "not json")]);
        assert_eq!(calls[0].arguments["path"], json!("not json"));
    }

    /// 渲染端转义与解析端反转义必须互为逆运算（历史调用靠这一对保持原值）。
    #[test]
    fn escape_round_trips_through_parser() {
        for value in [
            r#"d:\projects\a.rs"#,
            r#"\\server\share"#,
            r#"含 "引号" 与 \反斜杠"#,
            "多行\n值",
            "普通文本",
        ] {
            let (_, calls) = run(&[&call_with("p", "true", &escape(value))]);
            assert_eq!(
                calls[0].arguments["p"],
                json!(value),
                "转义后再解析应还原：{value}"
            );
        }
    }

    /// 拼一个单参数调用（`flag` 为 `string` 属性的值）。
    ///
    /// 用 `format!` 而不是裸字面量：值里可能带引号与反斜杠，交给它原样嵌入。
    fn call_with(name: &str, flag: &str, value: &str) -> String {
        format!(
            "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"t\">\
             <｜｜DSML｜｜ parameter name=\"{name}\" string=\"{flag}\">{value}</｜｜DSML｜｜ parameter>\
             </｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>"
        )
    }

    /// 没标记的普通正文原样透传。
    #[test]
    fn plain_text_passes_through() {
        let (events, calls) = run(&["just text"]);
        assert_eq!(text_of(&events), "just text");
        assert!(calls.is_empty());
    }

    /// 正文里的尖括号不是工具调用：不成标签就照原样走。
    #[test]
    fn bare_angle_brackets_stay_text() {
        let (events, calls) = run(&["a < b > c", "<invoke name=\"x\">"]);
        assert_eq!(
            text_of(&events),
            "a < b > c<invoke name=\"x\">",
            "没有 DSML 标记的伪 XML 不算工具调用"
        );
        assert!(structure_of(&events).is_empty());
        assert!(calls.is_empty());
    }

    /// 完整块：两个 invoke、字符串与数组参数、正文前后缀。
    #[test]
    fn parses_two_invokes() {
        let payload = concat!(
            "前缀",
            "<｜｜DSML｜｜ calls>\n",
            "<｜｜DSML｜｜ invoke name=\"view\">\n",
            "<｜｜DSML｜｜ parameter name=\"path\" string=\"true\">d:\\a.rs</｜｜DSML｜｜ parameter>\n",
            "<｜｜DSML｜｜ parameter name=\"view_range\" string=\"false\">[320,400]</｜｜DSML｜｜ parameter>\n",
            "</｜｜DSML｜｜ invoke>\n",
            "<｜｜DSML｜｜ invoke name=\"grep\">\n",
            "<｜｜DSML｜｜ parameter name=\"pattern\">已落盘|write_prompt</｜｜DSML｜｜ parameter>\n",
            "</｜｜DSML｜｜ invoke>\n",
            "</｜｜DSML｜｜ calls>",
            "后缀",
        );
        let (events, calls) = run(&[payload]);

        assert_eq!(text_of(&events), "前缀后缀", "标记块内外的布局不算正文");

        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "view");
        assert_eq!(calls[0].arguments["path"], json!("d:\\a.rs"));
        assert_eq!(calls[0].arguments["view_range"], json!([320, 400]));
        assert_eq!(calls[1].name, "grep");
        assert_eq!(calls[1].arguments["pattern"], json!("已落盘|write_prompt"));

        assert!(events.contains(&DsmlEvent::CallsStart));
        assert!(events.contains(&DsmlEvent::CallsEnd));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, DsmlEvent::CallEnd { .. }))
                .count(),
            2
        );
    }

    /// 切分粒度不影响结果：逐字符喂入与一次喂完必须一致（标签、参数值都可能被切开）。
    #[test]
    fn chunk_boundaries_do_not_matter() {
        let payload = concat!(
            "要说的话",
            "<｜｜DSML｜｜ calls>",
            "<｜｜DSML｜｜ invoke name=\"read_file\">",
            "<｜｜DSML｜｜ parameter name=\"path\" string=\"true\">d:/a\\\"b.rs</｜｜DSML｜｜ parameter>",
            "</｜｜DSML｜｜ invoke>",
            "</｜｜DSML｜｜ calls>",
            "收尾",
        );
        let whole = run(&[payload]);
        let split = run(&char_chunks(payload));
        assert_eq!(
            text_of(&whole.0),
            text_of(&split.0),
            "正文内容与切分无关（分片粒度可以不同）"
        );
        assert_eq!(
            structure_of(&whole.0),
            structure_of(&split.0),
            "结构化事件序列应与切分无关"
        );
        assert_eq!(whole.1, split.1);
        assert_eq!(split.1[0].arguments["path"], json!("d:/a\"b.rs"));
    }

    /// 半角竖线与空白变体同样识别。
    #[test]
    fn marker_variants_are_recognized() {
        let payload = concat!(
            "<||DSML|| calls>",
            "<||DSML||invoke name=\"a\">",
            "<||DSML|| parameter name=\"n\" string=\"false\">7</||DSML|| parameter>",
            "</||DSML|| invoke>",
            "</||DSML|| calls>",
        );
        let (_, calls) = run(&[payload]);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "a");
        assert_eq!(calls[0].arguments["n"], json!(7));
    }

    /// 参数值里的 `|`、换行、`<`、`>` 都当原始值，不当标签。
    #[test]
    fn param_value_is_opaque() {
        let payload = concat!(
            "<｜｜DSML｜｜ calls>",
            "<｜｜DSML｜｜ invoke name=\"powershell\">",
            "<｜｜DSML｜｜ parameter name=\"command\" string=\"true\">Get-ChildItem <b> | sort\n第二行</｜｜DSML｜｜ parameter>",
            "</｜｜DSML｜｜ invoke>",
            "</｜｜DSML｜｜ calls>",
        );
        let (_, calls) = run(&[payload]);
        assert_eq!(
            calls[0].arguments["command"],
            json!("Get-ChildItem <b> | sort\n第二行")
        );
    }

    /// `string` 属性缺省按字符串：数字形态的值也不会被猜成数字。
    #[test]
    fn param_defaults_to_string() {
        let payload = concat!(
            "<｜｜DSML｜｜ calls>",
            "<｜｜DSML｜｜ invoke name=\"a\">",
            "<｜｜DSML｜｜ parameter name=\"n\">7</｜｜DSML｜｜ parameter>",
            "</｜｜DSML｜｜ invoke>",
            "</｜｜DSML｜｜ calls>",
        );
        let (_, calls) = run(&[payload]);
        assert_eq!(calls[0].arguments["n"], json!("7"));
    }

    /// 参数漏写 `</parameter>`：下一个 `parameter` 视为当前参数结束，标签不能并进值里。
    #[test]
    fn missing_parameter_close_does_not_swallow_next_parameter() {
        let payload = concat!(
            "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"powershell\">",
            "<｜｜DSML｜｜ parameter name=\"command\" string=\"true\">git --no-pager log --oneline -3",
            "<｜｜DSML｜｜ parameter name=\"description\" string=\"true\">看最近三条提交",
            "</｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>",
        );
        let (_, calls) = run(&[payload]);

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "powershell");
        assert_eq!(
            calls[0].arguments["command"],
            json!("git --no-pager log --oneline -3"),
            "命令里不能混进标签"
        );
        assert_eq!(
            calls[0].arguments["description"],
            json!("看最近三条提交"),
            "第二个参数照样要解析出来"
        );
    }

    /// 闭合标签写坏（名字写错）时，值末尾的标签碎片要剪掉，别让客户端拿它去执行。
    #[test]
    fn broken_closing_tag_is_trimmed_from_value() {
        let payload = concat!(
            "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"powershell\">",
            "<｜｜DSML｜｜ parameter name=\"command\" string=\"true\">git --no-pager log -4</｜｜DSML｜｜ param>",
            "<｜｜DSML｜｜ parameter name=\"description\" string=\"true\">看最近四条",
            "</｜｜DSML｜｜ parameter></｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>",
        );
        let (_, calls) = run(&[payload]);

        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].arguments["command"],
            json!("git --no-pager log -4"),
            "写坏的闭合标签不能留在值里"
        );
        assert_eq!(calls[0].arguments["description"], json!("看最近四条"));
    }

    /// 值没闭合就直接 `</invoke>`：值到此为止，调用照常闭合。
    #[test]
    fn invoke_close_terminates_unclosed_value() {
        let payload = concat!(
            "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"powershell\">",
            "<｜｜DSML｜｜ parameter name=\"command\" string=\"true\">git status --short --branch",
            "</｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>",
        );
        let (_, calls) = run(&[payload]);

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "powershell");
        assert_eq!(
            calls[0].arguments["command"],
            json!("git status --short --branch")
        );
    }

    /// 值里本来就有尖括号（shell 的重定向之类）时不能被这套裁剪误伤。
    #[test]
    fn angle_brackets_in_value_survive() {
        let payload = concat!(
            "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"powershell\">",
            "<｜｜DSML｜｜ parameter name=\"command\" string=\"true\">Get-ChildItem > out.txt <b>",
            "</｜｜DSML｜｜ parameter></｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>",
        );
        let (_, calls) = run(&[payload]);

        assert_eq!(
            calls[0].arguments["command"],
            json!("Get-ChildItem > out.txt <b>")
        );
    }

    /// 闭合标签被截断（只剩 `</｜｜DSML｜｜`）时同样只留干净的命令。
    #[test]
    fn truncated_closing_tag_is_trimmed() {
        let payload = concat!(
            "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"powershell\">",
            "<｜｜DSML｜｜ parameter name=\"command\" string=\"true\">git --no-pager log --oneline -4</｜｜DSML｜｜",
            "\n<｜｜DSML｜｜ parameter name=\"description\" string=\"true\">看最近四条",
            "</｜｜DSML｜｜ parameter></｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>",
        );
        let (_, calls) = run(&[payload]);

        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].arguments["command"],
            json!("git --no-pager log --oneline -4"),
            "截断的闭合标签不能留在命令里"
        );
        assert_eq!(calls[0].arguments["description"], json!("看最近四条"));
    }

    /// 未闭合的块：收尾时原文当正文吐回，调用不成立。
    #[test]
    fn unclosed_block_rolls_back_to_text() {
        let payload = "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"a\"><｜｜DSML｜｜ parameter name=\"n\">没写完";
        let (events, calls) = run(&[payload]);
        assert!(calls.is_empty());
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                DsmlEvent::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, payload, "没闭合就原样回吐，不吞内容");
    }

    /// 块闭合但缓冲里还留着没换行的尾巴：尾巴照常当正文。
    #[test]
    fn trailing_text_without_tag_goes_out() {
        let payload = "<｜｜DSML｜｜ calls></｜｜DSML｜｜ calls>后半句";
        let (events, calls) = run(&[payload]);
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                DsmlEvent::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "后半句");
        assert!(calls.is_empty());
    }

    /// 落盘用的原文：吃掉的标记逐字保留（含块内换行与参数值），块外的正文不算在内。
    #[test]
    fn markup_keeps_consumed_tags_verbatim() {
        let payload = concat!(
            "先说一句",
            "<｜｜DSML｜｜ calls>\n",
            "<｜｜DSML｜｜ invoke name=\"read_file\">\n",
            "<｜｜DSML｜｜ parameter name=\"path\" string=\"true\">a.rs</｜｜DSML｜｜ parameter>\n",
            "</｜｜DSML｜｜ invoke>\n",
            "</｜｜DSML｜｜ calls>",
            "再说一句",
        );
        let (_, calls) = run(&[payload]);
        let mut parser = DsmlStreamParser::new();
        let mut events = Vec::new();
        parser.feed(payload, &mut events);
        parser.finish(&mut events);

        assert_eq!(calls.len(), 1);
        let head = "先说一句".len();
        let tail = "再说一句".len();
        assert_eq!(
            parser.markup(),
            &payload[head..payload.len() - tail],
            "落盘原文只含标记块，不含块外正文"
        );
    }

    /// 一个流里两个 calls 块：结果累加，序号连续。
    #[test]
    fn multiple_blocks_accumulate() {
        let payload = concat!(
            "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"a\"></｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>",
            "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"b\"></｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>",
        );
        let (events, calls) = run(&[payload]);
        let names: Vec<&str> = calls.iter().map(|call| call.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        let indexes: Vec<usize> = events
            .iter()
            .filter_map(|event| match event {
                DsmlEvent::CallEnd { index, .. } => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(indexes, vec![0, 1], "序号跨块连续");
    }
}
