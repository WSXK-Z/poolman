use std::{ sync::Arc};

use axum::{middleware, Router};
use tower_http::cors::{Any, CorsLayer};
use crate::{apis::base::AppState, config::ServerConfig};

// API 层：按 API 类型组织，每种类型下按接口格式分文件
pub mod base;
pub mod openai;

/// 入口层当前已实现的 API 格式（供健康检查展示）。
pub fn supported_formats() -> Vec<&'static str> {
    vec!["openai"]
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
    // 推理接口的路由层（后者最外层，先执行）：API Key 鉴权；健康检查与模型列表不经过它。
    let inference = Router::new()
        .merge(crate::apis::openai::routes())
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            crate::apis::base::require_api_key,
        ));
    Router::new()
        .merge(crate::apis::base::routes())
        .merge(inference)
        .layer(cors)
        .with_state(state)
}
