//! OpenAI 对话补全（`/v1/chat/completions`）。
//!
//! 调用方身份由鉴权中间件校验并注入；消息规整后交给模型分派入口，
//! 非流式返回 JSON，流式经 SSE 逐条输出正文 / 思考增量与终止状态。

use std::convert::Infallible;
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
use tracing::warn;

use crate::apis::base::{
    error_response, normalize_messages, parse_delta, ApiKeyAuth, AppState,
};
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
    Json(req): Json<ChatCompletionRequest>,
) -> impl IntoResponse {
    if req.messages.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "消息数组不能为空", "invalid_request_error");
    }

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
        return sse_response(state, request, model);
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
            error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string(), "server_error")
        }
    }
}

/// 流式对话补全：SSE 逐条输出增量。
fn sse_response(state: Arc<AppState>, request: InferRequest, model: String) -> axum::response::Response {
    let base = serde_json::json!({
        "id": format!("chatcmpl-{}", chrono::Utc::now().timestamp_millis()),
        "object": "chat.completion.chunk",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
    });

    let stream = async_stream::stream! {
        // 闭包是同步 FnMut，不能 await/block，故用 unbounded 通道回传增量。
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

        let mut seen_tool_calls = false;
        while let Some(item) = rx.recv().await {
            let delta = match item {
                Ok(delta) => delta,
                Err(error) => {
                    let event = serde_json::json!({ "error": { "message": error, "type": "server_error" } });
                    yield Ok::<_, Infallible>(Event::default().data(serde_json::to_string(&event).unwrap()));
                    return;
                }
            };
            let chunk = parse_delta(&delta);

            let mut delta_obj = serde_json::json!({});
            if !chunk.content.is_empty() {
                delta_obj["content"] = serde_json::json!(chunk.content);
            }
            if !chunk.reasoning.is_empty() {
                delta_obj["reasoning_content"] = serde_json::json!(chunk.reasoning);
            }
            if let Some(tool_calls) = chunk.tool_calls {
                delta_obj["tool_calls"] = index_tool_calls(tool_calls);
                seen_tool_calls = true;
            }

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
        let end = serde_json::json!({
            "id": base["id"],
            "object": base["object"],
            "created": base["created"],
            "model": base["model"],
            "choices": [{ "index": 0, "delta": {}, "finish_reason": finish_reason }],
        });
        yield Ok::<_, Infallible>(Event::default().data(serde_json::to_string(&end).unwrap()));
        yield Ok::<_, Infallible>(Event::default().data("[DONE]"));
    };

    (StatusCode::OK, Sse::new(stream)).into_response()
}

/// 本格式的路由：`POST /v1/chat/completions`。
pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/v1/chat/completions", post(chat_completions_handler))
}

/// 给工具调用补上流式增量必需的 `index`（客户端按它合并分片），已有的不覆盖。
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
