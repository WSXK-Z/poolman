//! OpenAI 对话补全（`/v1/chat/completions`）。
//!
//! 调用方身份由鉴权中间件校验并注入；消息规整后交给模型分派入口，
//! 非流式返回 JSON，流式经 SSE 逐条输出正文 / 思考增量与终止状态。

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
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};

use crate::apis::base::{error_response, normalize_messages, parse_delta, ApiKeyAuth, AppState};
use crate::providers::InferRequest;

#[derive(Debug, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Value>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub tools: Option<Value>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub stream_options: Option<StreamOptions>,
}

/// 流式选项：`include_usage` 为 true 时，按 OpenAI 约定在结束帧之后再补一个只带
/// `usage` 的块（`choices` 为空数组），供客户端统计本轮消耗。
#[derive(Debug, Deserialize)]
pub struct StreamOptions {
    #[serde(default)]
    pub include_usage: bool,
}

#[derive(Debug, Serialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Serialize)]
pub struct Choice {
    pub index: u32,
    pub message: Message,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Message {
    pub role: String,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub r#type: String,
    pub function: Function,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Function {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// 对话补全 handler：stream 决定 JSON 或 SSE 响应。调用方身份由鉴权中间件注入。
pub async fn chat_completions_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ApiKeyAuth>,
    headers: axum::http::HeaderMap,
    Json(req): Json<ChatCompletionRequest>,
) -> impl IntoResponse {
    if req.messages.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "消息数组不能为空",
            "invalid_request_error",
        );
    }

    // 入站摘要：区分「新会话的下一轮」与「同一轮被重复发出」靠这一行
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("-");
    let tool_results = req
        .messages
        .iter()
        .filter(|item| item["role"] == "tool")
        .count();
    let last_user_chars = req
        .messages
        .iter()
        .rev()
        .find(|item| item["role"] == "user")
        .and_then(|item| item["content"].as_str())
        .map_or(0, |text| text.chars().count());
    let tool_count = req
        .tools
        .as_ref()
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    info!(
        "入站请求 model={} stream={} messages={} tool_results={} last_user_chars={} tools={} client_session={:?} ua={}",
        req.model,
        req.stream,
        req.messages.len(),
        tool_results,
        last_user_chars,
        tool_count,
        req.session_id,
        user_agent
    );

    let model = req.model.clone();
    let request = InferRequest {
        model: model.clone(),
        messages: normalize_messages(req.messages.clone()),
        stream: req.stream,
        temperature: req.temperature,
        top_p: req.top_p,
        max_tokens: req.max_tokens,
        tools: req.tools.clone(),
        tool_choice: req.tool_choice.clone(),
        session_key: req.session_id.clone(),
        api_key: Some(auth.key),
    };

    if req.stream {
        // `stream_options.include_usage`：客户端要求时，在结束帧后补一个只带 usage 的块
        let include_usage = req
            .stream_options
            .as_ref()
            .is_some_and(|options| options.include_usage);
        return sse_response(state, request, model, include_usage);
    }

    match state.chat_router.chat_completions(request).await {
        Ok(result) => {
            let tool_calls = result.tool_calls;
            let message = Message {
                role: "assistant".to_string(),
                content: if tool_calls.is_some() {
                    None
                } else {
                    Some(result.text)
                },
                reasoning_content: result.reasoning.filter(|text| !text.is_empty()),
                tool_calls: tool_calls.and_then(|tc| serde_json::from_value(tc).ok()),
            };
            let resp = ChatCompletionResponse {
                id: format!("chatcmpl-{}", chrono::Utc::now().timestamp_millis()),
                object: "chat.completion".to_string(),
                created: chrono::Utc::now().timestamp() as u64,
                model,
                choices: vec![Choice {
                    index: 0,
                    message,
                    finish_reason: result.finish_reason.or(Some("stop".to_string())),
                }],
                usage: result.usage.and_then(|u| serde_json::from_value(u).ok()),
            };
            (
                StatusCode::OK,
                AxumJson(serde_json::to_value(&resp).unwrap_or(serde_json::json!({}))),
            )
                .into_response()
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
fn sse_response(
    state: Arc<AppState>,
    request: InferRequest,
    model: String,
    include_usage: bool,
) -> axum::response::Response {
    // 用量口径：网页渠道不回传本轮用量，这里用官方分词器按本轮实际发送的
    // prompt 计数（含每条消息的角色标记开销），与上游分词结果一致。
    let prompt_tokens = crate::tokenizer::count_messages(
        request
            .messages
            .iter()
            .filter_map(|message| message.get("content").and_then(Value::as_str)),
    );
    let base = serde_json::json!({
        "id": format!("chatcmpl-{}", chrono::Utc::now().timestamp_millis()),
        "object": "chat.completion.chunk",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
    });
    let response_id = base["id"].clone();

    let stream = async_stream::stream! {
        // 闭包是同步 FnMut，不能 await/block，故用 unbounded 通道回传增量。
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<String, String>>();
        let chunk_tx = tx.clone();
        let router = state.chat_router.clone();
        let finished = Arc::new(AtomicBool::new(false));

        tokio::spawn({
            let guard = TaskGuard {
                tx: tx.clone(),
                finished: finished.clone(),
            };
            let finished = finished.clone();
            async move {
                // 任务异常退出（panic）时由它兜底报错，别让流无声结束成「空回答」
                let _guard = guard;
                let result = router
                    .chat_completions_stream(
                        request,
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
        // 出站统计：这一轮客户端到底拿到了什么
        let mut frames = 0usize;
        let mut content_chars = 0usize;
        let mut reasoning_chars = 0usize;
        let mut tool_calls = 0usize;
        // 正文全文：渠道没回传 usage 时用它估算输出用量
        let mut completion_text = String::new();
        while let Some(item) = rx.recv().await {
            let delta = match item {
                Ok(delta) => delta,
                Err(error) => {
                    warn!("出站中止 request_id={} frames={} err={}", response_id, frames, error);
                    let event = serde_json::json!({ "error": { "message": error, "type": "server_error" } });
                    yield Ok::<_, Infallible>(Event::default().data(serde_json::to_string(&event).unwrap()));
                    // 这一轮还没有任何正文与工具调用：忽略 `error` 的客户端（VS Code 内置对话即如此）
                    // 只会看到「空响应」；补一条正文说明失败原因并正常收尾。
                    if content_chars == 0 && tool_calls == 0 {
                        let frame = serde_json::json!({
                            "id": base["id"],
                            "object": base["object"],
                            "created": base["created"],
                            "model": base["model"],
                            "choices": [{
                                "index": 0,
                                "delta": { "content": format!("网关错误：{}", error) },
                                "finish_reason": null,
                            }],
                        });
                        yield Ok::<_, Infallible>(Event::default().data(serde_json::to_string(&frame).unwrap()));
                        let end = serde_json::json!({
                            "id": base["id"],
                            "object": base["object"],
                            "created": base["created"],
                            "model": base["model"],
                            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
                        });
                        yield Ok::<_, Infallible>(Event::default().data(serde_json::to_string(&end).unwrap()));
                    }
                    yield Ok::<_, Infallible>(Event::default().data("[DONE]"));
                    return;
                }
            };
            let chunk = parse_delta(&delta);

            let mut delta_obj = serde_json::json!({});
            if !chunk.content.is_empty() {
                content_chars += chunk.content.chars().count();
                completion_text.push_str(&chunk.content);
                delta_obj["content"] = serde_json::json!(chunk.content);
            }
            if !chunk.reasoning.is_empty() {
                reasoning_chars += chunk.reasoning.chars().count();
                delta_obj["reasoning_content"] = serde_json::json!(chunk.reasoning);
            }
            if let Some(calls) = chunk.functions {
                tool_calls += calls.as_array().map_or(0, Vec::len);
                delta_obj["tool_calls"] = index_tool_calls(calls);
                seen_tool_calls = true;
            }

            frames += 1;
            let frame = serde_json::json!({
                "id": base["id"],
                "object": base["object"],
                "created": base["created"],
                "model": base["model"],
                "choices": [{ "index": 0, "delta": delta_obj, "finish_reason": null }],
            });
            yield Ok::<_, Infallible>(Event::default().data(serde_json::to_string(&frame).unwrap()));
        }

        let finish_reason = if seen_tool_calls { "tool_calls" } else { "stop" };
        // 出站摘要：正文 / 思考 / 工具调用各有多少、结束原因是什么。
        // 排查「客户端拿到回答却不接着走」时，看这一行就知道它收到了什么，不必复现。
        info!(
            "出站摘要 request_id={} frames={} content_chars={} reasoning_chars={} tool_calls={} finish_reason={}",
            response_id, frames, content_chars, reasoning_chars, tool_calls, finish_reason
        );
        let end = serde_json::json!({
            "id": base["id"],
            "object": base["object"],
            "created": base["created"],
            "model": base["model"],
            "choices": [{ "index": 0, "delta": {}, "finish_reason": finish_reason }],
        });
        yield Ok::<_, Infallible>(Event::default().data(serde_json::to_string(&end).unwrap()));
        // OpenAI 约定：`stream_options.include_usage` 时，结束帧之后再补一个只带 usage 的块，
        // 客户端按它统计本轮消耗。渠道没有回传真实用量时按正文估算。
        if include_usage {
            // 输出用量按本轮实际返回的正文统计，口径与 prompt 一致。
            let completion_tokens = crate::tokenizer::count(&completion_text);
            let usage_frame = serde_json::json!({
                "id": base["id"],
                "object": base["object"],
                "created": base["created"],
                "model": base["model"],
                "choices": [],
                "usage": {
                    "prompt_tokens": prompt_tokens,
                    "completion_tokens": completion_tokens,
                    "total_tokens": prompt_tokens + completion_tokens,
                },
            });
            yield Ok::<_, Infallible>(
                Event::default().data(serde_json::to_string(&usage_frame).unwrap()),
            );
        }
        yield Ok::<_, Infallible>(Event::default().data("[DONE]"));
    };

    (StatusCode::OK, Sse::new(stream)).into_response()
}


/// 本格式的路由：`POST /v1/chat/completions`。
pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/v1/chat/completions", post(chat_completions_handler))
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
                    map.entry("index".to_string())
                        .or_insert_with(|| serde_json::json!(index));
                    Value::Object(map)
                }
                other => other,
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::index_tool_calls;
    use serde_json::json;

    /// 非流式消息：有思考时带 `reasoning_content`，没有时该字段不出现。
    #[test]
    fn message_serializes_reasoning_only_when_present() {
        let with_reasoning = super::Message {
            role: "assistant".to_string(),
            content: Some("答".to_string()),
            reasoning_content: Some("想".to_string()),
            tool_calls: None,
        };
        let value = serde_json::to_value(&with_reasoning).unwrap();
        assert_eq!(value["reasoning_content"], "想");
        assert_eq!(value["content"], "答");
        assert!(value.get("tool_calls").is_none(), "空字段不出现");

        let without = super::Message {
            role: "assistant".to_string(),
            content: Some("答".to_string()),
            reasoning_content: None,
            tool_calls: None,
        };
        let value = serde_json::to_value(&without).unwrap();
        assert!(value.get("reasoning_content").is_none(), "{}", value);
    }

    /// 流式增量必须带 index，客户端按它合并分片；已有 index 时不覆盖。
    #[test]
    fn tool_call_deltas_get_index() {
        let indexed = index_tool_calls(json!([
            { "id": "call_a", "type": "function", "function": { "name": "a", "arguments": "{}" } },
            { "index": 7, "id": "call_b", "type": "function", "function": { "name": "b", "arguments": "{}" } },
        ]));
        assert_eq!(indexed[0]["index"], 0);
        assert_eq!(indexed[0]["function"]["name"], "a");
        assert_eq!(indexed[1]["index"], 7);

        // 非数组原样返回
        assert_eq!(index_tool_calls(json!("plain")), json!("plain"));
    }
}
