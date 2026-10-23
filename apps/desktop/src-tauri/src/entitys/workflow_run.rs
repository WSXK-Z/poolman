use sea_orm::entity::prelude::*;

/// 工作流运行记录：一次客户端请求对应一条。
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "workflow_runs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub request_id: Option<String>,
    pub workflow_version: String,
    /// 本次运行使用的图快照，避免运行中改图影响回放。
    pub graph_json: String,
    pub status: String,
    pub protocol: String,
    pub model: String,
    pub channel: Option<String>,
    pub stream: bool,
    pub local_session_id: Option<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub duration_ms: Option<i64>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub error: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
