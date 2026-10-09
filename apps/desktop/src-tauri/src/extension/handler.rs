use serde_json::Value;
use std::sync::Arc;
use tracing::{debug, info};

use super::credentials::CredentialStore;
use super::websocket::{
    ChatChunkPayload, ChatResponsePayload, ChunkCallbacks, ExecuteChunkCallbacks,
    ExecuteResultPayload, PendingCapture, PendingExecute, PendingRequests,
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
    pending: &PendingRequests,
    chunk_callbacks: &ChunkCallbacks,
    pending_execute: &PendingExecute,
    execute_chunk_callbacks: &ExecuteChunkCallbacks,
    pending_capture: &PendingCapture,
    latest_status: &Arc<tokio::sync::RwLock<Value>>,
    plugin_statuses: &Arc<tokio::sync::RwLock<std::collections::HashMap<String, Value>>>,
    plugin_id: &str,
    credentials: &Arc<CredentialStore>,
) {
    let msg: BridgeMessage = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(e) => {
            debug!("解析桥接消息失败: {}", e);
            return;
        }
    };

    match msg.type_.as_str() {
        "CHAT_CHUNK" => {
            // 流式增量块
            let delta = msg
                .payload
                .get("delta")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let tool_calls = msg.payload.get("tool_calls").cloned();
            let chunk = ChatChunkPayload { delta, tool_calls };
            if let Some(mut cb) = chunk_callbacks.lock().await.remove(&msg.id) {
                cb(chunk);
            }
        }
        "CHAT_RESPONSE" => {
            // 完整响应
            let content = msg
                .payload
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let model = msg
                .payload
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let usage = msg.payload.get("usage").cloned();
            let finished_reason = msg
                .payload
                .get("finishedReason")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let tool_calls = msg.payload.get("tool_calls").cloned();

            let response = ChatResponsePayload {
                content,
                model,
                usage,
                finished_reason,
                tool_calls,
            };
            if let Some(sender) = pending.lock().await.remove(&msg.id) {
                let _ = sender.send(response);
            }
            chunk_callbacks.lock().await.remove(&msg.id);
        }
        "CHAT_ERROR" => {
            let error = msg
                .payload
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("未知错误")
                .to_string();
            if let Some(sender) = pending.lock().await.remove(&msg.id) {
                let payload = ChatResponsePayload {
                    content: format!("错误: {}", error),
                    model: "unknown".to_string(),
                    usage: None,
                    finished_reason: Some("error".to_string()),
                    tool_calls: None,
                };
                let _ = sender.send(payload);
            }
            chunk_callbacks.lock().await.remove(&msg.id);
        }
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
            let status = msg.payload.clone();
            *latest_status.write().await = status.clone();
            plugin_statuses
                .write()
                .await
                .insert(plugin_id.to_string(), status);
        }
        "CHAT_REQUEST" | "CAPTURE_REQUEST" | "EXECUTE_REQUEST" => {
            // 服务端->插件下行消息，插件不应回发
            debug!("收到来自扩展的意外下行消息类型: {}", msg.type_);
        }
        _ => {
            debug!("未处理的消息类型: {}", msg.type_);
        }
    }
}
