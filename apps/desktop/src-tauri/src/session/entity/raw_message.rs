use sea_orm::entity::prelude::*;

/// 原始对话记录表（OpenAI 格式消息）
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "raw_messages")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// 所属会话 id
    pub session_id: String,
    /// 角色：system/user/assistant/tool
    pub role: String,
    /// 原始内容
    pub content: String,
    pub created_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
