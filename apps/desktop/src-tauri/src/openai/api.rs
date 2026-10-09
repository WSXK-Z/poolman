use axum::{
    extract::{Json, State},
    http::{HeaderMap, StatusCode},
    response::{sse::Event, IntoResponse, Json as AxumJson, Sse},
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tracing::warn;

use super::chat_router::ChatRouter;
use crate::apikey::ApiKeyStore;
use crate::config::ServerConfig;
use crate::extension::credentials::CredentialStore;
use crate::extension::websocket::WebSocketBridge;

#[derive(Debug, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<serde_json::Value>,
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
    pub tools: Option<serde_json::Value>,
    #[serde(default)]
    pub tool_choice: Option<serde_json::Value>,
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

pub struct AppState {
    pub chat_router: Arc<ChatRouter>,
    pub bridge: Arc<WebSocketBridge>,
    pub credentials: Arc<CredentialStore>,
    pub config: ServerConfig,
    pub api_keys: Arc<ApiKeyStore>,
}

/// 从请求头解析 API Key → 校验并返回 (key, 绑定平台)
/// 支持多种携带方式：Authorization: Bearer <key> / x-api-key / api-key
async fn resolve_api_key_platform(
    headers: &HeaderMap,
    store: &ApiKeyStore,
) -> Result<(String, String), StatusCode> {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let mut key = auth.strip_prefix("Bearer ").unwrap_or(auth).trim();
    if key.is_empty() {
        // 某些客户端（如 VS Code Copilot customendpoint）用 x-api-key / api-key
        key = headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .or_else(|| headers.get("api-key").and_then(|v| v.to_str().ok()))
            .unwrap_or("")
            .trim();
    }
    if key.is_empty() {
        warn!("API Key 鉴权失败：请求未携带 Authorization / x-api-key / api-key 头");
        return Err(StatusCode::UNAUTHORIZED);
    }
    let platform = match store.resolve(key).await {
        Ok(Some(p)) => p,
        _ => {
            warn!("API Key 鉴权失败：Key 无效（{}）", mask_key(key));
            return Err(StatusCode::UNAUTHORIZED);
        }
    };
    Ok((key.to_string(), platform))
}

/// 打码便于日志（仅显示前缀，安全截断避免多字节字符字节切片 panic）
fn mask_key(key: &str) -> String {
    if key.chars().count() <= 8 {
        "***".to_string()
    } else {
        let prefix: String = key.chars().take(8).collect();
        format!("{}…", prefix)
    }
}

pub async fn health_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let connected = state.bridge.connected.read().await;
    let status = serde_json::json!({
        "status": "ok",
        "uptime": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        "extension_connected": *connected,
        "config": serde_json::json!({
            "host": state.config.host,
            "port": state.config.port,
            "ws_port": state.config.ws_port,
            "request_timeout_ms": state.config.request_timeout_ms,
        }),
        "credentials": state.credentials.summary(),
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    AxumJson(status)
}

pub async fn models_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let models = vec![
        ("deepseek-chat", "deepseek-web"),
        ("deepseek-reasoner", "deepseek-web"),
        ("gpt-4", "chatgpt-web"),
        ("gpt-3.5-turbo", "chatgpt-web"),
        ("claude-3-opus", "claude-web"),
        ("claude-3-sonnet", "claude-web"),
        ("gemini-pro", "gemini-web"),
    ];
    let models: Vec<_> = models
        .into_iter()
        .filter(|(_, provider)| state.chat_router.is_provider_enabled(provider))
        .map(|(model, _)| serde_json::json!({ "id": model, "object": "model" }))
        .collect();
    AxumJson(serde_json::json!({ "object": "list", "data": models }))
}

/// 把 OpenAI 多模态 content 数组规整为纯文本（下游只接受字符串）
fn normalize_messages(messages: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    messages
        .into_iter()
        .map(|mut m| {
            if let Some(content) = m.get("content") {
                if let Some(arr) = content.as_array() {
                    let text: Vec<String> = arr
                        .iter()
                        .filter_map(|p| p.get("text").and_then(|t| t.as_str()).map(String::from))
                        .filter(|s| !s.is_empty())
                        .collect();
                    let joined = text.join("\n");
                    m["content"] = if joined.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::String(joined)
                    };
                }
            }
            m
        })
        .collect()
}

pub async fn chat_completions_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<ChatCompletionRequest>,
) -> impl IntoResponse {
    // 原始消息与转换后提示词在 deepseek.rs 转换处一并保存（同一文件）
    // API Key 鉴权 → (key, 绑定平台)：key 用于会话归属，平台决定转换器
    let (api_key, platform) = match resolve_api_key_platform(&headers, &state.api_keys).await {
        Ok(p) => p,
        Err(status) => {
            return (
                status,
                AxumJson(serde_json::json!({
                    "error": { "message": "无效或缺失的 API Key", "type": "authentication_error" }
                })),
            )
                .into_response();
        }
    };

    if req.messages.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            AxumJson(serde_json::json!({
                "error": { "message": "消息数组不能为空", "type": "invalid_request_error" }
            })),
        )
            .into_response();
    }

    let model = req.model.clone();
    let messages = normalize_messages(req.messages.clone());
    let options = serde_json::json!({
        "temperature": req.temperature,
        "topP": req.top_p,
        "maxTokens": req.max_tokens,
        "stream": req.stream,
        "sessionId": req.session_id,
        "tools": req.tools,
        "tool_choice": req.tool_choice,
        "converterPlatform": platform,
        "apiKey": api_key,
    });

    if req.stream {
        // 流式响应：on_chunk 收到 JSON 增量（{"THINK":..} / {"RESPONSE":..} / {"tool_calls":[...]}）或纯文本
        let state_clone = state.clone();
        let model_clone = model.clone();
        let messages_clone = messages.clone();
        let options_clone = options.clone();

        let base = serde_json::json!({
            "id": format!("chatcmpl-{}", chrono::Utc::now().timestamp_millis()),
            "object": "chat.completion.chunk",
            "created": chrono::Utc::now().timestamp(),
            "model": model_clone.clone(),
        });

        let stream = async_stream::stream! {
            // 用 unbounded 通道：闭包是同步 FnMut，不能 await/block（blocking_send 在运行时内会 panic）
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            let handler = state_clone.chat_router.clone();

            tokio::spawn(async move {
                let result = handler.handle_stream(&model_clone, messages_clone, Some(options_clone), move |delta| {
                    let _ = tx.send(delta);
                }).await;
                let _ = result;
            });

            let mut seen_tool_calls = false;
            while let Some(delta) = rx.recv().await {
                let mut content = String::new();
                let mut reasoning = String::new();
                let mut tool_calls: Option<serde_json::Value> = None;

                if let Ok(chunks) = serde_json::from_str::<serde_json::Value>(&delta) {
                    if let Some(tc) = chunks.get("tool_calls") {
                        if tc.is_array() {
                            tool_calls = Some(tc.clone());
                            seen_tool_calls = true;
                        }
                    }
                    if let Some(r) = chunks.get("RESPONSE").and_then(|v| v.as_str()) {
                        content.push_str(r);
                    }
                    if let Some(t) = chunks.get("THINK").and_then(|v| v.as_str()) {
                        reasoning.push_str(t);
                    }
                } else {
                    content.push_str(&delta);
                }

                let mut delta_obj = serde_json::json!({});
                if !content.is_empty() {
                    delta_obj["content"] = serde_json::json!(content);
                }
                if !reasoning.is_empty() {
                    delta_obj["reasoning_content"] = serde_json::json!(reasoning);
                }
                if let Some(tc) = tool_calls {
                    delta_obj["tool_calls"] = tc;
                }

                let chunk = serde_json::json!({
                    "id": base["id"],
                    "object": base["object"],
                    "created": base["created"],
                    "model": base["model"],
                    "choices": [{
                        "index": 0,
                        "delta": delta_obj,
                        "finish_reason": null,
                    }],
                });
                yield Ok::<_, Infallible>(Event::default().data(serde_json::to_string(&chunk).unwrap()));
            }

            let finish_reason = if seen_tool_calls { "tool_calls" } else { "stop" };
            let end = serde_json::json!({
                "id": base["id"],
                "object": base["object"],
                "created": base["created"],
                "model": base["model"],
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": finish_reason,
                }],
            });
            yield Ok::<_, Infallible>(Event::default().data(serde_json::to_string(&end).unwrap()));
            yield Ok::<_, Infallible>(Event::default().data("[DONE]"));
        };

        return (StatusCode::OK, Sse::new(stream)).into_response();
    }

    // 非流式
    match state
        .chat_router
        .handle(&model, messages, Some(options))
        .await
    {
        Ok(result) => {
            let content = result.content;
            let tool_calls = result.tool_calls;
            let usage = result.usage;
            let finished_reason = result.finished_reason;

            let message = Message {
                role: "assistant".to_string(),
                content: if tool_calls.is_some() {
                    None
                } else {
                    Some(content)
                },
                tool_calls: tool_calls.and_then(|tc| serde_json::from_value(tc).ok()),
            };

            let resp = ChatCompletionResponse {
                id: format!("chatcmpl-{}", chrono::Utc::now().timestamp_millis()),
                object: "chat.completion".to_string(),
                created: chrono::Utc::now().timestamp() as u64,
                model: req.model,
                choices: vec![Choice {
                    index: 0,
                    message,
                    finish_reason: finished_reason.or(Some("stop".to_string())),
                }],
                usage: usage.and_then(|u| serde_json::from_value(u).ok()),
            };

            (
                StatusCode::OK,
                AxumJson(serde_json::to_value(&resp).unwrap_or(serde_json::json!({}))),
            )
                .into_response()
        }
        Err(e) => {
            warn!("聊天请求出错: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                AxumJson(serde_json::json!({
                    "error": { "message": e.to_string(), "type": "server_error" }
                })),
            )
                .into_response()
        }
    }
}

pub fn create_app(
    chat_router: Arc<ChatRouter>,
    bridge: Arc<WebSocketBridge>,
    credentials: Arc<CredentialStore>,
    config: ServerConfig,
    api_keys: Arc<ApiKeyStore>,
) -> Router {
    let state = Arc::new(AppState {
        chat_router,
        bridge,
        credentials,
        config,
        api_keys,
    });

    Router::new()
        .route("/health", get(health_handler))
        .route("/v1/models", get(models_handler))
        .route("/v1/chat/completions", post(chat_completions_handler))
        .layer(CorsLayer::permissive())
        .with_state(state)
}
