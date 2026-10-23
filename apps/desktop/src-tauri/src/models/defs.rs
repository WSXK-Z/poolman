//! 模型模块的公共定义：推理请求 / 结果、流式回调、并发语义、能力声明与渠道执行契约。

use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::Value;


/// 一次推理请求（OpenAI chat_completions 形状）。
#[derive(Debug, Clone)]
pub struct InferRequest {
    /// 对外模型名。
    pub model: String,
    /// OpenAI 格式的消息数组。
    pub messages: Vec<Value>,
    /// 是否流式返回。
    pub stream: bool,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_tokens: Option<u32>,
    pub tools: Option<Value>,
    pub tool_choice: Option<Value>,
    /// 对外会话标识（当前只透传，不做对外会话管理）。
    pub session_key: Option<String>,
    /// 本地会话 id：网页渠道用它绑定远端会话，后续只发增量。
    pub web_context_key: Option<String>,
    /// 调用方 API Key（用于会话归属）。
    pub api_key: Option<String>,
}

impl InferRequest {
    /// 只带模型与消息的最小请求（其余走默认值）。
    pub fn new(model: impl Into<String>, messages: Vec<Value>) -> Self {
        Self {
            model: model.into(),
            messages,
            stream: false,
            temperature: None,
            top_p: None,
            max_tokens: None,
            tools: None,
            tool_choice: None,
            session_key: None,
            web_context_key: None,
            api_key: None,
        }
    }

    /// 转成底层渠道适配器认识的 `options`（camelCase 键，与既有适配器契约一致）。
    pub fn to_options(&self) -> Value {
        serde_json::json!({
            "temperature": self.temperature,
            "topP": self.top_p,
            "maxTokens": self.max_tokens,
            "stream": self.stream,
            "sessionId": self.session_key,
            "tools": self.tools,
            "tool_choice": self.tool_choice,
            "apiKey": self.api_key,
        })
    }
}

/// 模型推理结果。
#[derive(Debug, Clone, Default)]
pub struct ModelResponse {
    /// 推理结果文本。
    pub text: String,
    /// 思考内容（网页版思考片段的原文）；渠道不产出思考时为 None。
    pub reasoning: Option<String>,
    /// 解析出的工具调用（OpenAI 标准 tool_calls 数组）。
    pub tool_calls: Option<Value>,
    /// 用量（若供应商提供）。
    pub usage: Option<Value>,
    /// 结束原因（如 stop / tool_calls）。
    pub finish_reason: Option<String>,
    /// 结果是否完整（流式过程中可能不完整）。
    pub is_complete: bool,
}

/// 流式增量回调。
pub type ChunkSink = Box<dyn FnMut(String) + Send + Sync>;

/// 模型的并发语义（模型基础属性，供调度层读取）。
#[derive(Debug, Clone, Copy)]
pub struct Concurrency {
    /// 并发上限：同时在途的请求数。
    pub max_inflight: u32,
    /// 同一会话是否必须串行（有状态网页渠道为 true，无状态 API 为 false）。
    pub per_session_serial: bool,
}

impl Concurrency {
    /// 可完全并行（无状态模型，如官方 API）。
    pub fn parallel(max_inflight: u32) -> Self {
        Self {
            max_inflight,
            per_session_serial: false,
        }
    }

    /// 会话池并发（有状态网页渠道）：最多 `max_inflight` 个请求同时在途，各自占用独立会话。
    pub fn pooled(max_inflight: u32) -> Self {
        Self {
            max_inflight: max_inflight.max(1),
            per_session_serial: true,
        }
    }

    /// 单并发且同会话串行（最保守，网页渠道默认）。
    pub fn single() -> Self {
        Self::pooled(1)
    }
}

impl Default for Concurrency {
    fn default() -> Self {
        Self::single()
    }
}

/// 模型能力声明。
#[derive(Debug, Clone, Default)]
pub struct ModelCapability {
    /// 支持流式输出。
    pub stream: bool,
    /// 支持工具调用。
    pub tools: bool,
    /// 支持图像输入。
    pub vision: bool,
    /// 单次请求可容纳的输入上限（token）。
    pub context_length: u32,
    /// 单次回答的输出上限（token）。
    pub max_output_tokens: u32,
}

impl ModelCapability {
    /// 基础能力：支持流式与工具调用，输入上限 64K、输出上限 8K。
    pub fn basic() -> Self {
        Self {
            stream: true,
            tools: true,
            vision: false,
            context_length: 64 * 1024,
            max_output_tokens: 8 * 1024,
        }
    }
}

/// 渠道 HTTP 请求（模型内部构造，交由执行器送达渠道）。
#[derive(Debug, Clone)]
pub struct ChannelRequest {
    /// 请求地址。
    pub url: String,
    /// 请求方法（GET / POST 等）。
    pub method: String,
    /// 请求头。
    pub headers: Vec<(String, String)>,
    /// 请求体（GET 等无体请求为 None）。
    pub body: Option<String>,
    /// 请求体字节（base64 编码）。有值时执行器按字节还原后作为请求体发出，`body` 被忽略；
    /// 用于 multipart 等含二进制段的请求。
    pub body_base64: Option<String>,
    /// 本次请求所属渠道会话（供传输层维持会话粘性）。
    pub session_key: Option<String>,
}

/// 渠道 HTTP 响应。
#[derive(Debug, Clone, Default)]
pub struct ChannelResponse {
    /// 是否 2xx 成功。
    pub ok: bool,
    /// HTTP 状态码。
    pub status: u16,
    /// 响应体文本。
    pub body: String,
}

/// 网页渠道会话池的一条槽位：槽位序号 + 渠道会话 id。
///
/// 网页会话是有状态的（同一会话不能并发），因此按并发数为渠道预建若干会话，
/// 每个在途请求独享一个槽位；槽位表持久化后可在重启时核对复用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PooledSession {
    /// 槽位序号（0 起）。
    pub slot: u32,
    /// 渠道会话 id。
    pub session_id: String,
}

/// 网页渠道会话池的持久化存储：按渠道读写槽位表。
///
/// 由持久化层实现；模型只按自己的渠道读写，不感知存储介质。
pub trait SessionPoolStore: Send + Sync {
    /// 读取某渠道已登记的槽位。
    fn load<'a>(&'a self, channel: &'a str) -> BoxFuture<'a, Result<Vec<PooledSession>>>;

    /// 覆盖写入某渠道的槽位表（空表表示清空该渠道）。
    fn save<'a>(
        &'a self,
        channel: &'a str,
        sessions: &'a [PooledSession],
    ) -> BoxFuture<'a, Result<()>>;
}

/// 网页渠道的本地会话 ↔ 远端会话绑定。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebContextBinding {
    /// 渠道标识。
    pub channel: String,
    /// 本地会话 id。
    pub context_key: String,
    /// 远端网页会话 id。
    pub remote_session_id: String,
    /// 远端最近一条消息 id（DeepSeek 的 parent_message_id；其他渠道可为空）。
    pub parent_message_id: Option<i64>,
    /// 是否已经完成初始化（system / 工具 / 历史 / 首问）。
    pub primed: bool,
    /// system prompt 摘要哈希。
    pub system_hash: String,
    /// 工具表摘要哈希。
    pub tools_hash: String,
    /// 远端上下文 token 估算。
    pub approx_tokens: i64,
    /// 旧会话的摘要：上下文超限迁移时只把摘要灌入新会话，不再发全量历史。
    pub summary: String,
}

/// 网页渠道会话绑定的持久化存储。
pub trait WebContextStore: Send + Sync {
    fn load<'a>(
        &'a self,
        channel: &'a str,
        context_key: &'a str,
    ) -> BoxFuture<'a, Result<Option<WebContextBinding>>>;

    fn save<'a>(&'a self, binding: &'a WebContextBinding) -> BoxFuture<'a, Result<()>>;

    fn delete<'a>(&'a self, channel: &'a str, context_key: &'a str)
        -> BoxFuture<'a, Result<()>>;
}

/// 渠道执行器：模型用它把请求送到渠道（网页平台）。
///
/// 本契约在模型模块内定义（不依赖适配器模块），由传输层（插件连接）实现；
/// 同时提供该渠道的凭证（token / settings token）。
pub trait ChannelExecutor: Send + Sync {
    /// 渠道访问令牌；未捕获时返回 None。
    fn token(&self) -> Option<String>;

    /// 渠道设置令牌。
    fn settings_token(&self) -> Option<String>;

    /// 插件捕获的网页请求模板（含真实签名头）；未捕获时为 None。
    fn api_template(&self) -> Option<serde_json::Value> {
        None
    }

    /// 执行一次请求，返回完整响应。
    fn execute(&self, request: ChannelRequest) -> BoxFuture<'_, Result<ChannelResponse>>;

    /// 执行一次流式请求，原始响应文本分块经 `on_chunk` 送达，最终返回完整响应。
    fn execute_stream(
        &self,
        request: ChannelRequest,
        on_chunk: ChunkSink,
    ) -> BoxFuture<'_, Result<ChannelResponse>>;
}
