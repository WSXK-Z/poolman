//! OpenAI 响应接口（`/v1/responses`）。
//!
//! 调用方身份由鉴权中间件校验并注入；`input`（字符串或 item 数组）与 `instructions`
//! 被规整为消息后交给模型分派入口，非流式返回 JSON，流式经 SSE 输出命名事件
//! （`response.created` / `response.output_text.delta` / `response.completed`）。

use std::convert::Infallible;
use std::sync::Arc;

use axum::{
    extract::{Extension, Json, State},
    http::StatusCode,
    response::{sse::Event, IntoResponse, Json as AxumJson, Sse},
    routing::post,
    Router,
};
use serde::Deserialize;
use serde_json::Value;
use tracing::warn;

use crate::apis::base::{error_response, parse_delta, request_meta, ApiKeyAuth, AppState};
use crate::providers::InferRequest;
use crate::request_record::RequestMeta;

#[derive(Debug, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    /// 字符串或 item 数组。
    pub input: Value,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub session_id: Option<String>,
}

/// 响应接口 handler：stream 决定 JSON 或 SSE 响应。调用方身份由鉴权中间件注入。
pub async fn responses_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ApiKeyAuth>,
    headers: axum::http::HeaderMap,
    Json(req): Json<ResponsesRequest>,
) -> impl IntoResponse {
    let messages = input_to_messages(&req.input, req.instructions.as_deref());
    if messages.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "input 不能为空", "invalid_request_error");
    }

    let model = req.model.clone();
    let meta = request_meta("openai", "/v1/responses", &headers);
    let request = InferRequest {
        model: model.clone(),
        messages,
        stream: req.stream,
        temperature: req.temperature,
        top_p: req.top_p,
        max_tokens: req.max_output_tokens,
        tools: None,
        tool_choice: None,
        session_key: req.session_id.clone(),
        web_context_key: None,
        api_key: Some(auth.key),
    };

    if req.stream {
        return sse_response(state, request, meta, model);
    }

    match state.workflow_runtime.chat_completions(request, meta).await {
        Ok(result) => {
            let response = response_object(&model, &result.text, result.usage.as_ref());
            (StatusCode::OK, AxumJson(response)).into_response()
        }
        Err(e) => {
            warn!("响应请求出错: {}", e);
            error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string(), "server_error")
        }
    }
}

/// 构造 Response 对象（非流式，或作为流式完成事件的内容）。
fn response_object(model: &str, text: &str, usage: Option<&Value>) -> Value {
    let usage = usage_object(usage, text);
    serde_json::json!({
        "id": format!("resp_{}", uuid::Uuid::new_v4().simple()),
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "status": "completed",
        "model": model,
        "output": [{
            "id": format!("msg_{}", uuid::Uuid::new_v4().simple()),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text, "annotations": [] }],
        }],
        "usage": usage,
    })
}

/// 用量对象：优先用上游提供的 total 字段，缺失时按文本长度粗估输出。
fn usage_object(usage: Option<&Value>, text: &str) -> Value {
    let input = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| u64::from(crate::tokenizer::count(text)));
    let total = usage
        .and_then(|u| u.get("total_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(input + output);
    serde_json::json!({
        "input_tokens": input,
        "output_tokens": output,
        "total_tokens": total,
    })
}

/// 流式响应接口：输出命名事件。
fn sse_response(
    state: Arc<AppState>,
    request: InferRequest,
    meta: RequestMeta,
    model: String,
) -> axum::response::Response {
    // 用量口径：网页渠道不回传本轮用量，用官方分词器按本轮实际发送的 prompt
    // 计数（含每条消息的角色标记开销），与上游分词结果一致。
    let prompt_tokens = u64::from(crate::tokenizer::count_messages(
        request
            .messages
            .iter()
            .filter_map(|message| message.get("content").and_then(Value::as_str)),
    ));
    let stream = async_stream::stream! {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<String, String>>();
        let chunk_tx = tx.clone();
        let router = state.workflow_runtime.clone();

        tokio::spawn(async move {
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
        });

        let created = response_object(&model, "", None);
        yield Ok::<_, Infallible>(
            Event::default()
                .event("response.created")
                .data(serde_json::json!({ "type": "response.created", "response": created }).to_string()),
        );

        let mut text = String::new();
        while let Some(item) = rx.recv().await {
            let delta = match item {
                Ok(delta) => delta,
                Err(error) => {
                    yield Ok::<_, Infallible>(
                        Event::default()
                            .event("response.failed")
                            .data(serde_json::json!({ "type": "response.failed", "error": { "message": error } }).to_string()),
                    );
                    return;
                }
            };
            let chunk = parse_delta(&delta).content;
            if chunk.is_empty() {
                continue;
            }
            text.push_str(&chunk);
            yield Ok::<_, Infallible>(
                Event::default()
                    .event("response.output_text.delta")
                    .data(serde_json::json!({ "type": "response.output_text.delta", "delta": chunk }).to_string()),
            );
        }

        // 收尾事件必须带上本轮用量：客户端读 `response.completed` 里的 usage 统计消耗，
        // 这里传 None 会让前端一直显示 0。
        let usage = serde_json::json!({
            "prompt_tokens": prompt_tokens,
            "completion_tokens": u64::from(crate::tokenizer::count(&text)),
        });
        let completed = response_object(&model, &text, Some(&usage));
        yield Ok::<_, Infallible>(
            Event::default()
                .event("response.completed")
                .data(serde_json::json!({ "type": "response.completed", "response": completed }).to_string()),
        );
    };

    (StatusCode::OK, Sse::new(stream)).into_response()
}

/// `input` + `instructions` 规整为消息数组。
fn input_to_messages(input: &Value, instructions: Option<&str>) -> Vec<Value> {
    let mut messages = Vec::new();
    if let Some(instructions) = instructions.map(str::trim).filter(|s| !s.is_empty()) {
        messages.push(serde_json::json!({ "role": "system", "content": instructions }));
    }
    match input {
        Value::String(text) => {
            if !text.is_empty() {
                messages.push(serde_json::json!({ "role": "user", "content": text }));
            }
        }
        Value::Array(items) => {
            for item in items {
                let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                let text = content_to_text(item.get("content"));
                if !text.is_empty() {
                    messages.push(serde_json::json!({ "role": role, "content": text }));
                }
            }
        }
        _ => {}
    }
    messages
}

/// 提取 content 文本：字符串直接用；数组取各 part 的 `text`。
fn content_to_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// 本格式的路由：`POST /v1/responses`。
pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/v1/responses", post(responses_handler))
}

#[cfg(test)]
mod tests {
    use super::{content_to_text, input_to_messages};
    use serde_json::json;

    #[test]
    fn string_input_becomes_user_message() {
        let messages = input_to_messages(&json!("hi"), None);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "hi");
    }

    #[test]
    fn instructions_prepend_system_message() {
        let messages = input_to_messages(&json!("hi"), Some("be terse"));
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
    }

    #[test]
    fn item_array_maps_roles_and_text_parts() {
        let input = json!([
            { "role": "user", "content": [{ "type": "input_text", "text": "hello" }] },
            { "role": "assistant", "content": "world" }
        ]);
        let messages = input_to_messages(&input, None);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["content"], "hello");
        assert_eq!(messages[1]["content"], "world");
    }

    #[test]
    fn content_text_extraction() {
        assert_eq!(content_to_text(Some(&json!("a"))), "a");
        assert_eq!(
            content_to_text(Some(&json!([{ "text": "a" }, { "text": "b" }]))),
            "a\nb"
        );
        assert_eq!(content_to_text(None), "");
    }
}
