//! 启动装配：把插件供应商与其网页模型写入持久化表（幂等）。
//!
//! 首次启动时补齐定义；已存在则原样保留（不覆盖用户对启停/状态的修改）。

use anyhow::Result;
use serde_json::json;

use super::manager::ProviderManager;
use super::plugin::{channel_label, PLUGIN_PROVIDER_ID, PLUGIN_PROVIDER_NAME, WEB_MODELS};

/// 确保插件供应商与其网页模型已登记，返回本次新增的模型数（0 表示已就绪）。
pub async fn ensure_plugin_provider(manager: &ProviderManager) -> Result<usize> {
    manager
        .ensure_provider(
            PLUGIN_PROVIDER_ID,
            "plugin",
            PLUGIN_PROVIDER_NAME,
            json!({ "seed": "web-models" }),
        )
        .await?;

    let before = manager.list_models(Some(PLUGIN_PROVIDER_ID)).await?.len();
    for entry in WEB_MODELS {
        manager
            .ensure_model(
                PLUGIN_PROVIDER_ID,
                entry.id,
                entry.platform,
                channel_label(entry.platform),
            )
            .await?;
    }
    let after = manager.list_models(Some(PLUGIN_PROVIDER_ID)).await?.len();
    Ok(after.saturating_sub(before))
}

#[cfg(test)]
mod tests {
    use super::ensure_plugin_provider;
    use crate::providers::manager::ProviderManager;
    use crate::providers::plugin::WEB_MODELS;
    use sea_orm::Database;

    /// 首次启动补齐插件供应商与其全部网页模型；再次调用不新增。
    #[tokio::test]
    async fn seeds_plugin_provider_once() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let manager = ProviderManager::new(db).await.unwrap();

        let created = ensure_plugin_provider(&manager).await.unwrap();
        assert_eq!(created, WEB_MODELS.len());

        let models = manager.list_models(Some("plugin")).await.unwrap();
        assert_eq!(models.len(), WEB_MODELS.len());
        let deepseek = models.iter().find(|m| m.id == "deepseek-chat").unwrap();
        assert_eq!(deepseek.channel, "deepseek-web");
        assert_eq!(deepseek.display_name, "DeepSeek");

        // 二次调用幂等
        assert_eq!(ensure_plugin_provider(&manager).await.unwrap(), 0);
    }
}
