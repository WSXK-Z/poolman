use sea_orm::entity::prelude::*;

/// 工作流节点运行记录：每个执行过的节点一行。
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "workflow_node_runs")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub run_id: String,
    pub node_id: String,
    pub node_kind: String,
    pub seq: i64,
    pub status: String,
    pub started_at: i64,
    pub finished_at: i64,
    pub duration_ms: i64,
    pub output_port: Option<String>,
    pub error: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
