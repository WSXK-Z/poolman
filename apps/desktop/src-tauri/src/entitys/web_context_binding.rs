use sea_orm::entity::prelude::*;

/// 网页渠道会话绑定：本地会话 id ↔ 远端网页会话 id。
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "web_context_bindings")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// 渠道标识。
    pub channel: String,
    /// 本地会话 id。
    pub context_key: String,
    /// 远端网页会话 id。
    pub remote_session_id: String,
    /// 远端消息 id（DeepSeek 的 parent_message_id）。
    pub parent_message_id: Option<i64>,
    /// 是否已完成 bootstrap。
    pub primed: bool,
    pub system_hash: String,
    pub tools_hash: String,
    pub approx_tokens: i64,
    /// 旧会话摘要（迁移用）。
    pub summary: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
