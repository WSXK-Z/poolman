use sea_orm::entity::prelude::*;

/// 供应商实例表：`providers`。
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "providers")]
pub struct Model {
    /// 供应商实例 id（主键）。
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// 供应商类型（如 `plugin`）。
    pub kind: String,
    /// 展示名。
    pub name: String,
    /// 是否启用。
    pub enabled: bool,
    /// 类型相关配置（JSON 文本）。
    pub config: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
