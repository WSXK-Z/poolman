use sea_orm::entity::prelude::*;

/// 网页渠道会话池槽位表：`web_sessions`。
///
/// 每个网页渠道按并发设置预建若干会话，槽位表记录「槽位序号 → 网页会话 id」，
/// 进程重启后据此核对复用，避免重复创建会话。
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "web_sessions")]
pub struct Model {
    /// 承载渠道（网页平台 id，如 `deepseek-web`）。
    #[sea_orm(primary_key, auto_increment = false)]
    pub channel: String,
    /// 槽位序号（0 起）。
    #[sea_orm(primary_key, auto_increment = false)]
    pub slot: i32,
    /// 网页会话 id。
    pub session_id: String,
    pub updated_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
