//! DeepSeek 对话补全（`/chat/completions`）。
//!
//! 对外字段严格对齐 DeepSeek 官方 `create-chat-completion` 文档；模型名沿用网关
//! 既有的渠道命名（如 `deepseek-web`），不做官方模型名映射——路由层按渠道名解析。
//!
//! 与 OpenAI 格式的两处关键差异：
//! 1. 流式**不单独下发**只含 `usage` 的块：用量附加在 `[DONE]` 之前的最后一个内容块上；
//!    `stream_options.include_usage` 为真时，其余块也带 `usage` 字段，但值为 `null`。
//! 2. `usage` 带 `prompt_tokens_details` / `completion_tokens_details`，
//!    响应顶层带 `system_fingerprint`，`choices[].logprobs` 恒为 null。

use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::{
    extract::{Extension, Json, State},
    http::StatusCode,
    response::{sse::Event, IntoResponse, Json as AxumJson, Sse},
    routing::post,
    Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::apis::base::{
    error_response, normalize_messages, parse_delta, request_meta, ApiKeyAuth, AppState,
};
use crate::providers::InferRequest;
use crate::request_record::RequestMeta;

/// 思考模式开关与强度。
#[derive(Debug, Clone, Deserialize)]
pub struct Thinking {
    /// `enabled` / `disabled`，缺省按 `enabled`。
    #[serde(default, rename = "type")]
    pub r#type: Option<String>,
    /// `none` / `low` / `high` / `max`；`none` 等价于关闭思考。
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

/// 流式选项：必须与 `stream: true` 一起使用。
#[derive(Debug, Clone, Deserialize)]
pub struct StreamOptions {
    #[serde(default)]
    pub include_usage: bool,
}

/// 对话补全请求体（字段与官方文档一致；未列出的参数按未知字段忽略）。
#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    /// 对外模型名，走网关既有渠道命名（如 `deepseek-web`）。
    pub model: String,
    pub messages: Vec<Value>,
    #[serde(default)]
    pub thinking: Option<Thinking>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// 仅接收、不下发：网页渠道无法表达 JSON 模式，落地上游没有意义。
    #[serde(default)]
    pub response_format: Option<Value>,
    /// `string` 或 `string[]`（最多 16 个）。
    #[serde(default)]
    pub stop: Option<Value>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub stream_options: Option<StreamOptions>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub tools: Option<Value>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub logprobs: Option<bool>,
    #[serde(default)]
    pub top_logprobs: Option<u32>,
    #[serde(default)]
    pub user_id: Option<String>,
    /// 已废弃：接收后忽略。
    #[serde(default)]
    pub frequency_penalty: Option<f32>,
    /// 已废弃：接收后忽略。
    #[serde(default)]
    pub presence_penalty: Option<f32>,
}

/// 思考模式下 `max_tokens` 的文档默认值。
const THINKING_MAX_TOKENS: u32 = 64 * 1024;
/// `reasoning_effort = max` 时的文档默认值。
const THINKING_MAX_TOKENS_MAX_EFFORT: u32 = 128 * 1024;
/// 非思考模式的文档默认值。
const NON_THINKING_MAX_TOKENS: u32 = 8 * 1024;
/// 文档给出的 `max_tokens` 上限。
const MAX_TOKENS_LIMIT: u32 = 393_216;

/// 按 `thinking` 推算 `max_tokens` 的默认值。
///
/// 文档：未设置时，非思考模式默认 8K，思考模式默认 64K（`reasoning_effort` 为
/// `max` 时为 128K）。`type: disabled` 与 `reasoning_effort: none` 都表示关闭思考。
fn default_max_tokens(thinking: Option<&Thinking>) -> u32 {
    let effort = thinking.and_then(|t| t.reasoning_effort.as_deref());
    let enabled = match thinking.and_then(|t| t.r#type.as_deref()) {
        Some("disabled") => false,
        Some("enabled") => true,
        // 只给了 reasoning_effort：none 关闭，其余开启
        _ => effort != Some("none"),
    };
    if !enabled {
        return NON_THINKING_MAX_TOKENS;
    }
    match effort {
        Some("max") => THINKING_MAX_TOKENS_MAX_EFFORT,
        _ => THINKING_MAX_TOKENS,
    }
}

/// `system_fingerprint`：标识承载该模型的渠道，模型换了指纹就跟着变。
fn fingerprint(model: &str) -> String {
    format!("fp_{}", crate::chat_router::ChatRouter::map_model(model))
}

/// 估算本轮 prompt 的缓存命中拆分：`(命中, 未命中)`。
///
/// 客户端每轮都发完整历史，上一轮已发过的历史前缀正是会被上游上下文缓存复用的部分，
/// 因此按「除最后一条消息外都算命中、最后一条算未命中」拆分；只有一条消息（首轮）
/// 时全部计入未命中，与真实首轮无缓存一致。
///
/// 两个返回值之和等于按同样口径统计出的 `prompt_tokens`，
/// 从而保证 `prompt_cache_hit_tokens + prompt_cache_miss_tokens == prompt_tokens`。
fn prompt_cache_split(messages: &[Value]) -> (u64, u64) {
    let texts: Vec<&str> = messages
        .iter()
        .filter_map(|message| message.get("content").and_then(Value::as_str))
        .collect();
    if texts.is_empty() {
        return (0, 0);
    }
    if texts.len() == 1 {
        return (
            0,
            u64::from(crate::tokenizer::count_messages(texts.iter().copied())),
        );
    }
    let split = texts.len() - 1;
    let hit = u64::from(crate::tokenizer::count_messages(
        texts[..split].iter().copied(),
    ));
    let miss = u64::from(crate::tokenizer::count_messages(
        texts[split..].iter().copied(),
    ));
    (hit, miss)
}

/// 构造文档规定的 `usage` 对象。
///
/// `cached_tokens` 与 `prompt_cache_hit_tokens` 同值，由 [`prompt_cache_split`] 估算；
/// 三个 prompt 字段始终自洽：`hit + miss == prompt_tokens`。
///
/// `completion_tokens` 含思维链 token，`reasoning_tokens` 是它的子集（官方口径）。
fn usage_object(
    prompt_tokens: u64,
    cache_hit_tokens: u64,
    completion_tokens: u64,
    reasoning_tokens: u64,
) -> Value {
    let hit = cache_hit_tokens.min(prompt_tokens);
    let reasoning = reasoning_tokens.min(completion_tokens);
    json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": prompt_tokens + completion_tokens,
        "prompt_tokens_details": {
            "cached_tokens": hit,
            "prompt_cache_hit_tokens": hit,
            "prompt_cache_miss_tokens": prompt_tokens - hit,
        },
        "completion_tokens_details": {
            "reasoning_tokens": reasoning,
        },
    })
}

/// 从渠道回报的 usage 里取某个计数（缺失或不合法时返回 None，由调用方估算）。
fn usage_field(usage: Option<&Value>, key: &str) -> Option<u64> {
    usage.and_then(|u| u.get(key)).and_then(Value::as_u64)
}

/// 对话补全 handler：`stream` 决定 JSON 或 SSE 响应。调用方身份由鉴权中间件注入。
pub async fn chat_completions_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ApiKeyAuth>,
    headers: axum::http::HeaderMap,
    Json(req): Json<ChatRequest>,
) -> impl IntoResponse {
    if req.messages.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "messages 不能为空",
            "invalid_request_error",
        );
    }
    // 文档：stream_options 必须与 stream: true 一起使用，否则返回 400。
    if req.stream_options.is_some() && !req.stream {
        return error_response(
            StatusCode::BAD_REQUEST,
            "stream_options 必须与 stream: true 一起使用",
            "invalid_request_error",
        );
    }

    let tool_results = req
        .messages
        .iter()
        .filter(|item| item["role"] == "tool")
        .count();
    let tool_count = req
        .tools
        .as_ref()
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    // 用了文档里「暂不透传上游」的参数时留一行日志，便于排查「传了没效果」。
    let unsupported = [
        ("response_format", req.response_format.is_some()),
        ("stop", req.stop.is_some()),
        ("logprobs", req.logprobs.is_some()),
        ("top_logprobs", req.top_logprobs.is_some()),
        ("user_id", req.user_id.is_some()),
        ("frequency_penalty", req.frequency_penalty.is_some()),
        ("presence_penalty", req.presence_penalty.is_some()),
    ]
    .iter()
    .filter(|(_, present)| *present)
    .map(|(name, _)| *name)
    .collect::<Vec<_>>();
    info!(
        "入站请求 model={} stream={} messages={} tool_results={} tools={} thinking={:?} unsupported={:?}",
        req.model,
        req.stream,
        req.messages.len(),
        tool_results,
        tool_count,
        req.thinking.as_ref().and_then(|t| t.reasoning_effort.as_deref()),
        unsupported
    );

    let model = req.model.clone();
    let meta = request_meta("deepseek", "/chat/completions", &headers);
    let max_tokens = req
        .max_tokens
        .unwrap_or_else(|| default_max_tokens(req.thinking.as_ref()))
        .min(MAX_TOKENS_LIMIT);
    // 用量口径：与 openai 侧一致，用官方分词器按本轮实际发送的 prompt 计数。
    // 同时拆出缓存命中部分：上一轮已发过的历史前缀会被上游上下文缓存复用。
    let (cache_hit_tokens, cache_miss_tokens) = prompt_cache_split(&req.messages);
    let prompt_tokens = cache_hit_tokens + cache_miss_tokens;
    let include_usage = req
        .stream_options
        .as_ref()
        .is_some_and(|options| options.include_usage);
    let request = InferRequest {
        model: model.clone(),
        messages: normalize_messages(req.messages.clone()),
        stream: req.stream,
        temperature: req.temperature,
        top_p: req.top_p,
        max_tokens: Some(max_tokens),
        tools: req.tools.clone(),
        tool_choice: req.tool_choice.clone(),
        session_key: req.user_id.clone(),
        web_context_key: None,
        api_key: Some(auth.key),
    };

    if req.stream {
        return sse_response(
            state,
            request,
            meta,
            model,
            include_usage,
            prompt_tokens,
            cache_hit_tokens,
        );
    }

    match state.workflow_runtime.chat_completions(request, meta).await {
        Ok(result) => {
            let completion_tokens = usage_field(result.usage.as_ref(), "completion_tokens")
                .unwrap_or_else(|| u64::from(crate::tokenizer::count(&result.text)));
            let reasoning_tokens = result
                .reasoning
                .as_deref()
                .map(|text| u64::from(crate::tokenizer::count(text)))
                .unwrap_or(0);
            let prompt_tokens = usage_field(result.usage.as_ref(), "prompt_tokens")
                .unwrap_or(prompt_tokens);

            let mut message = serde_json::Map::new();
            message.insert("role".to_string(), json!("assistant"));
            message.insert(
                "content".to_string(),
                if result.text.is_empty() {
                    Value::Null
                } else {
                    json!(result.text)
                },
            );
            if let Some(reasoning) = result.reasoning.clone().filter(|text| !text.is_empty()) {
                message.insert("reasoning_content".to_string(), json!(reasoning));
            }
            if let Some(calls) = result.tool_calls.clone() {
                message.insert("tool_calls".to_string(), calls);
            }
            let finish_reason = result
                .finish_reason
                .clone()
                .unwrap_or_else(|| "stop".to_string());
            let response = json!({
                "id": format!("chatcmpl-{}", chrono::Utc::now().timestamp_millis()),
                "object": "chat.completion",
                "created": chrono::Utc::now().timestamp() as u64,
                "model": model,
                "system_fingerprint": fingerprint(&model),
                "choices": [{
                    "index": 0,
                    "message": Value::Object(message),
                    "finish_reason": finish_reason,
                    "logprobs": Value::Null,
                }],
                "usage": usage_object(
                    prompt_tokens,
                    cache_hit_tokens,
                    completion_tokens,
                    reasoning_tokens,
                ),
            });
            (StatusCode::OK, AxumJson(response)).into_response()
        }
        Err(e) => {
            warn!("聊天请求出错: {}", e);
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &e.to_string(),
                "server_error",
            )
        }
    }
}

/// 推理任务的兜底：任务异常退出（panic）时给客户端补一条错误，
/// 否则发送端随任务一起消失，流会「正常结束」，客户端只看到空回答。
struct TaskGuard {
    tx: tokio::sync::mpsc::UnboundedSender<Result<String, String>>,
    finished: Arc<AtomicBool>,
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        if !self.finished.load(Ordering::Relaxed) {
            warn!("推理任务异常退出，向客户端补发错误");
            let _ = self
                .tx
                .send(Err("网关内部错误：推理任务异常退出".to_string()));
        }
    }
}

/// 流式对话补全：SSE 逐条输出增量。
///
/// 与 OpenAI 的差别在收尾：这里**不补独立的 usage 块**，而是把完整用量挂在
/// `[DONE]` 之前的最后一个内容块上（该块 `delta` 为空、`finish_reason` 非 null）。
fn sse_response(
    state: Arc<AppState>,
    request: InferRequest,
    meta: RequestMeta,
    model: String,
    include_usage: bool,
    prompt_tokens: u64,
    cache_hit_tokens: u64,
) -> axum::response::Response {
    let base = json!({
        "id": format!("chatcmpl-{}", chrono::Utc::now().timestamp_millis()),
        "object": "chat.completion.chunk",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "system_fingerprint": fingerprint(&model),
    });
    let response_id = base["id"].clone();

    let stream = async_stream::stream! {
        // 闭包是同步 FnMut，不能 await/block，故用 unbounded 通道回传增量。
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<String, String>>();
        let chunk_tx = tx.clone();
        let router = state.workflow_runtime.clone();
        let finished = Arc::new(AtomicBool::new(false));

        tokio::spawn({
            let guard = TaskGuard {
                tx: tx.clone(),
                finished: finished.clone(),
            };
            let finished = finished.clone();
            async move {
                let _guard = guard;
                let result = router
                    .chat_completions_stream(
                        request,
                        meta,
                        Box::new(move |delta: String| {
                            let _ = chunk_tx.send(Ok(delta));
                        }),
                    )
                    .await;
                if let Err(error) = result {
                    let _ = tx.send(Err(error.to_string()));
                }
                finished.store(true, Ordering::Relaxed);
            }
        });

        let mut seen_tool_calls = false;
        let mut frames = 0usize;
        let mut content_chars = 0usize;
        let mut reasoning_chars = 0usize;
        let mut tool_calls = 0usize;
        // 收尾要给出整轮用量：正文与思考分别累计
        let mut completion_text = String::new();
        let mut reasoning_text = String::new();
        while let Some(item) = rx.recv().await {
            let delta = match item {
                Ok(delta) => delta,
                Err(error) => {
                    warn!("出站中止 request_id={} frames={} err={}", response_id, frames, error);
                    let event = json!({ "error": { "message": error, "type": "server_error" } });
                    yield Ok::<_, Infallible>(Event::default().data(serde_json::to_string(&event).unwrap()));
                    // 这一轮还没有任何正文与工具调用：补一条正文说明失败原因并正常收尾。
                    if content_chars == 0 && tool_calls == 0 {
                        let frame = json!({
                            "id": base["id"],
                            "object": base["object"],
                            "created": base["created"],
                            "model": base["model"],
                            "system_fingerprint": base["system_fingerprint"],
                            "choices": [{
                                "index": 0,
                                "delta": { "content": format!("网关错误：{}", error) },
                                "finish_reason": Value::Null,
                                "logprobs": Value::Null,
                            }],
                        });
                        yield Ok::<_, Infallible>(Event::default().data(serde_json::to_string(&frame).unwrap()));
                        let end = json!({
                            "id": base["id"],
                            "object": base["object"],
                            "created": base["created"],
                            "model": base["model"],
                            "system_fingerprint": base["system_fingerprint"],
                            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop", "logprobs": Value::Null }],
                            "usage": usage_object(prompt_tokens, cache_hit_tokens, 0, 0),
                        });
                        yield Ok::<_, Infallible>(Event::default().data(serde_json::to_string(&end).unwrap()));
                    }
                    yield Ok::<_, Infallible>(Event::default().data("[DONE]"));
                    return;
                }
            };
            let chunk = parse_delta(&delta);

            let mut delta_obj = json!({});
            if !chunk.content.is_empty() {
                content_chars += chunk.content.chars().count();
                completion_text.push_str(&chunk.content);
                delta_obj["content"] = json!(chunk.content);
            }
            if !chunk.reasoning.is_empty() {
                reasoning_chars += chunk.reasoning.chars().count();
                reasoning_text.push_str(&chunk.reasoning);
                delta_obj["reasoning_content"] = json!(chunk.reasoning);
            }
            if let Some(calls) = chunk.functions {
                tool_calls += calls.as_array().map_or(0, Vec::len);
                delta_obj["tool_calls"] = index_tool_calls(calls);
                seen_tool_calls = true;
            }

            frames += 1;
            let mut frame = json!({
                "id": base["id"],
                "object": base["object"],
                "created": base["created"],
                "model": base["model"],
                "system_fingerprint": base["system_fingerprint"],
                "choices": [{ "index": 0, "delta": delta_obj, "finish_reason": Value::Null, "logprobs": Value::Null }],
            });
            // include_usage 时中间块也带 usage 字段，但值为 null（文档行为）。
            if include_usage {
                frame["usage"] = Value::Null;
            }
            yield Ok::<_, Infallible>(Event::default().data(serde_json::to_string(&frame).unwrap()));
        }

        let finish_reason = if seen_tool_calls { "tool_calls" } else { "stop" };
        info!(
            "出站摘要 request_id={} frames={} content_chars={} reasoning_chars={} tool_calls={} finish_reason={}",
            response_id, frames, content_chars, reasoning_chars, tool_calls, finish_reason
        );
        // 思考 token 计入 completion（官方口径：reasoning_tokens 是 completion_tokens 的子集）。
        let reasoning_tokens = u64::from(crate::tokenizer::count(&reasoning_text));
        let completion_tokens = u64::from(crate::tokenizer::count(&completion_text)) + reasoning_tokens;
        // 收尾块：delta 为空、finish_reason 非 null，并挂上整轮 usage。
        // 无论 include_usage 与否，最后一块都带用量（文档行为）。
        let end = json!({
            "id": base["id"],
            "object": base["object"],
            "created": base["created"],
            "model": base["model"],
            "system_fingerprint": base["system_fingerprint"],
            "choices": [{ "index": 0, "delta": {}, "finish_reason": finish_reason, "logprobs": Value::Null }],
            "usage": usage_object(
                prompt_tokens,
                cache_hit_tokens,
                completion_tokens,
                reasoning_tokens,
            ),
        });
        yield Ok::<_, Infallible>(Event::default().data(serde_json::to_string(&end).unwrap()));
        yield Ok::<_, Infallible>(Event::default().data("[DONE]"));
    };

    (StatusCode::OK, Sse::new(stream)).into_response()
}

/// 本格式的路由：`POST /chat/completions`（DeepSeek 官方路径，不带 `/v1` 前缀）。
pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/chat/completions", post(chat_completions_handler))
}

/// 给工具调用补上流式增量必需的 `index`（客户端按它合并分片）：渠道侧已按整轮全局序号给出的不覆盖，
/// 缺的按位置补。
fn index_tool_calls(tool_calls: Value) -> Value {
    let Value::Array(calls) = tool_calls else {
        return tool_calls;
    };
    Value::Array(
        calls
            .into_iter()
            .enumerate()
            .map(|(index, call)| match call {
                Value::Object(mut map) => {
                    map.entry("index".to_string()).or_insert_with(|| json!(index));
                    Value::Object(map)
                }
                other => other,
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        default_max_tokens, index_tool_calls, prompt_cache_split, usage_object, ChatRequest, Thinking,
    };
    use serde_json::{json, Value};

    /// 未设置 max_tokens 时按 thinking 取文档默认值。
    #[test]
    fn default_max_tokens_follows_thinking_mode() {
        assert_eq!(default_max_tokens(None), 64 * 1024, "缺省按思考模式 64K");

        let disabled = Thinking {
            r#type: Some("disabled".to_string()),
            reasoning_effort: None,
        };
        assert_eq!(default_max_tokens(Some(&disabled)), 8 * 1024);

        let effort_none = Thinking {
            r#type: None,
            reasoning_effort: Some("none".to_string()),
        };
        assert_eq!(default_max_tokens(Some(&effort_none)), 8 * 1024, "none 等于关闭思考");

        let max = Thinking {
            r#type: Some("enabled".to_string()),
            reasoning_effort: Some("max".to_string()),
        };
        assert_eq!(default_max_tokens(Some(&max)), 128 * 1024);
    }

    /// usage 必须带两组 details，且 hit + miss 等于 prompt_tokens。
    #[test]
    fn usage_carries_token_details() {
        let usage = usage_object(100, 40, 30, 20);
        assert_eq!(usage["prompt_tokens"], 100);
        assert_eq!(usage["completion_tokens"], 30);
        assert_eq!(usage["total_tokens"], 130);
        assert_eq!(usage["prompt_tokens_details"]["prompt_cache_hit_tokens"], 40);
        assert_eq!(usage["prompt_tokens_details"]["prompt_cache_miss_tokens"], 60);
        assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 40);
        assert_eq!(usage["completion_tokens_details"]["reasoning_tokens"], 20);
    }

    /// 命中数超过 prompt_tokens 时被夹住，miss 不会出现负数。
    #[test]
    fn cache_hit_is_clamped_to_prompt_tokens() {
        let usage = usage_object(10, 99, 5, 0);
        assert_eq!(usage["prompt_tokens_details"]["prompt_cache_hit_tokens"], 10);
        assert_eq!(usage["prompt_tokens_details"]["prompt_cache_miss_tokens"], 0);
    }

    /// 首轮（只有一条消息）全部未命中；多轮时历史前缀算命中，两者之和等于 prompt 总量。
    #[test]
    fn prompt_cache_split_treats_history_prefix_as_hit() {
        let single = vec![json!({ "role": "user", "content": "你好" })];
        let (hit, miss) = prompt_cache_split(&single);
        assert_eq!(hit, 0, "首轮没有可复用的历史前缀");
        assert!(miss > 0, "首轮应全部计入未命中");

        let history = vec![
            json!({ "role": "user", "content": "你好" }),
            json!({ "role": "assistant", "content": "你好，有什么可以帮你？" }),
            json!({ "role": "user", "content": "介绍一下你自己" }),
        ];
        let (hit, miss) = prompt_cache_split(&history);
        assert!(hit > 0, "历史前缀应计入命中");
        assert!(miss > 0, "最后一条应计入未命中");
        let total = u64::from(crate::tokenizer::count_messages(
            history
                .iter()
                .filter_map(|message| message.get("content").and_then(Value::as_str)),
        ));
        assert_eq!(hit + miss, total, "命中与未命中之和应等于 prompt 总量");

        let empty: Vec<Value> = Vec::new();
        assert_eq!(prompt_cache_split(&empty), (0, 0));
    }

    /// 文档字段都能解析：thinking、stream_options、stop、user_id。
    #[test]
    fn request_parses_documented_fields() {
        let req: ChatRequest = serde_json::from_value(json!({
            "model": "deepseek-web",
            "messages": [{ "role": "user", "content": "hi" }],
            "thinking": { "type": "enabled", "reasoning_effort": "max" },
            "stream": true,
            "stream_options": { "include_usage": true },
            "stop": ["a", "b"],
            "user_id": "u-1",
            "max_tokens": 1024
        }))
        .expect("应能解析文档字段");

        assert!(req.stream);
        assert!(req.stream_options.expect("应有 stream_options").include_usage);
        assert_eq!(
            req.thinking.expect("应有 thinking").reasoning_effort.as_deref(),
            Some("max")
        );
        assert_eq!(req.max_tokens, Some(1024));
        assert_eq!(req.user_id.as_deref(), Some("u-1"));
        assert!(req.stop.is_some());
    }

    /// 废弃参数接收后不报错。
    #[test]
    fn deprecated_penalties_are_accepted() {
        let req: ChatRequest = serde_json::from_value(json!({
            "model": "deepseek-web",
            "messages": [{ "role": "user", "content": "hi" }],
            "frequency_penalty": 0.5,
            "presence_penalty": 0.5
        }))
        .expect("废弃参数不应导致解析失败");
        assert_eq!(req.frequency_penalty, Some(0.5));
        assert_eq!(req.presence_penalty, Some(0.5));
    }

    /// 流式工具调用分片要补 index。
    #[test]
    fn tool_call_deltas_get_index() {
        let indexed = index_tool_calls(json!([
            { "id": "call_a", "type": "function", "function": { "name": "a", "arguments": "{}" } },
            { "index": 7, "id": "call_b", "type": "function", "function": { "name": "b", "arguments": "{}" } },
        ]));
        assert_eq!(indexed[0]["index"], 0);
        assert_eq!(indexed[1]["index"], 7);
        assert_eq!(index_tool_calls(json!("plain")), json!("plain"));
    }
}