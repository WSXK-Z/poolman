use sea_orm::entity::prelude::*;

/// 转换后的实际对话记录表（转换器输出的提示词）
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "converted_messages")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// 所属会话 id
    pub session_id: String,
    /// 角色：system/user/assistant
    pub role: String,
    /// 转换后实际发给网页平台的内容
    pub content: String,
    pub created_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
