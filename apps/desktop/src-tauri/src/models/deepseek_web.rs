//! DeepSeek 网页版模型：实现插件模型抽象，一个模型对应 `deepseek-web` 渠道。
//!
//! 会话管理（创建 / 列表 / 删除 / 改标题 / 历史）与聊天请求组装、PoW 应答、流式分块
//! 全部在模型内部完成；对外的请求发送与凭证读取经 [`ChannelExecutor`]（由传输层实现）。
//!
//! 渠道细节（URL、PoW、SSE 分行）不外露；具体由哪台插件实例执行由插件管理器调度。
pub mod chat_stream_processor;
pub mod dsml_stream_parser;
pub mod session_pool;
pub mod sse_frame_processor;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use futures_util::future::BoxFuture;
use serde::Serialize;
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use super::defs::{
    ChannelExecutor, ChannelRequest, ChunkSink, Concurrency, InferRequest, ModelCapability,
    ModelResponse, SessionPoolStore,
};
use super::model::Model;
use super::plugin::PluginModel;
use super::pow::{build_pow_header, solve_pow};
use crate::dump::DumpConfig;
use crate::models::deepseek_web::chat_stream_processor::ChatStreamProcessor;
use crate::models::deepseek_web::dsml_stream_parser::escape;
use crate::models::deepseek_web::sse_frame_processor::SseFrameProcessor;

/// 渠道标识。
pub const CHANNEL: &str = "deepseek-web";

/// 拼装提示词时置顶的前缀：网页会话本身带着历史对话，要求模型只依据本次内容作答。
const PROMPT_PREFIX: &str =
    "不需要考虑之前的内容，输出中文思考和正文，你只需要根据以下内容回答问题：";

/// 系统提示词段标题：正文取自全部 system 消息。
const SYSTEM_HEADING: &str = "以下是【系统提示词】：";

/// 历史对话段标题：正文是除最新一条记录以外的消息。
const HISTORY_HEADING: &str = "以下是【历史对话内容】：";

/// 工具调用格式与限定规则段标题。
const TOOL_RULE_HEADING: &str = "以下是【工具调用格式和限定规则】：";

/// 可用工具段标题。
const TOOLS_HEADING: &str = "以下是【可用工具】：";

/// 工具调用块的开边 / 闭边（DSML 标记，全角竖线）。
const CALLS_OPEN: &str = "<｜｜DSML｜｜ calls>";
const CALLS_CLOSE: &str = "</｜｜DSML｜｜ calls>";

/// 单次调用的开边前缀（后面接 `工具名">`）/ 闭边。
const INVOKE_OPEN: &str = "<｜｜DSML｜｜ invoke name=\"";
const INVOKE_CLOSE: &str = "</｜｜DSML｜｜ invoke>";

/// 参数的开边前缀（后面接 `参数名" string="…">`）/ 闭边。
const PARAMETER_OPEN: &str = "<｜｜DSML｜｜ parameter name=\"";
const PARAMETER_CLOSE: &str = "</｜｜DSML｜｜ parameter>";

/// 工具调用格式与限定规则：网页版没有原生工具调用，靠这段文字约束模型按 DSML 标记输出调用。
///
/// 标记形态必须与 [`DsmlStreamParser`](crate::models::deepseek_web::dsml_stream_parser::DsmlStreamParser)
/// 认的一致，且与 [`DeepSeekWebModel::render_tool_calls`] 渲染历史调用的方式一致。
const TOOL_RULE: &str = r#"1. 需要调用工具时，调用写成 DSML 标记：整体包在 <｜｜DSML｜｜ calls> 与 </｜｜DSML｜｜ calls> 之间，一次调用一层 <｜｜DSML｜｜ invoke name="工具名"> 与 </｜｜DSML｜｜ invoke>，一个参数一条 <｜｜DSML｜｜ parameter name="参数名" string="true">参数值</｜｜DSML｜｜ parameter>，例如：
<｜｜DSML｜｜ calls>
<｜｜DSML｜｜ invoke name="read_file">
<｜｜DSML｜｜ parameter name="path" string="true">d:/project/src/main.rs</｜｜DSML｜｜ parameter>
</｜｜DSML｜｜ invoke>
</｜｜DSML｜｜ calls>
2. 竖线必须逐字写成全角 ｜｜DSML｜｜，不能写成 ||DSML||、DSML，也不能漏写；尖括号、斜杠、等号、引号一律用半角；标签必须成对出现，不能改写、不能漏写、不能嵌套，不要套 Markdown 代码块；每个参数写完值都要紧跟它自己的 </｜｜DSML｜｜ parameter>，不能省略、不能写错名字。
3. string="true" 表示参数值是纯文本，照原样写；参数值不是字符串（数字、布尔、数组、对象）时写 string="false"，值按 JSON 写，例如 <｜｜DSML｜｜ parameter name="timeout" string="false">30</｜｜DSML｜｜ parameter>、<｜｜DSML｜｜ parameter name="view_range" string="false">[320,400]</｜｜DSML｜｜ parameter>。
4. 参数值里不要出现 </｜｜DSML｜｜ parameter> 这样的片段；值里的反斜杠写成 \\、双引号写成 \"（Windows 路径也要写，例如 d:\\project\\a.rs）。
5. 需要调用多个工具时，一个工具一层 invoke，平铺在同一个 calls 块里。
6. 工具调用与正文不要混在同一条回答里：需要调用工具时这条回答只输出 DSML 标记，标记之外不写解释、前言、总结；不调用工具时才输出普通正文，正文里也不要夹带标记。
7. 工具名与参数名必须与工具定义完全一致，必填参数不能省略；只能调用下面列出的工具。
8. 历史里 invoke 的 id 属性是调用编号（新的调用不必写它）；工具结果的标题写成 【tool】name=工具名 id=调用编号（缺哪个字段就只写有的那个），其中 id 与上面某次 invoke 的 id 相同，据此把结果和那次调用对上。"#;

const COMPLETION_URL: &str = "https://chat.deepseek.com/api/v0/chat/completion";
const SESSION_CREATE_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/create";
const SESSION_LIST_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/fetch_page";
const SESSION_DELETE_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/delete";
const SESSION_TITLE_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/update_title";
const HISTORY_URL: &str = "https://chat.deepseek.com/api/v0/chat/history_messages";
const POW_CHALLENGE_URL: &str = "https://chat.deepseek.com/api/v0/chat/create_pow_challenge";

/// 网页版默认模型类型与思考开关。
const MODEL_TYPE: &str = "default";
const THINKING_ENABLED: bool = true;

/// DeepSeek 网页版模型。
pub struct DeepSeekWebModel {
    /// 展示名。
    display_name: String,
    /// 渠道执行器（发送请求 + 读取凭证）。
    executor: Arc<dyn ChannelExecutor>,
    /// 会话池容量：渠道并发设置 = 预建网页会话数。
    pool_size: u32,
    /// 会话池槽位（下标 = 槽位序号）：网页会话 id。
    pool: tokio::sync::Mutex<Vec<String>>,
    /// 会话池是否已与渠道对齐（对齐后不再重复核对）。
    pool_ready: AtomicBool,
    /// 会话池对齐互斥：并发请求同时进来时只让一个去核对 / 建会话。
    pool_lock: tokio::sync::Mutex<()>,
    /// 会话池持久化存储；None 表示槽位不落库。
    pool_store: Option<Arc<dyn SessionPoolStore>>,
    /// 会话池中标题已就位的会话 id（已命名或已在网页端用同名标题存在）。
    pool_titled: Mutex<HashSet<String>>,
    /// 并发闸门：同时在途的请求数不超过池容量（保证每个在途请求独占一个槽位）。
    gate: tokio::sync::Semaphore,
    /// 槽位轮转游标：为每个放行的请求分配槽位。
    slot_cursor: AtomicUsize,
    /// 会话 id → 最近一次响应消息 id（下轮请求的 parent_message_id）。
    session_map: Mutex<HashMap<String, i64>>,
    /// 并发语义（网页渠道：会话池并发）。
    concurrency: Concurrency,
    /// 分阶段落盘工具（响应）；None 表示不落盘。
    dump: Option<Arc<DumpConfig>>,
}

impl DeepSeekWebModel {
    pub fn new(display_name: impl Into<String>, executor: Arc<dyn ChannelExecutor>) -> Self {
        Self {
            display_name: display_name.into(),
            executor,
            pool_size: 1,
            pool: tokio::sync::Mutex::new(Vec::new()),
            pool_ready: AtomicBool::new(false),
            pool_lock: tokio::sync::Mutex::new(()),
            pool_store: None,
            pool_titled: Mutex::new(HashSet::new()),
            gate: tokio::sync::Semaphore::new(1),
            slot_cursor: AtomicUsize::new(0),
            session_map: Mutex::new(HashMap::new()),
            concurrency: Concurrency::single(),
            dump: None,
        }
    }

    /// 注入分阶段落盘工具：响应按开关落盘。
    pub fn with_dump(mut self, dump: Arc<DumpConfig>) -> Self {
        self.dump = Some(dump);
        self
    }

    /// 设置渠道并发：同时决定会话池容量与在途上限（网页渠道会话有状态，靠会话池并行）。
    pub fn with_concurrency(mut self, max_inflight: u32) -> Self {
        let max_inflight = max_inflight.max(1);
        self.pool_size = max_inflight;
        self.gate = tokio::sync::Semaphore::new(max_inflight as usize);
        self.concurrency = Concurrency::pooled(max_inflight);
        self
    }

    /// 注入会话池持久化存储：槽位跨进程沿用，重启后核对复用而不是重复创建。
    pub fn with_session_store(mut self, store: Arc<dyn SessionPoolStore>) -> Self {
        self.pool_store = Some(store);
        self
    }

    /// DeepSeek 网页默认请求头。
    fn default_headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![
            (
                "x-client-bundle-id".to_string(),
                "com.deepseek.chat".to_string(),
            ),
            ("x-client-platform".to_string(), "web".to_string()),
            ("x-client-version".to_string(), "2.4.0".to_string()),
            ("x-client-locale".to_string(), "zh_CN".to_string()),
            ("x-client-timezone-offset".to_string(), "28800".to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
        ];
        if let Some(token) = self.executor.token() {
            headers.push(("Authorization".to_string(), format!("Bearer {}", token)));
        }
        if let Some(settings_token) = self.executor.settings_token() {
            headers.push(("x-settings-token".to_string(), settings_token));
        }
        headers
    }

    /// 取访问令牌；渠道未捕获凭证时报错。
    fn token(&self) -> Result<String> {
        self.executor.token().ok_or_else(|| {
            anyhow!(
                "未获取到 DeepSeek 访问凭证：请先在浏览器打开 chat.deepseek.com 并登录，且插件已连接"
            )
        })
    }

    /// 发送一次渠道请求并解析 JSON 响应体。
    ///
    /// `session_key` 为本次请求所属的网页会话：传输层据此维持会话粘性（同一网页会话固定落到同一台插件），
    /// 与会话无关的辅助请求（创建 / 列表）传 None。
    async fn send(
        &self,
        method: &str,
        url: &str,
        body: Option<String>,
        session_key: Option<&str>,
    ) -> Result<Value> {
        let response = self
            .executor
            .execute(ChannelRequest {
                url: url.to_string(),
                method: method.to_string(),
                headers: self.default_headers(),
                body,
                session_key: session_key.map(str::to_string),
            })
            .await?;
        if !response.ok {
            anyhow::bail!(
                "渠道请求失败（HTTP {}）：{}",
                response.status,
                response.body.chars().take(200).collect::<String>()
            );
        }
        Ok(serde_json::from_str(&response.body).unwrap_or(Value::Null))
    }

    /// 取得并求解一次 PoW 挑战，返回 `x-ds-pow-response` 头值；失败时返回 None。
    async fn pow_header(&self, session_key: Option<&str>) -> Option<String> {
        let response = self
            .executor
            .execute(ChannelRequest {
                url: POW_CHALLENGE_URL.to_string(),
                method: "POST".to_string(),
                headers: self.default_headers(),
                body: Some(json!({ "target_path": "/api/v0/chat/completion" }).to_string()),
                session_key: session_key.map(str::to_string),
            })
            .await
            .ok()?;
        if !response.ok || response.body.is_empty() {
            warn!("PoW 挑战获取失败（HTTP {}）", response.status);
            return None;
        }
        let json: Value = serde_json::from_str(&response.body).ok()?;
        let challenge = json
            .get("data")
            .and_then(|d| d.get("biz_data"))
            .and_then(|b| b.get("challenge"))
            .or_else(|| json.get("data").and_then(|d| d.get("challenge")))
            .or_else(|| json.get("challenge"))?;
        if !challenge.is_object() {
            return None;
        }
        let algorithm = challenge
            .get("algorithm")
            .and_then(Value::as_str)
            .unwrap_or("DeepSeekHashV1");
        let challenge_hex = challenge
            .get("challenge")
            .and_then(Value::as_str)
            .unwrap_or("");
        let salt = challenge.get("salt").and_then(Value::as_str).unwrap_or("");
        let expire_at = challenge
            .get("expire_at")
            .or_else(|| challenge.get("expireAt"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let difficulty = challenge
            .get("difficulty")
            .and_then(Value::as_u64)
            .unwrap_or(144_000)
            .max(1);
        let signature = challenge
            .get("signature")
            .and_then(Value::as_str)
            .unwrap_or("");
        let target_path = challenge
            .get("target_path")
            .and_then(Value::as_str)
            .unwrap_or("/api/v0/chat/completion");
        let answer = solve_pow(challenge_hex, salt, expire_at, difficulty);
        if answer < 0 {
            warn!("PoW 求解失败（difficulty 范围内未找到答案）");
            return None;
        }
        Some(build_pow_header(
            algorithm,
            challenge_hex,
            salt,
            answer,
            signature,
            target_path,
        ))
    }

    /// 记录会话最近一次响应的消息 id，供下轮请求的 parent_message_id 使用。
    fn remember_message_id(&self, session_id: &str, message_id: Option<i64>) {
        if let Some(id) = message_id {
            if id > 0 {
                self.session_map
                    .lock()
                    .unwrap()
                    .insert(session_id.to_string(), id);
            }
        }
    }

    /// 本轮请求的 parent_message_id：有已知的上一轮响应消息 id 时用它，否则为 null。
    fn parent_message_id(&self, session_id: &str) -> Value {
        match self.session_map.lock().unwrap().get(session_id).copied() {
            Some(id) if id > 0 => Value::Number(id.into()),
            _ => Value::Null,
        }
    }

    /// 把推理请求拼成渠道所需的 prompt，顺序固定为：
    /// 固定前缀 → 系统提示词 → 历史对话 → 工具调用格式与限定规则 → 可用工具 → 最新一条记录。
    ///
    /// 工具结果的标题带上工具名与调用 id，与助手标记里那次 `invoke` 一一对应。
    /// 提示词组装不属于模型；这里只做渠道所需的最小拼接。
    fn prompt_of(request: &InferRequest) -> String {
        let mut sections: Vec<String> = vec![PROMPT_PREFIX.to_string()];

        let system = Self::render_system(&request.messages);
        if !system.is_empty() {
            sections.push(format!("{}\n{}", SYSTEM_HEADING, system));
        }

        // system 消息已单独成段，其余消息里末尾那条是最新记录，之前的都算历史对话。
        let turns: Vec<&Value> = request
            .messages
            .iter()
            .filter(|message| !Self::is_system(message))
            .collect();
        let Some((latest, history)) = turns.split_last() else {
            return sections.join("\n\n");
        };
        let tool_names = Self::tool_names(&request.messages);
        let history = history
            .iter()
            .filter_map(|message| Self::render_message(message, &tool_names))
            .collect::<Vec<_>>();
        if !history.is_empty() {
            sections.push(format!("{}\n{}", HISTORY_HEADING, history.join("\n\n")));
        }
        // 工具段只在确实有工具时出现：格式规则与工具清单成对给出。
        if let Some(tools) = Self::render_tools(request.tools.as_ref()) {
            sections.push(format!("{}\n{}", TOOL_RULE_HEADING, TOOL_RULE));
            sections.push(format!("{}\n{}", TOOLS_HEADING, tools));
        }
        if let Some(latest) = Self::render_message(latest, &tool_names) {
            sections.push(latest);
        }
        sections.join("\n\n")
    }

    /// 是否为 system 消息。
    fn is_system(message: &Value) -> bool {
        message.get("role").and_then(Value::as_str) == Some("system")
    }

    /// 系统提示词段：取全部 system 消息正文，多条之间空行分隔。
    fn render_system(messages: &[Value]) -> String {
        messages
            .iter()
            .filter(|message| Self::is_system(message))
            .map(Self::message_content)
            .filter(|content| !content.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// 取消息正文：字符串，或多模态内容数组里的文本片段；非文本片段只留占位标记，
    /// 避免把图片的 base64 塞进提示词。
    fn message_content(message: &Value) -> String {
        match message.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .map(Self::render_part)
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        }
    }

    /// 单条消息 → `【角色】正文`；助手消息里的历史工具调用按 DSML 标记接在正文之后。
    ///
    /// 正文只有空白时按空处理：模型写在标记前后的排版、客户端存下的空段都不算回答内容，
    /// 原样写进提示词只会变成大段空行。
    /// 工具结果消息的标题带上工具名与调用 id，模型才能把它和上面那次 `invoke` 一一对上。
    fn render_message(message: &Value, tool_names: &HashMap<String, String>) -> Option<String> {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        let mut content = Self::message_content(message);
        if content.trim().is_empty() {
            content.clear();
        }
        for call in Self::render_tool_calls(message) {
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str(&call);
        }
        if role.is_empty() && content.is_empty() {
            return None;
        }
        let heading = match Self::tool_result_label(message, tool_names) {
            Some(label) => format!("【{}】{}", role, label),
            None => format!("【{}】", role),
        };
        if content.is_empty() {
            return Some(heading);
        }
        Some(format!("{}\n{}", heading, content))
    }

    /// 工具结果消息的来源标签：`name=工具名 id=调用id`，缺哪半就只写另一半，都缺则不标。
    ///
    /// 字段名跟着写上，模型不必猜哪个是工具名、哪个是调用编号。
    /// 工具名优先取消息自带的 `name`，没有就按 `tool_call_id` 到本轮全部助手消息的
    /// `tool_calls` 里反查——客户端通常只回 `tool_call_id`。
    fn tool_result_label(message: &Value, tool_names: &HashMap<String, String>) -> Option<String> {
        if message.get("role").and_then(Value::as_str) != Some("tool") {
            return None;
        }
        let id = message.get("tool_call_id").and_then(Value::as_str);
        let name = message
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| id.and_then(|id| tool_names.get(id).map(String::as_str)));
        match (name, id) {
            (Some(name), Some(id)) => Some(format!("name={name} id={id}")),
            (Some(name), None) => Some(format!("name={name}")),
            (None, Some(id)) => Some(format!("id={id}")),
            (None, None) => None,
        }
    }

    /// 全部助手消息里的 `tool_calls` → `调用id → 工具名`，供工具结果标注来源。
    fn tool_names(messages: &[Value]) -> HashMap<String, String> {
        let mut names = HashMap::new();
        for message in messages {
            let Some(calls) = message.get("tool_calls").and_then(Value::as_array) else {
                continue;
            };
            for call in calls {
                let Some(id) = call.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let function = call.get("function").unwrap_or(call);
                if let Some(name) = function.get("name").and_then(Value::as_str) {
                    names.insert(id.to_string(), name.to_string());
                }
            }
        }
        names
    }

    /// 助手消息里的 `tool_calls` → DSML 标记，一次调用一层 `invoke`，多个平铺在一个 `calls` 块里。
    ///
    /// 参数是字符串时按 JSON 解析成对象，解析不了就原样保留。渲染出来的标记与解析器认的格式一致：
    /// 字符串参数标 `string="true"` 原样写，其余（数字 / 布尔 / 数组 / 对象）标 `string="false"` 按 JSON 写，
    /// 这样同一段标记被解析回来仍是同一个调用（见 `history_markup_round_trips_through_parser`）。
    /// 调用带 `id` 时一并写进 `invoke` 属性（解析器不读它），与工具结果标题上的 id 对应。
    fn render_tool_calls(message: &Value) -> Vec<String> {
        let Some(calls) = message.get("tool_calls").and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        for call in calls {
            let function = call.get("function").unwrap_or(call);
            let Some(name) = function.get("name").and_then(Value::as_str) else {
                continue;
            };
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .map(|id| format!(" id=\"{id}\""))
                .unwrap_or_default();
            let arguments = match function.get("arguments") {
                Some(Value::String(raw)) => {
                    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.clone()))
                }
                Some(other) => other.clone(),
                None => json!({}),
            };
            let Some(params) = arguments.as_object() else {
                // 参数不是对象（客户端给了坏数据）：只还原工具名，别编造参数
                warn!(
                    "历史工具调用的 arguments 不是 JSON 对象，按无参还原：{}",
                    name
                );
                lines.push(format!("{INVOKE_OPEN}{name}\"{id}>{INVOKE_CLOSE}"));
                continue;
            };
            lines.push(format!("{INVOKE_OPEN}{name}\"{id}>"));
            for (key, value) in params {
                // 字符串参数按解析端的反转义规则转义，保证渲染后再解析回来还是同一个值
                let (flag, literal) = match value {
                    Value::String(text) => ("true", escape(text)),
                    other => ("false", other.to_string()),
                };
                lines.push(format!(
                    "{PARAMETER_OPEN}{key}\" string=\"{flag}\">{literal}{PARAMETER_CLOSE}"
                ));
            }
            lines.push(INVOKE_CLOSE.to_string());
        }
        if lines.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(lines.len() + 2);
        out.push(CALLS_OPEN.to_string());
        out.extend(lines);
        out.push(CALLS_CLOSE.to_string());
        out
    }

    /// 多模态内容片段 → 文本；非文本片段给占位标记（如 `[image_url]`）。
    fn render_part(part: &Value) -> String {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            return text.to_string();
        }
        let kind = part
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("未知片段");
        format!("[{}]", kind)
    }

    /// 工具清单：每项一行 `- 名称：描述`，参数以紧凑 JSON 附在其后；没有可用工具时返回 None。
    fn render_tools(tools: Option<&Value>) -> Option<String> {
        let items = tools
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())?;
        let lines = items
            .iter()
            .filter_map(|item| {
                let function = item.get("function").unwrap_or(item);
                let name = function.get("name").and_then(Value::as_str).unwrap_or("");
                if name.is_empty() {
                    return None;
                }
                let mut line = format!("- {}", name);
                if let Some(description) = function.get("description").and_then(Value::as_str) {
                    if !description.is_empty() {
                        line.push_str(&format!("：{}", description));
                    }
                }
                if let Some(parameters) = function.get("parameters") {
                    line.push_str(&format!("\n  参数：{}", parameters));
                }
                Some(line)
            })
            .collect::<Vec<_>>();
        if lines.is_empty() {
            return None;
        }
        Some(lines.join("\n"))
    }

    /// 聊天完成请求体。
    fn completion_body(&self, session_id: &str, parent_message_id: &Value, prompt: &str) -> String {
        json!({
            "chat_session_id": session_id,
            "parent_message_id": parent_message_id,
            "model_type": MODEL_TYPE,
            "prompt": prompt,
            "thinking_enabled": THINKING_ENABLED,
            "ref_file_ids": [],
            "search_enabled": false,
        })
        .to_string()
    }

    /// 聊天头（含 PoW 应答，取不到则不附带）。
    async fn completion_headers(&self, session_key: Option<&str>) -> Vec<(String, String)> {
        let mut headers = self.default_headers();
        if let Some(pow) = self.pow_header(session_key).await {
            headers.push(("x-ds-pow-response".to_string(), pow));
        }
        headers
    }

    /// 聊天共用实现：`on_chunk` 为 Some 时流式回吐正文增量，工具调用在收尾时解析。
    ///
    /// 每次请求是无状态的（整段对话都拼在提示词里），因此不绑定调用方会话：
    /// 先取并发闸门，再从会话池轮转一个网页会话独占使用，池容量即可并发的请求数。
    async fn chat(
        &self,
        request: InferRequest,
        on_chunk: Option<ChunkSink>,
    ) -> Result<ModelResponse> {
        self.token()?;
        let prompt = Self::prompt_of(&request);
        // 请求没带工具时不做工具检测：模型就算写了 DSML 标记也只当正文。
        let recognize_tools = request
            .tools
            .as_ref()
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty());
        let _slot_permit = self
            .gate
            .acquire()
            .await
            .map_err(|_| anyhow!("渠道并发闸门已关闭"))?;
        let session_id = self.acquire_pool_slot().await?;
        // 用之前核对：槽位上的会话若已被网页端删掉，这里会补建一个，别让整轮请求白跑
        let session_id = self.ensure_pool_session(session_id).await?;
        let parent_message_id = self.parent_message_id(&session_id);
        info!(
            "DeepSeek 网页推理 session={} parent_message_id={} prompt_len={}",
            session_id,
            parent_message_id,
            prompt.len()
        );
        let channel_request = ChannelRequest {
            url: COMPLETION_URL.to_string(),
            method: "POST".to_string(),
            headers: self.completion_headers(Some(&session_id)).await,
            body: Some(self.completion_body(&session_id, &parent_message_id, &prompt)),
            session_key: Some(session_id.clone()),
        };

        let (text, reasoning, tool_calls, tool_call_raw, message_id, status, raw_body) =
            match on_chunk {
                Some(user_sink) => {
                    // 帧处理器与增量处理器各自独立：闭包是 FnMut，跨 chunk 的状态用 Arc<Mutex> 共享。
                    let frames = Arc::new(Mutex::new(SseFrameProcessor::new()));
                    let processor = Arc::new(Mutex::new(ChatStreamProcessor::new(
                        user_sink,
                        recognize_tools,
                    )));
                    let feeder_frames = frames.clone();
                    let feeder_processor = processor.clone();
                    let response = self
                        .executor
                        .execute_stream(
                            channel_request,
                            Box::new(move |raw| {
                                let (Ok(mut frames), Ok(mut processor)) =
                                    (feeder_frames.lock(), feeder_processor.lock())
                                else {
                                    return;
                                };
                                processor.feed(frames.feed(&raw));
                            }),
                        )
                        .await?;
                    if !response.ok {
                        anyhow::bail!("DeepSeek 网页推理失败（HTTP {}）", response.status);
                    }
                    // 缓冲区里可能还留着没换行的尾巴：收尾时一并放出，再让处理器补完检测。
                    let mut processor = processor.lock().unwrap();
                    let (tail, message_id) = {
                        let mut frames = frames.lock().unwrap();
                        (frames.finish(), frames.message_id())
                    };
                    processor.feed(tail);
                    processor.finish();
                    let text = processor.text().to_string();
                    let reasoning = processor.reasoning().to_string();
                    let tool_calls = processor.tool_calls().to_vec();
                    let tool_call_raw = processor.tool_call_raw().to_string();
                    (
                        text,
                        reasoning,
                        tool_calls,
                        tool_call_raw,
                        message_id,
                        response.status,
                        response.body,
                    )
                }
                None => {
                    let response = self.executor.execute(channel_request).await?;
                    if !response.ok {
                        anyhow::bail!("DeepSeek 网页推理失败（HTTP {}）", response.status);
                    }
                    let (processor, message_id) = parse_sse_text(&response.body, recognize_tools);
                    (
                        processor.text().to_string(),
                        processor.reasoning().to_string(),
                        processor.tool_calls().to_vec(),
                        processor.tool_call_raw().to_string(),
                        message_id,
                        response.status,
                        response.body,
                    )
                }
            };
        // 记下本轮响应的消息 id：下一轮请求的 parent_message_id 靠它接上服务端的消息链。
        self.remember_message_id(&session_id, message_id);

        let has_calls = !tool_calls.is_empty();
        let finish_reason = if has_calls { "tool_calls" } else { "stop" };
        // 响应落盘（开发期排查）：落的是解析后的内容，不影响返回给客户端的结果。
        if let Some(dump) = &self.dump {
            if dump.response_enabled() {
                let payload = ResponseDump {
                    received_at: chrono::Utc::now().to_rfc3339(),
                    stage: "response",
                    model: request.model.clone(),
                    channel: CHANNEL,
                    web_session: Some(session_id.clone()),
                    status,
                    raw_len: raw_body.len(),
                    reasoning: reasoning.clone(),
                    reasoning_len: reasoning.chars().count(),
                    text: text.clone(),
                    text_len: text.chars().count(),
                    tool_calls: Value::Array(tool_calls.clone()),
                    finish_reason,
                };
                let path = dump.write_response(&payload)?;
                info!("响应已落盘：{}", path.display());
            }
        }
        // 工具调用单独落一份（模型吐的原文 + 解析出的调用）：排查「模型到底写了什么」时
        // 不必再从响应快照里反推标记。
        if has_calls {
            if let Some(dump) = &self.dump {
                if dump.response_enabled() {
                    let payload = ToolCallDump {
                        received_at: chrono::Utc::now().to_rfc3339(),
                        stage: "tool-call",
                        model: request.model.clone(),
                        channel: CHANNEL,
                        web_session: Some(session_id.clone()),
                        status,
                        raw_markup: tool_call_raw.clone(),
                        raw_markup_len: tool_call_raw.chars().count(),
                        calls: Value::Array(tool_calls.clone()),
                        // `calls` 里 arguments 是 JSON 字符串，在 JSON 文件里会被再转义一层
                        // （Windows 路径看着像多了一层反斜杠）；这里给一份客户端解析后的形态。
                        parsed: Value::Array(
                            tool_calls
                                .iter()
                                .map(|call| {
                                    let function = &call["function"];
                                    let arguments = function["arguments"]
                                        .as_str()
                                        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                                        .unwrap_or(Value::Null);
                                    json!({ "name": function["name"], "arguments": arguments })
                                })
                                .collect(),
                        ),
                    };
                    let path = dump.write_tool_calls(&payload)?;
                    info!("工具调用已落盘：{}", path.display());
                }
            }
        }
        if text.trim().is_empty() && !has_calls {
            warn!(
                "网页回答为空：session={} status={} 未解析出正文与工具调用，原始响应：{}",
                session_id,
                status,
                raw_body.chars().take(300).collect::<String>()
            );
        } else {
            // 会话这时才有内容，网页端接受改名：把池内会话命名成 poolman_<槽位序号>。
            self.ensure_pool_title(&session_id).await;
        }
        Ok(ModelResponse {
            text,
            reasoning: if reasoning.is_empty() {
                None
            } else {
                Some(reasoning)
            },
            tool_calls: if has_calls {
                Some(Value::Array(tool_calls))
            } else {
                None
            },
            usage: None,
            finish_reason: Some(finish_reason.to_string()),
            is_complete: true,
        })
    }
}

/// 响应阶段落盘快照（开发期排查用）：网页返回经 `SseFrameProcessor` 解析后的内容。
#[derive(Debug, Serialize)]
struct ResponseDump {
    received_at: String,
    /// 阶段标识，便于与请求阶段的快照区分。
    stage: &'static str,
    /// 对外模型名。
    model: String,
    /// 承载它的渠道。
    channel: &'static str,
    /// 执行本次请求的渠道会话。
    web_session: Option<String>,
    /// 网页返回的 HTTP 状态。
    status: u16,
    /// 网页原始返回文本长度（流式下原始文本不落盘，只记长度）。
    raw_len: usize,
    /// 思考内容（THINK 片段）。
    reasoning: String,
    reasoning_len: usize,
    /// 正文（不含思考与工具调用标记）。
    text: String,
    text_len: usize,
    /// 解析出的工具调用（OpenAI 标准 tool_calls 数组，无调用时为空数组）。
    tool_calls: Value,
    finish_reason: &'static str,
}

/// 工具调用阶段的落盘快照（开发期排查用）：模型吐出的 DSML 原文 + 解析出的调用。
///
/// 与响应快照分开成文件：工具调用一有就落一份，排查时直接把原文和结果对着看。
#[derive(Debug, Serialize)]
struct ToolCallDump {
    received_at: String,
    /// 阶段标识，便于与请求 / 响应阶段的快照区分。
    stage: &'static str,
    /// 对外模型名。
    model: String,
    /// 承载它的渠道。
    channel: &'static str,
    /// 执行本次请求的渠道会话。
    web_session: Option<String>,
    /// 网页返回的 HTTP 状态。
    status: u16,
    /// 检测器吃掉的 DSML 原文：标记逐字保留（含块内换行与参数值），即「模型吐了什么」。
    raw_markup: String,
    /// 原始文本长度（字符数）。
    raw_markup_len: usize,
    /// 原文解析出的工具调用（OpenAI 标准 tool_calls 数组），即「解析出了什么」。
    ///
    /// 注意 `arguments` 按 OpenAI 契约是 **JSON 字符串**，在 JSON 文件里会被再转义一层：
    /// 想看可读形态（路径不会被看成多一层反斜杠）用 `parsed`。
    calls: Value,
    /// 同一批调用的可读形态：`arguments` 是对象，等于客户端 `JSON.parse(arguments)` 之后的形态。
    parsed: Value,
}

impl Model for DeepSeekWebModel {
    fn id(&self) -> &str {
        CHANNEL
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    fn capability(&self) -> ModelCapability {
        ModelCapability::basic()
    }

    fn concurrency(&self) -> Concurrency {
        self.concurrency
    }

    fn infer<'a>(&'a self, request: InferRequest) -> BoxFuture<'a, Result<ModelResponse>> {
        Box::pin(async move { self.chat(request, None).await })
    }

    fn infer_stream<'a>(
        &'a self,
        request: InferRequest,
        on_chunk: ChunkSink,
    ) -> BoxFuture<'a, Result<ModelResponse>> {
        Box::pin(async move { self.chat(request, Some(on_chunk)).await })
    }
}

impl PluginModel for DeepSeekWebModel {
    fn channel(&self) -> &str {
        CHANNEL
    }

    fn create_channel_session(&self) -> BoxFuture<'_, Result<Option<String>>> {
        Box::pin(async move {
            let json = self
                .send("POST", SESSION_CREATE_URL, Some("{}".to_string()), None)
                .await?;
            let data = json.get("data");
            let candidates = [
                data.and_then(|d| d.get("biz_data"))
                    .and_then(|b| b.get("chat_session"))
                    .and_then(|s| s.get("id")),
                data.and_then(|d| d.get("biz_data"))
                    .and_then(|b| b.get("chat_session_id")),
                data.and_then(|d| d.get("biz_data"))
                    .and_then(|b| b.get("id")),
                data.and_then(|d| d.get("chat_session_id")),
                data.and_then(|d| d.get("id")),
                json.get("chat_session_id"),
            ];
            for candidate in candidates {
                if let Some(id) = candidate.and_then(Value::as_str) {
                    if id.len() > 10 {
                        debug!("已创建 DeepSeek 会话 {}", id);
                        return Ok(Some(id.to_string()));
                    }
                }
            }
            Ok(None)
        })
    }

    fn list_channel_sessions(&self) -> BoxFuture<'_, Result<Value>> {
        Box::pin(async move {
            let url = format!("{SESSION_LIST_URL}?lte_cursor.pinned=false");
            let json = self.send("GET", &url, None, None).await?;
            Ok(extract_sessions(&json))
        })
    }

    fn delete_channel_session<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let body = json!({ "chat_session_id": session_id }).to_string();
            let json = self
                .send("POST", SESSION_DELETE_URL, Some(body), Some(session_id))
                .await?;
            Ok(is_biz_ok(&json))
        })
    }

    fn rename_channel_session<'a>(
        &'a self,
        session_id: &'a str,
        title: &'a str,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let body = json!({ "chat_session_id": session_id, "title": title }).to_string();
            let response = self
                .executor
                .execute(ChannelRequest {
                    url: SESSION_TITLE_URL.to_string(),
                    method: "POST".to_string(),
                    headers: self.default_headers(),
                    body: Some(body),
                    session_key: Some(session_id.to_string()),
                })
                .await?;
            if !response.ok {
                warn!("设置会话标题失败（HTTP {}）", response.status);
                return Ok(false);
            }
            if response.body.is_empty() {
                return Ok(true);
            }
            let json: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
            Ok(is_biz_ok(&json))
        })
    }

    fn channel_history<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let url = format!(
                "{}?chat_session_id={}&cache_version=8",
                HISTORY_URL,
                encode_query(session_id)
            );
            self.send("GET", &url, None, Some(session_id)).await
        })
    }

    fn prepare_channel_sessions(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { self.prepare_session_pool().await })
    }
}

/// 从会话列表响应里抽取会话数组；无法识别时原样返回。
fn extract_sessions(json: &Value) -> Value {
    json.get("data")
        .and_then(|data| data.get("biz_data"))
        .and_then(|biz| biz.get("chat_sessions"))
        .cloned()
        .filter(Value::is_array)
        .unwrap_or_else(|| json.clone())
}

/// 会话接口业务成功判定：`code == 0` 且 `data.biz_code == 0`（缺失按 0 计）。
fn is_biz_ok(json: &Value) -> bool {
    let code_ok = json.get("code").and_then(Value::as_i64).unwrap_or(0) == 0;
    let biz_ok = json
        .get("data")
        .and_then(|d| d.get("biz_code"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
        == 0;
    code_ok && biz_ok
}

/// 解析整段响应文本（非流式情形）：返回收尾后的增量处理器（供读取正文 / 思考 / 工具调用）
/// 与响应消息 id（供下轮 `parent_message_id`）。
fn parse_sse_text(body: &str, recognize_tools: bool) -> (ChatStreamProcessor, Option<i64>) {
    let mut frames = SseFrameProcessor::new();
    let mut processor = ChatStreamProcessor::new(Box::new(|_| {}), recognize_tools);
    processor.feed(frames.feed(body));
    // 整段文本的最后一行可能没有换行符，收尾时补处理。
    processor.feed(frames.finish());
    processor.finish();
    (processor, frames.message_id())
}

/// 按 RFC 3986 对查询参数值做百分号编码。
fn encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::session_pool::POOL_TITLE_PREFIX;
    use super::{DeepSeekWebModel, CHANNEL, PROMPT_PREFIX};
    use crate::models::defs::{
        ChannelExecutor, ChannelRequest, ChannelResponse, ChunkSink, InferRequest, PooledSession,
        SessionPoolStore,
    };
    use crate::models::model::Model;
    use crate::models::plugin::PluginModel;
    use futures_util::future::BoxFuture;
    use serde_json::{json, Value};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// 记录请求并按 URL 返回预设响应。
    ///
    /// 会话接口是一个小型的网页端仿真：创建会登记新会话、改标题会改标题、删除会移除会话，
    /// 会话列表返回当前登记的会话——会话池据此核对槽位。
    struct MockExecutor {
        requests: Mutex<Vec<ChannelRequest>>,
        completion_body: String,
        /// 流式分块粒度（按字符数切分）；0 表示整段一次送出。
        stream_chunk: usize,
        /// `history_messages` 的响应体；None 时给一份空历史。
        history_body: Option<String>,
        /// 网页端当前存在的会话（会话 id → 标题），按 id 排序便于断言。
        sessions: Mutex<BTreeMap<String, String>>,
        /// 创建接口的会话编号：保证每次创建拿到不同的会话 id。
        created: AtomicUsize,
        /// 会话列表取不到（模拟「启动时插件还没连上」）。
        list_unavailable: AtomicBool,
    }

    impl MockExecutor {
        /// 会话 id 形如 `sess-00000000`，长度足以通过渠道侧的会话 id 校验。
        fn new_session_id(&self) -> String {
            format!("sess-{:08}", self.created.fetch_add(1, Ordering::SeqCst))
        }

        /// 预先在网页端登记一个会话（模拟已存在的会话）。
        fn seed_session(&self, id: &str, title: &str) {
            self.sessions
                .lock()
                .unwrap()
                .insert(id.to_string(), title.to_string());
        }

        /// 网页端当前存在的会话（会话 id → 标题）。
        fn sessions(&self) -> BTreeMap<String, String> {
            self.sessions.lock().unwrap().clone()
        }

        /// completion 请求体（按发生顺序）。
        fn completion_bodies(&self) -> Vec<Value> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.url.ends_with("/chat/completion"))
                .map(|request| {
                    serde_json::from_str(request.body.as_deref().unwrap_or("{}"))
                        .expect("completion 请求体应是 JSON")
                })
                .collect()
        }

        /// 创建会话的请求次数。
        fn create_count(&self) -> usize {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.url.ends_with("/chat_session/create"))
                .count()
        }

        /// 改名请求次数。
        fn rename_count(&self) -> usize {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.url.ends_with("/chat_session/update_title"))
                .count()
        }

        /// 让会话列表取不到（模拟启动时插件尚未连上）。
        fn set_list_unavailable(&self, unavailable: bool) {
            self.list_unavailable
                .store(unavailable, std::sync::atomic::Ordering::SeqCst);
        }

        /// 从请求里取 `chat_session_id`：会话管理接口放在正文里，历史接口放在查询串里。
        fn session_id_of(request: &ChannelRequest) -> Option<String> {
            let from_body = request
                .body
                .as_deref()
                .and_then(|body| serde_json::from_str::<Value>(body).ok())
                .and_then(|body| {
                    body.get("chat_session_id")
                        .and_then(Value::as_str)
                        .map(String::from)
                });
            from_body.or_else(|| {
                let query = request.url.split_once("chat_session_id=")?.1;
                query.split('&').next().map(String::from)
            })
        }
    }

    impl ChannelExecutor for MockExecutor {
        fn token(&self) -> Option<String> {
            Some("tok".to_string())
        }

        fn settings_token(&self) -> Option<String> {
            None
        }

        fn execute(
            &self,
            request: ChannelRequest,
        ) -> BoxFuture<'_, anyhow::Result<ChannelResponse>> {
            Box::pin(async move {
                let body = if request.url.contains("chat_session/create") {
                    let id = self.new_session_id();
                    self.seed_session(&id, "");
                    json!({ "data": { "biz_data": { "chat_session": { "id": id } } } }).to_string()
                } else if request.url.contains("create_pow_challenge") {
                    json!({ "data": {} }).to_string()
                } else if request.url.contains("fetch_page") {
                    let sessions: Vec<Value> = self
                        .sessions()
                        .into_iter()
                        .map(|(id, title)| json!({ "id": id, "title": title }))
                        .collect();
                    json!({ "data": { "biz_data": { "chat_sessions": sessions } } }).to_string()
                } else if request.url.contains("history_messages") {
                    if let Some(body) = self.history_body.clone() {
                        body
                    } else {
                        let known = MockExecutor::session_id_of(&request)
                            .is_some_and(|id| self.sessions.lock().unwrap().contains_key(&id));
                        if known {
                            json!({ "data": { "biz_data": { "chat_messages": [] } } }).to_string()
                        } else {
                            // 网页端不认这个会话 id（与真实接口同形）
                            json!({
                                "code": 0,
                                "msg": "",
                                "data": {
                                    "biz_code": 1,
                                    "biz_msg": "invalid chat session id"
                                }
                            })
                            .to_string()
                        }
                    }
                } else if request.url.contains("chat_session/delete") {
                    if let Some(id) = MockExecutor::session_id_of(&request) {
                        self.sessions.lock().unwrap().remove(&id);
                    }
                    json!({ "code": 0, "data": { "biz_code": 0 } }).to_string()
                } else if request.url.contains("chat_session/update_title") {
                    let id = MockExecutor::session_id_of(&request);
                    // 网页端要求会话已有内容才接受改名：这个会话还没跑过对话就拒绝（与真实接口同形）
                    let has_content = id.as_ref().is_some_and(|id| {
                        self.requests.lock().unwrap().iter().any(|done| {
                            done.url.ends_with("/chat/completion")
                                && MockExecutor::session_id_of(done).as_deref() == Some(id.as_str())
                        })
                    });
                    if !has_content {
                        json!({
                            "code": 0,
                            "msg": "",
                            "data": { "biz_code": 5, "biz_msg": "EMPTY_CHAT_SESSION" }
                        })
                        .to_string()
                    } else {
                        if let Some(id) = id {
                            let title = request
                                .body
                                .as_deref()
                                .and_then(|body| serde_json::from_str::<Value>(body).ok())
                                .and_then(|body| {
                                    body.get("title").and_then(Value::as_str).map(String::from)
                                })
                                .unwrap_or_default();
                            // 只改已存在的会话：改标题不会凭空造出会话
                            if let Some(entry) = self.sessions.lock().unwrap().get_mut(&id) {
                                *entry = title;
                            }
                        }
                        json!({ "code": 0, "data": { "biz_code": 0 } }).to_string()
                    }
                } else {
                    self.completion_body.clone()
                };
                // 会话列表取不到：模拟插件尚未连上时 `list_channel_sessions` 失败
                let list_down = self
                    .list_unavailable
                    .load(std::sync::atomic::Ordering::SeqCst)
                    && request.url.contains("fetch_page");
                self.requests.lock().unwrap().push(request);
                Ok(ChannelResponse {
                    ok: !list_down,
                    status: if list_down { 503 } else { 200 },
                    body,
                })
            })
        }

        fn execute_stream(
            &self,
            request: ChannelRequest,
            on_chunk: ChunkSink,
        ) -> BoxFuture<'_, anyhow::Result<ChannelResponse>> {
            Box::pin(async move {
                let body = self.completion_body.clone();
                let mut on_chunk = on_chunk;
                if self.stream_chunk == 0 {
                    on_chunk(body.clone());
                } else {
                    let chars: Vec<char> = body.chars().collect();
                    for piece in chars.chunks(self.stream_chunk) {
                        on_chunk(piece.iter().collect());
                    }
                }
                self.requests.lock().unwrap().push(request);
                Ok(ChannelResponse {
                    ok: true,
                    status: 200,
                    body,
                })
            })
        }
    }

    fn model(completion_body: &str) -> DeepSeekWebModel {
        model_split(completion_body, 0)
    }

    /// 新建一个仿真执行器（会话接口带状态）。
    fn executor(completion_body: &str, stream_chunk: usize) -> Arc<MockExecutor> {
        executor_with(completion_body, stream_chunk, None)
    }

    /// 新建一个仿真执行器，可指定会话历史的响应体（已有消息链的网页会话）。
    fn executor_with(
        completion_body: &str,
        stream_chunk: usize,
        history_body: Option<String>,
    ) -> Arc<MockExecutor> {
        Arc::new(MockExecutor {
            requests: Mutex::new(Vec::new()),
            completion_body: completion_body.to_string(),
            stream_chunk,
            history_body,
            sessions: Mutex::new(BTreeMap::new()),
            created: AtomicUsize::new(0),
            list_unavailable: AtomicBool::new(false),
        })
    }

    /// 带分块粒度的模型：验证被任意边界切开的 DSML 标记。
    fn model_split(completion_body: &str, stream_chunk: usize) -> DeepSeekWebModel {
        DeepSeekWebModel::new(
            "DeepSeek",
            executor(completion_body, stream_chunk) as Arc<dyn ChannelExecutor>,
        )
    }

    /// 内存版会话池存储：跨「进程」复用同一份槽位表。
    #[derive(Default)]
    struct MemoryStore {
        slots: Mutex<BTreeMap<String, Vec<PooledSession>>>,
    }

    impl SessionPoolStore for MemoryStore {
        fn load<'a>(
            &'a self,
            channel: &'a str,
        ) -> BoxFuture<'a, anyhow::Result<Vec<PooledSession>>> {
            Box::pin(async move {
                Ok(self
                    .slots
                    .lock()
                    .unwrap()
                    .get(channel)
                    .cloned()
                    .unwrap_or_default())
            })
        }

        fn save<'a>(
            &'a self,
            channel: &'a str,
            sessions: &'a [PooledSession],
        ) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                self.slots
                    .lock()
                    .unwrap()
                    .insert(channel.to_string(), sessions.to_vec());
                Ok(())
            })
        }
    }

    /// 会话池容量为 `size`、槽位落库到 `store` 的模型。
    fn pooled_model(
        executor: &Arc<MockExecutor>,
        store: &Arc<MemoryStore>,
        size: u32,
    ) -> DeepSeekWebModel {
        DeepSeekWebModel::new("DeepSeek", executor.clone() as Arc<dyn ChannelExecutor>)
            .with_concurrency(size)
            .with_session_store(store.clone())
    }

    /// 槽位表里已登记的会话 id（按槽位序号）。
    fn stored_sessions(store: &Arc<MemoryStore>) -> Vec<String> {
        store
            .slots
            .lock()
            .unwrap()
            .get(CHANNEL)
            .map(|sessions| {
                sessions
                    .iter()
                    .map(|session| session.session_id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn user_request() -> InferRequest {
        InferRequest::new(
            "deepseek-web",
            vec![json!({ "role": "user", "content": "hi" })],
        )
    }

    /// 带工具的请求。
    fn tool_request() -> InferRequest {
        let mut request = user_request();
        request.tools = Some(json!([{
            "type": "function",
            "function": { "name": "read_file", "description": "读取文件" }
        }]));
        request
    }

    /// 一个 `response/content` SSE 帧。
    fn content_frame(text: &str) -> String {
        format!("data: {}\n", json!({ "p": "response/content", "v": text }))
    }

    /// 一个 DSML `invoke`（三层结构里的中间层，参数一律按字符串）。
    fn dsml_invoke(name: &str, params: &[(&str, &str)]) -> String {
        let mut out = format!("<｜｜DSML｜｜ invoke name=\"{name}\">");
        for (key, value) in params {
            out.push_str(&format!(
                "<｜｜DSML｜｜ parameter name=\"{key}\" string=\"true\">{value}</｜｜DSML｜｜ parameter>"
            ));
        }
        out.push_str("</｜｜DSML｜｜ invoke>");
        out
    }

    /// 把若干 `invoke` 包成一个 DSML `calls` 块。
    fn dsml_calls(invokes: &[String]) -> String {
        format!(
            "<｜｜DSML｜｜ calls>{}</｜｜DSML｜｜ calls>",
            invokes.concat()
        )
    }

    /// Model 基础：标识、能力、并发，以及非流式推理抽取正文。
    #[tokio::test]
    async fn infer_extracts_content_and_is_single_concurrency() {
        let body = concat!(
            "data: {\"p\":\"response/thinking_content\",\"v\":\"think\"}\n",
            "data: {\"p\":\"response/content\",\"v\":\"你好\"}\n",
            "data: {\"p\":\"response/content\",\"v\":\"世界\"}\n",
        );
        let model = model(body);

        assert_eq!(model.id(), CHANNEL);
        assert_eq!(model.display_name(), "DeepSeek");
        assert_eq!(model.channel(), "deepseek-web");
        assert!(model.capability().stream);
        assert!(model.concurrency().per_session_serial);

        let response = model.infer(user_request()).await.expect("推理应成功");
        assert_eq!(response.text, "你好世界");
        assert!(response.is_complete);
    }

    /// 流式：正文增量按序回吐，最终结果累计完整。
    #[tokio::test]
    async fn infer_stream_emits_deltas() {
        // 正文要扣到上限（`ChatStreamProcessor::BODY_HOLD_BACK`）才往下流，这里给足长度
        let first = "第一".repeat(70);
        let body = format!(
            "data: {{\"p\":\"response/content\",\"v\":\"{first}\"}}\n\
             data: {{\"p\":\"response/content\",\"v\":\"第二\"}}\n"
        );
        let model = model(&body);
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let response = model
            .infer_stream(
                user_request(),
                Box::new(move |delta| sink.lock().unwrap().push(delta)),
            )
            .await
            .expect("流式推理应成功");
        assert_eq!(response.text, format!("{first}第二"));
        let contents: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|delta| crate::apis::base::parse_delta(delta).content)
            .collect();
        assert_eq!(contents, vec![first, "第二".to_string()]);
    }

    /// 非流式：正文里的 DSML 工具调用解析成 tool_calls；有调用时这条回答不带正文。
    #[tokio::test]
    async fn infer_extracts_tool_calls_from_dsml() {
        let body = content_frame("先读文件")
            + &content_frame(&dsml_calls(&[dsml_invoke(
                "read_file",
                &[("path", "a.rs")],
            )]))
            + &content_frame("读完就答");
        let response = model(&body)
            .infer(tool_request())
            .await
            .expect("推理应成功");

        assert_eq!(response.text, "", "有工具调用时只输出工具调用");
        assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
        let calls = response.tool_calls.expect("应解析出工具调用");
        assert_eq!(calls.as_array().unwrap().len(), 1);
        let call = &calls[0];
        assert!(call["id"].as_str().unwrap().starts_with("call_"));
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "read_file");
        assert_eq!(call["function"]["arguments"], "{\"path\":\"a.rs\"}");
    }

    /// 请求没带工具时，同样的 DSML 按普通正文返回。
    #[tokio::test]
    async fn infer_keeps_dsml_as_text_without_tools() {
        let markup = dsml_calls(&[dsml_invoke("read_file", &[("path", "a.rs")])]);
        let response = model(&content_frame(&markup))
            .infer(user_request())
            .await
            .expect("推理应成功");

        assert_eq!(response.text, markup);
        assert!(response.tool_calls.is_none());
        assert_eq!(response.finish_reason.as_deref(), Some("stop"));
    }

    /// 块没闭合（模型写一半断了）时按正文回吐，不吞掉模型输出。
    #[tokio::test]
    async fn infer_falls_back_when_dsml_is_unclosed() {
        let half = "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"read_file\">";
        let response = model(&content_frame(half))
            .infer(tool_request())
            .await
            .expect("推理应成功");

        assert_eq!(response.text, half);
        assert!(response.tool_calls.is_none());
    }

    /// 多个 DSML 块 → 多个工具调用。
    #[tokio::test]
    async fn infer_extracts_multiple_tool_calls() {
        // 第二个调用带一个 `string="false"` 的参数：值按 JSON 解析成数字
        let numeric = concat!(
            "<｜｜DSML｜｜ invoke name=\"b\">",
            "<｜｜DSML｜｜ parameter name=\"n\" string=\"false\">1</｜｜DSML｜｜ parameter>",
            "</｜｜DSML｜｜ invoke>",
        );
        let body = content_frame(&dsml_calls(&[dsml_invoke("a", &[])]))
            + &content_frame(&dsml_calls(&[numeric.to_string()]));
        let response = model(&body)
            .infer(tool_request())
            .await
            .expect("推理应成功");

        let calls = response.tool_calls.expect("应解析出工具调用");
        let names: Vec<&str> = calls
            .as_array()
            .unwrap()
            .iter()
            .map(|call| call["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["a", "b"]);
        assert_eq!(calls[1]["function"]["arguments"], "{\"n\":1}");
    }

    /// 流式：DSML 标记被任意边界切开也不外泄给客户端，收尾增量里给出工具调用；
    /// 这条回答只有工具调用，调用前后的正文都不发。
    #[tokio::test]
    async fn infer_stream_hides_markup_and_reports_tool_calls() {
        let markup = dsml_calls(&[dsml_invoke("read_file", &[("path", "a.rs")])]);
        let body = content_frame("先读文件") + &content_frame(&markup);
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let response = model_split(&body, 7)
            .infer_stream(
                tool_request(),
                Box::new(move |delta| sink.lock().unwrap().push(delta)),
            )
            .await
            .expect("流式推理应成功");

        let deltas = seen.lock().unwrap().clone();
        let contents: String = deltas
            .iter()
            .map(|delta| crate::apis::base::parse_delta(delta).content)
            .collect();
        assert_eq!(contents, "", "有工具调用时正文不发、标记也不外泄");
        assert!(
            deltas.iter().any(|delta| delta.contains("read_file")),
            "应有承载工具调用的增量：{:?}",
            deltas
        );
        assert_eq!(response.text, "");
        assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(
            response.tool_calls.unwrap()[0]["function"]["name"],
            "read_file"
        );

        // 增量经 API 层解析器后必须还原成工具调用（客户端拿到的那一层）
        let parsed: Vec<Value> = deltas
            .iter()
            .filter_map(|delta| crate::apis::base::parse_delta(delta).functions)
            .flat_map(|calls| calls.as_array().cloned().unwrap_or_default())
            .collect();
        assert_eq!(parsed.len(), 1, "{:?}", deltas);
        assert_eq!(parsed[0]["function"]["name"], "read_file");
        assert_eq!(parsed[0]["function"]["arguments"], "{\"path\":\"a.rs\"}");
    }

    /// 流式：最后一行没有换行符也要解析（分块边界不可预期）。
    #[tokio::test]
    async fn infer_stream_parses_trailing_line_without_newline() {
        let body = "data: {\"p\":\"response/content\",\"v\":\"尾\"}";
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let response = model_split(body, 3)
            .infer_stream(
                user_request(),
                Box::new(move |delta| sink.lock().unwrap().push(delta)),
            )
            .await
            .expect("流式推理应成功");

        assert_eq!(response.text, "尾");
        let contents: String = seen
            .lock()
            .unwrap()
            .iter()
            .map(|delta| crate::apis::base::parse_delta(delta).content)
            .collect();
        assert_eq!(contents, "尾");
    }

    /// 流式：DSML 标记跨多个真实协议帧拼接，标记被吃掉不外泄；有调用时正文整条不发。
    #[tokio::test]
    async fn infer_stream_joins_markup_across_frames() {
        let body = concat!(
            "data: {\"v\":{\"response\":{\"message_id\":8,\"fragments\":[{\"type\":\"RESPONSE\",\"content\":\"\"}]}}}\n",
            "data: {\"p\":\"response/fragments/0/content\",\"o\":\"APPEND\",\"v\":\"前\"}\n",
            "data: {\"p\":\"response/fragments/0/content\",\"o\":\"APPEND\",\"v\":\"<｜｜DSML｜｜ invoke name=\\\"read\"}\n",
            "data: {\"p\":\"response/fragments/0/content\",\"o\":\"APPEND\",\"v\":\"_file\\\"><｜｜DSML｜｜ parameter name=\\\"path\\\" string=\\\"true\\\">a.rs</｜｜DSML｜｜ parameter>\"}\n",
            "data: {\"p\":\"response/fragments/0/content\",\"o\":\"APPEND\",\"v\":\"</｜｜DSML｜｜ invoke>\"}\n",
            "data: {\"p\":\"response/fragments/0/content\",\"o\":\"APPEND\",\"v\":\"后\"}\n",
        );
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let response = model_split(body, 5)
            .infer_stream(
                tool_request(),
                Box::new(move |delta| sink.lock().unwrap().push(delta)),
            )
            .await
            .expect("流式推理应成功");

        let contents: String = seen
            .lock()
            .unwrap()
            .iter()
            .map(|delta| crate::apis::base::parse_delta(delta).content)
            .collect();
        assert_eq!(contents, "", "有工具调用时正文不发、标记也不外泄");
        assert_eq!(response.text, "");
        let calls = response.tool_calls.expect("应解析出工具调用");
        assert_eq!(calls[0]["function"]["name"], "read_file");
        assert_eq!(calls[0]["function"]["arguments"], "{\"path\":\"a.rs\"}");
    }

    /// `parent_message_id` 接上服务端的消息链：首轮为 null，之后用上一轮响应的消息 id。
    #[tokio::test]
    async fn parent_message_id_follows_previous_response() {
        // 真实流首帧带 response_message_id：它是下一轮的 parent
        let body = concat!(
            "data: {\"request_message_id\":5,\"response_message_id\":6,\"model_type\":\"default\"}\n",
            "data: {\"p\":\"response/content\",\"v\":\"答\"}\n",
        );
        let executor = executor(body, 0);
        let model = DeepSeekWebModel::new("DeepSeek", executor.clone() as Arc<dyn ChannelExecutor>);

        model.infer(user_request()).await.expect("首轮推理应成功");
        model
            .infer_stream(user_request(), Box::new(|_| {}))
            .await
            .expect("次轮推理应成功");

        let bodies = executor.completion_bodies();
        assert_eq!(bodies.len(), 2, "两轮各发一次 completion：{:?}", bodies);
        assert_eq!(
            bodies[0]["parent_message_id"],
            Value::Null,
            "首轮没有上一轮消息 id"
        );
        assert_eq!(
            bodies[1]["parent_message_id"], 6,
            "次轮用首轮响应的消息 id：{:?}",
            bodies[1]
        );
        // 单槽位池：两轮落在同一个网页会话里
        assert_eq!(bodies[0]["chat_session_id"], bodies[1]["chat_session_id"]);
    }

    /// 复用已存在的网页会话：使用前先取一次会话历史的 `current_message_id` 当 parent。
    #[tokio::test]
    async fn history_current_message_id_seeds_parent() {
        let executor = executor_with(
            &content_frame("答"),
            0,
            Some(
                json!({ "data": { "biz_data": { "chat_session": { "current_message_id": 42 } } } })
                    .to_string(),
            ),
        );
        let store = Arc::new(MemoryStore::default());
        // 槽位表里登记的就是这个已存在的会话
        store.slots.lock().unwrap().insert(
            CHANNEL.to_string(),
            vec![PooledSession {
                slot: 0,
                session_id: "sess-existing".to_string(),
            }],
        );
        executor.seed_session("sess-existing", &format!("{POOL_TITLE_PREFIX}1"));
        let model = pooled_model(&executor, &store, 1);

        model.infer(user_request()).await.expect("推理应成功");

        let bodies = executor.completion_bodies();
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["chat_session_id"], "sess-existing");
        assert_eq!(
            bodies[0]["parent_message_id"], 42,
            "首轮就该接上网页会话里已有的消息链：{:?}",
            bodies[0]
        );
    }

    /// 会话池：按并发设置预建会话，槽位表落库；此时还不改名（网页端要求会话先有内容）。
    #[tokio::test]
    async fn session_pool_prepares_sessions() {
        let executor = executor(&content_frame("答"), 0);
        let store = Arc::new(MemoryStore::default());
        let model = pooled_model(&executor, &store, 3);

        assert_eq!(
            executor.create_count(),
            0,
            "预热前不该建会话：构造模型本身不发请求"
        );
        model
            .prepare_session_pool()
            .await
            .expect("预热会话池应成功");

        assert_eq!(executor.sessions().len(), 3, "按并发设置预建三个会话");
        assert_eq!(
            executor.rename_count(),
            0,
            "会话还没有内容，此时改名会被网页端拒绝（EMPTY_CHAT_SESSION）"
        );
        assert_eq!(
            stored_sessions(&store).len(),
            3,
            "槽位表要落库，重启后才能复用"
        );
        assert_eq!(model.concurrency().max_inflight, 3);
    }

    /// 命名推迟到首轮对话成功之后：每个池内会话只改一次名，标题为 `poolman_<槽位序号>`。
    #[tokio::test]
    async fn session_pool_titles_sessions_after_first_turn() {
        let executor = executor(&content_frame("答"), 0);
        let store = Arc::new(MemoryStore::default());
        let model = pooled_model(&executor, &store, 2);

        model.infer(user_request()).await.expect("第一轮应成功");
        model.infer(user_request()).await.expect("第二轮应成功");

        let titles: Vec<String> = executor.sessions().into_values().collect();
        assert_eq!(
            titles,
            vec![
                format!("{POOL_TITLE_PREFIX}1"),
                format!("{POOL_TITLE_PREFIX}2"),
            ],
            "两个槽位各自首轮成功后都拿到 poolman_<序号> 标题"
        );
        assert_eq!(executor.rename_count(), 2, "每个会话只改一次名");

        // 再次用到槽位 0：标题已就位，不该重复改名
        model.infer(user_request()).await.expect("第三轮应成功");
        assert_eq!(executor.rename_count(), 2, "已命名的会话不再改名");
    }

    /// 重启复用：槽位表里的会话在网页端仍然存在（且标题已就位），不重复创建、也不重复改名。
    #[tokio::test]
    async fn session_pool_reuses_persisted_sessions() {
        let executor = executor(&content_frame("答"), 0);
        let store = Arc::new(MemoryStore::default());

        // 同一实例跑两轮：两个槽位各用一次，各命名一次
        let model = pooled_model(&executor, &store, 2);
        model.infer(user_request()).await.expect("第一轮应成功");
        model.infer(user_request()).await.expect("第二轮应成功");
        let created = executor.create_count();
        let renames = executor.rename_count();
        let slots = stored_sessions(&store);
        assert_eq!(created, 2);
        assert_eq!(renames, 2, "两个槽位各命名一次");

        // 重新构造模型（等价于进程重启）：同一份槽位表 + 同一批网页会话
        pooled_model(&executor, &store, 2)
            .infer(user_request())
            .await
            .expect("重启后推理应成功");
        assert_eq!(
            executor.create_count(),
            created,
            "会话都还在，不应重复创建：{:?}",
            executor.sessions()
        );
        assert_eq!(
            executor.rename_count(),
            renames,
            "标题已是池内命名，不该重复改名：{:?}",
            executor.sessions()
        );
        assert_eq!(stored_sessions(&store), slots, "槽位表保持不变");
    }

    /// 网页端已删除的会话：核对后补建，不复用失效 id；补建的会话在首轮后命名。
    #[tokio::test]
    async fn session_pool_rebuilds_missing_sessions() {
        let executor = executor(&content_frame("答"), 0);
        let store = Arc::new(MemoryStore::default());
        store.slots.lock().unwrap().insert(
            CHANNEL.to_string(),
            vec![PooledSession {
                slot: 0,
                session_id: "sess-gone".to_string(),
            }],
        );

        pooled_model(&executor, &store, 1)
            .infer(user_request())
            .await
            .expect("推理应成功");

        let slots = stored_sessions(&store);
        assert_eq!(slots.len(), 1);
        assert_ne!(slots[0], "sess-gone", "失效会话不该继续占用槽位");
        assert!(executor.sessions().contains_key(&slots[0]));
        assert_eq!(
            executor.sessions().get(&slots[0]).map(String::as_str),
            Some(format!("{POOL_TITLE_PREFIX}1").as_str()),
            "补建的会话首轮后被命名"
        );
    }

    /// 网页端有同名 `poolman_<序号>` 会话时按标题认领，不重复创建也不重复改名（列表分页看不到旧 id 的兜底）。
    #[tokio::test]
    async fn session_pool_adopts_sessions_by_title() {
        let executor = executor(&content_frame("答"), 0);
        executor.seed_session("sess-adopt", &format!("{POOL_TITLE_PREFIX}1"));
        let store = Arc::new(MemoryStore::default());
        store.slots.lock().unwrap().insert(
            CHANNEL.to_string(),
            vec![PooledSession {
                slot: 0,
                session_id: "sess-old".to_string(),
            }],
        );

        pooled_model(&executor, &store, 1)
            .infer(user_request())
            .await
            .expect("推理应成功");

        assert_eq!(executor.create_count(), 0, "应认领同名会话而不是新建");
        assert_eq!(
            executor.rename_count(),
            0,
            "认领来的会话标题已经对了，不该改"
        );
        assert_eq!(stored_sessions(&store), vec!["sess-adopt".to_string()]);
    }

    /// 启动时插件还没连上（会话列表取不到）只能保守保留槽位；
    /// 这些槽位若在网页端已经没了，用之前会核对出来并补建，不会一直拿空回答。
    #[tokio::test]
    async fn stale_pool_slot_is_replaced_on_use() {
        let executor = executor(&content_frame("答"), 0);
        let store = Arc::new(MemoryStore::default());
        store.slots.lock().unwrap().insert(
            CHANNEL.to_string(),
            vec![PooledSession {
                slot: 0,
                session_id: "sess-stale".to_string(),
            }],
        );
        // 网页端不认 sess-stale；列表又取不到，所以只能等用的时候核对
        executor.set_list_unavailable(true);
        let model = pooled_model(&executor, &store, 1);

        let response = model.infer(user_request()).await.expect("推理应成功");
        assert_eq!(response.text, "答", "失效槽位应被补建后正常作答");

        let slots = stored_sessions(&store);
        assert_eq!(slots.len(), 1);
        assert_ne!(slots[0], "sess-stale", "失效会话被替换并落库");
        assert_eq!(executor.create_count(), 1, "只补建了一个会话");
        assert_eq!(executor.rename_count(), 1, "补建的会话首轮后命名");
        assert_eq!(
            executor.sessions().get(&slots[0]).map(String::as_str),
            Some(format!("{POOL_TITLE_PREFIX}1").as_str())
        );
    }

    /// 请求按槽位轮转：相邻请求用不同会话，槽位用完后回到起点。
    #[tokio::test]
    async fn requests_rotate_over_pool_slots() {
        let executor = executor(&content_frame("答"), 0);
        let store = Arc::new(MemoryStore::default());
        let model = pooled_model(&executor, &store, 2);

        model.infer(user_request()).await.expect("第一轮应成功");
        model.infer(user_request()).await.expect("第二轮应成功");
        model.infer(user_request()).await.expect("第三轮应成功");

        let used: Vec<Value> = executor
            .completion_bodies()
            .iter()
            .map(|body| body["chat_session_id"].clone())
            .collect();
        assert_eq!(used.len(), 3);
        assert_ne!(used[0], used[1], "相邻请求应落到不同槽位");
        assert_eq!(used[0], used[2], "槽位按容量轮转回到起点");
    }

    /// 空回答必须把原始响应打进日志：否则「请求走完了但客户端什么都没有」无从排查。
    #[tokio::test]
    async fn empty_answer_logs_raw_response() {
        let body = concat!(
            "data: {\"p\":\"response/status\",\"v\":\"x\"}\n",
            "data: {\"code\":1,\"msg\":\"会话无效\"}\n",
        );
        let logs = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || LogSink(sink.clone()))
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let response = model(body).infer(user_request()).await.expect("推理应成功");
        assert!(response.text.is_empty());
        assert!(response.tool_calls.is_none());

        let logged = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        assert!(logged.contains("网页回答为空"), "{}", logged);
        assert!(logged.contains("会话无效"), "应打出原始响应：{}", logged);
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

    /// 历史里只有空白的消息按空处理：不给提示词塞成段空行。
    #[test]
    fn whitespace_only_history_message_renders_without_blanks() {
        let request = InferRequest::new(
            "deepseek-web",
            vec![
                json!({ "role": "user", "content": "问题" }),
                json!({ "role": "assistant", "content": "\n\n   \n\n" }),
                json!({ "role": "user", "content": "继续" }),
            ],
        );
        let prompt = DeepSeekWebModel::prompt_of(&request);
        assert!(
            prompt.contains("【assistant】\n\n【user】\n继续"),
            "空白正文应折成空段：{}",
            prompt
        );
    }

    /// 有工具调用的消息即便正文只有空白，标记也照常渲染。
    #[test]
    fn whitespace_only_message_keeps_its_tool_calls() {
        let request = InferRequest::new(
            "deepseek-web",
            vec![
                json!({ "role": "user", "content": "问题" }),
                json!({
                    "role": "assistant",
                    "content": "\n  ",
                    "tool_calls": [{ "id": "call_1", "type": "function", "function": {
                        "name": "view", "arguments": "{\"path\":\"a.rs\"}"
                    }}],
                }),
                json!({ "role": "tool", "content": "文件内容" }),
            ],
        );
        let prompt = DeepSeekWebModel::prompt_of(&request);
        assert!(prompt.contains("【assistant】\n"), "{}", prompt);
        assert!(prompt.contains("<｜｜DSML｜｜ calls>"), "{}", prompt);
        assert!(prompt.contains("name=\"view\""), "{}", prompt);
        assert!(
            !prompt.contains("【assistant】\n\n\n"),
            "不该带空白正文：{}",
            prompt
        );
    }

    /// 提示词拼接：系统提示词单独置顶，其余消息里只有最新一条不算历史，工具段在最新记录之前。
    #[test]
    fn prompt_orders_system_history_rules_tools_and_latest() {
        let mut request = InferRequest::new(
            "deepseek-web",
            vec![
                json!({ "role": "system", "content": "你是助手" }),
                json!({ "role": "user", "content": "第一个问题" }),
                json!({ "role": "assistant", "content": "第一个回答" }),
                json!({ "role": "tool", "tool_call_id": "c1", "content": "工具结果" }),
                json!({ "role": "user", "content": "第二个问题" }),
            ],
        );
        request.tools = Some(json!([
            {
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "读取文件",
                    "parameters": { "type": "object", "properties": { "path": { "type": "string" } } }
                }
            }
        ]));

        let prompt = DeepSeekWebModel::prompt_of(&request);
        // 段标题由常量给出，顺序固定
        let expected_order = [
            super::PROMPT_PREFIX,
            super::SYSTEM_HEADING,
            super::HISTORY_HEADING,
            "【user】\n第一个问题",
            "【assistant】\n第一个回答",
            "【tool】id=c1\n工具结果",
            super::TOOL_RULE_HEADING,
            super::TOOLS_HEADING,
            "- read_file：读取文件",
        ];
        let mut cursor = 0usize;
        for fragment in expected_order {
            let at = prompt
                .find(fragment)
                .unwrap_or_else(|| panic!("提示词缺少片段 {:?}：\n{}", fragment, prompt));
            assert!(at >= cursor, "片段顺序不对：{:?}\n{}", fragment, prompt);
            cursor = at;
        }
        // 系统提示词段紧跟标题，且不带角色标注
        assert!(
            prompt.contains(&format!("{}\n你是助手", super::SYSTEM_HEADING)),
            "{}",
            prompt
        );
        assert!(!prompt.contains("【system】"), "{}", prompt);
        // 规则正文由模型自己给出：DSML 标记格式 + 限定规则
        assert!(prompt.contains(super::CALLS_OPEN), "{}", prompt);
        assert!(prompt.contains("必须逐字写成全角"), "{}", prompt);
        assert!(prompt.contains("只能调用下面列出的工具"), "{}", prompt);
        // 参数为紧凑 JSON
        assert!(prompt.contains(r#"参数：{"properties""#), "{}", prompt);
        // 最新一条记录拼在最末尾
        assert!(prompt.ends_with("【user】\n第二个问题"), "{}", prompt);
    }

    /// 最新一条记录是工具返回时同样拼在末尾；历史里助手的旧工具调用还原成 DSML 标记。
    #[test]
    fn prompt_appends_latest_tool_result_and_restores_history_tool_calls() {
        let request = InferRequest::new(
            "deepseek-web",
            vec![
                json!({ "role": "user", "content": "读文件" }),
                json!({
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": "c1",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{\"path\":\"a.rs\"}" }
                    }]
                }),
                json!({ "role": "tool", "tool_call_id": "c1", "content": "文件内容" }),
            ],
        );

        let prompt = DeepSeekWebModel::prompt_of(&request);
        // 助手历史消息：正文为空也不丢调用，字符串参数按 JSON 还原成 DSML 标记
        assert!(
            prompt.contains("【assistant】\n<｜｜DSML｜｜ calls>"),
            "{}",
            prompt
        );
        // 调用 id 一并写进 invoke，供工具结果对应
        assert!(
            prompt.contains("<｜｜DSML｜｜ invoke name=\"read_file\" id=\"c1\">"),
            "{}",
            prompt
        );
        assert!(
            prompt.contains("<｜｜DSML｜｜ parameter name=\"path\" string=\"true\">a.rs</｜｜DSML｜｜ parameter>"),
            "{}",
            prompt
        );
        assert!(prompt.contains("</｜｜DSML｜｜ calls>"), "{}", prompt);
        // 标记属于历史段
        assert!(
            prompt.find("<｜｜DSML｜｜").unwrap() > prompt.find(super::HISTORY_HEADING).unwrap(),
            "{}",
            prompt
        );
        // 最新记录（工具返回）在最后，标题带上工具名与调用 id
        assert!(
            prompt.ends_with("【tool】name=read_file id=c1\n文件内容"),
            "{}",
            prompt
        );
        // 没有工具时不给格式规则段
        assert!(!prompt.contains(super::TOOL_RULE_HEADING), "{}", prompt);
    }

    /// 工具结果标题标出工具名与 id：客户端只回 `tool_call_id` 时按 id 反查名字。
    #[test]
    fn tool_result_label_names_the_call_it_answers() {
        let request = InferRequest::new(
            "deepseek-web",
            vec![
                json!({ "role": "user", "content": "看看这两个文件" }),
                json!({
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [
                        { "id": "c1", "type": "function", "function": { "name": "view", "arguments": "{\"path\":\"a.rs\"}" } },
                        { "id": "c2", "type": "function", "function": { "name": "grep", "arguments": "{\"pattern\":\"x\"}" } },
                    ],
                }),
                json!({ "role": "tool", "tool_call_id": "c1", "content": "a.rs 内容" }),
                json!({ "role": "tool", "tool_call_id": "c2", "content": "grep 结果" }),
            ],
        );

        let prompt = DeepSeekWebModel::prompt_of(&request);
        assert!(
            prompt.contains("【tool】name=view id=c1\na.rs 内容"),
            "{}",
            prompt
        );
        assert!(
            prompt.contains("【tool】name=grep id=c2\ngrep 结果"),
            "{}",
            prompt
        );
    }

    /// 客户端自带工具名时用它；既没有 id 也没有名字时不编标签。
    #[test]
    fn tool_result_label_falls_back_to_message_name() {
        let request = InferRequest::new(
            "deepseek-web",
            vec![
                json!({ "role": "user", "content": "问题" }),
                json!({ "role": "assistant", "content": "回答" }),
                json!({ "role": "tool", "name": "powershell", "content": "输出" }),
                json!({ "role": "tool", "content": "无名结果" }),
            ],
        );

        let prompt = DeepSeekWebModel::prompt_of(&request);
        assert!(
            prompt.contains("【tool】name=powershell\n输出"),
            "{}",
            prompt
        );
        assert!(prompt.contains("【tool】\n无名结果"), "{}", prompt);
    }

    /// 多模态正文取文本片段；非文本片段只留占位标记，不把 base64 塞进提示词。
    #[test]
    fn prompt_renders_multimodal_parts_without_payload() {
        let request = InferRequest::new(
            "deepseek-web",
            vec![json!({
                "role": "user",
                "content": [
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } },
                    { "type": "text", "text": "图片背景是什么颜色" }
                ]
            })],
        );

        let prompt = DeepSeekWebModel::prompt_of(&request);
        assert!(prompt.contains("图片背景是什么颜色"), "{}", prompt);
        assert!(
            prompt.contains("[image_url]"),
            "非文本片段应有占位：{}",
            prompt
        );
        assert!(!prompt.contains("base64"), "不应带上图片负载：{}", prompt);
        assert!(
            prompt.starts_with(PROMPT_PREFIX),
            "固定前缀不应被多模态处理影响：{}",
            prompt
        );
    }

    /// 无工具或空工具数组时不拼接工具段与规则段。
    #[test]
    fn prompt_skips_tool_section_when_absent() {
        let mut request = InferRequest::new(
            "deepseek-web",
            vec![json!({ "role": "user", "content": "hi" })],
        );
        let expected = format!("{PROMPT_PREFIX}\n\n【user】\nhi");
        assert_eq!(DeepSeekWebModel::prompt_of(&request), expected);

        request.tools = Some(json!([]));
        assert_eq!(
            DeepSeekWebModel::prompt_of(&request),
            expected,
            "空工具数组不应产生工具段"
        );
    }

    /// 只有系统提示词与最新一条记录、没有历史时，不产生历史段标题。
    #[test]
    fn prompt_skips_history_heading_without_history() {
        let request = InferRequest::new(
            "deepseek-web",
            vec![
                json!({ "role": "system", "content": "系统" }),
                json!({ "role": "user", "content": "问题" }),
            ],
        );
        let prompt = DeepSeekWebModel::prompt_of(&request);
        assert_eq!(
            prompt,
            format!(
                "{}\n\n{}\n系统\n\n【user】\n问题",
                super::PROMPT_PREFIX,
                super::SYSTEM_HEADING
            )
        );
    }

    /// 固定前缀必须在最开头（其后才是各段）。
    #[test]
    fn prompt_starts_with_fixed_prefix() {
        let mut request = InferRequest::new(
            "deepseek-web",
            vec![
                json!({ "role": "system", "content": "系统" }),
                json!({ "role": "user", "content": "问题" }),
            ],
        );
        request.tools = Some(json!([{ "function": { "name": "t", "description": "工具" } }]));

        let prompt = DeepSeekWebModel::prompt_of(&request);
        assert!(prompt.starts_with(super::PROMPT_PREFIX), "{}", prompt);
        let prefix_at = 0usize;
        for later in [
            super::SYSTEM_HEADING,
            super::TOOL_RULE_HEADING,
            super::TOOLS_HEADING,
            "【user】\n问题",
        ] {
            assert!(
                prompt.find(later).unwrap() > prefix_at,
                "{} 应在前缀之后",
                later
            );
        }
    }

    /// 提示词里的标记字面量必须与渲染器共用同一套常量（模型看到的格式与解析器认的格式同源）。
    #[test]
    fn tool_call_markup_agrees_with_prompt() {
        let mut request = InferRequest::new(
            "deepseek-web",
            vec![json!({ "role": "user", "content": "hi" })],
        );
        request.tools = Some(json!([{ "function": { "name": "t" } }]));
        let prompt = DeepSeekWebModel::prompt_of(&request);
        for literal in [
            super::CALLS_OPEN,
            super::CALLS_CLOSE,
            super::INVOKE_OPEN,
            super::INVOKE_CLOSE,
            super::PARAMETER_OPEN,
            super::PARAMETER_CLOSE,
        ] {
            assert!(prompt.contains(literal), "规则段应含 {literal}：{prompt}");
        }
        assert!(
            !prompt.contains("<tool_call>"),
            "旧的 <tool_call> 格式不该再出现在提示词里：{prompt}"
        );
    }

    /// 历史工具调用渲染出来的标记，必须能被解析器还原成同一个调用。
    #[test]
    fn history_markup_round_trips_through_parser() {
        let message = json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [
                {
                    "id": "c1",
                    "type": "function",
                    "function": { "name": "read_file", "arguments": "{\"path\":\"a.rs\"}" }
                },
                {
                    "id": "c2",
                    "type": "function",
                    "function": {
                        "name": "view",
                        "arguments": "{\"path\":\"b.rs\",\"view_range\":[320,400],\"raw\":true}"
                    }
                }
            ]
        });
        let markup = DeepSeekWebModel::render_tool_calls(&message).join("\n");

        let mut parser = crate::models::deepseek_web::dsml_stream_parser::DsmlStreamParser::new();
        let mut events = Vec::new();
        parser.feed(&markup, &mut events);
        parser.finish(&mut events);

        let calls = parser.calls();
        assert_eq!(calls.len(), 2, "渲染的标记应还原成两个调用：{markup}");
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments["path"], json!("a.rs"));
        assert_eq!(calls[1].name, "view");
        assert_eq!(calls[1].arguments["path"], json!("b.rs"));
        assert_eq!(calls[1].arguments["view_range"], json!([320, 400]));
        assert_eq!(calls[1].arguments["raw"], json!(true));
    }

    /// 落盘关闭时模型侧不产出任何文件：响应与工具调用两条落盘路径都跳过，链路照常返回结果。
    #[tokio::test]
    async fn disabled_dump_keeps_dir_clean() {
        let dir = std::env::temp_dir().join(format!("poolman-nodump-{}", uuid::Uuid::new_v4()));
        let dump = Arc::new(crate::dump::DumpConfig::new(
            false,
            dir.to_string_lossy().to_string(),
        ));
        let markup = dsml_calls(&[dsml_invoke("read_file", &[("path", "a.rs")])]);
        let body = content_frame("答") + &content_frame(&markup);
        let executor = executor(&body, 0);
        let model =
            DeepSeekWebModel::new("DeepSeek", executor as Arc<dyn ChannelExecutor>).with_dump(dump);

        let response = model.infer(tool_request()).await.expect("推理应成功");
        assert_eq!(response.text, "", "有工具调用时只输出工具调用");
        assert_eq!(
            response.tool_calls.unwrap()[0]["function"]["name"],
            "read_file"
        );
        assert!(!dir.exists(), "落盘关闭时不应创建目录，更不应写文件");
    }

    /// 响应落盘：落一份解析结果快照，且不打断链路，客户端拿到正常结果。
    #[tokio::test]
    async fn response_dump_writes_file_and_continues() {
        let dir = std::env::temp_dir().join(format!("poolman-dump-{}", uuid::Uuid::new_v4()));
        let dump = Arc::new(crate::dump::DumpConfig::new(
            true,
            dir.to_string_lossy().to_string(),
        ));
        let body = "data: {\"p\":\"response/content\",\"v\":\"答\"}\n";
        let executor = executor(body, 0);
        let model = DeepSeekWebModel::new("DeepSeek", executor.clone() as Arc<dyn ChannelExecutor>)
            .with_dump(dump);

        let response = model.infer(user_request()).await.expect("落盘不应打断链路");
        assert_eq!(response.text, "答");
        assert!(
            !executor.requests.lock().unwrap().is_empty(),
            "落盘后仍应发起渠道请求"
        );

        let files: Vec<_> = std::fs::read_dir(&dir)
            .expect("落盘目录应已创建")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect();
        assert_eq!(files.len(), 1, "一次请求落一份响应快照");
        assert!(
            files[0]
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("response-")),
            "快照文件名应带阶段前缀：{:?}",
            files[0]
        );

        let doc: Value = serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
        assert_eq!(doc["stage"], "response");
        assert_eq!(doc["channel"], CHANNEL);
        assert_eq!(doc["model"], "deepseek-web");
        // 会话池给这次请求分配的槽位就是本次使用的网页会话
        let pooled = executor
            .sessions()
            .keys()
            .next()
            .cloned()
            .unwrap_or_default();
        assert_eq!(doc["web_session"], json!(pooled));
        assert!(!pooled.is_empty(), "应记录了本次使用的网页会话");
        assert_eq!(doc["status"], 200);
        assert_eq!(doc["text"], "答");
        assert_eq!(doc["text_len"], 1);
        assert_eq!(doc["reasoning"], "", "该次没有思考片段");
        assert_eq!(doc["tool_calls"], json!([]));
        assert_eq!(doc["finish_reason"], "stop");
        assert_eq!(
            doc["raw_len"],
            body.len() as u64,
            "记下网页原始返回文本长度"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 工具调用单独落一份：既有模型吐出的原文，也有解析出的调用，且不打扰链路。
    #[tokio::test]
    async fn tool_call_dump_records_raw_and_parsed() {
        let dir = std::env::temp_dir().join(format!("poolman-dump-{}", uuid::Uuid::new_v4()));
        let dump = Arc::new(crate::dump::DumpConfig::new(
            true,
            dir.to_string_lossy().to_string(),
        ));
        // 模型按提示词规则转义 Windows 路径：原文两级反斜杠，落值一级
        let markup = dsml_calls(&[dsml_invoke(
            "read_file",
            &[("path", r#"d:\\projects\\a.rs"#)],
        )]);
        let body = content_frame("先读文件") + &content_frame(&markup);
        let executor = executor(&body, 7);
        let model =
            DeepSeekWebModel::new("DeepSeek", executor as Arc<dyn ChannelExecutor>).with_dump(dump);
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        model
            .infer_stream(
                tool_request(),
                Box::new(move |delta| sink.lock().unwrap().push(delta)),
            )
            .await
            .expect("流式推理应成功");

        let doc: Value =
            serde_json::from_slice(&std::fs::read(dump_file(&dir, "tool-call-")).unwrap()).unwrap();
        assert_eq!(doc["stage"], "tool-call");
        assert_eq!(doc["channel"], "deepseek-web");
        assert_eq!(doc["raw_markup"], markup, "落的是模型吐出的标记原文");
        assert_eq!(
            doc["raw_markup_len"].as_u64().unwrap(),
            markup.chars().count() as u64
        );
        assert_eq!(doc["calls"][0]["function"]["name"], "read_file");
        // calls.arguments 是 JSON 字符串（客户端拿到的那层），解出来是单层反斜杠的路径
        let arguments: Value =
            serde_json::from_str(doc["calls"][0]["function"]["arguments"].as_str().unwrap())
                .unwrap();
        assert_eq!(arguments["path"], json!(r#"d:\projects\a.rs"#));
        // parsed 是可读形态：路径就是单层反斜杠，不会因为文件再转义一层看着变多
        assert_eq!(doc["parsed"][0]["name"], "read_file");
        assert_eq!(
            doc["parsed"][0]["arguments"]["path"],
            json!(r#"d:\projects\a.rs"#)
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 落盘目录里按前缀取最近写出的那份快照。
    fn dump_file(dir: &std::path::Path, prefix: &str) -> std::path::PathBuf {
        std::fs::read_dir(dir)
            .expect("落盘目录应已创建")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(prefix))
            })
            .unwrap_or_else(|| panic!("应落一份 {prefix}* 快照"))
    }

    /// 响应快照落的是解析后的三类内容：思考、正文、工具调用。
    #[tokio::test]
    async fn response_dump_records_parsed_content() {
        let dir = std::env::temp_dir().join(format!("poolman-dump-{}", uuid::Uuid::new_v4()));
        let dump = Arc::new(crate::dump::DumpConfig::new(
            true,
            dir.to_string_lossy().to_string(),
        ));
        let body = concat!(
            "data: {\"v\":{\"response\":{\"message_id\":6,\"fragments\":[{\"type\":\"THINK\",\"content\":\"想\"}]}}}\n",
            "data: {\"v\":\"一下\"}\n",
            "data: {\"p\":\"response/fragments\",\"o\":\"APPEND\",\"v\":[{\"type\":\"RESPONSE\",\"content\":\"<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\\\"read_file\\\"><｜｜DSML｜｜ parameter name=\\\"path\\\" string=\\\"true\\\">a.rs</｜｜DSML｜｜ parameter></｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>\"}]}\n",
        );
        let executor = executor(body, 0);
        let model =
            DeepSeekWebModel::new("DeepSeek", executor as Arc<dyn ChannelExecutor>).with_dump(dump);
        model.infer(tool_request()).await.expect("推理应成功");

        let file = std::fs::read_dir(&dir)
            .expect("落盘目录应已创建")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("response-"))
            })
            .expect("应落一份响应快照");
        let doc: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();

        assert_eq!(doc["stage"], "response");
        assert_eq!(doc["reasoning"], "想一下", "思考原文");
        assert_eq!(doc["reasoning_len"], 3);
        assert_eq!(doc["text"], "", "标记不算正文");
        assert_eq!(doc["tool_calls"][0]["function"]["name"], "read_file");
        assert_eq!(
            doc["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a.rs\"}"
        );
        assert_eq!(doc["finish_reason"], "tool_calls");
        assert!(doc["raw_len"].as_u64().unwrap() > 0, "记下原始文本长度");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 渠道会话管理由模型自行完成。
    #[tokio::test]
    async fn channel_sessions_are_model_owned() {
        let model = model("");
        let session = model
            .create_channel_session()
            .await
            .unwrap()
            .expect("应创建出会话 id");
        assert!(session.starts_with("sess-"), "{}", session);

        let listed = model.list_channel_sessions().await.unwrap();
        assert!(listed.is_array(), "{:?}", listed);
        assert_eq!(listed.as_array().unwrap().len(), 1, "{:?}", listed);
        assert_eq!(listed[0]["id"], json!(session));

        // 网页端要求会话已有内容才接受改名：空会话改名返回 false（不报错）
        assert!(!model.rename_channel_session(&session, "t").await.unwrap());
        assert!(model.channel_history(&session).await.is_ok());
        assert!(model.delete_channel_session(&session).await.unwrap());
        assert!(
            model
                .list_channel_sessions()
                .await
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty(),
            "删除后会话列表里不应再有它"
        );
    }

    /// SSE 帧解析：真实协议（快照 / 追加片段 / 追加片段内容）与简写正文帧都能取到正文。
    #[test]
    fn sse_frames_yield_response_deltas() {
        let mut parser = super::SseFrameProcessor::new();
        // 简写正文帧
        assert_eq!(
            parser.process_line("data: {\"p\":\"response/content\",\"v\":\"a\"}"),
            vec![("RESPONSE".to_string(), "a".to_string())]
        );
        assert_eq!(
            parser.process_line("data: {\"p\":\"response/content\",\"v\":\"b\"}"),
            vec![("RESPONSE".to_string(), "b".to_string())]
        );
        assert!(parser.process_line("data: [DONE]").is_empty());
        assert!(parser.process_line("").is_empty());
        assert!(parser.process_line("data: 不是 JSON").is_empty());
        // 无路径的纯文本帧按正文收下
        assert_eq!(
            parser.process_line("data: {\"v\":\"裸文本\"}"),
            vec![("RESPONSE".to_string(), "裸文本".to_string())]
        );

        // 真实协议：快照 → 只放出新增部分；追加片段；按下标追加内容
        let mut parser = super::SseFrameProcessor::new();
        let snapshot = "data: {\"v\":{\"response\":{\"message_id\":8,\"fragments\":[\
            {\"type\":\"THINK\",\"content\":\"想一想\"},{\"type\":\"RESPONSE\",\"content\":\"你好\"}]}}}";
        assert_eq!(
            parser.process_line(snapshot),
            vec![
                ("THINK".to_string(), "想一想".to_string()),
                ("RESPONSE".to_string(), "你好".to_string()),
            ],
            "快照按片段类型给出内容"
        );
        // 同一快照重发不重复输出（sent_length 去重）
        assert!(parser.process_line(snapshot).is_empty());
        // 追加片段
        assert_eq!(
            parser.process_line("data: {\"p\":\"response/fragments\",\"o\":\"APPEND\",\"v\":[{\"type\":\"RESPONSE\",\"content\":\"世界\"}]}"),
            vec![("RESPONSE".to_string(), "世界".to_string())]
        );
        // 按下标追加内容；-1 表示最后一个片段
        assert_eq!(
            parser.process_line(
                "data: {\"p\":\"response/fragments/1/content\",\"o\":\"APPEND\",\"v\":\"！\"}"
            ),
            vec![("RESPONSE".to_string(), "！".to_string())],
            "只放新增部分"
        );
        assert_eq!(
            parser.process_line(
                "data: {\"p\":\"response/fragments/-1/content\",\"o\":\"APPEND\",\"v\":\"啦\"}"
            ),
            vec![("RESPONSE".to_string(), "啦".to_string())]
        );
        // SET 覆盖成更长的内容：只放新增部分
        assert_eq!(
            parser.process_line(
                "data: {\"p\":\"response/fragments/1/content\",\"o\":\"SET\",\"v\":\"你好！呀\"}"
            ),
            vec![("RESPONSE".to_string(), "呀".to_string())]
        );
        // SET 覆盖成更短的内容：增量表达不了替换，只把游标退回去，后续追加仍能出增量
        assert!(parser
            .process_line(
                "data: {\"p\":\"response/fragments/1/content\",\"o\":\"SET\",\"v\":\"重写\"}"
            )
            .is_empty());
        assert_eq!(
            parser.process_line(
                "data: {\"p\":\"response/fragments/1/content\",\"o\":\"APPEND\",\"v\":\"好\"}"
            ),
            vec![("RESPONSE".to_string(), "好".to_string())]
        );
    }

    /// 真实网页数据：无路径续帧属于当前片段（思考的续帧不能被当成正文），参数帧一律忽略。
    #[test]
    fn sse_frames_attribute_bare_continuation_to_current_fragment() {
        let mut parser = super::SseFrameProcessor::new();
        parser.process_line(
            "data: {\"v\":{\"response\":{\"message_id\":6,\"fragments\":[{\"id\":2,\"type\":\"THINK\",\"content\":\"The\"}]}}}",
        );
        assert_eq!(
            parser.process_line(
                "data: {\"p\":\"response/fragments/-1/content\",\"o\":\"APPEND\",\"v\":\" user\"}"
            ),
            vec![("THINK".to_string(), " user".to_string())],
            "思考片段的下标续帧仍是思考"
        );
        assert_eq!(
            parser.process_line("data: {\"v\":\" just\"}"),
            vec![("THINK".to_string(), " just".to_string())],
            "无路径续帧沿用当前片段类型"
        );
        // 参数 / 状态帧（路径不是正文）不当作正文
        assert!(parser
            .process_line(
                "data: {\"p\":\"response/fragments/-1/elapsed_secs\",\"o\":\"SET\",\"v\":0.83}"
            )
            .is_empty());
        assert!(parser
            .process_line("data: {\"p\":\"response/status\",\"o\":\"SET\",\"v\":\"FINISHED\"}")
            .is_empty());
        assert!(parser
            .process_line("data: {\"p\":\"response\",\"o\":\"BATCH\",\"v\":[{\"p\":\"quasi_status\",\"v\":\"FINISHED\"}]}")
            .is_empty());
        // 新片段（正文）与它的续帧
        assert_eq!(
            parser.process_line("data: {\"p\":\"response/fragments\",\"o\":\"APPEND\",\"v\":[{\"id\":3,\"type\":\"RESPONSE\",\"content\":\"你好\"}]}"),
            vec![("RESPONSE".to_string(), "你好".to_string())]
        );
        assert_eq!(
            parser.process_line("data: {\"p\":\"response/fragments/-1/content\",\"v\":\"！\"}"),
            vec![("RESPONSE".to_string(), "！".to_string())],
            "缺 o 字段按追加处理"
        );
        assert_eq!(
            parser.process_line("data: {\"v\":\"有什么\"}"),
            vec![("RESPONSE".to_string(), "有什么".to_string())]
        );
    }

    /// 真实网页数据（流式）：思考走 reasoning、正文走 content，状态帧两边都不进。
    #[tokio::test]
    async fn infer_stream_emits_thinking_and_answer_separately() {
        let body = concat!(
            "event: ready\n",
            "data: {\"request_message_id\":5,\"response_message_id\":6,\"model_type\":\"default\"}\n",
            "\n",
            "data: {\"v\":{\"response\":{\"message_id\":6,\"status\":\"WIP\",\"fragments\":[{\"id\":2,\"type\":\"THINK\",\"content\":\"The\"}]}}}\n",
            "\n",
            "data: {\"p\":\"response/fragments/-1/content\",\"o\":\"APPEND\",\"v\":\" user\"}\n",
            "data: {\"v\":\" thinks\"}\n",
            "data: {\"p\":\"response/fragments/-1/elapsed_secs\",\"o\":\"SET\",\"v\":0.83}\n",
            "data: {\"p\":\"response/fragments\",\"o\":\"APPEND\",\"v\":[{\"id\":3,\"type\":\"RESPONSE\",\"content\":\"你好\"}]}\n",
            "data: {\"p\":\"response/fragments/-1/content\",\"v\":\"！\"}\n",
            "data: {\"v\":\"有什么可以帮你的吗\"}\n",
            "data: {\"p\":\"response/status\",\"o\":\"SET\",\"v\":\"FINISHED\"}\n",
            "event: close\n",
            "data: {\"click_behavior\":\"none\"}\n",
        );
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let response = model_split(body, 7)
            .infer_stream(
                user_request(),
                Box::new(move |delta| sink.lock().unwrap().push(delta)),
            )
            .await
            .expect("流式推理应成功");

        let deltas = seen.lock().unwrap().clone();
        let mut reasoning = String::new();
        let mut content = String::new();
        for delta in &deltas {
            let chunk = crate::apis::base::parse_delta(delta);
            reasoning.push_str(&chunk.reasoning);
            content.push_str(&chunk.content);
        }
        assert_eq!(reasoning, "The user thinks");
        assert_eq!(content, "你好！有什么可以帮你的吗");
        assert_eq!(response.text, "你好！有什么可以帮你的吗", "正文不含思考");
        assert_eq!(
            response.reasoning.as_deref(),
            Some("The user thinks"),
            "思考单独返回"
        );
        assert!(response.tool_calls.is_none());
        assert_eq!(response.finish_reason.as_deref(), Some("stop"));
    }

    /// 真实网页数据（流式）：DSML 标记由无路径续帧逐块送达，既不外泄也能解析成调用。
    #[tokio::test]
    async fn infer_stream_reads_tool_call_from_bare_continuation_frames() {
        let body = concat!(
            "data: {\"v\":{\"response\":{\"message_id\":6,\"fragments\":[]}}}\n",
            "data: {\"p\":\"response/fragments\",\"o\":\"APPEND\",\"v\":[{\"id\":3,\"type\":\"RESPONSE\",\"content\":\"\"}]}\n",
            "data: {\"v\":\"<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\\\"read\"}\n",
            "data: {\"v\":\"_file\\\"><｜｜DSML｜｜ parameter name=\\\"path\\\" string=\\\"true\\\">a.rs</｜｜DSML｜｜ parameter>\"}\n",
            "data: {\"v\":\"</｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>\"}\n",
            "data: {\"p\":\"response/status\",\"o\":\"SET\",\"v\":\"FINISHED\"}\n",
        );
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let response = model_split(body, 6)
            .infer_stream(
                tool_request(),
                Box::new(move |delta| sink.lock().unwrap().push(delta)),
            )
            .await
            .expect("流式推理应成功");

        let deltas = seen.lock().unwrap().clone();
        let content: String = deltas
            .iter()
            .map(|delta| crate::apis::base::parse_delta(delta).content)
            .collect();
        assert_eq!(content, "", "标记不能作为正文流出");
        assert!(response.text.is_empty(), "{}", response.text);
        let calls = response.tool_calls.expect("应解析出工具调用");
        assert_eq!(calls[0]["function"]["name"], "read_file");
        assert_eq!(calls[0]["function"]["arguments"], "{\"path\":\"a.rs\"}");
        assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
    }

    /// 真实协议下工具调用解析出来、思考单独返回；有调用时正文整条不发。
    #[tokio::test]
    async fn infer_parses_real_protocol_frames() {
        let body = concat!(
            "data: {\"v\":{\"response\":{\"message_id\":8,\"fragments\":[{\"type\":\"THINK\",\"content\":\"想\"},{\"type\":\"RESPONSE\",\"content\":\"\"}]}}}\n",
            "data: {\"p\":\"response/fragments/1/content\",\"o\":\"APPEND\",\"v\":\"先读文件\"}\n",
            "data: {\"p\":\"response/fragments/1/content\",\"o\":\"APPEND\",\"v\":\"<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\\\"read_file\\\">\"}\n",
            "data: {\"p\":\"response/fragments/1/content\",\"o\":\"APPEND\",\"v\":\"<｜｜DSML｜｜ parameter name=\\\"path\\\" string=\\\"true\\\">a.rs</｜｜DSML｜｜ parameter>\"}\n",
            "data: {\"p\":\"response/fragments/1/content\",\"o\":\"APPEND\",\"v\":\"</｜｜DSML｜｜ invoke></｜｜DSML｜｜ calls>\"}\n",
        );
        let response = model(body).infer(tool_request()).await.expect("推理应成功");

        assert_eq!(response.text, "", "有工具调用时正文不发，标记也不进正文");
        assert_eq!(response.reasoning.as_deref(), Some("想"), "思考单独返回");
        let calls = response.tool_calls.expect("应解析出工具调用");
        assert_eq!(calls[0]["function"]["name"], "read_file");
        assert_eq!(calls[0]["function"]["arguments"], "{\"path\":\"a.rs\"}");
        assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
    }
}
