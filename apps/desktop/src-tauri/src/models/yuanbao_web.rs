//! 腾讯元宝网页版模型：实现插件模型抽象，一个模型对应 `yuanbao-web` 渠道。
//!
//! 会话管理（创建 / 列表 / 删除 / 历史）与聊天请求组装、流式帧解析都在模型内部完成；
//! 对外的请求发送与凭证读取经 [`ChannelExecutor`]（由传输层实现）。
//!
//! 历史管理：有本地会话绑定时与远端会话一一对应，首次分段 bootstrap，后续只发新增内容；
//! 没有绑定时沿用完整 prompt + 调用方会话 id 的兼容路径。
//!
//! 请求签名（`x-uskey`、`x-bus-params-md5` 等）由页面上下文生成，模型侧只填业务头。

pub mod frame_processor;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use super::deepseek_web::chat_stream_processor::{ChatStreamProcessor, ToolSchemas};
use super::defs::{
    ChannelExecutor, ChannelRequest, ChunkSink, Concurrency, InferRequest, ModelCapability,
    ModelResponse, WebContextStore,
};
use super::model::Model;
use super::plugin::PluginModel;
use super::prompt;
use super::web_context::text_hash;
use super::yuanbao_web::frame_processor::YuanbaoFrameProcessor;
use crate::apis::base::DeltaChunk;
use crate::throttle::{ChatFrequency, ChatThrottle, DEFAULT_CHAT_INTERVAL_MS};

/// bootstrap 单段最大字符数：元宝单条输入较保守。
const BOOTSTRAP_SEGMENT_CHARS: usize = 6_000;

/// 渠道标识。
pub const CHANNEL: &str = "yuanbao-web";

/// 专用的标题生成会话绑定 key。
const TITLE_CONTEXT_KEY: &str = "__poolman_title__";
/// 专用的摘要生成会话绑定 key。
const SUMMARY_CONTEXT_KEY: &str = "__poolman_summary__";
/// 专用会话的固定标题（元宝网页端不支持改名，仅保留名称供日志/未来使用）。
const TITLE_SESSION_TITLE: &str = "poolman-标题生成";
const SUMMARY_SESSION_TITLE: &str = "poolman-摘要生成";

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

/// 修改会话标题。
const SESSION_UPDATE_URL: &str = "https://yuanbao.tencent.com/api/user/agent/conversation/update";

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
    /// 本地会话 ↔ 远端会话绑定。
    web_context_store: Option<Arc<dyn WebContextStore>>,
    /// 同一本地会话的请求串行化：远端网页会话不能并发使用。
    bound_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// 对话请求排队：网页端限制提问频率，间隔由桌面端「对话请求频率」决定。
    throttle: ChatThrottle,
}

impl YuanbaoWebModel {
    /// 用展示名与执行器构造模型。
    pub fn new(display_name: impl Into<String>, executor: std::sync::Arc<dyn ChannelExecutor>) -> Self {
        Self {
            display_name: display_name.into(),
            executor,
            sessions: Mutex::new(HashMap::new()),
            web_context_store: None,
            bound_locks: Mutex::new(HashMap::new()),
            throttle: ChatThrottle::new(ChatFrequency::in_memory(DEFAULT_CHAT_INTERVAL_MS)),
        }
    }

    /// 改用共享的对话请求频率：各渠道共用一份设置，桌面端修改后立即生效。
    pub fn with_chat_frequency(mut self, frequency: std::sync::Arc<ChatFrequency>) -> Self {
        self.throttle = frequency.throttle();
        self
    }

    /// 注入本地会话绑定存储：有绑定时按远端会话增量发送。
    pub fn with_web_context_store(mut self, store: Arc<dyn WebContextStore>) -> Self {
        self.web_context_store = Some(store);
        self
    }

    /// 同一本地会话的串行锁。
    fn context_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.bound_locks.lock().unwrap();
        locks
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
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

    /// 会话管理类请求的头：`content-type` 是 JSON，`x-agentid` 指向具体会话。
    ///
    /// 网页端在会话级接口上把 `x-agentid` 写成 `{agentId}/{conversationId}`，
    /// 创建 / 列表这类不针对具体会话的接口才只带 `{agentId}`；聊天接口的
    /// `content-type` 是 `text/plain`，与这些 JSON 接口不同，因此不能直接复用
    /// [`Self::default_headers`]。
    fn session_headers(&self, session_id: &str) -> Vec<(String, String)> {
        let mut headers: Vec<(String, String)> = self
            .default_headers()
            .into_iter()
            .filter(|(name, _)| {
                let name = name.to_ascii_lowercase();
                name != "content-type" && name != "x-agentid"
            })
            .collect();
        headers.push((
            "content-type".to_string(),
            "application/json".to_string(),
        ));
        headers.push((
            "x-agentid".to_string(),
            format!("{AGENT_ID}/{session_id}"),
        ));
        headers
    }

    /// 发送一次会话级请求并解析响应体为 JSON。
    async fn send_session(
        &self,
        method: &str,
        url: &str,
        session_id: &str,
        body: Option<String>,
    ) -> Result<Value> {
        let response = self
            .executor
            .execute(ChannelRequest {
                url: url.to_string(),
                method: method.to_string(),
                headers: self.session_headers(session_id),
                body,
                body_base64: None,
                session_key: Some(session_id.to_string()),
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
        if request.web_context_key.is_none()
            && crate::request_record::detect_title_request(
                &request.messages,
                request.tools.as_ref(),
            )
            && self.web_context_store.is_some()
        {
            return self.chat_title(request, on_chunk).await;
        }
        if let (Some(key), Some(store)) = (
            request.web_context_key.clone(),
            self.web_context_store.clone(),
        ) {
            return self
                .chat_bound(request, on_chunk, key, store)
                .await;
        }
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
            // 排队等放行：网页端对提问频率有限制，间隔从上一个对话**结束**时算起。
            // guard 持有到本次尝试结束，drop 时才记下完成时刻。
            let _guard = self.throttle.wait().await;
            let result = match pending_sink.take() {
                Some(sink) => self
                    .stream_once(url, body, sink, request.tools.as_ref())
                    .await,
                None => self.complete_once(url, body, request.tools.as_ref()).await,
            };
            match result {
                Ok(response) if !response.text.trim().is_empty() || response.tool_calls.is_some() => {
                    return Ok(response)
                }
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
                    // 至少退避一秒：频率设为「不限制」时也要给网页端一点喘息时间。
                    if error.downcast_ref::<UpstreamFailure>().is_some() {
                        let backoff = self
                            .throttle
                            .interval()
                            .max(std::time::Duration::from_secs(1));
                        tokio::time::sleep(backoff * 2).await;
                    }
                }
            }
        }
    }

    /// 专用标题/摘要会话：长期复用，失败或过长时换新会话重试一次。
    async fn run_auxiliary(
        &self,
        store: &Arc<dyn WebContextStore>,
        key: &str,
        _title: &str,
        prompt: &str,
    ) -> Result<String> {
        let lock = self.context_lock(key);
        let _guard = lock.lock().await;
        let mut attempts = 0;
        loop {
            attempts += 1;
            let mut binding = store.load(CHANNEL, key).await?.unwrap_or_default();
            binding.channel = CHANNEL.to_string();
            binding.context_key = key.to_string();
            if binding.remote_session_id.is_empty() {
                binding.remote_session_id = self.create_session().await?;
                binding.primed = true;
            }
            let url = format!("{CHAT_URL_PREFIX}{}", binding.remote_session_id);
            let body = Self::completion_body(&binding.remote_session_id, prompt);
            let _guard = self.throttle.wait().await;
            match self.complete_once(url, body, None).await {
                Ok(response)
                    if !looks_like_context_limit(&response.text)
                        && (!response.text.trim().is_empty() || response.tool_calls.is_some()) =>
                {
                    binding.approx_tokens += i64::from(crate::tokenizer::count(prompt))
                        + i64::from(crate::tokenizer::count(&response.text));
                    if let Err(error) = store.save(&binding).await {
                        warn!("保存元宝专用会话绑定失败 key={} err={}", key, error);
                    }
                    return Ok(response.text);
                }
                Ok(response) => {
                    warn!(
                        "元宝专用会话不可用 key={} text_len={}",
                        key,
                        response.text.len()
                    );
                    if !binding.remote_session_id.is_empty() {
                        let _ = self
                            .delete_channel_session(&binding.remote_session_id)
                            .await;
                    }
                    let _ = store.delete(CHANNEL, key).await;
                    if attempts >= 2 {
                        anyhow::bail!("专用会话未返回有效内容");
                    }
                }
                Err(error) => {
                    warn!("元宝专用会话失败 key={} err={}", key, error);
                    if !binding.remote_session_id.is_empty() {
                        let _ = self
                            .delete_channel_session(&binding.remote_session_id)
                            .await;
                    }
                    let _ = store.delete(CHANNEL, key).await;
                    if attempts >= 2 {
                        return Err(error);
                    }
                }
            }
        }
    }

    /// 标题请求走专用标题会话，不再使用普通会话绑定。
    async fn chat_title(
        &self,
        request: InferRequest,
        on_chunk: Option<ChunkSink>,
    ) -> Result<ModelResponse> {
        let Some(store) = self.web_context_store.clone() else {
            anyhow::bail!("标题会话缺少绑定存储");
        };
        let lock = self.context_lock(TITLE_CONTEXT_KEY);
        let _guard = lock.lock().await;
        let prompt = Self::prompt_of(&request);
        let mut pending_sink = on_chunk;
        let mut attempts = 0;
        loop {
            attempts += 1;
            let mut binding = store
                .load(CHANNEL, TITLE_CONTEXT_KEY)
                .await?
                .unwrap_or_default();
            binding.channel = CHANNEL.to_string();
            binding.context_key = TITLE_CONTEXT_KEY.to_string();
            if binding.remote_session_id.is_empty() {
                binding.remote_session_id = self.create_session().await?;
                binding.primed = true;
            }
            let url = format!("{CHAT_URL_PREFIX}{}", binding.remote_session_id);
            let body = Self::completion_body(&binding.remote_session_id, &prompt);
            let _guard = self.throttle.wait().await;
            let result = match pending_sink.take() {
                Some(sink) => self.stream_once(url, body, sink, None).await,
                None => self.complete_once(url, body, None).await,
            };
            match result {
                Ok(response)
                    if !looks_like_context_limit(&response.text)
                        && (!response.text.trim().is_empty() || response.tool_calls.is_some()) =>
                {
                    binding.approx_tokens +=
                        i64::from(crate::tokenizer::count(&prompt))
                            + i64::from(crate::tokenizer::count(&response.text));
                    if let Err(error) = store.save(&binding).await {
                        warn!("保存元宝标题会话绑定失败 err={}", error);
                    }
                    let _ = TITLE_SESSION_TITLE;
                    return Ok(response);
                }
                Ok(response) => {
                    warn!(
                        "元宝标题会话不可用 text_len={}",
                        response.text.len()
                    );
                    if !binding.remote_session_id.is_empty() {
                        let _ = self
                            .delete_channel_session(&binding.remote_session_id)
                            .await;
                    }
                    let _ = store.delete(CHANNEL, TITLE_CONTEXT_KEY).await;
                    if attempts >= 2 {
                        anyhow::bail!("标题会话未返回有效内容");
                    }
                }
                Err(error) => {
                    if !binding.remote_session_id.is_empty() {
                        let _ = self
                            .delete_channel_session(&binding.remote_session_id)
                            .await;
                    }
                    let _ = store.delete(CHANNEL, TITLE_CONTEXT_KEY).await;
                    if attempts >= 2 {
                        return Err(error);
                    }
                }
            }
        }
    }

    /// 上下文迁移前，用专用摘要会话压缩旧对话。
    async fn summarize_for_migration(
        &self,
        request: &InferRequest,
        store: &Arc<dyn WebContextStore>,
        existing_summary: &str,
    ) -> Result<String> {
        let prompt = prompt::build_summary_prompt(request, existing_summary, 60);
        self.run_auxiliary(
            store,
            SUMMARY_CONTEXT_KEY,
            SUMMARY_SESSION_TITLE,
            &prompt,
        )
        .await
    }

    /// 本地会话绑定远端会话：首次分段 bootstrap，后续只发新增内容。
    async fn chat_bound(
        &self,
        request: InferRequest,
        on_chunk: Option<ChunkSink>,
        key: String,
        store: Arc<dyn WebContextStore>,
    ) -> Result<ModelResponse> {
        let context_lock = self.context_lock(&key);
        let _context_guard = context_lock.lock().await;
        let system_text = request
            .messages
            .iter()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("system"))
            .filter_map(|message| message.get("content").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n");
        let tools_text = request
            .tools
            .as_ref()
            .filter(|value| !value.is_null())
            .map(Value::to_string)
            .unwrap_or_default();
        let system_hash = text_hash(&system_text);
        let tools_hash = text_hash(&tools_text);

        let mut binding = store
            .load(CHANNEL, &key)
            .await?
            .unwrap_or_default();
        binding.channel = CHANNEL.to_string();
        binding.context_key = key.clone();
        // 静态上下文变了：旧远端会话不能再增量续用，重新初始化。
        if !binding.remote_session_id.is_empty()
            && binding.primed
            && (binding.system_hash != system_hash || binding.tools_hash != tools_hash)
        {
            info!("元宝会话静态上下文变化，重建远端会话 key={}", key);
            let _ = store.delete(CHANNEL, &key).await;
            binding.remote_session_id.clear();
            binding.primed = false;
        }
        binding.system_hash = system_hash;
        binding.tools_hash = tools_hash;

        let mut pending_sink = on_chunk;
        let mut attempts = 0;
        loop {
            attempts += 1;
            let mut failed: Option<anyhow::Error> = None;
            let conversation_id = if binding.remote_session_id.is_empty() {
                let created = self.create_session().await?;
                info!("元宝会话绑定创建远端会话 key={} remote={}", key, created);
                binding.remote_session_id = created.clone();
                binding.primed = false;
                created
            } else {
                binding.remote_session_id.clone()
            };
            let url = format!("{CHAT_URL_PREFIX}{conversation_id}");

            // 初始化段：只灌上下文，不要求回答（本轮提问由下面的提问轮单独发出）。
            let mut bootstrapped = false;
            if !binding.primed {
                let segments = if binding.summary.trim().is_empty() {
                    prompt::build_context_segments(&request, BOOTSTRAP_SEGMENT_CHARS)
                } else {
                    prompt::build_context_segments_with_summary(
                        &request,
                        &binding.summary,
                        BOOTSTRAP_SEGMENT_CHARS,
                    )
                };
                info!(
                    "元宝会话 bootstrap key={} remote={} segments={}",
                    key,
                    conversation_id,
                    segments.len()
                );
                for segment in segments.iter() {
                    let body = Self::completion_body(&conversation_id, segment);
                    let _guard = self.throttle.wait().await;
                    // 初始化段不带工具：这些回答只是 OK，不解析 DSML。
                    match self.complete_once(url.clone(), body, None).await {
                        Ok(_) => {
                            // 初始化段的回答（OK）只用于推进远端消息链，内容丢弃。
                            binding.approx_tokens += tokens_of(segment);
                        }
                        Err(error) => {
                            failed = Some(error);
                            break;
                        }
                    }
                }
                if failed.is_none() {
                    binding.primed = true;
                    if let Err(error) = store.save(&binding).await {
                        warn!("保存元宝会话绑定失败 key={} err={}", key, error);
                    }
                    bootstrapped = true;
                }
            }

            // 提问轮：刚初始化完是首轮提问，之后是增量；两轮都带工具调用强调。
            if failed.is_none() {
                let text = if bootstrapped {
                    prompt::build_question_prompt(&request)
                } else {
                    prompt::build_delta_prompt(&request)
                };
                info!(
                    "元宝会话提问 key={} remote={} prompt_len={}",
                    key,
                    conversation_id,
                    text.len()
                );
                let body = Self::completion_body(&conversation_id, &text);
                let _guard = self.throttle.wait().await;
                let result = match pending_sink.take() {
                    Some(sink) => self
                        .stream_once(url.clone(), body, sink, request.tools.as_ref())
                        .await,
                    None => self
                        .complete_once(url.clone(), body, request.tools.as_ref())
                        .await,
                };
                match result {
                    Ok(response)
                        if !response.text.trim().is_empty() || response.tool_calls.is_some() =>
                    {
                        binding.approx_tokens += tokens_of(&text);
                        if let Err(error) = store.save(&binding).await {
                            warn!("保存元宝会话绑定失败 key={} err={}", key, error);
                        }
                        return Ok(response);
                    }
                    Ok(response) => {
                        failed = Some(anyhow::anyhow!("元宝网页回答为空"));
                        let _ = response;
                    }
                    Err(error) => failed = Some(error),
                }
            }

            if let Some(error) = failed {
                warn!(
                    "元宝绑定会话第 {} 次失败 key={} remote={} err={}",
                    attempts, key, conversation_id, error
                );
                if attempts < 2 && looks_like_context_limit(&format!("{error:#}")) {
                    let summary = match self
                        .summarize_for_migration(&request, &store, &binding.summary)
                        .await
                    {
                        Ok(summary) if !summary.trim().is_empty() => summary,
                        _ if !binding.summary.trim().is_empty() => binding.summary.clone(),
                        _ => "（摘要生成失败，旧历史未保留）".to_string(),
                    };
                    binding.summary = summary;
                }
                if !binding.remote_session_id.is_empty() {
                    let _ = self
                        .delete_channel_session(&binding.remote_session_id)
                        .await;
                }
                let _ = store.delete(CHANNEL, &key).await;
                binding.remote_session_id.clear();
                binding.primed = false;
                binding.approx_tokens = 0;
                if attempts >= 2 {
                    return Err(error);
                }
            }
        }
    }

    /// 非流式：整块收取响应，按帧解析出正文、用量与工具调用。
    ///
    /// 帧处理器只负责把 `data:` 行还原成正文；正文再交给 [`ChatStreamProcessor`]
    /// 识别 DSML 调用——网页端没有原生工具调用，调用只能靠解析标记得到。
    async fn complete_once(
        &self,
        url: String,
        body: String,
        tools: Option<&Value>,
    ) -> Result<ModelResponse> {
        let recognize_tools = tools_recognized(tools);
        let schemas = ToolSchemas::from_tools(tools);
        let response = self
            .executor
            .execute(ChannelRequest {
                url: url.clone(),
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
        let mut frames = YuanbaoFrameProcessor::new();
        frames.feed(&response.body);
        frames.finish();
        if let Some(message) = frames.upstream_error() {
            return Err(anyhow::Error::new(UpstreamFailure(message.to_string())));
        }

        let mut chat = ChatStreamProcessor::new(Box::new(|_| {}), recognize_tools)
            .with_tool_schemas(schemas.clone());
        chat.feed([(
            DeltaChunk::RESPONSE.to_string(),
            frames.text().to_string(),
        )]);
        chat.finish();

        let mut parsed = to_model_response(&frames, &chat);
        if let Some((broken, reason)) = failure_of(&chat) {
            if let Some(calls) = self
                .repair_calls(&url, tools, &broken, &reason, &schemas)
                .await
            {
                parsed.tool_calls = Some(Value::Array(calls));
                parsed.finish_reason = Some("tool_calls".to_string());
            }
        }
        if parsed.text.trim().is_empty() && parsed.tool_calls.is_none() {
            // 空回答时留下上游原文：否则「请求走完了但客户端什么都没有」无从排查
            warn!("元宝网页回答为空，原始响应：{}", head_chars(&response.body));
        }
        Ok(parsed)
    }

    /// 流式：原始增量交给帧处理器，正文增量经 [`ChatStreamProcessor`] 过滤后实时送出。
    ///
    /// DSML 标记不会流到客户端：处理器把正文与调用分开，调用在收尾时一次性送出。
    async fn stream_once(
        &self,
        url: String,
        body: String,
        sink: ChunkSink,
        tools: Option<&Value>,
    ) -> Result<ModelResponse> {
        let recognize_tools = tools_recognized(tools);
        let schemas = ToolSchemas::from_tools(tools);
        // 出口只能移动一次：先放进共享单元，修复轮需要时再取一个代理。
        let sink_cell = std::sync::Arc::new(Mutex::new(Some(sink)));
        let shared = std::sync::Arc::new(Mutex::new((
            YuanbaoFrameProcessor::new(),
            ChatStreamProcessor::new(
                forwarding_sink(&sink_cell).expect("出口刚放入，必然可取"),
                recognize_tools,
            )
            .with_tool_schemas(schemas.clone()),
        )));
        let feeder = shared.clone();
        // 流式响应体不随结果回传，留一段原文给空回答排查。
        let raw_head = std::sync::Arc::new(Mutex::new(String::new()));
        let header = raw_head.clone();
        let response = self
            .executor
            .execute_stream(
                ChannelRequest {
                    url: url.clone(),
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
                    let (frames, chat) = &mut *guard;
                    let delta = frames.feed(&raw);
                    if !delta.is_empty() {
                        chat.feed([(DeltaChunk::RESPONSE.to_string(), delta)]);
                    }
                }),
            )
            .await?;
        if !response.ok {
            anyhow::bail!("元宝网页推理失败（HTTP {}）", response.status);
        }

        let (mut parsed, failure) = {
            let Ok(mut guard) = shared.lock() else {
                anyhow::bail!("流式处理器锁不可用");
            };
            let (frames, chat) = &mut *guard;
            let tail = frames.finish();
            if !tail.is_empty() {
                chat.feed([(DeltaChunk::RESPONSE.to_string(), tail)]);
            }
            chat.finish();
            if let Some(message) = frames.upstream_error() {
                return Err(anyhow::Error::new(UpstreamFailure(message.to_string())));
            }
            (to_model_response(frames, chat), failure_of(chat))
        };

        // 解析失败：用规范化提示词再问一轮（持锁会跨 await，所以放到锁外）。
        if let Some((broken, reason)) = failure {
            if let Some(calls) = self
                .repair_calls(&url, tools, &broken, &reason, &schemas)
                .await
            {
                if let Some(mut sink) = forwarding_sink(&sink_cell) {
                    emit_delta(&mut sink, DeltaChunk::FUNCTIONS, Value::Array(calls.clone()));
                }
                parsed.tool_calls = Some(Value::Array(calls));
                parsed.finish_reason = Some("tool_calls".to_string());
            }
        }

        if parsed.text.trim().is_empty() && parsed.tool_calls.is_none() {
            warn!(
                "元宝网页回答为空，原始响应：{}",
                head_chars(&*raw_head.lock().unwrap())
            );
        }
        Ok(parsed)
    }

    /// 修复轮：把解析失败的坏块交给规范化提示词重写，取回可解析的工具调用。
    ///
    /// 与正文分开走一次非流式请求：修复只需要一个调用块，不需要再流式回吐。
    async fn repair_calls(
        &self,
        url: &str,
        tools: Option<&Value>,
        broken: &str,
        reason: &str,
        schemas: &ToolSchemas,
    ) -> Option<Vec<Value>> {
        let conversation_id = url.rsplit('/').next().unwrap_or_default();
        if conversation_id.is_empty() {
            return None;
        }
        let repair_prompt = prompt::repair_prompt(tools, broken, reason);
        info!(
            "元宝工具调用解析失败，发起修复重写 reason={} prompt_len={}",
            reason,
            repair_prompt.len()
        );
        let body = Self::completion_body(conversation_id, &repair_prompt);
        let _guard = self.throttle.wait().await;
        let response = match self
            .executor
            .execute(ChannelRequest {
                url: url.to_string(),
                method: "POST".to_string(),
                headers: self.default_headers(),
                body: Some(body),
                body_base64: None,
                session_key: None,
            })
            .await
        {
            Ok(response) if response.ok => response,
            Ok(response) => {
                warn!("元宝工具调用修复请求失败（HTTP {}）", response.status);
                return None;
            }
            Err(error) => {
                warn!("元宝工具调用修复请求失败：{}", error);
                return None;
            }
        };

        let mut frames = YuanbaoFrameProcessor::new();
        frames.feed(&response.body);
        frames.finish();
        let mut chat =
            ChatStreamProcessor::new(Box::new(|_| {}), true).with_tool_schemas(schemas.clone());
        chat.feed([(
            DeltaChunk::RESPONSE.to_string(),
            frames.text().to_string(),
        )]);
        chat.finish();
        let calls = chat.tool_calls();
        if calls.is_empty() {
            warn!("元宝工具调用修复轮没有产出可解析的调用");
            None
        } else {
            Some(calls.to_vec())
        }
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
        session_id: &'a str,
        title: &'a str,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let title = title.trim();
            // 空标题在网页端等于「清空标题」，请求本身会被接受，因此在这里就挡掉。
            if title.is_empty() {
                warn!("设置元宝会话标题失败：标题为空 session={}", session_id);
                return Ok(false);
            }
            let body = json!({ "conversationId": session_id, "title": title }).to_string();
            let json = self
                .send_session("POST", SESSION_UPDATE_URL, session_id, Some(body))
                .await?;
            Ok(is_biz_ok(&json))
        })
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
fn tokens_of(text: &str) -> i64 {
    i64::from(crate::tokenizer::count(text))
}

fn looks_like_context_limit(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "上下文",
        "长度",
        "过长",
        "轮数",
        "context length",
        "context_length",
        "too long",
    ]
    .iter()
    .any(|keyword| lower.contains(keyword))
}

fn head_chars(text: &str) -> String {
    text.chars().take(RAW_HEAD_CHARS).collect()
}

/// 帧处理器 + 增量处理器 → 模型响应。
///
/// 正文与工具调用都取自 [`ChatStreamProcessor`]（它已把 DSML 标记从正文里剔掉），
/// 用量与结束原因来自帧处理器的收尾帧。
fn to_model_response(frames: &YuanbaoFrameProcessor, chat: &ChatStreamProcessor) -> ModelResponse {
    let usage = frames.token_usage().map(|usage| {
        json!({
            "prompt_tokens": usage.prompt_tokens,
            "completion_tokens": usage.completion_tokens,
            "total_tokens": usage.total_tokens,
        })
    });
    let calls = chat.tool_calls();
    let tool_calls = if calls.is_empty() {
        None
    } else {
        Some(Value::Array(calls.to_vec()))
    };
    let finish_reason = if tool_calls.is_some() {
        "tool_calls".to_string()
    } else {
        frames
            .stop_reason()
            .map(str::to_string)
            .unwrap_or_else(|| "stop".to_string())
    };
    if let Some(message_id) = frames.message_id() {
        debug!("元宝网页回答 message_id={}", message_id);
    }
    let reasoning = chat.reasoning();
    ModelResponse {
        text: chat.text().to_string(),
        reasoning: if reasoning.trim().is_empty() {
            None
        } else {
            Some(reasoning.to_string())
        },
        tool_calls,
        usage,
        finish_reason: Some(finish_reason),
        is_complete: true,
    }
}

/// 请求是否要求识别工具调用：只有确实带了工具定义才解析 DSML。
fn tools_recognized(tools: Option<&Value>) -> bool {
    tools
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty())
}

/// 解析失败时的坏块原文与原因；没有失败返回 None。
fn failure_of(chat: &ChatStreamProcessor) -> Option<(String, String)> {
    let broken = chat.parse_failure()?.to_string();
    let reason = chat
        .failure_reason()
        .unwrap_or("块内标签序列无法判定")
        .to_string();
    Some((broken, reason))
}

/// 取一个把增量转发到同一个出口的代理；出口已被取走时返回 None。
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
    use crate::models::web_context::DbWebContextStore;
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

    /// 一条真实的元宝工具调用标记（取自安装目录的请求记录）。
    fn dsml_markup() -> String {
        [
            "<｜｜DSML｜｜ calls>",
            "<｜｜DSML｜｜ invoke name=\"glob\">",
            "<｜｜DSML｜｜ parameter name=\"pattern\" string=\"true\">**/*</｜｜DSML｜｜ parameter>",
            "</｜｜DSML｜｜ invoke>",
            "</｜｜DSML｜｜ calls>",
        ]
        .join("\n")
    }

    /// 带 DSML 调用的流式响应体。
    fn dsml_stream() -> String {
        let frames = [
            json!({ "type": "text" }),
            json!({ "type": "text", "msg": dsml_markup() }),
            json!({ "type": "meta", "messageId": "m1", "stopReason": "stop" }),
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

    /// 带工具定义的请求：只有一个 `glob` 工具。
    fn request_with_tools() -> InferRequest {
        let mut request = request(Some("caller"));
        request.tools = Some(json!([{
            "type": "function",
            "function": {
                "name": "glob",
                "parameters": {
                    "type": "object",
                    "properties": { "pattern": { "type": "string" } },
                    "required": ["pattern"]
                }
            }
        }]));
        request
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

    #[tokio::test]
    async fn bound_session_splits_large_bootstrap() {
        let path = std::env::temp_dir().join(format!(
            "poolman-yuanbao-large-{}.db",
            uuid::Uuid::new_v4()
        ));
        let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
        let db = sea_orm::Database::connect(&url)
            .await
            .expect("初始化测试数据库失败");
        let store = Arc::new(
            DbWebContextStore::new(db)
                .await
                .expect("初始化绑定表失败"),
        );
        let mut responses = vec![create_body(CONV)];
        for _ in 0..12 {
            responses.push(completion_stream());
        }
        let executor = Arc::new(MockExecutor::new(responses));
        let model = YuanbaoWebModel::new("腾讯元宝", executor.clone() as Arc<dyn ChannelExecutor>)
            .with_web_context_store(store);
        let mut request = InferRequest::new(
            CHANNEL,
            vec![
                json!({ "role": "system", "content": "系统提示词".repeat(5_000) }),
                json!({ "role": "user", "content": "最新问题" }),
            ],
        );
        request.web_context_key = Some("yb_sess_large".to_string());
        model.infer(request).await.expect("大 bootstrap 应成功");

        let bodies = completion_prompts(&executor);
        assert!(bodies.len() > 2, "bootstrap 应拆成多段");
        for prompt in &bodies[..bodies.len() - 1] {
            assert!(prompt.contains("不要回答"));
            assert!(!prompt.contains("最新问题"), "初始化段不应带本轮提问");
            assert!(prompt.chars().count() <= 6_300);
        }
        let last = bodies.last().unwrap();
        assert!(last.contains("请回答本轮提问"));
        assert!(last.contains("最新问题"));
        assert!(last.chars().count() <= 6_300);

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn title_requests_reuse_dedicated_session() {
        let path = std::env::temp_dir().join(format!(
            "poolman-yuanbao-title-{}.db",
            uuid::Uuid::new_v4()
        ));
        let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
        let db = sea_orm::Database::connect(&url)
            .await
            .expect("初始化测试数据库失败");
        let store = Arc::new(
            DbWebContextStore::new(db)
                .await
                .expect("初始化绑定表失败"),
        );
        let executor = Arc::new(MockExecutor::new(vec![
            create_body(CONV),
            completion_stream(),
            completion_stream(),
        ]));
        let model = YuanbaoWebModel::new("腾讯元宝", executor.clone() as Arc<dyn ChannelExecutor>)
            .with_web_context_store(store);
        for _ in 0..2 {
            let mut request = InferRequest::new(
                CHANNEL,
                vec![
                    json!({
                        "role": "system",
                        "content": "Create a concise title for an AI coding-assistant session."
                    }),
                    json!({
                        "role": "user",
                        "content": "Generate the session title from this JSON array of human messages: []"
                    }),
                ],
            );
            request.api_key = Some("sk-test".to_string());
            model.infer(request).await.expect("标题请求应成功");
        }
        let creates = executor
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.url.contains("/conversation/create"))
            .count();
        assert_eq!(creates, 1, "元宝标题会话应长期复用");
        assert_eq!(completion_prompts(&executor).len(), 2);

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn bound_session_bootstraps_once_then_sends_delta() {
        let path = std::env::temp_dir().join(format!(
            "poolman-yuanbao-bound-{}.db",
            uuid::Uuid::new_v4()
        ));
        let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
        let db = sea_orm::Database::connect(&url)
            .await
            .expect("初始化测试数据库失败");
        let store = Arc::new(
            DbWebContextStore::new(db)
                .await
                .expect("初始化绑定表失败"),
        );
        let executor = Arc::new(MockExecutor::new(vec![
            create_body(CONV),
            completion_stream(),
            completion_stream(),
            completion_stream(),
        ]));
        let model = YuanbaoWebModel::new("腾讯元宝", executor.clone() as Arc<dyn ChannelExecutor>)
            .with_web_context_store(store);

        let mut first = request(None);
        first.web_context_key = Some("yb_sess_test".to_string());
        let first_response = model.infer(first).await.expect("首次请求应成功");
        let bodies = completion_prompts(&executor);
        assert_eq!(bodies.len(), 2, "首次绑定会话：初始化一轮 + 提问一轮");
        assert!(bodies[0].contains("初始化完成"), "{}", bodies[0]);
        assert!(
            !bodies[0].contains("你好"),
            "初始化段不应带本轮提问：{}",
            bodies[0]
        );
        assert!(bodies[1].contains("请回答本轮提问"), "{}", bodies[1]);
        assert!(bodies[1].contains("你好"));

        let mut second = InferRequest::new(
            CHANNEL,
            vec![
                json!({ "role": "user", "content": "你好" }),
                json!({ "role": "assistant", "content": first_response.text }),
                json!({ "role": "user", "content": "第二问" }),
            ],
        );
        second.web_context_key = Some("yb_sess_test".to_string());
        model.infer(second).await.expect("后续请求应成功");
        let bodies = completion_prompts(&executor);
        assert_eq!(bodies.len(), 3);
        assert!(bodies[2].contains("继续上一轮对话"));
        assert!(bodies[2].contains("第二问"));
        assert!(!bodies[2].contains("你好"), "后续不应重发历史");

        let _ = std::fs::remove_file(path);
    }

    fn completion_prompts(executor: &Arc<MockExecutor>) -> Vec<String> {
        executor
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.url.contains("/api/chat/"))
            .filter_map(|request| {
                request
                    .body
                    .as_deref()
                    .and_then(|body| serde_json::from_str::<Value>(body).ok())
                    .and_then(|body| body.get("prompt").and_then(Value::as_str).map(String::from))
            })
            .collect()
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

    /// 改名：走会话级接口，带 JSON 头与会话级 `x-agentid`，正文是会话 id + 新标题。
    #[tokio::test]
    async fn renames_session_via_session_endpoint() {
        let executor = Arc::new(MockExecutor::new(vec![
            json!({ "data": { "biz_code": 0 } }).to_string()
        ]));
        let ok = model(executor.clone())
            .rename_channel_session(CONV, "  新标题  ")
            .await
            .unwrap();
        assert!(ok);

        let requests = executor.requests.lock().unwrap();
        let request = requests.first().expect("应发出一次请求");
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, SESSION_UPDATE_URL);
        assert_eq!(request.session_key.as_deref(), Some(CONV));

        let header = |name: &str| {
            request
                .headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(header("content-type"), Some("application/json"));
        assert_eq!(header("x-agentid"), Some(format!("{AGENT_ID}/{CONV}").as_str()));
        assert_eq!(
            header("x-agentid").map(|value| value.matches('/').count()),
            Some(1),
            "会话级 x-agentid 只能是 agentId/conversationId 两段"
        );

        let body: Value =
            serde_json::from_str(request.body.as_deref().unwrap()).expect("请求体应为 JSON");
        assert_eq!(body["conversationId"], json!(CONV));
        assert_eq!(body["title"], json!("新标题"), "标题应去掉首尾空白");
    }

    /// 空标题直接拒绝，不发请求（网页端会把它当成清空标题）。
    #[tokio::test]
    async fn rejects_blank_session_title() {
        let executor = Arc::new(MockExecutor::new(vec![
            json!({ "data": { "biz_code": 0 } }).to_string()
        ]));
        let ok = model(executor.clone())
            .rename_channel_session(CONV, "   ")
            .await
            .unwrap();
        assert!(!ok);
        assert!(executor.requests.lock().unwrap().is_empty(), "空标题不应发请求");
    }

    /// 业务失败码要透传成 `false`，便于命令层报错。
    #[tokio::test]
    async fn rename_reports_business_failure() {
        let executor = Arc::new(MockExecutor::new(vec![
            json!({ "data": { "biz_code": 1001 } }).to_string()
        ]));
        let ok = model(executor)
            .rename_channel_session(CONV, "新标题")
            .await
            .unwrap();
        assert!(!ok);
    }

    /// 非流式：DSML 调用解析成 tool_calls，正文里不留标记。
    #[tokio::test]
    async fn parses_dsml_tool_calls_from_stream() {
        let executor = Arc::new(MockExecutor::new(vec![
            create_body(CONV),
            dsml_stream(),
        ]));
        let response = model(executor)
            .infer(request_with_tools())
            .await
            .expect("应成功");

        let calls = response.tool_calls.expect("应解析出工具调用");
        let array = calls.as_array().expect("tool_calls 是数组");
        assert_eq!(array.len(), 1, "{calls}");
        assert_eq!(array[0]["function"]["name"], json!("glob"));
        assert_eq!(array[0]["function"]["arguments"], json!("{\"pattern\":\"**/*\"}"));
        assert!(
            response.text.trim().is_empty(),
            "标记不应留在正文：{}",
            response.text
        );
        assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
    }

    /// 请求没带工具时，同样的标记按普通正文返回（保持旧行为）。
    #[tokio::test]
    async fn keeps_dsml_as_text_without_tools() {
        let executor = Arc::new(MockExecutor::new(vec![
            create_body(CONV),
            dsml_stream(),
        ]));
        let response = model(executor).infer(request(Some("caller"))).await.expect("应成功");
        assert!(response.tool_calls.is_none());
        assert!(
            response.text.contains("DSML"),
            "不带工具时标记原样返回：{}",
            response.text
        );
    }

    /// 流式：标记不外泄给客户端，工具调用单独作为 FUNCTIONS 增量送出。
    #[tokio::test]
    async fn streams_tool_calls_without_leaking_markup() {
        let executor = Arc::new(MockExecutor::new(vec![
            create_body(CONV),
            dsml_stream(),
        ]));
        let deltas = Arc::new(Mutex::new(Vec::<String>::new()));
        let collected = deltas.clone();
        let sink: ChunkSink = Box::new(move |delta: String| {
            collected.lock().unwrap().push(delta);
        });
        let model = model(executor);
        let response = model
            .infer_stream(request_with_tools(), sink)
            .await
            .expect("应成功");

        let deltas = deltas.lock().unwrap();
        assert!(
            !deltas.iter().any(|delta| delta.contains("DSML")),
            "DSML 标记不应流给客户端：{deltas:?}"
        );
        assert!(
            deltas.iter().any(|delta| delta.contains("FUNCTIONS")),
            "工具调用应作为 FUNCTIONS 增量送出：{deltas:?}"
        );
        assert!(response.tool_calls.is_some());
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
