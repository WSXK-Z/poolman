//! 供应商与模型的管理器（sea-orm + SQLite 持久化）。
//!
//! 负责「有哪些供应商、各自提供哪些模型」的定义与持久化，并保存供应商启停、
//! 模型启停与模型状态。运行期模型实例（渠道适配器 + 执行器）由工厂按这里的行构建。
//! 请求路由期的启停快照由 `providers::provider_control::ProviderControl` 维护。

use anyhow::Result;
use std::collections::HashMap;
use futures_util::future::BoxFuture;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DeleteResult, EntityTrait, QueryFilter,
    QueryOrder, Set,
};
use sea_orm::sea_query::Expr;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::entitys::{model as model_entity, provider, web_session};
use crate::models::defs::{PooledSession, SessionPoolStore};

/// 供应商实例（对外视图）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub enabled: bool,
    pub config: Value,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 模型（对外视图）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub provider_id: String,
    pub channel: String,
    pub display_name: String,
    pub enabled: bool,
    pub status: String,
    pub config: Value,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 路由/执行所需的解析结果：模型 + 其所属供应商。
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub model: Model,
    pub provider: Provider,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn parse_config(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

impl From<provider::Model> for Provider {
    fn from(row: provider::Model) -> Self {
        Self {
            id: row.id,
            kind: row.kind,
            name: row.name,
            enabled: row.enabled,
            config: parse_config(&row.config),
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

impl From<model_entity::Model> for Model {
    fn from(row: model_entity::Model) -> Self {
        Self {
            id: row.id,
            provider_id: row.provider_id,
            channel: row.channel,
            display_name: row.display_name,
            enabled: row.enabled,
            status: row.status,
            config: parse_config(&row.config),
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

/// 供应商与模型的持久化管理器。
pub struct ProviderManager {
    db: DatabaseConnection,
}

impl ProviderManager {
    /// 初始化并同步两张表（幂等）。
    pub async fn new(db: DatabaseConnection) -> Result<Self> {
        db.get_schema_builder()
            .register(provider::Entity)
            .register(model_entity::Entity)
            .register(web_session::Entity)
            .sync(&db)
            .await?;
        Ok(Self { db })
    }

    // ---------- 供应商 ----------

    /// 创建供应商实例（类型 + 展示名 + 配置）。
    pub async fn create_provider(
        &self,
        kind: &str,
        name: &str,
        config: Value,
    ) -> Result<Provider> {
        let now = now_ms();
        let row = provider::ActiveModel {
            id: Set(format!("prov-{}", uuid::Uuid::new_v4().simple())),
            kind: Set(kind.to_string()),
            name: Set(name.to_string()),
            enabled: Set(true),
            config: Set(config.to_string()),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&self.db)
        .await?;
        Ok(row.into())
    }

    /// 列出全部供应商（按创建时间）。
    pub async fn list_providers(&self) -> Result<Vec<Provider>> {
        let rows = provider::Entity::find()
            .order_by_asc(provider::Column::CreatedAt)
            .all(&self.db)
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// 读取单个供应商。
    pub async fn get_provider(&self, id: &str) -> Result<Option<Provider>> {
        Ok(provider::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .map(Into::into))
    }

    /// 幂等创建供应商：已存在则返回现有行。
    pub async fn ensure_provider(
        &self,
        id: &str,
        kind: &str,
        name: &str,
        config: Value,
    ) -> Result<Provider> {
        if let Some(existing) = self.get_provider(id).await? {
            return Ok(existing);
        }
        let now = now_ms();
        let row = provider::ActiveModel {
            id: Set(id.to_string()),
            kind: Set(kind.to_string()),
            name: Set(name.to_string()),
            enabled: Set(true),
            config: Set(config.to_string()),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&self.db)
        .await?;
        Ok(row.into())
    }

    /// 设置供应商启停。
    pub async fn set_provider_enabled(&self, id: &str, enabled: bool) -> Result<bool> {
        let Some(row) = provider::Entity::find_by_id(id).one(&self.db).await? else {
            return Ok(false);
        };
        let mut active: provider::ActiveModel = row.into();
        active.enabled = Set(enabled);
        active.updated_at = Set(now_ms());
        active.update(&self.db).await?;
        Ok(true)
    }

    /// 删除供应商及其全部模型。
    pub async fn delete_provider(&self, id: &str) -> Result<bool> {
        model_entity::Entity::delete_many()
            .filter(model_entity::Column::ProviderId.eq(id))
            .exec(&self.db)
            .await?;
        let result: DeleteResult = provider::Entity::delete_by_id(id).exec(&self.db).await?;
        Ok(result.rows_affected > 0)
    }

    // ---------- 模型 ----------

    /// 新增模型（对外模型名 + 承载渠道 + 展示名）。
    pub async fn create_model(
        &self,
        provider_id: &str,
        id: &str,
        channel: &str,
        display_name: &str,
    ) -> Result<Model> {
        let now = now_ms();
        let row = model_entity::ActiveModel {
            id: Set(id.to_string()),
            provider_id: Set(provider_id.to_string()),
            channel: Set(channel.to_string()),
            display_name: Set(display_name.to_string()),
            enabled: Set(true),
            status: Set("inactive".to_string()),
            config: Set("{}".to_string()),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&self.db)
        .await?;
        Ok(row.into())
    }

    /// 列出模型；给定供应商时只列它的。
    pub async fn list_models(&self, provider_id: Option<&str>) -> Result<Vec<Model>> {
        let mut query = model_entity::Entity::find().order_by_asc(model_entity::Column::CreatedAt);
        if let Some(provider_id) = provider_id {
            query = query.filter(model_entity::Column::ProviderId.eq(provider_id));
        }
        Ok(query.all(&self.db).await?.into_iter().map(Into::into).collect())
    }

    /// 幂等创建模型：已存在则返回现有行。
    pub async fn ensure_model(
        &self,
        provider_id: &str,
        id: &str,
        channel: &str,
        display_name: &str,
    ) -> Result<Model> {
        if let Some(row) = model_entity::Entity::find_by_id(id).one(&self.db).await? {
            return Ok(row.into());
        }
        self.create_model(provider_id, id, channel, display_name).await
    }

    /// 按对外模型名解析出模型与其供应商（供路由/执行使用）。
    pub async fn resolve(&self, model_id: &str) -> Result<Option<ResolvedModel>> {
        let Some(model) = model_entity::Entity::find_by_id(model_id).one(&self.db).await? else {
            return Ok(None);
        };
        let Some(provider) = provider::Entity::find_by_id(&model.provider_id)
            .one(&self.db)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(ResolvedModel {
            model: model.into(),
            provider: provider.into(),
        }))
    }

    /// 设置模型启停。
    pub async fn set_model_enabled(&self, id: &str, enabled: bool) -> Result<bool> {
        let Some(row) = model_entity::Entity::find_by_id(id).one(&self.db).await? else {
            return Ok(false);
        };
        let mut active: model_entity::ActiveModel = row.into();
        active.enabled = Set(enabled);
        active.updated_at = Set(now_ms());
        active.update(&self.db).await?;
        Ok(true)
    }

    /// 更新模型状态（如 active / inactive）。
    pub async fn set_model_status(&self, id: &str, status: &str) -> Result<bool> {
        let Some(row) = model_entity::Entity::find_by_id(id).one(&self.db).await? else {
            return Ok(false);
        };
        let mut active: model_entity::ActiveModel = row.into();
        active.status = Set(status.to_string());
        active.updated_at = Set(now_ms());
        active.update(&self.db).await?;
        Ok(true)
    }

    /// 删除模型。
    pub async fn delete_model(&self, id: &str) -> Result<bool> {
        let result: DeleteResult = model_entity::Entity::delete_by_id(id).exec(&self.db).await?;
        Ok(result.rows_affected > 0)
    }

    /// 模型是否可被路由使用：存在、自身启用、且所属供应商启用。
    pub async fn is_model_routable(&self, id: &str) -> Result<bool> {
        Ok(self
            .resolve(id)
            .await?
            .map(|resolved| resolved.model.enabled && resolved.provider.enabled)
            .unwrap_or(false))
    }

    // ---------- 渠道级启停 ----------

    /// 按渠道汇总启停：同一渠道下任一行启用即视为启用，无记录视为启用。
    pub async fn channel_enablement(&self, provider_id: &str) -> Result<HashMap<String, bool>> {
        let mut map: HashMap<String, bool> = HashMap::new();
        for model in self.list_models(Some(provider_id)).await? {
            let entry = map.entry(model.channel).or_insert(false);
            *entry = *entry || model.enabled;
        }
        Ok(map)
    }

    /// 按渠道批量设置启停：该渠道下的全部模型行取同一个值，返回受影响行数。
    pub async fn set_channel_enabled(
        &self,
        provider_id: &str,
        channel: &str,
        enabled: bool,
    ) -> Result<u64> {
        let result = model_entity::Entity::update_many()
            .col_expr(model_entity::Column::Enabled, Expr::value(enabled))
            .col_expr(model_entity::Column::UpdatedAt, Expr::value(now_ms()))
            .filter(model_entity::Column::ProviderId.eq(provider_id))
            .filter(model_entity::Column::Channel.eq(channel))
            .exec(&self.db)
            .await?;
        Ok(result.rows_affected)
    }

    // ---------- 网页渠道会话池 ----------

    /// 读取某渠道已登记的会话池槽位（按槽位序号）。
    pub async fn list_web_sessions(&self, channel: &str) -> Result<Vec<PooledSession>> {
        let rows = web_session::Entity::find()
            .filter(web_session::Column::Channel.eq(channel))
            .order_by_asc(web_session::Column::Slot)
            .all(&self.db)
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| PooledSession {
                slot: row.slot.max(0) as u32,
                session_id: row.session_id,
            })
            .collect())
    }

    /// 覆盖写入某渠道的会话池槽位：先清空该渠道的登记再整表写入。
    pub async fn replace_web_sessions(
        &self,
        channel: &str,
        sessions: &[PooledSession],
    ) -> Result<()> {
        web_session::Entity::delete_many()
            .filter(web_session::Column::Channel.eq(channel))
            .exec(&self.db)
            .await?;
        let now = now_ms();
        for session in sessions {
            web_session::ActiveModel {
                channel: Set(channel.to_string()),
                slot: Set(session.slot as i32),
                session_id: Set(session.session_id.clone()),
                updated_at: Set(now),
            }
            .insert(&self.db)
            .await?;
        }
        Ok(())
    }
}

impl SessionPoolStore for ProviderManager {
    fn load<'a>(&'a self, channel: &'a str) -> BoxFuture<'a, Result<Vec<PooledSession>>> {
        Box::pin(async move { self.list_web_sessions(channel).await })
    }

    fn save<'a>(
        &'a self,
        channel: &'a str,
        sessions: &'a [PooledSession],
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.replace_web_sessions(channel, sessions).await })
    }
}

#[cfg(test)]
mod tests {
    use super::ProviderManager;
    use crate::models::defs::{PooledSession, SessionPoolStore};
    use sea_orm::Database;
    use serde_json::json;

    async fn manager() -> ProviderManager {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        ProviderManager::new(db).await.unwrap()
    }

    /// 供应商与模型持久化、解析与级联删除。
    #[tokio::test]
    async fn persists_providers_models_and_resolves() {
        let manager = manager().await;

        let provider = manager
            .create_provider("plugin", "浏览器插件", json!({ "note": "web" }))
            .await
            .unwrap();
        assert_eq!(provider.kind, "plugin");
        assert!(provider.enabled);
        assert_eq!(provider.config["note"], json!("web"));

        manager
            .create_model(&provider.id, "deepseek-chat", "deepseek-web", "DeepSeek")
            .await
            .unwrap();
        manager
            .create_model(&provider.id, "gpt-4o", "chatgpt-web", "ChatGPT")
            .await
            .unwrap();

        let models = manager.list_models(Some(&provider.id)).await.unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(manager.list_models(None).await.unwrap().len(), 2);
        assert_eq!(manager.list_providers().await.unwrap().len(), 1);

        // 解析：模型 → 供应商
        let resolved = manager.resolve("deepseek-chat").await.unwrap().unwrap();
        assert_eq!(resolved.model.channel, "deepseek-web");
        assert_eq!(resolved.provider.id, provider.id);
        assert!(manager.resolve("unknown").await.unwrap().is_none());

        // 删除供应商：级联删除其模型
        assert!(manager.delete_provider(&provider.id).await.unwrap());
        assert_eq!(manager.list_models(None).await.unwrap().len(), 0);
        assert!(manager.get_provider(&provider.id).await.unwrap().is_none());
    }

    /// 启停与状态：模型可路由性取决于模型自身与所属供应商的启用。
    #[tokio::test]
    async fn routability_respects_model_and_provider_enablement() {
        let manager = manager().await;
        let provider = manager
            .create_provider("plugin", "P", json!({}))
            .await
            .unwrap();
        manager
            .create_model(&provider.id, "deepseek-web", "deepseek-web", "DeepSeek")
            .await
            .unwrap();

        assert!(manager.is_model_routable("deepseek-web").await.unwrap());

        // 模型停用 → 不可路由
        assert!(manager.set_model_enabled("deepseek-web", false).await.unwrap());
        assert!(!manager.is_model_routable("deepseek-web").await.unwrap());
        manager.set_model_enabled("deepseek-web", true).await.unwrap();

        // 供应商停用 → 不可路由
        assert!(manager.set_provider_enabled(&provider.id, false).await.unwrap());
        assert!(!manager.is_model_routable("deepseek-web").await.unwrap());

        // 状态更新
        assert!(manager.set_model_status("deepseek-web", "active").await.unwrap());
        let model = manager.resolve("deepseek-web").await.unwrap().unwrap().model;
        assert_eq!(model.status, "active");

        // 不存在的 id：返回 false 而非报错
        assert!(!manager.set_model_enabled("nope", false).await.unwrap());
        assert!(!manager.delete_model("nope").await.unwrap());
        assert!(!manager.set_provider_enabled("nope", false).await.unwrap());
    }

    /// 网页渠道会话池槽位：按渠道整表读写（重启后据此核对复用）。
    #[tokio::test]
    async fn web_session_pool_round_trips_per_channel() {
        let manager = manager().await;
        let deepseek = vec![
            PooledSession {
                slot: 0,
                session_id: "sess-a".to_string(),
            },
            PooledSession {
                slot: 2,
                session_id: "sess-c".to_string(),
            },
        ];
        manager
            .replace_web_sessions("deepseek-web", &deepseek)
            .await
            .unwrap();
        manager
            .replace_web_sessions(
                "chatgpt-web",
                &[PooledSession {
                    slot: 0,
                    session_id: "sess-other".to_string(),
                }],
            )
            .await
            .unwrap();

        // 按渠道隔离，且按槽位序号排序
        assert_eq!(
            manager.list_web_sessions("deepseek-web").await.unwrap(),
            deepseek
        );
        assert_eq!(
            manager
                .list_web_sessions("chatgpt-web")
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(manager
            .list_web_sessions("kimi-web")
            .await
            .unwrap()
            .is_empty());

        // 整表覆盖：旧槽位被清掉
        manager
            .replace_web_sessions(
                "deepseek-web",
                &[PooledSession {
                    slot: 1,
                    session_id: "sess-b".to_string(),
                }],
            )
            .await
            .unwrap();
        assert_eq!(
            manager.list_web_sessions("deepseek-web").await.unwrap(),
            vec![PooledSession {
                slot: 1,
                session_id: "sess-b".to_string()
            }]
        );

        // 会话池存储契约：模型只按渠道读写自己的槽位
        let store: &dyn SessionPoolStore = &manager;
        assert_eq!(store.load("chatgpt-web").await.unwrap().len(), 1);
        store.save("chatgpt-web", &[]).await.unwrap();
        assert!(store.load("chatgpt-web").await.unwrap().is_empty());
    }

    /// 渠道级启停：按渠道汇总（任一行启用即启用），批量设置作用于该渠道全部模型行。
    #[tokio::test]
    async fn channel_enablement_aggregates_and_updates_all_rows() {
        let manager = manager().await;
        let provider = manager
            .create_provider("plugin", "P", json!({}))
            .await
            .unwrap();
        for (id, channel) in [
            ("deepseek-web", "deepseek-web"),
            ("deepseek-chat", "deepseek-web"),
            ("gpt-4o", "chatgpt-web"),
        ] {
            manager.create_model(&provider.id, id, channel, channel).await.unwrap();
        }

        // 初始全部启用
        let enablement = manager.channel_enablement(&provider.id).await.unwrap();
        assert_eq!(enablement.get("deepseek-web"), Some(&true));
        assert_eq!(enablement.get("chatgpt-web"), Some(&true));

        // 停用其中一个渠道：该渠道全部行一起关闭
        assert_eq!(
            manager
                .set_channel_enabled(&provider.id, "deepseek-web", false)
                .await
                .unwrap(),
            2
        );
        let enablement = manager.channel_enablement(&provider.id).await.unwrap();
        assert_eq!(enablement.get("deepseek-web"), Some(&false));
        assert_eq!(enablement.get("chatgpt-web"), Some(&true));
        assert!(!manager.is_model_routable("deepseek-chat").await.unwrap());

        // 重新启用其中一个模型行即让整条渠道恢复
        manager.set_model_enabled("deepseek-chat", true).await.unwrap();
        let enablement = manager.channel_enablement(&provider.id).await.unwrap();
        assert_eq!(enablement.get("deepseek-web"), Some(&true));

        // 无行可改时返回 0
        assert_eq!(
            manager
                .set_channel_enabled(&provider.id, "unknown-web", false)
                .await
                .unwrap(),
            0
        );
    }

    /// 幂等：重复 ensure 不产生重复行。
    #[tokio::test]
    async fn ensure_is_idempotent() {
        let manager = manager().await;
        let first = manager
            .ensure_provider("plugin", "plugin", "浏览器插件", json!({}))
            .await
            .unwrap();
        let second = manager
            .ensure_provider("plugin", "plugin", "别的名字", json!({}))
            .await
            .unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(second.name, "浏览器插件");
        assert_eq!(manager.list_providers().await.unwrap().len(), 1);

        manager
            .ensure_model("plugin", "deepseek-web", "deepseek-web", "DeepSeek")
            .await
            .unwrap();
        manager
            .ensure_model("plugin", "deepseek-web", "deepseek-web", "DeepSeek")
            .await
            .unwrap();
        assert_eq!(manager.list_models(None).await.unwrap().len(), 1);
    }
}

