//! DeepSeek 官方接口格式：按接口分子模块。
//!
//! 与 [`crate::apis::openai`] 并列，只负责**对外字段格式**对齐 DeepSeek 官方文档；
//! 模型名沿用网关既有的渠道命名（`deepseek-web` 等），不做官方模型名映射。
//! 状态、鉴权与 CORS 等横切关注点由 [`crate::apis::base`] 装配。

pub mod chat_completions;
pub mod models;

use std::sync::Arc;

use axum::Router;

use crate::apis::base::AppState;

/// 合并 DeepSeek 各接口格式的路由（`Router<Arc<AppState>>`，状态由总装配注入）。
pub fn routes() -> Router<Arc<AppState>> {
    chat_completions::routes().merge(models::routes())
}