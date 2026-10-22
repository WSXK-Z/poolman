//! 本地会话管理命令：同步请求记录、列出会话、删除会话与会话关联请求。

use std::sync::Arc;

use crate::handlers::{command_set, CommandSet};
use crate::local_session::{
    LocalSessionContentInfo, LocalSessionInfo, LocalSessionRequestInfo, LocalSessionStore,
    LocalSessionSyncResult,
};

pub(super) fn handler() -> CommandSet<tauri::Wry> {
    command_set![
        sync_local_sessions,
        list_local_sessions,
        list_local_session_requests,
        list_local_session_contents,
        delete_local_sessions
    ]
}

/// 扫描尚未归属会话的请求记录并生成/补齐本地会话。
#[tauri::command]
pub async fn sync_local_sessions(
    state: tauri::State<'_, Arc<LocalSessionStore>>,
) -> Result<LocalSessionSyncResult, String> {
    state.sync().await.map_err(|e| e.to_string())
}

/// 列出全部本地会话。
#[tauri::command]
pub async fn list_local_sessions(
    state: tauri::State<'_, Arc<LocalSessionStore>>,
) -> Result<Vec<LocalSessionInfo>, String> {
    state.list_sessions().await.map_err(|e| e.to_string())
}

/// 列出某个本地会话关联的请求。
#[tauri::command]
pub async fn list_local_session_requests(
    state: tauri::State<'_, Arc<LocalSessionStore>>,
    session_id: String,
) -> Result<Vec<LocalSessionRequestInfo>, String> {
    state
        .list_session_requests(&session_id)
        .await
        .map_err(|e| e.to_string())
}

/// 列出某个本地会话的内容表。
#[tauri::command]
pub async fn list_local_session_contents(
    state: tauri::State<'_, Arc<LocalSessionStore>>,
    session_id: String,
) -> Result<Vec<LocalSessionContentInfo>, String> {
    state
        .list_session_contents(&session_id)
        .await
        .map_err(|e| e.to_string())
}

/// 删除一个或多个本地会话（关联请求保留，但不再参与会话归并）。
#[tauri::command]
pub async fn delete_local_sessions(
    state: tauri::State<'_, Arc<LocalSessionStore>>,
    session_ids: Vec<String>,
) -> Result<u64, String> {
    state
        .delete_sessions(&session_ids)
        .await
        .map_err(|e| e.to_string())
}
