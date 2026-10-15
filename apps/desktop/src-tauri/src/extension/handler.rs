//! WebSocket 上行消息分发器。
//!
//! 根据消息 ID 将网页执行的响应块、执行结果和捕获完成事件交还等待中的请求，
//! 同时把供应商状态与凭证同步到桌面进程共享状态。

use serde_json::Value;
use std::sync::Arc;
use tracing::{debug, info};

use super::credentials::CredentialStore;
use super::manager::ExtensionManager;
use super::websocket::{
    ExecuteChunkCallbacks, ExecuteResultPayload, PendingCapture, PendingExecute,
};

#[derive(Debug, serde::Deserialize)]
struct BridgeMessage {
    id: String,
    #[serde(rename = "type")]
    type_: String,
    #[allow(dead_code)]
    timestamp: i64,
    payload: Value,
}

pub async fn handle_bridge_message(
    text: &str,
    pending_execute: &PendingExecute,
    execute_chunk_callbacks: &ExecuteChunkCallbacks,
    pending_capture: &PendingCapture,
    manager: &Arc<ExtensionManager>,
    plugin_id: &str,
    credentials: &Arc<CredentialStore>,
) {
    // 只有结构完整的协议消息才进入状态处理；解析错误在这里记录并丢弃。
    let msg: BridgeMessage = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(e) => {
            debug!("解析桥接消息失败: {}", e);
            return;
        }
    };

    match msg.type_.as_str() {
        "EXECUTE_CHUNK" => {
            // 执行流式增量块（回调须保留至 EXECUTE_RESULT 才移除，否则后续块找不到回调被丢弃）
            let data = msg
                .payload
                .get("data")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            if let Some(cb) = execute_chunk_callbacks.lock().await.get_mut(&msg.id) {
                cb(data);
            }
        }
        "EXECUTE_RESULT" => {
            // 执行完成
            let ok = msg
                .payload
                .get("ok")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let status = msg
                .payload
                .get("status")
                .and_then(|v| v.as_u64())
                .unwrap_or(500) as u16;
            let body = msg
                .payload
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let payload = ExecuteResultPayload { ok, status, body };
            if let Some(sender) = pending_execute.lock().await.remove(&msg.id) {
                let _ = sender.send(payload);
            }
            execute_chunk_callbacks.lock().await.remove(&msg.id);
        }
        "EXECUTE_ERROR" => {
            let error = msg
                .payload
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("未知执行错误")
                .to_string();
            if let Some(sender) = pending_execute.lock().await.remove(&msg.id) {
                let payload = ExecuteResultPayload {
                    ok: false,
                    status: 500,
                    body: format!("错误: {}", error),
                };
                let _ = sender.send(payload);
            }
            execute_chunk_callbacks.lock().await.remove(&msg.id);
        }
        "CREDENTIAL_SYNC" => {
            // 插件上报凭证
            let provider = msg
                .payload
                .get("provider")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let token = msg
                .payload
                .get("token")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let settings_token = msg
                .payload
                .get("settingsToken")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let hif_headers = msg.payload.get("hifHeaders").and_then(|v| {
                serde_json::from_value::<super::credentials::HifHeaders>(v.clone()).ok()
            });
            let api_template = msg.payload.get("apiTemplate").cloned();

            if !provider.is_empty() {
                credentials.upsert(provider, token, settings_token, hif_headers, api_template);
                info!("已同步凭证，provider: {}", provider);
                // 如果有等待的捕获请求，完成它
                if let Some(sender) = pending_capture.lock().await.remove(provider) {
                    let _ = sender.send(());
                }
            }
        }
        "STATUS_SYNC" => {
            manager.update_status(plugin_id, msg.payload.clone());
        }
        "CAPTURE_REQUEST" | "EXECUTE_REQUEST" => {
            // 服务端->插件下行消息，插件不应回发
            debug!("收到来自扩展的意外下行消息类型: {}", msg.type_);
        }
        _ => {
            debug!("未处理的消息类型: {}", msg.type_);
        }
    }
}
