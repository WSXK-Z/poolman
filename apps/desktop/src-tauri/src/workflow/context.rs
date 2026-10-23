//! 工作流单个节点的输入输出上下文。

use std::sync::Arc;

use anyhow::Result;

use crate::chat_router::{ChatRouter, PendingRecord, RouteDecision};
use crate::local_session::SessionPlan;
use crate::models::defs::{ChunkSink, InferRequest, ModelResponse};
use crate::request_record::RequestMeta;
use crate::title_engine::TitleSettings;

pub struct WorkflowContext {
    pub router: Arc<ChatRouter>,
    /// 标题生成设置（来源策略与本地模型目录）。
    pub title: Arc<TitleSettings>,
    pub request: InferRequest,
    pub meta: RequestMeta,
    pub run_id: String,
    pub title_request: bool,
    pub session_plan: Option<SessionPlan>,
    pub pending_record: PendingRecord,
    pub route: Option<Result<RouteDecision>>,
    pub result: Option<Result<ModelResponse>>,
    pub client_sink: Option<ChunkSink>,
    pub request_id: Option<String>,
    pub channel: Option<String>,
    pub local_session_id: Option<String>,
}
