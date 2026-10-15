//! 会话命令：列出、查看、删除本地会话记录。

use std::sync::Arc;

use crate::handlers::{command_set, CommandSet};
use crate::session::manager::{SessionDetail, SessionInfo, SessionManager};

/// 本组命令的处理器。
pub(super) fn handler() -> CommandSet<tauri::Wry> {
    command_set![list_sessions, get_session, delete_session]
}

/// 列出会话（可按 API Key 过滤）
#[tauri::command]
pub async fn list_sessions(
    state: tauri::State<'_, Arc<SessionManager>>,
    api_key: Option<String>,
) -> Result<Vec<SessionInfo>, String> {
    state
        .list_sessions(api_key.as_deref())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_session(
    state: tauri::State<'_, Arc<SessionManager>>,
    id: String,
) -> Result<Option<SessionDetail>, String> {
    state.get_session(&id).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn delete_session(
    state: tauri::State<'_, Arc<SessionManager>>,
    id: String,
) -> Result<bool, String> {
    state.delete_session(&id).await.map_err(|e| e.to_string())
}
