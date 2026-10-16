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
use std::time::Duration;

use anyhow::{anyhow, Result};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use super::defs::{
    ChannelExecutor, ChannelRequest, ChunkSink, Concurrency, InferRequest, ModelCapability,
    ModelResponse, SessionPoolStore,
};
use super::model::Model;
use super::plugin::PluginModel;
use super::pow::{build_pow_header, solve_pow};
use crate::apis::base::DeltaChunk;
use crate::models::deepseek_web::chat_stream_processor::{ChatStreamProcessor, ToolSchemas};
use crate::models::deepseek_web::sse_frame_processor::{SseFrameProcessor, UpstreamError};

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
const INVOKE_OPEN: &str = "<｜｜DSML｜｜ invoke";
const INVOKE_CLOSE: &str = "</｜｜DSML｜｜ invoke>";

/// 参数的开边前缀（后面接 `参数名" string="…">`）/ 闭边。
const PARAMETER_OPEN: &str = "<｜｜DSML｜｜ parameter";
const PARAMETER_CLOSE: &str = "</｜｜DSML｜｜ parameter>";

/// 工具调用格式与限定规则：网页版没有原生工具调用，靠这段文字约束模型按 DSML 标记输出调用。
///
/// 标记形态必须与 [`ChatStreamProcessor`] 认的一致，
/// 且与 [`DeepSeekWebModel::render_tool_calls`] 渲染历史调用的方式一致。
const TOOL_RULE: &str = r#"## DSML 工具调用（极简·硬约束）

- **要调工具**：整条回复**只**输出 DSML，标记外零字符。
- **不调工具**：只输出正文，禁止出现任何 DSML 标签。

```dsml
<｜｜DSML｜｜ calls>
<｜｜DSML｜｜ invoke name="工具名">
<｜｜DSML｜｜ parameter name="参数名" string="true">值</｜｜DSML｜｜ parameter>
</｜｜DSML｜｜ invoke>
</｜｜DSML｜｜ calls>
```

| 规则 | 写法 |
|---|---|
| 分隔符 | 竖线**必须全角** `｜｜DSML｜｜`；`< > / = "` 全半角 |
| 标签 | 成对、不嵌套、不改写、不套代码块 |
| 字符串参数 | `string="true"`，原样写；`\`→`\\`，`"`→`\"` |
| 非字符串 | `string="false"`，值写 JSON：`30`、`true`、`[1,2]` |
| 多工具 | 多个 `invoke` **平铺**在同一 `calls` 内 |
| 命名 | `name` 与工具定义**逐字一致**；必填参数不可省 |
| 新调用 | **不写** `id`；`id` 仅历史用于关联 |
| 结果标题 | `【tool】name=工具名 id=调用编号` |
| 禁区 | 参数值内**不得含** `</｜｜DSML｜｜ parameter>` 或任何 DSML 片段 |

示例：
```dsml
<｜｜DSML｜｜ calls>
<｜｜DSML｜｜ invoke name="read_file">
<｜｜DSML｜｜ parameter name="path" string="true">d:\\project\\src\\main.rs</｜｜DSML｜｜ parameter>
</｜｜DSML｜｜ invoke>
<｜｜DSML｜｜ invoke name="bash">
<｜｜DSML｜｜ parameter name="command" string="true">ls -la</｜｜DSML｜｜ parameter>
<｜｜DSML｜｜ parameter name="timeout" string="false">30000</｜｜DSML｜｜ parameter>
</｜｜DSML｜｜ invoke>
</｜｜DSML｜｜ calls>
```
"#;

/// 工具调用修复提示词（`src/prompts/dsml_normalizer.md`，编译期嵌入）：DSML 解析失败时
/// 把坏块交给模型按它重写；`{{REGISTERED_TOOLS}}` 占位符由 [`DeepSeekWebModel::repair_prompt`] 填充。
const NORMALIZER_PROMPT: &str = include_str!("../prompts/dsml_normalizer.md");

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

/// 网页端报告的「对话达到长度上限」结束原因：这个池内会话要换新。
const CONTEXT_LIMIT_REASON: &str = "context_length_exceeded";

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
    /// 提问队列：网页端限制提问频率，提问按到达顺序排队，每秒放行一个。
    ask_queue: AskQueue,
    /// 并发语义（网页渠道：会话池并发）。
    concurrency: Concurrency,
}

/// 反转义 [`ChatStreamProcessor`] 解析参数值用的转义：值里的 `\` 与 `"` 写成转义形态。
///
/// 渲染历史调用（[`DeepSeekWebModel::render_tool_calls`]）必须用它，否则同一个值渲染后再解析回来
/// 会被反转义吃掉一层反斜杠（如 UNC 路径 `\\server\share` 变成 `\server\share`）。
fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
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
            ask_queue: AskQueue::new(),
            concurrency: Concurrency::single(),
        }
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

    /// 组装修复提示词：规范化器提示词（`dsml_normalizer.md`）填上可用工具清单，
    /// 末尾附上解析失败的坏块原文——模型对它只回规范 DSML 调用块或「无」。
    fn repair_prompt(tools: Option<&Value>, broken_markup: &str) -> String {
        let registered = Self::render_tools(tools).unwrap_or_else(|| "（无）".to_string());
        format!(
            "{}\n\n{}\n\n{}",
            PROMPT_PREFIX,
            NORMALIZER_PROMPT.replace("{{REGISTERED_TOOLS}}", &registered),
            broken_markup
        )
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
                lines.push(format!(
                    "{INVOKE_OPEN} name=\"{name}\"{id}>{INVOKE_CLOSE}"
                ));
                continue;
            };
            lines.push(format!("{INVOKE_OPEN} name=\"{name}\"{id}>"));
            for (key, value) in params {
                // 字符串参数按解析端的反转义规则转义，保证渲染后再解析回来还是同一个值
                let (flag, literal) = match value {
                    Value::String(text) => ("true", escape(text)),
                    other => ("false", other.to_string()),
                };
                lines.push(format!(
                    "{PARAMETER_OPEN} name=\"{key}\" string=\"{flag}\">{literal}{PARAMETER_CLOSE}"
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

    /// 聊天共用实现：`on_chunk` 为 Some 时流式回吐正文增量，工具调用在收尾时解析；
    /// 解析失败（DSML 块没写完）时用规范化器提示词（`dsml_normalizer.md`）重写一轮，最多修复一次。
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
        // 工具参数类型以请求里的 schema 为准：模型爱把数字、数组也标成字符串
        let schemas = ToolSchemas::from_tools(request.tools.as_ref());
        let _slot_permit = self
            .gate
            .acquire()
            .await
            .map_err(|_| anyhow!("渠道并发闸门已关闭"))?;
        let mut session_id = self.acquire_pool_slot().await?;
        // 用之前核对：槽位上的会话若已被网页端删掉，这里会补建一个，别让整轮请求白跑
        session_id = self.ensure_pool_session(session_id).await?;
        // 增量出口只能移动一次，而「上下文到上限」时这一轮要换会话重跑：包一层共享单元转发，
        // 每次尝试各取一个代理。
        let sink_cell = Arc::new(Mutex::new(on_chunk));
        let mut switched_session = false;
        let mut round = loop {
            let parent_message_id = self.parent_message_id(&session_id);
            info!(
                "DeepSeek 网页推理 session={} parent_message_id={} prompt_len={}",
                session_id,
                parent_message_id,
                prompt.len()
            );
            let attempt = self
                .run_completion_round(
                    &session_id,
                    &parent_message_id,
                    &prompt,
                    recognize_tools,
                    &schemas,
                    forwarding_sink(&sink_cell),
                )
                .await?;
            // 网页端说上下文到顶了：这个池内会话的累积历史已经顶满。后台删掉它、按同标题换一个
            // 新会话再跑一次，客户端看不到这次切换。
            if !switched_session && hits_context_limit(&attempt) {
                switched_session = true;
                match self.recycle_pool_session(&session_id).await {
                    Ok(Some(created)) => {
                        info!(
                            "网页上下文到上限，已换新会话 {}（原 {}）后重跑本轮",
                            created, session_id
                        );
                        session_id = created;
                        continue;
                    }
                    Ok(None) => {
                        warn!("网页上下文到上限，但会话 {} 不在会话池内，未换新", session_id)
                    }
                    Err(error) => warn!("网页上下文到上限，换新会话失败: {}", error),
                }
            }
            break attempt;
        };
        // 工具调用没写完：用规范化器提示词（dsml_normalizer.md）把坏块重写一轮，修出调用就当正常结果
        let mut repaired = false;
        if let Some(broken) = round.parse_failure.take() {
            let prompt = Self::repair_prompt(request.tools.as_ref(), &broken);
            info!("工具调用解析失败，发起修复重写 prompt_len={}", prompt.len());
            // 修复请求接在本轮回答之后：网页会话里它是紧接着的下一条消息
            let parent = round
                .message_id
                .map(|id| Value::Number(id.into()))
                .unwrap_or(Value::Null);
            match self
                .run_completion_round(&session_id, &parent, &prompt, true, &schemas, None)
                .await
            {
                Ok(attempt) => {
                    if !attempt.tool_calls.is_empty() {
                        repaired = true;
                        // 修复出的调用按原增量格式发出：index 接在已有调用之后（客户端按 index 拼参数）
                        let base = round.tool_calls.len();
                        if let Some(sink) = forwarding_sink(&sink_cell) {
                            let indexed: Vec<Value> = attempt
                                .tool_calls
                                .iter()
                                .enumerate()
                                .map(|(offset, call)| {
                                    let mut call = call.clone();
                                    call["index"] = json!(base + offset);
                                    info!(
                                        tool = %call["function"]["name"].as_str().unwrap_or_default(),
                                        arguments = %call["function"]["arguments"].as_str().unwrap_or_default(),
                                        "修复出工具调用"
                                    );
                                    call
                                })
                                .collect();
                            emit_delta(sink, DeltaChunk::FUNCTIONS, Value::Array(indexed));
                        }
                        round.tool_calls.extend(attempt.tool_calls);
                    }
                    // 修复轮的回答才是网页会话里最新的一条消息
                    if attempt.message_id.is_some() {
                        round.message_id = attempt.message_id;
                    }
                }
                Err(error) => warn!("工具调用修复请求失败，按解析失败处理：{}", error),
            }
            if !repaired {
                // 修复不成（模型回「无」或请求失败）：流式客户端补一条解析失败提示（非流式维持空回答）
                if let Some(sink) = forwarding_sink(&sink_cell) {
                    emit_delta(sink, DeltaChunk::RESPONSE, json!("工具调用格式不正确"));
                }
            }
        }
        let CompletionRound {
            text,
            reasoning,
            tool_calls,
            upstream_error,
            message_id,
            status,
            raw_body,
            parse_failure: _,
        } = round;
        // 记下本轮响应的消息 id：下一轮请求的 parent_message_id 靠它接上服务端的消息链。
        self.remember_message_id(&session_id, message_id);

        let has_calls = !tool_calls.is_empty();
        let finish_reason = if has_calls { "tool_calls" } else { "stop" };
        if text.trim().is_empty() && !has_calls {
            // 整块（非流式）时响应体在手上；流式没有响应体，只能靠日志说明这轮没内容
            let raw = head_chars(&raw_body, 300);
            if let Some(upstream) = upstream_error {
                // 换会话重跑过还是这个错：把上游原因原样报给调用方（例如提示词本身就超长，
                // 只能由调用方缩短），别只说「空回答」让它无从判断。
                let reason = match upstream.finish_reason.as_deref() {
                    Some(reason) => format!("{}（{}）", upstream.message, reason),
                    None => upstream.message.clone(),
                };
                warn!(
                    "网页上游报错：session={} status={} message={}",
                    session_id, status, reason
                );
                anyhow::bail!("{}", reason);
            }
            warn!(
                "网页回答为空：session={} status={} 未解析出正文与工具调用，原始响应：{}",
                session_id, status, raw
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

    /// 跑一轮网页推理：发完成请求，按流式（`on_chunk` 为 Some）或整块收结果，
    /// 返回解析产物与排查用的原文证据。
    ///
    /// 会话与父消息 id 由调用方给定：同一个会话在「上下文到上限」后会被换成新的再跑一轮。
    async fn run_completion_round(
        &self,
        session_id: &str,
        parent_message_id: &Value,
        prompt: &str,
        recognize_tools: bool,
        schemas: &ToolSchemas,
        on_chunk: Option<ChunkSink>,
    ) -> Result<CompletionRound> {
        let channel_request = ChannelRequest {
            url: COMPLETION_URL.to_string(),
            method: "POST".to_string(),
            headers: self.completion_headers(Some(session_id)).await,
            body: Some(self.completion_body(session_id, parent_message_id, prompt)),
            session_key: Some(session_id.to_string()),
        };
        // 排队等放行：网页端每秒最多接受一次提问。
        self.ask_queue.enqueue().await;

        let (text, reasoning, tool_calls, parse_failure, upstream_error, message_id, status, raw_body) =
            match on_chunk {
                Some(user_sink) => {
                    // 帧处理器与增量处理器各自独立：闭包是 FnMut，跨 chunk 的状态用 Arc<Mutex> 共享。
                    let frames = Arc::new(Mutex::new(SseFrameProcessor::new()));
                    let processor = Arc::new(Mutex::new(
                        ChatStreamProcessor::new(user_sink, recognize_tools)
                            .with_tool_schemas(schemas.clone()),
                    ));
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
                    let (tail, message_id, upstream_error) = {
                        let mut frames = frames.lock().unwrap();
                        (
                            frames.finish(),
                            frames.message_id(),
                            frames.upstream_error().cloned(),
                        )
                    };
                    processor.feed(tail);
                    processor.finish();
                    (
                        processor.text().to_string(),
                        processor.reasoning().to_string(),
                        processor.tool_calls().to_vec(),
                        processor.parse_failure().map(str::to_string),
                        upstream_error,
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
                    let (processor, message_id, upstream_error) =
                        parse_sse_text(&response.body, recognize_tools, schemas);
                    (
                        processor.text().to_string(),
                        processor.reasoning().to_string(),
                        processor.tool_calls().to_vec(),
                        processor.parse_failure().map(str::to_string),
                        upstream_error,
                        message_id,
                        response.status,
                        response.body,
                    )
                }
            };
        Ok(CompletionRound {
            text,
            reasoning,
            tool_calls,
            parse_failure,
            upstream_error,
            message_id,
            status,
            raw_body,
        })
    }
}

/// 提问队列：提问按到达顺序排队，每秒放行一个。
///
/// 网页端对提问频率有限制，提交太快会直接报错；这里给每个提问定一个放行时刻，
/// 排在队尾之后 [`AskQueue::INTERVAL`]，因此相邻两次提问的间隔不小于一秒。
struct AskQueue {
    /// 队尾的放行时刻；None 表示队列为空。
    tail: tokio::sync::Mutex<Option<tokio::time::Instant>>,
}

impl AskQueue {
    /// 相邻两次放行之间的间隔。
    const INTERVAL: Duration = Duration::from_secs(1);

    fn new() -> Self {
        Self {
            tail: tokio::sync::Mutex::new(None),
        }
    }

    /// 排到队尾，等到轮到自己再返回。
    ///
    /// 队伍空时立即放行；否则接在上一个提问之后一秒。取号在锁内完成，
    /// 因此放行顺序与排队顺序一致。
    async fn enqueue(&self) {
        let scheduled = {
            let mut tail = self.tail.lock().await;
            let (scheduled, next) = Self::reserve(*tail, tokio::time::Instant::now());
            *tail = Some(next);
            scheduled
        };
        tokio::time::sleep_until(scheduled).await;
    }

    /// 取号：定出这次提问的放行时刻，并给出队尾的新时刻。
    ///
    /// 队尾已经过去（队列空了一会儿）就地放行，否则接在队尾之后一个间隔。
    fn reserve(
        tail: Option<tokio::time::Instant>,
        now: tokio::time::Instant,
    ) -> (tokio::time::Instant, tokio::time::Instant) {
        let scheduled = match tail {
            Some(previous) if previous > now => previous,
            _ => now,
        };
        (scheduled, scheduled + Self::INTERVAL)
    }
}

/// 一轮推理的产物：解析出的内容 + 上游报错 + 排查用的原文证据。
struct CompletionRound {
    text: String,
    reasoning: String,
    tool_calls: Vec<Value>,
    /// DSML 块开了却没写出可解析的调用时的坏块原文：触发一次规范化修复重写。
    parse_failure: Option<String>,
    upstream_error: Option<UpstreamError>,
    message_id: Option<i64>,
    status: u16,
    raw_body: String,
}

/// 这一轮是不是「网页端说上下文到上限」：换会话重跑的信号。
fn hits_context_limit(round: &CompletionRound) -> bool {
    round.text.trim().is_empty()
        && round.tool_calls.is_empty()
        && round
            .upstream_error
            .as_ref()
            .and_then(|error| error.finish_reason.as_deref())
            == Some(CONTEXT_LIMIT_REASON)
}

/// 取一个把增量转发到同一个出口的代理；请求不带出口时返回 None。
///
/// 出口（[`ChunkSink`]）只能移动一次，而「上下文到上限」时同一轮要换会话重跑，
/// 因此每次尝试各取一个代理，统一转发到最初那个出口。
fn forwarding_sink(cell: &Arc<Mutex<Option<ChunkSink>>>) -> Option<ChunkSink> {
    if !cell.lock().map(|guard| guard.is_some()).unwrap_or(false) {
        return None;
    }
    let cell = cell.clone();
    Some(Box::new(move |delta: String| {
        if let Ok(mut guard) = cell.lock() {
            if let Some(sink) = guard.as_mut() {
                sink(delta);
            }
        }
    }))
}

/// 出站一条单键增量（与 [`ChatStreamProcessor`] 的增量同格式：`{"键":值}`）。
fn emit_delta(mut sink: ChunkSink, key: &str, value: Value) {
    let mut frame = serde_json::Map::with_capacity(1);
    frame.insert(key.to_string(), value);
    sink(Value::Object(frame).to_string());
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

/// 取正文开头（按字符截断，不切断多字节字符）。
fn head_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// 解析整段响应文本（非流式情形）：返回收尾后的增量处理器（供读取正文 / 思考 / 工具调用）
/// 与响应消息 id（供下轮 `parent_message_id`）。
fn parse_sse_text(
    body: &str,
    recognize_tools: bool,
    schemas: &ToolSchemas,
) -> (ChatStreamProcessor, Option<i64>, Option<UpstreamError>) {
    let mut frames = SseFrameProcessor::new();
    let mut processor =
        ChatStreamProcessor::new(Box::new(|_| {}), recognize_tools).with_tool_schemas(schemas.clone());
    processor.feed(frames.feed(body));
    // 整段文本的最后一行可能没有换行符，收尾时补处理。
    processor.feed(frames.finish());
    processor.finish();
    (
        processor,
        frames.message_id(),
        frames.upstream_error().cloned(),
    )
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
    use super::{AskQueue, DeepSeekWebModel, ToolSchemas, parse_sse_text, CHANNEL, PROMPT_PREFIX};
    use crate::models::defs::{
        ChannelExecutor, ChannelRequest, ChannelResponse, ChunkSink, InferRequest, PooledSession,
        SessionPoolStore,
    };
    use crate::models::model::Model;
    use crate::models::plugin::PluginModel;
    use futures_util::future::BoxFuture;
    use serde_json::{json, Value};
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

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
        /// 首个完成请求返回「上下文到长度上限」，之后恢复正常回答
        /// （模拟网页端在那个会话上顶到上限，换成新会话后就能答）。
        limit_first_completion: AtomicBool,
        /// 完成请求的预设响应队列：非空时逐个顶替默认响应体（模拟「先回坏块、修复轮回好块」）。
        completion_queue: Mutex<VecDeque<String>>,
    }

    /// 「上下文到长度上限」的响应体：网页端在 hint 负载里给出原因与结束原因。
    const LIMIT_BODY: &str = concat!(
        "event: ready\n",
        "data: {\"request_message_id\":59,\"response_message_id\":60,\"model_type\":\"default\"}\n\n",
        "data: {\"type\":\"error\",\"content\":\"达到对话长度上限，请开启新对话\",\"clear_response\":true,\"finish_reason\":\"context_length_exceeded\"}\n",
    );

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

        /// 预设接下来几个完成请求的响应体（按序消费，用完回落到默认响应体）。
        fn queue_completions(&self, bodies: &[String]) {
            self.completion_queue
                .lock()
                .unwrap()
                .extend(bodies.iter().cloned());
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
                } else if request.url.contains("chat/completion") {
                    // 完成请求：首个按「上下文到长度上限」回（与流式路径同一个开关）；
                    // 预设队列非空时逐个取用（修复链路：先回坏块、修复轮回修好的调用）
                    if self.limit_first_completion.swap(false, Ordering::SeqCst) {
                        LIMIT_BODY.to_string()
                    } else {
                        self.completion_queue
                            .lock()
                            .unwrap()
                            .pop_front()
                            .unwrap_or_else(|| self.completion_body.clone())
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
                // 首个完成请求按「上下文到长度上限」回，之后恢复正常回答；预设队列优先。
                let limited = self.limit_first_completion.swap(false, Ordering::SeqCst);
                let body = if limited {
                    LIMIT_BODY.to_string()
                } else {
                    self.completion_queue
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or_else(|| self.completion_body.clone())
                };
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
            limit_first_completion: AtomicBool::new(false),
            completion_queue: Mutex::new(VecDeque::new()),
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
        // 正文边到边发：第一段够长，足以在流中就看到内容
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

    /// 非流式：正文里的 DSML 工具调用解析成 tool_calls；调用之前的正文照常返回，之后的不返回。
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

        assert_eq!(response.text, "先读文件", "调用之前的正文照常返回");
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

    /// 块没闭合（模型写一半断了）时这轮没有可用回答：写坏的标记不当回答发出去，只落一条空回答。
    #[tokio::test]
    async fn infer_drops_unclosed_dsml_markup() {
        let half = "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"read_file\">";
        let response = model(&content_frame(half))
            .infer(tool_request())
            .await
            .expect("写坏的标记不是正文，也不该让整轮请求失败");

        assert_eq!(response.text, "", "写坏的标记不能当回答发出去");
        assert!(response.tool_calls.is_none());
    }

    /// 上游用 `event: hint` 说明这轮为什么没有回答（例如达到对话长度上限）：
    /// 原样报给调用方，而不是只回一条空回答。
    #[tokio::test]
    async fn infer_reports_upstream_hint_error() {
        let body = concat!(
            "event: ready\n",
            "data: {\"request_message_id\":59,\"response_message_id\":60,\"model_type\":\"default\"}\n\n",
            "event: hint\n",
            "data: {\"type\":\"error\",\"content\":\"达到对话长度上限，请开启新对话\",\"clear_response\":true,\"finish_reason\":\"context_length_exceeded\"}\n\n",
            "event: close\n",
            "data: {\"click_behavior\":\"none\",\"auto_resume\":false}\n",
        );
        let error = model(body)
            .infer(user_request())
            .await
            .expect_err("上游报错时要报错，不能返回空回答");

        let message = error.to_string();
        assert!(message.contains("达到对话长度上限"), "{}", message);
        assert!(message.contains("context_length_exceeded"), "{}", message);
    }

    /// 网页端说达到对话长度上限：后台删掉这个池内会话、换一个同标题的新会话重跑一轮，
    /// 客户端只看到正常回答（无感切换）。
    #[tokio::test]
    async fn context_limit_error_switches_session_transparently() {
        let executor = executor(&content_frame("答"), 0);
        executor.seed_session("sess-full", &format!("{POOL_TITLE_PREFIX}1"));
        let store = Arc::new(MemoryStore::default());
        store.slots.lock().unwrap().insert(
            CHANNEL.to_string(),
            vec![PooledSession {
                slot: 0,
                session_id: "sess-full".to_string(),
            }],
        );
        // 第一个完成请求撞长度上限，换会话后恢复正常回答
        executor
            .limit_first_completion
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let model = pooled_model(&executor, &store, 1);
        let response = model
            .infer(user_request())
            .await
            .expect("换会话后应正常回答，不向客户端报错");
        assert_eq!(response.text, "答");

        let slots = stored_sessions(&store);
        assert_eq!(slots.len(), 1);
        assert_ne!(slots[0], "sess-full", "顶到上限的会话要被换掉");
        assert!(
            !executor.sessions().contains_key("sess-full"),
            "旧会话应从网页端删除：{:?}",
            executor.sessions()
        );
        // 两次完成请求落在不同会话上：第一次是旧会话，第二次是换新的那个
        let completions: Vec<String> = executor
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.url.ends_with("/chat/completion"))
            .map(|request| request.body.clone().unwrap_or_default())
            .collect();
        assert_eq!(completions.len(), 2, "撞上限后应重跑一轮");
        assert!(
            completions[0].contains("sess-full") && !completions[1].contains("sess-full"),
            "第二轮必须换到新会话上"
        );
        assert!(
            executor.rename_count() >= 1,
            "新会话要按原槽位标题命名（网页端拒绝空会话改名时留给首轮成功后补）"
        );
    }

    /// 换会话重跑后仍然撞上限（提示词本身就超长）：这轮报错，把上游原因交给调用方。
    #[tokio::test]
    async fn context_limit_error_after_switch_reports_upstream_reason() {
        let executor = executor(LIMIT_BODY, 0);
        executor.seed_session("sess-full", &format!("{POOL_TITLE_PREFIX}1"));
        let store = Arc::new(MemoryStore::default());
        store.slots.lock().unwrap().insert(
            CHANNEL.to_string(),
            vec![PooledSession {
                slot: 0,
                session_id: "sess-full".to_string(),
            }],
        );

        let error = pooled_model(&executor, &store, 1)
            .infer(user_request())
            .await
            .expect_err("换会话后还是超长只能报错");
        assert!(error.to_string().contains("达到对话长度上限"), "{}", error);
        // 只重跑一次，不无限换会话
        let completions = executor
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.url.ends_with("/chat/completion"))
            .count();
        assert_eq!(completions, 2, "只重跑一次");
        assert_ne!(stored_sessions(&store)[0], "sess-full");
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
    /// 调用之前的正文照常发，之后的正文不发。
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
        assert_eq!(contents, "先读文件", "调用之前的正文照常发、标记不外泄");
        assert!(
            deltas.iter().any(|delta| delta.contains("read_file")),
            "应有承载工具调用的增量：{:?}",
            deltas
        );
        assert_eq!(response.text, "先读文件");
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

    /// 提问排队取号：队尾在将来就接在队尾之后，否则立即放行；相邻放行固定隔一个间隔。
    #[tokio::test]
    async fn ask_queue_reserves_one_slot_per_interval() {
        let now = tokio::time::Instant::now();

        let (scheduled, tail) = AskQueue::reserve(None, now);
        assert_eq!(scheduled, now, "空队列立即放行");
        assert_eq!(tail, now + AskQueue::INTERVAL, "队尾顺延一个间隔");

        let (scheduled, tail) = AskQueue::reserve(Some(tail), now);
        assert_eq!(scheduled, now + AskQueue::INTERVAL, "接在上一个提问之后");
        assert_eq!(tail, now + AskQueue::INTERVAL * 2);

        let (scheduled, tail) = AskQueue::reserve(Some(now - AskQueue::INTERVAL), now);
        assert_eq!(scheduled, now, "队尾已过去时按当前时刻排");
        assert_eq!(tail, now + AskQueue::INTERVAL);
    }

    /// 提问排队：按到达顺序放行，三个提问要跨两个间隔才走完。
    #[tokio::test]
    async fn ask_queue_releases_asks_in_queue_order() {
        let queue = Arc::new(AskQueue::new());
        let entered = Arc::new(Mutex::new(None::<tokio::time::Instant>));
        let released = Arc::new(Mutex::new(Vec::<(usize, tokio::time::Instant)>::new()));
        let mut handles = Vec::new();
        for index in 0..3 {
            let queue = queue.clone();
            let entered = entered.clone();
            let released = released.clone();
            handles.push(tokio::spawn(async move {
                entered
                    .lock()
                    .unwrap()
                    .get_or_insert_with(tokio::time::Instant::now);
                queue.enqueue().await;
                released
                    .lock()
                    .unwrap()
                    .push((index, tokio::time::Instant::now()));
            }));
        }
        for handle in handles {
            handle.await.expect("排队任务不应结束于失败");
        }

        let released = released.lock().unwrap();
        let order: Vec<usize> = released.iter().map(|(index, _)| *index).collect();
        assert_eq!(order, vec![0, 1, 2], "放行顺序应与排队顺序一致");
        // 计时从入队之前算起、记在放行之后结束，两头只会往长里算；容差留给定时器提前几毫秒到点。
        let entered = entered.lock().unwrap().expect("应记录过入队时刻");
        let elapsed = released.last().unwrap().1 - entered;
        assert!(
            elapsed + Duration::from_millis(20) >= AskQueue::INTERVAL * 2,
            "三个提问应跨两个间隔，实际 {elapsed:?}"
        );
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
        assert!(prompt.contains("必须全角"), "{}", prompt);
        assert!(prompt.contains("逐字一致"), "{}", prompt);
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
        let processor = parse_sse_text(&content_frame(&markup), true, &ToolSchemas::default()).0;

        let calls = processor.tool_calls();
        assert_eq!(calls.len(), 2, "渲染的标记应还原成两个调用：{markup}");
        let arguments = |index: usize| -> Value {
            serde_json::from_str(calls[index]["function"]["arguments"].as_str().unwrap()).unwrap()
        };
        assert_eq!(calls[0]["function"]["name"], "read_file");
        assert_eq!(arguments(0)["path"], json!("a.rs"));
        assert_eq!(calls[1]["function"]["name"], "view");
        assert_eq!(arguments(1)["path"], json!("b.rs"));
        assert_eq!(arguments(1)["view_range"], json!([320, 400]));
        assert_eq!(arguments(1)["raw"], json!(true));
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

        assert_eq!(response.text, "先读文件", "调用之前的正文照常返回，标记不进正文");
        assert_eq!(response.reasoning.as_deref(), Some("想"), "思考单独返回");
        let calls = response.tool_calls.expect("应解析出工具调用");
        assert_eq!(calls[0]["function"]["name"], "read_file");
        assert_eq!(calls[0]["function"]["arguments"], "{\"path\":\"a.rs\"}");
        assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
    }

    /// 规范化器提示词（`dsml_normalizer.md`）与解析器共用同一套标记字面量，且保留运行时占位符。
    #[test]
    fn normalizer_prompt_agrees_with_markup_constants() {
        assert!(
            super::NORMALIZER_PROMPT.contains("{{REGISTERED_TOOLS}}"),
            "占位符不能丢"
        );
        for literal in [
            super::CALLS_OPEN,
            super::CALLS_CLOSE,
            super::INVOKE_OPEN,
            super::INVOKE_CLOSE,
            super::PARAMETER_OPEN,
            super::PARAMETER_CLOSE,
        ] {
            assert!(
                super::NORMALIZER_PROMPT.contains(literal),
                "修复提示词应含 {literal}"
            );
        }
    }

    /// 修复提示词：占位符换成工具清单，坏块原文附在末尾，前面带固定前缀。
    #[test]
    fn repair_prompt_fills_tools_and_appends_broken_markup() {
        let tools = json!([{ "function": { "name": "read_file", "description": "读取文件" } }]);
        let prompt = DeepSeekWebModel::repair_prompt(Some(&tools), "<坏块>");
        assert!(prompt.starts_with(super::PROMPT_PREFIX), "{}", prompt);
        assert!(
            !prompt.contains("{{REGISTERED_TOOLS}}"),
            "占位符应被替换：{}",
            prompt
        );
        assert!(prompt.contains("- read_file：读取文件"), "{}", prompt);
        assert!(prompt.ends_with("<坏块>"), "{}", prompt);

        // 没有工具时占位符填「（无）」：修复模型对任何输入都只能回「无」
        let empty = DeepSeekWebModel::repair_prompt(None, "x");
        assert!(!empty.contains("{{REGISTERED_TOOLS}}"), "{}", empty);
    }

    /// 工具调用没写完时发起一次修复：坏块连同规范化器提示词（含工具清单）再问一轮，
    /// 修出的调用按原格式返回。
    #[tokio::test]
    async fn repair_round_recovers_broken_tool_call() {
        let half = "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"read_file\">";
        let fixed = content_frame(&dsml_calls(&[dsml_invoke("read_file", &[("path", "a.rs")])]));
        // 默认响应体给「无」：首轮与修复轮都从预设队列取
        let executor = executor(&content_frame("无"), 0);
        executor.queue_completions(&[content_frame(half), fixed]);
        let model = DeepSeekWebModel::new("DeepSeek", executor.clone() as Arc<dyn ChannelExecutor>);

        let response = model
            .infer(tool_request())
            .await
            .expect("修复后应正常返回调用");
        assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
        let calls = response.tool_calls.expect("坏块应被修复成调用");
        assert_eq!(calls[0]["function"]["name"], "read_file");
        assert_eq!(calls[0]["function"]["arguments"], "{\"path\":\"a.rs\"}");

        // 第二次 completion 是修复请求：提示词里带规范化器、工具清单与坏块原文
        let bodies = executor.completion_bodies();
        assert_eq!(bodies.len(), 2, "首轮 + 修复轮：{:?}", bodies.len());
        let repair_prompt = bodies[1]["prompt"].as_str().unwrap();
        assert!(
            repair_prompt.contains("工具调用规范化器"),
            "{}",
            repair_prompt
        );
        assert!(
            repair_prompt.contains("- read_file：读取文件"),
            "应填充工具清单：{}",
            repair_prompt
        );
        assert!(repair_prompt.contains(half), "应带上坏块原文：{}", repair_prompt);
    }

    /// 修复也失败（模型没给出调用）：流式客户端收到一条解析失败提示，整轮仍不报错。
    #[tokio::test]
    async fn repair_failure_reports_to_stream() {
        let half = "<｜｜DSML｜｜ calls><｜｜DSML｜｜ invoke name=\"read_file\">";
        // 队列只预设首轮的坏块：修复轮回落到默认响应体「无」
        let executor = executor(&content_frame("无"), 0);
        executor.queue_completions(&[content_frame(half)]);
        let model = DeepSeekWebModel::new("DeepSeek", executor.clone() as Arc<dyn ChannelExecutor>);
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();

        let response = model
            .infer_stream(
                tool_request(),
                Box::new(move |delta| sink.lock().unwrap().push(delta)),
            )
            .await
            .expect("修复失败也不该让整轮请求报错");

        let contents: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|delta| crate::apis::base::parse_delta(delta).content)
            .collect();
        assert_eq!(
            contents,
            vec!["工具调用格式不正确".to_string()],
            "修复不成才提示解析失败"
        );
        assert!(response.tool_calls.is_none(), "{:?}", response.tool_calls);
        assert!(response.text.is_empty(), "「无」不应当成回答发给客户端");
    }
}
