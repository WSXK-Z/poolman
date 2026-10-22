//! 本地请求管理命令：列出、查看与清空请求记录。

use std::sync::Arc;

use crate::handlers::{command_set, CommandSet};
use crate::request_record::{RequestRecordDetail, RequestRecordStore, RequestRecordSummary};

/// 本组命令的处理器。
pub(super) fn handler() -> CommandSet<tauri::Wry> {
    command_set![
        list_request_records,
        get_request_record,
        count_request_records,
        clear_request_records
    ]
}

/// 列出最近的请求记录（只含预览串）。
#[tauri::command]
pub async fn list_request_records(
    state: tauri::State<'_, Arc<RequestRecordStore>>,
    limit: Option<u64>,
) -> Result<Vec<RequestRecordSummary>, String> {
    state
        .list(limit.unwrap_or(1_000))
        .await
        .map_err(|e| e.to_string())
}

/// 读取一条请求记录的完整原文。
#[tauri::command]
pub async fn get_request_record(
    state: tauri::State<'_, Arc<RequestRecordStore>>,
    id: String,
) -> Result<Option<RequestRecordDetail>, String> {
    state.get(&id).await.map_err(|e| e.to_string())
}

/// 请求记录总数。
#[tauri::command]
pub async fn count_request_records(
    state: tauri::State<'_, Arc<RequestRecordStore>>,
) -> Result<u64, String> {
    state.count().await.map_err(|e| e.to_string())
}

/// 清空全部请求记录，返回删除条数。
#[tauri::command]
pub async fn clear_request_records(
    state: tauri::State<'_, Arc<RequestRecordStore>>,
) -> Result<u64, String> {
    state.clear().await.map_err(|e| e.to_string())
}
