//! OpenAI 兼容接口：按接口格式分子模块。
//!
//! 每种格式各自提供 handler 与 `routes()`（`chat_completions` 对话补全、
//! `completions` 文本补全、`responses` 响应接口），由本模块统一合并；
//! 状态与 CORS 等横切关注点由 [`crate::apis::base`] 装配。

pub mod chat_completions;
pub mod completions;
pub mod responses;

use std::sync::Arc;

use axum::Router;

use crate::apis::base::AppState;

/// 合并 OpenAI 各接口格式的路由（`Router<Arc<AppState>>`，状态由总装配注入）。
pub fn routes() -> Router<Arc<AppState>> {
    chat_completions::routes()
        .merge(completions::routes())
        .merge(responses::routes())
}
