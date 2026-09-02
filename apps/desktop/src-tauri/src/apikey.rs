//! API Key 管理：外部用户接入凭据（sea-orm + SQLite 持久化）。
//!
//! 创建 API Key 时绑定「使用平台」（如 vscode-copilot），
//! 外部用户连接时携带该 Key，网关据此调用对应平台的转换器。

use anyhow::Result;
use sea_orm::entity::prelude::*;
use sea_orm::{
    ActiveModelTrait, Database, DatabaseConnection, DeleteResult, EntityTrait, Set,
};
use serde::{Deserialize, Serialize};

/// 已创建的 API Key
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApiKey {
    /// 密钥（对外发放，形如 sk-xxxx）
    pub key: String,
    /// 绑定平台（决定使用哪个转换器，如 vscode-copilot）
    pub platform: String,
    pub created_at: i64,
    #[serde(default)]
    pub note: String,
}

/// sea-orm 实体：api_keys 表
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "api_keys")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub key: String,
    pub platform: String,
    pub created_at: i64,
    pub note: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

impl From<Model> for ApiKey {
    fn from(m: Model) -> Self {
        ApiKey {
            key: m.key,
            platform: m.platform,
            created_at: m.created_at,
            note: m.note,
        }
    }
}

/// API Key 存储（SQLite 持久化）
pub struct ApiKeyStore {
    db: DatabaseConnection,
}

impl ApiKeyStore {
    /// 初始化数据库连接并建表（实体优先：自动幂等同步表结构）
    pub async fn init(db_path: &str) -> Result<Self> {
        let url = format!("sqlite://{}?mode=rwc", db_path);
        let db = Database::connect(&url).await?;
        // 将数据库架构与实体定义同步：缺失的表/列/索引自动创建（幂等）
        db.get_schema_builder()
            .register(Entity)
            .sync(&db)
            .await?;
        Ok(Self { db })
    }

    /// 共享数据库连接（供其它持久化模块复用）
    pub fn db(&self) -> DatabaseConnection {
        self.db.clone()
    }

    /// 创建 API Key 并绑定平台
    pub async fn create(&self, platform: &str, note: Option<String>) -> Result<ApiKey> {
        let key = format!("sk-{}", uuid::Uuid::new_v4().simple());
        let ak = ApiKey {
            key: key.clone(),
            platform: platform.to_string(),
            created_at: chrono::Utc::now().timestamp_millis(),
            note: note.unwrap_or_default(),
        };
        let am = ActiveModel {
            key: Set(ak.key.clone()),
            platform: Set(ak.platform.clone()),
            created_at: Set(ak.created_at),
            note: Set(ak.note.clone()),
        };
        am.insert(&self.db).await?;
        Ok(ak)
    }

    /// 校验 Key → 返回绑定的平台
    pub async fn resolve(&self, key: &str) -> Result<Option<String>> {
        let row = Entity::find_by_id(key).one(&self.db).await?;
        Ok(row.map(|m| m.platform))
    }

    /// 列出全部 Key
    pub async fn list(&self) -> Result<Vec<ApiKey>> {
        let rows = Entity::find().all(&self.db).await?;
        let mut v: Vec<ApiKey> = rows.into_iter().map(Into::into).collect();
        v.sort_by_key(|k| k.created_at);
        Ok(v)
    }

    /// 吊销 Key
    pub async fn revoke(&self, key: &str) -> Result<bool> {
        let res: DeleteResult = Entity::delete_by_id(key).exec(&self.db).await?;
        Ok(res.rows_affected > 0)
    }
}
