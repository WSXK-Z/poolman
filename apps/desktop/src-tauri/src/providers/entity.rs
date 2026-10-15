//! 供应商与模型的持久化实体（sea-orm + SQLite）。
//!
//! - `provider`：用户创建的供应商实例（类型 + 配置 + 启停）。
//! - `model`：供应商提供的模型（承载渠道 + 展示名 + 启停 + 状态 + 配置）。

/// 供应商实例表：`providers`。
pub mod provider {
    use sea_orm::entity::prelude::*;

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
}

/// 模型表：`models`。
pub mod model {
    use sea_orm::entity::prelude::*;

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
}
