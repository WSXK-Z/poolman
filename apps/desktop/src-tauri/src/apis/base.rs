//! API 层的共享装配：应用状态、鉴权中间件、通用响应助手与基础路由。
//!
//! 各 API 格式（如 `openai` 的三种接口形式）只提供自己的 handler 与 `routes()`；
//! 鉴权、健康检查、模型列表等横切关注点由本模块提供，CORS 与总装配见 [`crate::apis`]。
//! 需要鉴权的路由用 [`require_api_key`] 做 `route_layer`，handler 再经 [`ApiKeyAuth`] 扩展取身份。

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json as AxumJson, Response},
    routing::get,
    Router,
};
use serde_json::Value;
use tracing::warn;

use crate::apikey::ApiKeyStore;
use crate::chat_router::ChatRouter;
use crate::config::ServerConfig;
use crate::extension::credentials::CredentialStore;
use crate::extension::websocket::WebSocketBridge;

/// 入口层共享状态：各 API 格式的 handler 都从这里取依赖。
pub struct AppState {
    /// 模型分派入口（按模型选供应商）。
    pub chat_router: Arc<ChatRouter>,
    /// 插件连接（健康检查/凭证同步等）。
    pub bridge: Arc<WebSocketBridge>,
    /// 供应商凭证库。
    pub credentials: Arc<CredentialStore>,
    /// 运行配置。
    pub config: ServerConfig,
    /// 外部接入凭据。
    pub api_keys: Arc<ApiKeyStore>,
    /// 请求落盘工具配置（开发期链路排查）。
    pub dump: Arc<crate::dump::DumpConfig>,
}

/// 调用方身份：由鉴权中间件解析后注入请求扩展，handler 从扩展读取。
#[derive(Debug, Clone)]
pub struct ApiKeyAuth {
    /// 通过校验的 API Key（用于会话归属）。
    pub key: String,
    /// Key 绑定的平台标识（决定用哪个提示词转换器）。
    #[allow(dead_code)]
    pub platform: String,
}

/// API Key 鉴权中间件：解析请求头里的 Key，有效则把 [`ApiKeyAuth`] 注入请求扩展后放行，
/// 否则直接返回 401。
pub async fn require_api_key(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    match resolve_api_key_platform(request.headers(), &state.api_keys).await {
        Ok((key, platform)) => {
            request.extensions_mut().insert(ApiKeyAuth { key, platform });
            next.run(request).await
        }
        Err(status) => error_response(status, "无效或缺失的 API Key", "authentication_error"),
    }
}

/// 从请求头解析 API Key → 校验并返回 `(key, 绑定平台)`。
/// 支持多种携带方式：`Authorization: Bearer` / `x-api-key` / `api-key`。
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
    match store.resolve(key).await {
        Ok(Some(platform)) => Ok((key.to_string(), platform)),
        _ => {
            warn!(
                "API Key 鉴权失败：该 Key 不在库里（{}），请在桌面端「API Key」页确认发放值",
                mask_key(key)
            );
            Err(StatusCode::UNAUTHORIZED)
        }
    }
}

/// 打码便于日志（仅显示前缀，安全截断避免多字节字符字节切片 panic）。
/// 打码便于日志：合法 Key 形如 `sk-` + 32 位（35 字符），因此长值只显示前缀；
/// 明显不是 Key 的短值原样显示——它是配置错误，藏起来会让排查无从下手。两种情况都带上长度。
pub fn mask_key(key: &str) -> String {
    let len = key.chars().count();
    if len <= 12 {
        return format!("{:?}（仅 {} 字符，不是网关发放的 Key）", key, len);
    }
    let prefix: String = key.chars().take(8).collect();
    format!("{}…（{} 字符）", prefix, len)
}

/// 统一错误响应（OpenAI 风格 `error` 对象）。
pub fn error_response(status: StatusCode, message: &str, kind: &str) -> axum::response::Response {
    (
        status,
        AxumJson(serde_json::json!({ "error": { "message": message, "type": kind } })),
    )
        .into_response()
}

/// 一条流式增量解析结果。
///
/// 三个键（[`DeltaChunk::RESPONSE`] / [`DeltaChunk::THINK`] / [`DeltaChunk::FUNCTIONS`]）是**渠道无关**的
/// 中间增量协议：渠道侧按这套键封装增量，本层用 [`parse_delta`] 解析，两边只认这里的一份定义。
#[derive(Debug, Default)]
pub struct DeltaChunk {
    /// 正文增量。
    pub content: String,
    /// 思考增量。
    pub reasoning: String,
    /// 工具调用（OpenAI 标准 tool_calls 数组）。
    pub functions: Option<Value>,
}

impl DeltaChunk {
    /// 正文增量键。
    pub const RESPONSE: &str = "RESPONSE";

    /// 思考增量键。
    pub const THINK: &str = "THINK";

    /// 工具调用增量键（值须为数组）。
    pub const FUNCTIONS: &str = "FUNCTIONS";
}

/// 解析底层流式增量：兼容 JSON 包裹（`{"RESPONSE"/"THINK"/"FUNCTIONS"}`）与纯文本。
pub fn parse_delta(raw: &str) -> DeltaChunk {
    let mut chunk = DeltaChunk::default();
    match serde_json::from_str::<Value>(raw) {
        Ok(value) => {
            if let Some(text) = value.get(DeltaChunk::FUNCTIONS).filter(|v| v.is_array()) {
                chunk.functions = Some(text.clone());
            }
            if let Some(text) = value.get(DeltaChunk::RESPONSE).and_then(Value::as_str) {
                chunk.content.push_str(text);
            }
            if let Some(text) = value.get(DeltaChunk::THINK).and_then(Value::as_str) {
                chunk.reasoning.push_str(text);
            }
        }
        Err(_) => chunk.content.push_str(raw),
    }
    chunk
}

/// 把 OpenAI 多模态 content 数组规整为纯文本（下游只接受字符串）。
pub fn normalize_messages(messages: Vec<Value>) -> Vec<Value> {
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
                        Value::Null
                    } else {
                        Value::String(joined)
                    };
                }
            }
            m
        })
        .collect()
}

/// 健康检查：进程状态、插件连接、凭证摘要与已实现格式。
pub async fn health_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let connected = state.bridge.connected.read().await;
    AxumJson(serde_json::json!({
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
        "api_formats": crate::apis::supported_formats(),
        "timestamp": chrono::Utc::now().to_rfc3339(),
    }))
}

/// 模型列表：由供应商注册表提供，已按启停过滤。
pub async fn models_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let models: Vec<_> = state
        .chat_router
        .list_models()
        .into_iter()
        .map(|model| {
            serde_json::json!({
                "id": model.id,
                "object": "model",
                "owned_by": model.owned_by,
                "provider": model.provider,
            })
        })
        .collect();
    AxumJson(serde_json::json!({ "object": "list", "data": models }))
}

/// 装配基础路由：健康检查、模型列表。
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/health", get(health_handler))
        .route("/v1/models", get(models_handler))
}

#[cfg(test)]
mod tests {
    use super::mask_key;

    /// 短值（明显不是网关发放的 Key）原样显示并标注长度；长值只露前缀。
    #[test]
    fn mask_key_reveals_only_short_non_keys() {
        // 真实发放形状：sk- + 32 位 = 35 字符 → 只露前缀
        let real = format!("sk-{}", "a".repeat(32));
        let masked = mask_key(&real);
        assert!(masked.starts_with("sk-aaaaa"), "{}", masked);
        assert!(
            !masked.contains(&"a".repeat(20)),
            "不能露出完整 Key：{}",
            masked
        );
        assert!(masked.contains("35 字符"), "应带长度：{}", masked);

        // 配置错误（粘成掩码或占位符）→ 原样显示，否则无法排查
        assert_eq!(mask_key("***"), "\"***\"（仅 3 字符，不是网关发放的 Key）");
        assert_eq!(
            mask_key("sk-short"),
            "\"sk-short\"（仅 8 字符，不是网关发放的 Key）"
        );
        // 空串与多字节：按字符计数，不应 panic
        assert!(mask_key("").contains("0 字符"));
        assert!(mask_key("密钥密钥").contains("4 字符"));
    }
}