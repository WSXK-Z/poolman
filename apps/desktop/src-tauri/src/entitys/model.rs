use sea_orm::entity::prelude::*;

/// 模型表：`models`。
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "models")]
pub struct Model {
    /// 对外模型名（主键）。
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// 所属供应商实例 id。
    pub provider_id: String,
    /// 承载渠道（网页平台 id，如 `deepseek-web`）。
    pub channel: String,
    /// 展示名。
    pub display_name: String,
    /// 是否启用。
    pub enabled: bool,
    /// 状态（如 `active` / `inactive`）。
    pub status: String,
    /// 模型相关配置（JSON 文本）。
    pub config: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
