//! API Key 命令：创建、列出、吊销。

use std::sync::Arc;

use crate::apikey::{ApiKey, ApiKeyStore};
use crate::handlers::{command_set, CommandSet};

/// 本组命令的处理器。
pub(super) fn handler() -> CommandSet<tauri::Wry> {
    command_set![create_api_key, list_api_keys, revoke_api_key]
}

/// 创建 API Key（绑定平台，决定用哪个转换器）
#[tauri::command]
pub async fn create_api_key(
    state: tauri::State<'_, Arc<ApiKeyStore>>,
    platform: String,
    note: Option<String>,
) -> Result<ApiKey, String> {
    state
        .create(&platform, note)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn list_api_keys(
    state: tauri::State<'_, Arc<ApiKeyStore>>,
) -> Result<Vec<ApiKey>, String> {
    state.list().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn revoke_api_key(
    state: tauri::State<'_, Arc<ApiKeyStore>>,
    key: String,
) -> Result<bool, String> {
    state.revoke(&key).await.map_err(|e| e.to_string())
}
