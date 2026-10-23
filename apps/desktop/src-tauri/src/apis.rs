use std::{ sync::Arc};

use axum::{extract::DefaultBodyLimit, middleware, Router};
use tower_http::cors::{Any, CorsLayer};
use crate::{apis::base::AppState, config::ServerConfig};

// API 层：按 API 类型组织，每种类型下按接口格式分文件
pub mod base;
pub mod deepseek;
pub mod openai;

/// 入口层当前已实现的 API 格式（供健康检查展示）。
pub fn supported_formats() -> Vec<&'static str> {
    vec!["openai", "deepseek"]
}
/// 按配置构造 CORS 层。
fn cors_layer(config: &ServerConfig) -> CorsLayer {
    let layer = CorsLayer::new().allow_methods(Any).allow_headers(Any);
    if config.cors_origin.trim() == "*" {
        return layer.allow_origin(Any);
    }
    match config.cors_origin.parse::<axum::http::HeaderValue>() {
        Ok(origin) => layer.allow_origin(origin),
        Err(_) => layer.allow_origin(Any),
    }
}

/// 装配完整应用：横切关注点 + 各 API 格式路由。
pub fn create_app(state: Arc<AppState>) -> Router {
    let cors = cors_layer(&state.config);
    // 请求体上限：axum 默认 2 MiB，长对话（整段历史 + 工具结果）与多模态请求很容易超过，
    // 客户端表现为 413 `length limit exceeded`；按配置放宽（0 = 不限制）。
    let body_limit = match state.config.max_request_body_bytes {
        0 => DefaultBodyLimit::disable(),
        bytes => DefaultBodyLimit::max(bytes.min(usize::MAX as u64) as usize),
    };
    // 推理接口的路由层（后者最外层，先执行）：API Key 鉴权；健康检查与模型列表不经过它。
    let inference = Router::new()
        .merge(crate::apis::openai::routes())
        .merge(crate::apis::deepseek::routes())
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            crate::apis::base::require_api_key,
        ));
    Router::new()
        .merge(crate::apis::base::routes())
        .merge(inference)
        .layer(body_limit)
        .layer(cors)
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::create_app;
    use crate::apikey::ApiKeyStore;
    use crate::apis::base::AppState;
    use crate::chat_router::ChatRouter;
    use crate::config::{ServerConfig, DEFAULT_REQUEST_BODY_LIMIT_BYTES};
    use crate::extension::credentials::CredentialStore;
    use crate::extension::websocket::WebSocketBridge;
    use crate::providers::provider_control::ProviderControl;
    use crate::throttle::ChatFrequency;
    use crate::workflow::{WorkflowGraphStore, WorkflowRunStore, WorkflowRuntime};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tower::ServiceExt;

    /// 3 MiB 的合法对话请求体：超过 axum 默认的 2 MiB 上限，正是用户报 413 的形状。
    fn large_chat_body() -> String {
        let filler = "a".repeat(3 * 1024 * 1024);
        serde_json::json!({
            "model": "deepseek-web",
            "messages": [{ "role": "user", "content": filler }],
        })
        .to_string()
    }

    /// 以指定请求体上限装配应用，返回 (app, 有效 API Key)。
    async fn app_with_limit(max_request_body_bytes: u64) -> (axum::Router, String) {
        let db_path =
            std::env::temp_dir().join(format!("poolman-body-limit-{}.db", uuid::Uuid::new_v4()));
        let api_keys = Arc::new(
            ApiKeyStore::init(&db_path.to_string_lossy())
                .await
                .expect("初始化测试数据库失败"),
        );
        let key = api_keys
            .create("vscode-copilot", None)
            .await
            .expect("创建测试 Key 失败")
            .key;
        let chat_router = Arc::new(ChatRouter::new(
            Vec::new(),
            Arc::new(ProviderControl::new(HashMap::new())),
        ));
        let run_store = Arc::new(
            WorkflowRunStore::new(api_keys.db())
                .await
                .expect("初始化工作流运行表失败"),
        );
        let graph_store = Arc::new(WorkflowGraphStore::new(
            db_path.with_extension("workflow.json"),
        ));
        let workflow_runtime = Arc::new(WorkflowRuntime::new(
            chat_router.clone(),
            graph_store,
            run_store,
            1,
        ));
        let state = Arc::new(AppState {
            chat_router,
            workflow_runtime,
            bridge: Arc::new(WebSocketBridge::default()),
            credentials: Arc::new(CredentialStore::new()),
            config: ServerConfig {
                host: "127.0.0.1".to_string(),
                port: 0,
                ws_port: 0,
                log_level: "error".to_string(),
                request_timeout_ms: 5_000,
                stream_idle_timeout_ms: 5_000,
                max_request_body_bytes,
                cors_origin: "*".to_string(),
                ws_auth_token: None,
            },
            api_keys,
            chat_frequency: ChatFrequency::in_memory(1_000),
        });
        (create_app(state), key)
    }

    /// 发一条 3 MiB 的对话请求。
    async fn post_large_chat(app: axum::Router, key: &str) -> StatusCode {
        let request = Request::post("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {key}"))
            .body(Body::from(large_chat_body()))
            .expect("构造请求失败");
        app.oneshot(request).await.expect("应用应能应答").status()
    }

    /// 默认上限下 3 MiB 请求体不再被 axum 的 2 MiB 默认值拒绝（修复 413）。
    #[tokio::test]
    async fn accepts_bodies_above_axum_default_limit() {
        let (app, key) = app_with_limit(DEFAULT_REQUEST_BODY_LIMIT_BYTES).await;
        let status = post_large_chat(app, &key).await;
        assert_ne!(
            status,
            StatusCode::PAYLOAD_TOO_LARGE,
            "3 MiB 请求体不应被默认上限拒绝，实际 {status}"
        );
    }

    /// 超过配置上限时仍按配置返回 413。
    #[tokio::test]
    async fn rejects_bodies_above_configured_limit() {
        let (app, key) = app_with_limit(1024 * 1024).await;
        let status = post_large_chat(app, &key).await;
        assert_eq!(
            status,
            StatusCode::PAYLOAD_TOO_LARGE,
            "超过 1 MiB 配置上限应返回 413"
        );
    }
}
