//! OpenAI 文本补全（`/v1/completions`，legacy）。
//!
//! 调用方身份由鉴权中间件校验并注入；`prompt`（字符串或字符串数组）被折叠为
//! 一条用户消息后交给模型分派入口，非流式返回 JSON，流式经 SSE 逐条输出文本增量。

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

use crate::apis::base::{error_response, parse_delta, ApiKeyAuth, AppState};
use crate::providers::InferRequest;

#[derive(Debug, Deserialize)]
pub struct CompletionRequest {
    pub model: String,
    /// 字符串或字符串数组。
    pub prompt: Value,
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
}

/// 文本补全 handler：stream 决定 JSON 或 SSE 响应。调用方身份由鉴权中间件注入。
pub async fn completions_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<ApiKeyAuth>,
    Json(req): Json<CompletionRequest>,
) -> impl IntoResponse {
    let prompt = prompt_to_text(&req.prompt);
    if prompt.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "prompt 不能为空", "invalid_request_error");
    }

    let model = req.model.clone();
    let request = InferRequest {
        model: model.clone(),
        messages: vec![serde_json::json!({ "role": "user", "content": prompt })],
        stream: req.stream,
        temperature: req.temperature,
        top_p: req.top_p,
        max_tokens: req.max_tokens,
        tools: None,
        tool_choice: None,
        session_key: req.session_id.clone(),
        api_key: Some(auth.key),
    };

    if req.stream {
        return sse_response(state, request, model);
    }

    match state.chat_router.chat_completions(request).await {
        Ok(result) => {
            let response = serde_json::json!({
                "id": format!("cmpl-{}", chrono::Utc::now().timestamp_millis()),
                "object": "text_completion",
                "created": chrono::Utc::now().timestamp(),
                "model": model,
                "choices": [{
                    "text": result.text,
                    "index": 0,
                    "logprobs": Value::Null,
                    "finish_reason": result.finish_reason.unwrap_or_else(|| "stop".to_string()),
                }],
                "usage": result.usage,
            });
            (StatusCode::OK, AxumJson(response)).into_response()
        }
        Err(e) => {
            warn!("文本补全请求出错: {}", e);
            error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string(), "server_error")
        }
    }
}

/// 流式文本补全：SSE 逐条输出文本增量。
fn sse_response(state: Arc<AppState>, request: InferRequest, model: String) -> axum::response::Response {
    let id = format!("cmpl-{}", chrono::Utc::now().timestamp_millis());
    let created = chrono::Utc::now().timestamp();

    let stream = async_stream::stream! {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<String, String>>();
        let chunk_tx = tx.clone();
        let router = state.chat_router.clone();

        tokio::spawn(async move {
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
        });

        while let Some(item) = rx.recv().await {
            let delta = match item {
                Ok(delta) => delta,
                Err(error) => {
                    let event = serde_json::json!({ "error": { "message": error, "type": "server_error" } });
                    yield Ok::<_, Infallible>(Event::default().data(serde_json::to_string(&event).unwrap()));
                    return;
                }
            };
            let text = parse_delta(&delta).content;
            if text.is_empty() {
                continue;
            }
            let frame = serde_json::json!({
                "id": id,
                "object": "text_completion",
                "created": created,
                "model": model,
                "choices": [{ "text": text, "index": 0, "logprobs": Value::Null, "finish_reason": Value::Null }],
            });
            yield Ok::<_, Infallible>(Event::default().data(serde_json::to_string(&frame).unwrap()));
        }

        let end = serde_json::json!({
            "id": id,
            "object": "text_completion",
            "created": created,
            "model": model,
            "choices": [{ "text": "", "index": 0, "logprobs": Value::Null, "finish_reason": "stop" }],
        });
        yield Ok::<_, Infallible>(Event::default().data(serde_json::to_string(&end).unwrap()));
        yield Ok::<_, Infallible>(Event::default().data("[DONE]"));
    };

    (StatusCode::OK, Sse::new(stream)).into_response()
}

/// `prompt` 折叠为纯文本：字符串直接用；数组按顺序拼接。
fn prompt_to_text(prompt: &Value) -> String {
    match prompt {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// 本格式的路由：`POST /v1/completions`。
pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/v1/completions", post(completions_handler))
}

#[cfg(test)]
mod tests {
    use super::prompt_to_text;
    use serde_json::json;

    #[test]
    fn prompt_folds_string_and_array() {
        assert_eq!(prompt_to_text(&json!("x")), "x");
        assert_eq!(prompt_to_text(&json!(["a", "b"])), "a\nb");
        assert_eq!(prompt_to_text(&json!(3)), "");
    }
}
