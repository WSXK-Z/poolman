//! 网页渠道会话绑定存储：本地会话 id → 远端网页会话 id。

use anyhow::Result;
use futures_util::future::BoxFuture;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set,
};
use uuid::Uuid;

use crate::entitys::web_context_binding::{ActiveModel, Column, Entity};
use crate::models::defs::{WebContextBinding, WebContextStore};

/// 稳定的短哈希：用于判断 system prompt / 工具表是否变化。
pub fn text_hash(text: &str) -> String {
    Uuid::new_v5(&Uuid::NAMESPACE_OID, text.as_bytes()).simple().to_string()
}

/// 基于 SQLite 的绑定存储。
pub struct DbWebContextStore {
    db: DatabaseConnection,
}

impl DbWebContextStore {
    pub async fn new(db: DatabaseConnection) -> Result<Self> {
        db.get_schema_builder()
            .register(Entity)
            .sync(&db)
            .await?;
        Ok(Self { db })
    }

    fn binding_id(channel: &str, context_key: &str) -> String {
        let seed = format!("{channel}\u{1f}{context_key}");
        format!(
            "wcb_{}",
            Uuid::new_v5(&Uuid::NAMESPACE_OID, seed.as_bytes()).simple()
        )
    }
}

impl WebContextStore for DbWebContextStore {
    fn load<'a>(
        &'a self,
        channel: &'a str,
        context_key: &'a str,
    ) -> BoxFuture<'a, Result<Option<WebContextBinding>>> {
        Box::pin(async move {
            let row = Entity::find()
                .filter(Column::Channel.eq(channel))
                .filter(Column::ContextKey.eq(context_key))
                .one(&self.db)
                .await?;
            Ok(row.map(|row| WebContextBinding {
                channel: row.channel,
                context_key: row.context_key,
                remote_session_id: row.remote_session_id,
                parent_message_id: row.parent_message_id,
                primed: row.primed,
                system_hash: row.system_hash,
                tools_hash: row.tools_hash,
                approx_tokens: row.approx_tokens,
                summary: row.summary.unwrap_or_default(),
            }))
        })
    }

    fn save<'a>(&'a self, binding: &'a WebContextBinding) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let id = Self::binding_id(&binding.channel, &binding.context_key);
            let now = chrono::Utc::now().timestamp_millis();
            if let Some(existing) = Entity::find_by_id(id.clone()).one(&self.db).await? {
                let mut active: ActiveModel = existing.into();
                active.remote_session_id = Set(binding.remote_session_id.clone());
                active.parent_message_id = Set(binding.parent_message_id);
                active.primed = Set(binding.primed);
                active.system_hash = Set(binding.system_hash.clone());
                active.tools_hash = Set(binding.tools_hash.clone());
                active.approx_tokens = Set(binding.approx_tokens);
                active.summary = Set(Some(binding.summary.clone()));
                active.updated_at = Set(now);
                active.update(&self.db).await?;
            } else {
                ActiveModel {
                    id: Set(id),
                    channel: Set(binding.channel.clone()),
                    context_key: Set(binding.context_key.clone()),
                    remote_session_id: Set(binding.remote_session_id.clone()),
                    parent_message_id: Set(binding.parent_message_id),
                    primed: Set(binding.primed),
                    system_hash: Set(binding.system_hash.clone()),
                    tools_hash: Set(binding.tools_hash.clone()),
                    approx_tokens: Set(binding.approx_tokens),
                    summary: Set(Some(binding.summary.clone())),
                    created_at: Set(now),
                    updated_at: Set(now),
                }
                .insert(&self.db)
                .await?;
            }
            Ok(())
        })
    }

    fn delete<'a>(
        &'a self,
        channel: &'a str,
        context_key: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            Entity::delete_many()
                .filter(Column::Channel.eq(channel))
                .filter(Column::ContextKey.eq(context_key))
                .exec(&self.db)
                .await?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::DbWebContextStore;
    use crate::models::defs::{WebContextBinding, WebContextStore};
    use sea_orm::Database;

    #[tokio::test]
    async fn binding_store_roundtrip() {
        let path = std::env::temp_dir().join(format!(
            "poolman-web-binding-{}.db",
            uuid::Uuid::new_v4()
        ));
        let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
        let db = Database::connect(&url).await.expect("初始化测试数据库失败");
        let store = DbWebContextStore::new(db).await.expect("同步绑定表失败");
        let binding = WebContextBinding {
            channel: "deepseek-web".to_string(),
            context_key: "sess_test".to_string(),
            remote_session_id: "remote_1".to_string(),
            parent_message_id: Some(42),
            primed: true,
            system_hash: "s1".to_string(),
            tools_hash: "t1".to_string(),
            approx_tokens: 100,
            summary: String::new(),
        };
        store.save(&binding).await.expect("保存绑定失败");
        let loaded = store
            .load("deepseek-web", "sess_test")
            .await
            .expect("读取绑定失败")
            .expect("绑定应存在");
        assert_eq!(loaded, binding);

        let mut updated = binding.clone();
        updated.remote_session_id = "remote_2".to_string();
        updated.parent_message_id = Some(43);
        updated.approx_tokens = 200;
        store.save(&updated).await.expect("更新绑定失败");
        let loaded = store
            .load("deepseek-web", "sess_test")
            .await
            .expect("读取绑定失败")
            .expect("绑定应存在");
        assert_eq!(loaded.remote_session_id, "remote_2");
        assert_eq!(loaded.parent_message_id, Some(43));
        assert_eq!(loaded.approx_tokens, 200);

        store
            .delete("deepseek-web", "sess_test")
            .await
            .expect("删除绑定失败");
        assert!(store
            .load("deepseek-web", "sess_test")
            .await
            .expect("读取绑定失败")
            .is_none());

        let _ = std::fs::remove_file(path);
    }
}
