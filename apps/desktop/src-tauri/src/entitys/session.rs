use sea_orm::entity::prelude::*;

/// 会话表：`sessions`。
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "sessions")]
pub struct Model {
    /// 工具自身会话 id（时间序列 UUIDv7，主键，全局唯一）。
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// 网页平台会话 id（如 DeepSeek 会话 id，可能重复，不作为主键）。
    pub web_session_id: String,
    /// 标题。
    pub title: String,
    /// 调用的网页平台（如 `deepseek-web`）。
    pub platform: String,
    /// 关联的 API Key（用于按 key 分平台查看）。
    pub api_key: String,
    /// 最近一次执行该会话的插件 id（调度会话粘性的落库副本；尚无记录时为 NULL）。
    pub plugin_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
