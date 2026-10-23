//! 供应商命令：供应商与模型的列出、渠道启停、网页渠道会话管理。

use std::sync::Arc;

use crate::chat_router::ChatRouter;
use crate::extension::websocket::WebSocketBridge;
use crate::handlers::{command_set, CommandSet};
use crate::providers::provider_control::{ManagedProvider, ProviderControl};
use crate::providers::ProviderManager;

/// 本组命令的处理器。
pub(super) fn handler() -> CommandSet<tauri::Wry> {
    command_set![
        list_managed_providers,
        set_provider_enabled,
        list_providers,
        list_provider_sessions,
        delete_provider_session,
        rename_provider_session,
    ]
}

/// 列出渠道运行期状态（扩展上报的连接/登录 + 桌面端启停）。
#[tauri::command]
pub async fn list_managed_providers(
    state: tauri::State<'_, Arc<ProviderControl>>,
    bridge: tauri::State<'_, Arc<WebSocketBridge>>,
) -> Result<Vec<ManagedProvider>, String> {
    let status = bridge.manager.latest_status();
    Ok(state.list(&status))
}

#[tauri::command]
pub async fn set_provider_enabled(
    manager: tauri::State<'_, Arc<ProviderManager>>,
    state: tauri::State<'_, Arc<ProviderControl>>,
    bridge: tauri::State<'_, Arc<WebSocketBridge>>,
    provider: String,
    enabled: bool,
) -> Result<(), String> {
    if enabled {
        let status = bridge.manager.latest_status();
        let authed = status
            .get(&provider)
            .and_then(|item| item.get("authed"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if !authed {
            return Err(format!("供应商 {} 尚未登录，不能启用", provider));
        }
    }
    // 先落库（渠道下全部模型行），成功后再刷新路由期快照
    manager
        .set_channel_enabled(crate::providers::plugin::PLUGIN_PROVIDER_ID, &provider, enabled)
        .await
        .map_err(|e| e.to_string())?;
    state
        .set_enabled(&provider, enabled)
        .map_err(|e| e.to_string())
}

/// 列出供应商及其模型（供「供应商管理」界面展示；来自持久化表）。
#[tauri::command]
pub async fn list_providers(
    manager: tauri::State<'_, Arc<ProviderManager>>,
) -> Result<Vec<serde_json::Value>, String> {
    let providers = manager.list_providers().await.map_err(|e| e.to_string())?;
    let mut result = Vec::with_capacity(providers.len());
    for provider in providers {
        let models = manager
            .list_models(Some(&provider.id))
            .await
            .map_err(|e| e.to_string())?;
        let models: Vec<serde_json::Value> = models
            .iter()
            .map(|model| {
                serde_json::json!({
                    "id": model.id,
                    "channel": model.channel,
                    "displayName": model.display_name,
                    "enabled": model.enabled,
                    "status": model.status,
                })
            })
            .collect();
        result.push(serde_json::json!({
            "id": provider.id,
            "kind": provider.kind,
            "name": provider.name,
            "enabled": provider.enabled,
            "models": models,
        }));
    }
    Ok(result)
}

#[tauri::command]
pub async fn list_provider_sessions(
    provider: String,
    chat_router: tauri::State<'_, Arc<ChatRouter>>,
) -> Result<Vec<serde_json::Value>, String> {
    let model = chat_router
        .channel(&provider)
        .ok_or_else(|| format!("{} 尚未实现网页会话管理", provider))?;
    let sessions = model
        .list_channel_sessions()
        .await
        .map_err(|e| e.to_string())?;
    Ok(sessions.as_array().cloned().unwrap_or_default())
}

#[tauri::command]
pub async fn delete_provider_session(
    provider: String,
    session_id: String,
    chat_router: tauri::State<'_, Arc<ChatRouter>>,
) -> Result<(), String> {
    let model = chat_router
        .channel(&provider)
        .ok_or_else(|| format!("{} 尚未实现网页会话管理", provider))?;
    model
        .delete_channel_session(&session_id)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 修改网页渠道的远端会话标题。
#[tauri::command]
pub async fn rename_provider_session(
    provider: String,
    session_id: String,
    title: String,
    chat_router: tauri::State<'_, Arc<ChatRouter>>,
) -> Result<(), String> {
    let renamed = chat_router
        .rename_remote_session(&provider, &session_id, &title)
        .await
        .map_err(|e| e.to_string())?;
    if !renamed {
        return Err(format!("{provider} 网页端未确认标题修改"));
    }
    Ok(())
}
