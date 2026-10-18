//! 腾讯元宝网页版模型：实现插件模型抽象，一个模型对应 `yuanbao-web` 渠道。
//!
//! 会话管理（创建 / 列表 / 删除 / 历史）与聊天请求组装、流式帧解析都在模型内部完成；
//! 对外的请求发送与凭证读取经 [`ChannelExecutor`]（由传输层实现）。
//!
//! 历史管理与 DeepSeek 网页渠道一致：每轮把整段对话拼进 `prompt`，并要求模型只依据本次内容作答，
//! 因此不依赖服务端会话里的历史。会话本身只作为请求的落点。
//!
//! 请求签名（`x-uskey`、`x-bus-params-md5` 等）由页面上下文生成，模型侧只填业务头。

pub mod frame_processor;

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use super::defs::{
    ChannelExecutor, ChannelRequest, ChunkSink, Concurrency, InferRequest, ModelCapability,
    ModelResponse,
};
use super::model::Model;
use super::plugin::PluginModel;
use super::yuanbao_web::frame_processor::YuanbaoFrameProcessor;
use crate::apis::base::DeltaChunk;

/// 渠道标识。
pub const CHANNEL: &str = "yuanbao-web";

/// 对话接口前缀，后面接会话 id。
const CHAT_URL_PREFIX: &str = "https://yuanbao.tencent.com/api/chat/";

/// 创建会话。
const SESSION_CREATE_URL: &str = "https://yuanbao.tencent.com/api/user/agent/conversation/create";

/// 会话列表。
const SESSION_LIST_URL: &str =
    "https://yuanbao.tencent.com/api/user/agent/conversation/v3/list";

/// 会话详情（历史）。
const SESSION_DETAIL_URL: &str =
    "https://yuanbao.tencent.com/api/user/agent/conversation/v1/detail";

/// 删除 / 清空会话。
const SESSION_CLEAR_URL: &str = "https://yuanbao.tencent.com/api/user/agent/conversation/v1/clear";

/// 网页端使用的智能体标识。
const AGENT_ID: &str = "naQivTmsDa";

/// 网页端使用的模型名。
const MODEL: &str = "gpt_175B_0404";

/// 网页端使用的模型 id。
const CHAT_MODEL_ID: &str = "hunyuan_gpt_175B_0404";

/// 聊天请求体里的 `chatModelExtInfo` 原文。
fn chat_model_ext_info() -> String {
    serde_json::json!({
        "modelId": CHAT_MODEL_ID,
        "agentModeModelSetting": { "modelId": CHAT_MODEL_ID },
        "supportFunctions": { "internetSearch": "" },
        "internetSearch": "autoInternetSearch",
    })
    .to_string()
}

/// 网页端默认打开的联网搜索能力。
const SUPPORT_FUNCTIONS: [&str; 2] = ["openAutoSearchSwitch", "autoInternetSearch"];

/// 网页请求模板里可复用的签名 / 设备头：页面 JS 生成，桌面端无法离线复刻。
const TEMPLATE_HEADERS: [&str; 29] = [
    "x-uskey",
    "x-bus-params-md5",
    "x-timestamp",
    "x-commit-tag",
    "x-device-id",
    "x-hy92",
    "x-hy93",
    "x-hy106",
    "x-id",
    "t-userid",
    "x-instance-id",
    "x-language",
    "x-os_version",
    "x-platform",
    "x-exp-params",
    "appid",
    "sign",
    "ts",
    "traceid",
    "x-traceparent",
    "x-trid-channel",
    "chat_version",
    "x-event-input-type",
    "x-input-type",
    "x-web-ch-id",
    "x-web-third-source",
    "x-webdriver",
    "x-webversion",
    "x-ybuitest",
];

/// 时区偏移（小时）。
const OFFSET_OF_HOUR: i32 = 8;

/// 时区偏移（分钟）。
const OFFSET_OF_MINUTE: i32 = 0;

/// 会话 id 的最小长度：太短的返回值不像真实会话 id。
const MIN_SESSION_ID_LEN: usize = 10;

/// 空回答排查时留存的原始响应长度（字符）。
const RAW_HEAD_CHARS: usize = 2_000;

/// 相邻两次提问的最小间隔：网页端对提问频率有限制，连续提交会回「服务繁忙」。
const MIN_CHAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// 元宝上游在流里报的错（如「服务繁忙，请稍后再试。」）。
#[derive(Debug)]
struct UpstreamFailure(String);

impl std::fmt::Display for UpstreamFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "元宝上游报错：{}", self.0)
    }
}

impl std::error::Error for UpstreamFailure {}

/// 腾讯元宝网页版模型。
pub struct YuanbaoWebModel {
    /// 展示名。
    display_name: String,
    /// 渠道执行器（发送请求 + 读取凭证）。
    executor: std::sync::Arc<dyn ChannelExecutor>,
    /// 调用方会话标识 → 网页会话 id：同一调用方复用同一网页会话，避免每轮新建。
    sessions: Mutex<HashMap<String, String>>,
    /// 上一次提问的时刻：用于限制提问频率。
    last_chat: tokio::sync::Mutex<Option<tokio::time::Instant>>,
}

impl YuanbaoWebModel {
    /// 用展示名与执行器构造模型。
    pub fn new(display_name: impl Into<String>, executor: std::sync::Arc<dyn ChannelExecutor>) -> Self {
        Self {
            display_name: display_name.into(),
            executor,
            sessions: Mutex::new(HashMap::new()),
            last_chat: tokio::sync::Mutex::new(None),
        }
    }

    /// 限制提问频率：距上次提问不足 [`MIN_CHAT_INTERVAL`] 时先等待。
    async fn throttle(&self) {
        let mut last = self.last_chat.lock().await;
        if let Some(previous) = *last {
            let elapsed = previous.elapsed();
            if elapsed < MIN_CHAT_INTERVAL {
                tokio::time::sleep(MIN_CHAT_INTERVAL - elapsed).await;
            }
        }
        *last = Some(tokio::time::Instant::now());
    }

    /// 把推理请求拼成渠道所需的 prompt（规则见 [`super::prompt`]）。
    pub(crate) fn prompt_of(request: &InferRequest) -> String {
        super::prompt::build_prompt(request)
    }

    /// 请求头：业务头由模型填，签名与指纹头由页面上下文补齐。
    fn default_headers(&self) -> Vec<(String, String)> {
        let mut headers: Vec<(String, String)> = Vec::new();
        // 插件捕获过的真实请求头里带页面生成的签名；页面缓存缺失时仍能补齐。
        if let Some(template) = self.executor.api_template() {
            if let Some(captured) = template.get("headers").and_then(Value::as_object) {
                for name in TEMPLATE_HEADERS {
                    if let Some(value) = captured.get(name).and_then(Value::as_str) {
                        headers.push((name.to_string(), value.to_string()));
                    }
                }
            }
        }
        headers.extend([
            ("content-type".to_string(), "text/plain;charset=UTF-8".to_string()),
            ("accept".to_string(), "*/*".to_string()),
            ("x-agentid".to_string(), AGENT_ID.to_string()),
            ("x-source".to_string(), "web".to_string()),
            ("x-requested-with".to_string(), "XMLHttpRequest".to_string()),
        ]);
        headers
    }

    /// 发送一次请求并解析响应体为 JSON。
    async fn send(
        &self,
        method: &str,
        url: &str,
        body: Option<String>,
    ) -> Result<Value> {
        let response = self
            .executor
            .execute(ChannelRequest {
                url: url.to_string(),
                method: method.to_string(),
                headers: self.default_headers(),
                body,
                body_base64: None,
                session_key: None,
            })
            .await?;
        if !response.ok {
            anyhow::bail!(
                "元宝网页请求失败（HTTP {}）：{}",
                response.status,
                response.body.chars().take(200).collect::<String>()
            );
        }
        Ok(serde_json::from_str(&response.body).unwrap_or(Value::Null))
    }

    /// 在渠道创建一个会话，返回会话 id。
    async fn create_session(&self) -> Result<String> {
        let body = json!({ "agentId": AGENT_ID }).to_string();
        let json = self.send("POST", SESSION_CREATE_URL, Some(body)).await?;
        find_session_id(&json).ok_or_else(|| {
            anyhow::anyhow!(
                "元宝创建会话的响应缺少会话 id：{}",
                json.to_string().chars().take(200).collect::<String>()
            )
        })
    }

    /// 取本次请求要用的网页会话：同一调用方复用，没有则新建。
    async fn resolve_session(&self, session_key: Option<&str>) -> Result<String> {
        let Some(key) = session_key.map(str::trim).filter(|key| !key.is_empty()) else {
            return self.create_session().await;
        };
        if let Some(id) = self
            .sessions
            .lock()
            .map_err(|_| anyhow::anyhow!("会话缓存锁不可用"))?
            .get(key)
            .cloned()
        {
            return Ok(id);
        }
        let created = self.create_session().await?;
        self.sessions
            .lock()
            .map_err(|_| anyhow::anyhow!("会话缓存锁不可用"))?
            .insert(key.to_string(), created.clone());
        Ok(created)
    }

    /// 丢弃缓存里的网页会话，下次请求会新建。
    fn forget_session(&self, session_key: Option<&str>) {
        let Some(key) = session_key.map(str::trim).filter(|key| !key.is_empty()) else {
            return;
        };
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.remove(key);
        }
    }

    /// 聊天完成请求体：`prompt` 是本轮拼好的整段对话。
    fn completion_body(conversation_id: &str, prompt: &str) -> String {
        json!({
            "model": MODEL,
            "prompt": prompt,
            "plugin": "Adaptive",
            "displayPrompt": prompt,
            "displayPromptType": 1,
            "agentId": AGENT_ID,
            "isTemporary": false,
            "projectId": "",
            "chatModelId": CHAT_MODEL_ID,
            "supportFunctions": SUPPORT_FUNCTIONS,
            "docOpenid": "",
            "options": {
                "imageIntention": {
                    "needIntentionModel": true,
                    "backendUpdateFlag": 2,
                    "intentionStatus": true,
                }
            },
            "multimedia": [],
            "supportHint": 1,
            "chatModelExtInfo": chat_model_ext_info(),
            "applicationIdList": [],
            "version": "v2",
            "extReportParams": Value::Null,
            "isAtomInput": false,
            "conversationId": conversation_id,
            "offsetOfHour": OFFSET_OF_HOUR,
            "offsetOfMinute": OFFSET_OF_MINUTE,
        })
        .to_string()
    }

    /// 推理共用实现：`on_chunk` 为 Some 时把正文增量实时送出。
    ///
    /// 网页会话不可用时（创建失败、请求失败、回答为空）丢弃缓存并重试一次：
    /// 会话可能已在网页端被删除，沿用只会一直失败。
    async fn chat(&self, request: InferRequest, on_chunk: Option<ChunkSink>) -> Result<ModelResponse> {
        let prompt = Self::prompt_of(&request);
        let session_key = request.session_key.clone();
        // 增量出口只能取一次：首轮走流式，重试时退回整块收取，避免同一批内容向客户端重复推送。
        let mut pending_sink = on_chunk;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let conversation_id = self.resolve_session(session_key.as_deref()).await?;
            let url = format!("{CHAT_URL_PREFIX}{conversation_id}");
            info!(
                "元宝网页推理 conversation={} prompt_len={} attempt={}",
                conversation_id,
                prompt.len(),
                attempt
            );
            let body = Self::completion_body(&conversation_id, &prompt);
            self.throttle().await;
            let result = match pending_sink.take() {
                Some(sink) => self.stream_once(url, body, sink).await,
                None => self.complete_once(url, body).await,
            };
            match result {
                Ok(response) if !response.text.trim().is_empty() => return Ok(response),
                Ok(response) => {
                    warn!("元宝网页回答为空（第 {} 次），丢弃缓存会话后重试", attempt);
                    self.forget_session(session_key.as_deref());
                    if attempt >= 2 {
                        return Ok(response);
                    }
                }
                Err(error) => {
                    warn!("元宝网页请求失败（第 {} 次）：{}", attempt, error);
                    self.forget_session(session_key.as_deref());
                    if attempt >= 2 {
                        return Err(error);
                    }
                    // 上游明确报错（如「服务繁忙」）时先退避，立即重试只会继续撞限流。
                    if error.downcast_ref::<UpstreamFailure>().is_some() {
                        tokio::time::sleep(MIN_CHAT_INTERVAL * 2).await;
                    }
                }
            }
        }
    }

    /// 非流式：整块收取响应，按帧解析出正文与用量。
    async fn complete_once(&self, url: String, body: String) -> Result<ModelResponse> {
        let response = self
            .executor
            .execute(ChannelRequest {
                url,
                method: "POST".to_string(),
                headers: self.default_headers(),
                body: Some(body),
                body_base64: None,
                session_key: None,
            })
            .await?;
        if !response.ok {
            anyhow::bail!("元宝网页推理失败（HTTP {}）", response.status);
        }
        let mut processor = YuanbaoFrameProcessor::new();
        processor.feed(&response.body);
        processor.finish();
        if let Some(message) = processor.upstream_error() {
            return Err(anyhow::Error::new(UpstreamFailure(message.to_string())));
        }
        let parsed = to_model_response(&processor);
        if parsed.text.trim().is_empty() {
            // 空回答时留下上游原文：否则「请求走完了但客户端什么都没有」无从排查
            warn!("元宝网页回答为空，原始响应：{}", head_chars(&response.body));
        }
        Ok(parsed)
    }

    /// 流式：原始增量交给帧处理器，正文增量经 `sink` 实时送出。
    async fn stream_once(&self, url: String, body: String, sink: ChunkSink) -> Result<ModelResponse> {
        let shared = std::sync::Arc::new(Mutex::new((YuanbaoFrameProcessor::new(), sink)));
        let feeder = shared.clone();
        // 流式响应体不随结果回传，留一段原文给空回答排查。
        let raw_head = std::sync::Arc::new(Mutex::new(String::new()));
        let header = raw_head.clone();
        let response = self
            .executor
            .execute_stream(
                ChannelRequest {
                    url,
                    method: "POST".to_string(),
                    headers: self.default_headers(),
                    body: Some(body),
                    body_base64: None,
                    session_key: None,
                },
                Box::new(move |raw: String| {
                    if let Ok(mut head) = header.lock() {
                        if head.chars().count() < RAW_HEAD_CHARS {
                            head.push_str(&raw);
                        }
                    }
                    let Ok(mut guard) = feeder.lock() else {
                        return;
                    };
                    let (processor, sink) = &mut *guard;
                    let delta = processor.feed(&raw);
                    if !delta.is_empty() {
                        emit_delta(sink, DeltaChunk::RESPONSE, json!(delta));
                    }
                }),
            )
            .await?;
        if !response.ok {
            anyhow::bail!("元宝网页推理失败（HTTP {}）", response.status);
        }
        let Ok(mut guard) = shared.lock() else {
            anyhow::bail!("流式处理器锁不可用");
        };
        let (processor, sink) = &mut *guard;
        let tail = processor.finish();
        if !tail.is_empty() {
            emit_delta(sink, DeltaChunk::RESPONSE, json!(tail));
        }
        if let Some(message) = processor.upstream_error() {
            return Err(anyhow::Error::new(UpstreamFailure(message.to_string())));
        }
        let parsed = to_model_response(processor);
        if parsed.text.trim().is_empty() {
            warn!("元宝网页回答为空，原始响应：{}", head_chars(&*raw_head.lock().unwrap()));
        }
        Ok(parsed)
    }
}

impl Model for YuanbaoWebModel {
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
        Concurrency::single()
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

impl PluginModel for YuanbaoWebModel {
    fn channel(&self) -> &str {
        CHANNEL
    }

    fn create_channel_session(&self) -> BoxFuture<'_, Result<Option<String>>> {
        Box::pin(async move { self.create_session().await.map(Some) })
    }

    fn list_channel_sessions(&self) -> BoxFuture<'_, Result<Value>> {
        Box::pin(async move {
            let json = self.send("GET", SESSION_LIST_URL, None).await?;
            Ok(extract_sessions(&json))
        })
    }

    fn delete_channel_session<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let body = json!({ "conversationId": session_id }).to_string();
            let json = self.send("POST", SESSION_CLEAR_URL, Some(body)).await?;
            Ok(is_biz_ok(&json))
        })
    }

    fn rename_channel_session<'a>(
        &'a self,
        _session_id: &'a str,
        _title: &'a str,
    ) -> BoxFuture<'a, Result<bool>> {
        // 网页端没有独立的改名接口；标题由首轮提问自动生成。
        Box::pin(async { Ok(false) })
    }

    fn channel_history<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let url = format!("{SESSION_DETAIL_URL}?conversationId={session_id}");
            self.send("GET", &url, None).await
        })
    }
}

/// 从创建会话的响应里找会话 id。
///
/// 网页端实际返回 `{"id":"0RJo0qtNKQi"}`；其余候选是不同版本/包装层的兜底。
fn find_session_id(json: &Value) -> Option<String> {
    let data = json.get("data");
    let candidates = [
        json.get("id"),
        data.and_then(|data| data.get("biz_data"))
            .and_then(|biz| biz.get("conversation_id")),
        data.and_then(|data| data.get("biz_data"))
            .and_then(|biz| biz.get("conversationId")),
        data.and_then(|data| data.get("biz_data"))
            .and_then(|biz| biz.get("id")),
        data.and_then(|data| data.get("conversation_id")),
        data.and_then(|data| data.get("conversationId")),
        data.and_then(|data| data.get("id")),
        json.get("conversation_id"),
        json.get("conversationId"),
    ];
    candidates
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find(|id| id.len() > MIN_SESSION_ID_LEN)
        .map(str::to_string)
}

/// 会话列表响应 → 会话数组。
///
/// 网页端实际返回 `data.list`；其余候选是不同版本/包装层的兜底。
fn extract_sessions(json: &Value) -> Value {
    let data = json.get("data");
    let list = data
        .and_then(|data| data.get("list"))
        .or_else(|| json.get("list"))
        .or_else(|| {
            data.and_then(|data| data.get("biz_data"))
                .and_then(|biz| biz.get("conversations"))
        })
        .or_else(|| {
            data.and_then(|data| data.get("biz_data"))
                .and_then(|biz| biz.get("list"))
        })
        .or_else(|| data.and_then(|data| data.get("conversations")))
        .or_else(|| json.get("conversations"))
        .and_then(Value::as_array);
    Value::Array(list.cloned().unwrap_or_default())
}

/// 网页端的业务成功标记：没有 `biz_code` 或它为 0。
fn is_biz_ok(json: &Value) -> bool {
    json.get("data")
        .and_then(|data| data.get("biz_code"))
        .and_then(Value::as_i64)
        .is_none_or(|code| code == 0)
}

/// 取正文开头一小段（按字符截断，避免切坏 UTF-8）。
fn head_chars(text: &str) -> String {
    text.chars().take(RAW_HEAD_CHARS).collect()
}

/// 帧处理器 → 模型响应。
fn to_model_response(processor: &YuanbaoFrameProcessor) -> ModelResponse {
    let text = processor.text().to_string();
    let usage = processor.token_usage().map(|usage| {
        json!({
            "prompt_tokens": usage.prompt_tokens,
            "completion_tokens": usage.completion_tokens,
            "total_tokens": usage.total_tokens,
        })
    });
    let finish_reason = match processor.stop_reason() {
        Some(reason) => reason.to_string(),
        None if processor.is_finished() => "stop".to_string(),
        None => "stop".to_string(),
    };
    if let Some(message_id) = processor.message_id() {
        debug!("元宝网页回答 message_id={}", message_id);
    }
    ModelResponse {
        text,
        reasoning: None,
        tool_calls: None,
        usage,
        finish_reason: Some(finish_reason),
        is_complete: true,
    }
}

/// 出站一条单键增量（与 [`DeltaChunk`] 的增量同格式：`{"键":值}`）。
fn emit_delta(sink: &mut ChunkSink, key: &str, value: Value) {
    let mut frame = serde_json::Map::with_capacity(1);
    frame.insert(key.to_string(), value);
    sink(Value::Object(frame).to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::defs::ChannelResponse;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    /// 记录请求并按顺序返回预设响应。
    struct MockExecutor {
        requests: Mutex<Vec<ChannelRequest>>,
        responses: Mutex<Vec<String>>,
    }

    impl MockExecutor {
        fn new(responses: Vec<String>) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(responses),
            }
        }

        fn take_response(&self) -> String {
            let mut responses = self.responses.lock().unwrap();
            if responses.len() > 1 {
                responses.remove(0)
            } else {
                responses.first().cloned().unwrap_or_default()
            }
        }
    }

    impl ChannelExecutor for MockExecutor {
        fn token(&self) -> Option<String> {
            Some("token".to_string())
        }

        fn settings_token(&self) -> Option<String> {
            None
        }

        fn execute(&self, request: ChannelRequest) -> BoxFuture<'_, Result<ChannelResponse>> {
            self.requests.lock().unwrap().push(request);
            let body = self.take_response();
            Box::pin(async move {
                Ok(ChannelResponse { ok: true, status: 200, body })
            })
        }

        fn execute_stream(
            &self,
            request: ChannelRequest,
            mut on_chunk: ChunkSink,
        ) -> BoxFuture<'_, Result<ChannelResponse>> {
            self.requests.lock().unwrap().push(request);
            let body = self.take_response();
            Box::pin(async move {
                // 按行分块送达：真实流式里每个 `data:` 行是独立的 chunk。
                for line in body.split_inclusive('\n') {
                    on_chunk(line.to_string());
                }
                Ok(ChannelResponse { ok: true, status: 200, body: String::new() })
            })
        }
    }

    /// 会话 id 常量。
    const CONV: &str = "conv-0000000001";

    /// 创建会话的响应体（网页端实际形态）。
    fn create_body(id: &str) -> String {
        json!({ "id": id }).to_string()
    }

    /// 一轮对话的响应体：开始标记、两条正文帧、收尾帧与结束标记。
    fn completion_stream() -> String {
        let frames = [
            json!({ "type": "text" }),
            json!({ "type": "text", "msg": "你" }),
            json!({ "type": "text", "msg": "好" }),
            json!({
                "type": "meta",
                "messageId": "m1",
                "stopReason": "stop",
                "tokenUsageInfo": { "promptTokens": 5, "completionTokens": 2, "totalTokens": 7 }
            }),
        ];
        let mut body = String::new();
        for frame in frames {
            body.push_str("data: ");
            body.push_str(&frame.to_string());
            body.push_str("\n\n");
        }
        body.push_str("data: [DONE]\n\n");
        body
    }

    /// 上游报错帧（如「服务繁忙」）的响应体。
    fn error_stream() -> String {
        concat!(
            "event: error\n",
            "data: {\"type\":\"error\",\"msg\":\"服务繁忙，请稍后再试。\",\"error\":{}}\n\n",
        )
        .to_string()
    }

    fn model(executor: Arc<MockExecutor>) -> YuanbaoWebModel {
        YuanbaoWebModel::new("腾讯元宝", executor as Arc<dyn ChannelExecutor>)
    }

    fn request(session_key: Option<&str>) -> InferRequest {
        let mut request = InferRequest::new(
            CHANNEL,
            vec![json!({ "role": "user", "content": "你好" })],
        );
        request.session_key = session_key.map(str::to_string);
        request
    }

    /// 非流式：先建会话，再按会话 id 发对话请求，正文与用量都取到。
    #[tokio::test]
    async fn infers_once_creating_a_session() {
        let executor = Arc::new(MockExecutor::new(vec![create_body(CONV), completion_stream()]));
        let response = model(executor.clone())
            .infer(request(Some("caller")))
            .await
            .expect("推理应成功");
        assert_eq!(response.text, "你好");
        assert_eq!(
            response.usage,
            Some(json!({ "prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7 }))
        );
        let requests = executor.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].url, SESSION_CREATE_URL);
        assert_eq!(requests[1].url, format!("{CHAT_URL_PREFIX}{CONV}"));
        let body: Value = serde_json::from_str(requests[1].body.as_deref().unwrap()).unwrap();
        assert_eq!(body["conversationId"], CONV);
        assert!(body["prompt"].as_str().unwrap().contains("你好"));
    }

    /// 同一调用方复用已创建的会话，不再重复建。
    #[tokio::test]
    async fn reuses_session_for_same_caller() {
        let executor = Arc::new(MockExecutor::new(vec![create_body(CONV), completion_stream()]));
        let model = model(executor.clone());
        model.infer(request(Some("caller"))).await.unwrap();
        model.infer(request(Some("caller"))).await.unwrap();
        let requests = executor.requests.lock().unwrap();
        let creates = requests.iter().filter(|r| r.url == SESSION_CREATE_URL).count();
        assert_eq!(creates, 1, "同一调用方只应建一次会话");
    }

    /// 流式：正文增量逐条送出，收尾响应仍带完整正文与用量。
    #[tokio::test]
    async fn streams_text_deltas() {
        let executor = Arc::new(MockExecutor::new(vec![create_body(CONV), completion_stream()]));
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink_seen = seen.clone();
        let sink: ChunkSink = Box::new(move |delta: String| {
            sink_seen.lock().unwrap().push(delta);
        });
        let response = model(executor)
            .infer_stream(request(None), sink)
            .await
            .expect("流式推理应成功");
        assert_eq!(response.text, "你好");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "两条正文帧应各送一条增量：{seen:?}");
        assert!(seen[0].contains("你"));
        assert!(seen[1].contains("好"));
    }

    /// 空回答会丢弃缓存会话并重试一次。
    #[tokio::test]
    async fn retries_after_empty_answer() {
        let executor = Arc::new(MockExecutor::new(vec![
            create_body(CONV),
            "data: [DONE]\n\n".to_string(),
            create_body("conv-0000000002"),
            completion_stream(),
        ]));
        let response = model(executor.clone())
            .infer(request(Some("caller")))
            .await
            .expect("重试后应成功");
        assert_eq!(response.text, "你好");
        let requests = executor.requests.lock().unwrap();
        let creates = requests.iter().filter(|r| r.url == SESSION_CREATE_URL).count();
        assert_eq!(creates, 2, "空回答后应重新建会话");
    }

    /// 创建会话的响应缺少 id 时报错。
    #[tokio::test]
    async fn create_session_requires_id() {
        let executor = Arc::new(MockExecutor::new(vec![json!({ "data": {} }).to_string()]));
        let error = model(executor)
            .create_channel_session()
            .await
            .expect_err("缺少会话 id 应报错");
        assert!(error.to_string().contains("缺少会话 id"));
    }

    /// 会话列表按网页端实际形态（`data.list`）取出会话数组。
    #[tokio::test]
    async fn lists_sessions_from_web_list() {
        let executor = Arc::new(MockExecutor::new(vec![json!({
            "code": 0,
            "message": "success",
            "data": { "total_count": 1, "list": [{ "id": "c1", "title": "你好" }] }
        })
        .to_string()]));
        let list = model(executor).list_channel_sessions().await.unwrap();
        assert_eq!(list, json!([{ "id": "c1", "title": "你好" }]));
    }

    /// 兼容包装层：会话数组也可能放在 `data.biz_data.conversations`。
    #[tokio::test]
    async fn lists_sessions_from_business_field() {
        let executor = Arc::new(MockExecutor::new(vec![json!({
            "data": { "biz_code": 0, "biz_data": { "conversations": [{ "id": "c1" }] } }
        })
        .to_string()]));
        let list = model(executor).list_channel_sessions().await.unwrap();
        assert_eq!(list, json!([{ "id": "c1" }]));
    }

    /// 上游报错要原样报给调用方，而不是变成空回答。
    #[tokio::test]
    async fn upstream_error_is_reported() {
        let executor = Arc::new(MockExecutor::new(vec![
            create_body(CONV),
            error_stream(),
            create_body("conv-0000000002"),
            error_stream(),
        ]));
        let error = model(executor)
            .infer(request(None))
            .await
            .expect_err("上游报错应返回错误");
        let message = error.to_string();
        assert!(message.contains("元宝上游报错"), "{message}");
        assert!(message.contains("服务繁忙"), "{message}");
    }

    /// 删除会话成功时返回真。
    #[tokio::test]
    async fn deletes_session() {
        let executor = Arc::new(MockExecutor::new(vec![
            json!({ "data": { "biz_code": 0 } }).to_string()
        ]));
        let ok = model(executor).delete_channel_session(CONV).await.unwrap();
        assert!(ok);
    }

    /// 历史拼装与公共实现逐字一致。
    #[test]
    fn prompt_matches_shared_builder() {
        let request = InferRequest::new(
            CHANNEL,
            vec![
                json!({ "role": "system", "content": "系统提示" }),
                json!({ "role": "user", "content": "第一轮问题" }),
                json!({ "role": "assistant", "content": "第一轮回答" }),
                json!({ "role": "user", "content": "第二轮问题" }),
            ],
        );
        assert_eq!(
            YuanbaoWebModel::prompt_of(&request),
            super::super::prompt::build_prompt(&request)
        );
    }

    /// 历史在前、最新一条记录在末尾。
    #[test]
    fn prompt_puts_latest_turn_last() {
        let request = InferRequest::new(
            CHANNEL,
            vec![
                json!({ "role": "user", "content": "旧问题" }),
                json!({ "role": "user", "content": "新问题" }),
            ],
        );
        let prompt = YuanbaoWebModel::prompt_of(&request);
        let old_at = prompt.find("旧问题").expect("应包含历史问题");
        let new_at = prompt.find("新问题").expect("应包含最新问题");
        assert!(old_at < new_at, "历史应排在最新一条之前");
    }
}
