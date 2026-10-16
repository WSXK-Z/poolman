use sea_orm::entity::prelude::*;

/// API Key 表：`api_keys`。
///
/// 外部用户接入凭据：Key 绑定「使用平台」（如 `vscode-copilot`），网关据此选择转换器。
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "api_keys")]
pub struct Model {
    /// 密钥（对外发放，形如 `sk-xxxx`）。
    #[sea_orm(primary_key, auto_increment = false)]
    pub key: String,
    /// 绑定平台（决定使用哪个转换器，如 `vscode-copilot`）。
    pub platform: String,
    pub created_at: i64,
    pub note: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
