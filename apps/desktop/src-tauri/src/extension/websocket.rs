use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio_tungstenite::{accept_async, connect_async, tungstenite::Message};
use tracing::{error, info, warn};
use uuid::Uuid;

use super::credentials::CredentialStore;
use super::handler::handle_bridge_message;

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ChatChunkPayload {
    pub delta: Option<String>,
    pub tool_calls: Option<Value>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ChatResponsePayload {
    pub content: String,
    pub model: String,
    pub usage: Option<serde_json::Value>,
    pub finished_reason: Option<String>,
    pub tool_calls: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct ChatRequestPayload {
    pub model: String,
    pub messages: Vec<serde_json::Value>,
    pub options: Option<serde_json::Value>,
}

pub type PendingRequests =
    Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<ChatResponsePayload>>>>;
pub type ChunkCallbacks =
    Arc<Mutex<HashMap<String, Box<dyn FnMut(ChatChunkPayload) + Send + Sync>>>>;

#[derive(Debug, Clone)]
pub struct ExecuteRequestPayload {
    pub provider: String,
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct ExecuteResultPayload {
    pub ok: bool,
    pub status: u16,
    pub body: String,
}

pub type PendingExecute =
    Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<ExecuteResultPayload>>>>;
pub type ExecuteChunkCallbacks = Arc<Mutex<HashMap<String, Box<dyn FnMut(String) + Send + Sync>>>>;
pub type PendingCapture = Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>>;

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type WsSink = futures_util::stream::SplitSink<WsStream, Message>;
type WsSplitStream = futures_util::stream::SplitStream<WsStream>;

pub struct WebSocketBridge {
    pub sender: Arc<Mutex<Option<WsSink>>>,
    pub senders: Arc<Mutex<HashMap<String, WsSink>>>,
    pub plugin_statuses: Arc<RwLock<HashMap<String, serde_json::Value>>>,
    pub enabled_plugins: Arc<RwLock<HashMap<String, bool>>>,
    pub enabled_providers: Arc<RwLock<HashMap<String, bool>>>,
    pub pending: PendingRequests,
    pub chunk_callbacks: ChunkCallbacks,
    pub pending_execute: PendingExecute,
    pub execute_chunk_callbacks: ExecuteChunkCallbacks,
    pub pending_capture: PendingCapture,
    pub connected: Arc<RwLock<bool>>,
    pub latest_status: Arc<RwLock<serde_json::Value>>,
    credentials: Arc<CredentialStore>,
}

impl WebSocketBridge {
    pub fn new(credentials: Arc<CredentialStore>) -> Self {
        Self {
            sender: Arc::new(Mutex::new(None)),
            senders: Arc::new(Mutex::new(HashMap::new())),
            plugin_statuses: Arc::new(RwLock::new(HashMap::new())),
            enabled_plugins: Arc::new(RwLock::new(HashMap::new())),
            enabled_providers: Arc::new(RwLock::new(HashMap::new())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            chunk_callbacks: Arc::new(Mutex::new(HashMap::new())),
            pending_execute: Arc::new(Mutex::new(HashMap::new())),
            execute_chunk_callbacks: Arc::new(Mutex::new(HashMap::new())),
            pending_capture: Arc::new(Mutex::new(HashMap::new())),
            connected: Arc::new(RwLock::new(false)),
            latest_status: Arc::new(RwLock::new(serde_json::json!({}))),
            credentials,
        }
    }

    pub async fn list_plugins(&self) -> Vec<serde_json::Value> {
        let statuses = self.plugin_statuses.read().await;
        let enabled = self.enabled_plugins.read().await;
        let provider_enabled = self.enabled_providers.read().await;
        statuses
            .iter()
            .map(|(id, providers)| {
                let mut provider_states = providers.clone();
                if let Some(map) = provider_states.as_object_mut() {
                    for (provider, item) in map.iter_mut() {
                        if let Some(item) = item.as_object_mut() {
                            item.insert(
                                "desktopEnabled".to_string(),
                                serde_json::json!(provider_enabled
                                    .get(&format!("{id}:{provider}"))
                                    .copied()
                                    .unwrap_or(true)),
                            );
                        }
                    }
                }
                serde_json::json!({
                    "id": id,
                    "enabled": enabled.get(id).copied().unwrap_or(true),
                    "providers": provider_states,
                })
            })
            .collect()
    }

    pub async fn set_plugin_enabled(&self, id: &str, value: bool) -> anyhow::Result<()> {
        if !self.plugin_statuses.read().await.contains_key(id) {
            anyhow::bail!("插件连接已断开");
        }
        self.enabled_plugins
            .write()
            .await
            .insert(id.to_string(), value);
        Ok(())
    }

    pub async fn set_plugin_provider_enabled(
        &self,
        id: &str,
        provider: &str,
        value: bool,
    ) -> anyhow::Result<()> {
        let key = format!("{id}:{provider}");
        self.enabled_providers.write().await.insert(key, value);
        Ok(())
    }

    pub async fn send_provider_control(
        &self,
        id: &str,
        provider: &str,
        action: &str,
    ) -> anyhow::Result<()> {
        let message = serde_json::json!({
            "id": Uuid::new_v4().to_string(),
            "type": "PROVIDER_CONTROL",
            "timestamp": chrono::Utc::now().timestamp_millis(),
            "payload": { "provider": provider, "action": action }
        });
        let text = serde_json::to_string(&message)?;
        let mut senders = self.senders.lock().await;
        let sink = senders
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("插件连接已断开"))?;
        sink.send(Message::Text(text)).await?;
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn connect(&self, url: &str) -> anyhow::Result<()> {
        let url = url::Url::parse(url)?;
        let (ws_stream, _) = connect_async(url.clone()).await?;
        let (sink, _stream) = ws_stream.split();
        *self.sender.lock().await = Some(sink);
        *self.connected.write().await = true;
        info!("WebSocket 已连接到 {}", url);
        Ok(())
    }

    /// 监听扩展 WebSocket 连接（服务端模式）：扩展作为客户端连接 ws://host:ws_port
    pub async fn listen(&self, addr: &str) -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        info!("WebSocket 服务端正在监听 ws://{}", addr);
        loop {
            let (tcp, _) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    warn!("WebSocket 接受连接错误: {}", e);
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                    continue;
                }
            };
            match accept_async(tokio_tungstenite::MaybeTlsStream::Plain(tcp)).await {
                Ok(ws) => {
                    let (sink, stream) = ws.split();
                    let plugin_id = Uuid::new_v4().to_string();
                    self.senders.lock().await.insert(plugin_id.clone(), sink);
                    self.plugin_statuses
                        .write()
                        .await
                        .insert(plugin_id.clone(), serde_json::json!({}));
                    *self.connected.write().await = true;
                    info!("WebSocket 插件已连接: {}", plugin_id);
                    if let Err(e) = self.run_listener(stream, plugin_id).await {
                        warn!("WebSocket 监听错误: {}", e);
                    }
                }
                Err(e) => {
                    warn!("WebSocket 握手错误: {}", e);
                }
            }
        }
    }

    pub async fn send_request(
        &self,
        payload: ChatRequestPayload,
        timeout_ms: u64,
        on_chunk: Option<Box<dyn FnMut(ChatChunkPayload) + Send + Sync>>,
    ) -> anyhow::Result<ChatResponsePayload> {
        let id = Uuid::new_v4().to_string();
        let message = serde_json::json!({
            "id": id,
            "type": "CHAT_REQUEST",
            "timestamp": chrono::Utc::now().timestamp_millis(),
            "payload": {
                "model": payload.model,
                "messages": payload.messages,
                "options": payload.options,
            }
        });

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);

        if let Some(cb) = on_chunk {
            self.chunk_callbacks.lock().await.insert(id.clone(), cb);
        }

        let json = serde_json::to_string(&message)?;
        let provider = match payload.model.as_str() {
            "openai" | "chatgpt" => "chatgpt-web",
            "claude" => "claude-web",
            "gemini" => "gemini-web",
            "kimi" => "kimi-web",
            "qwen" => "qwen-web",
            "deepseek" => "deepseek-web",
            other => other,
        };
        let selected = {
            let statuses = self.plugin_statuses.read().await;
            let enabled = self.enabled_plugins.read().await;
            let provider_enabled = self.enabled_providers.read().await;
            statuses.iter().find_map(|(id, status)| {
                if !enabled.get(id).copied().unwrap_or(true) {
                    return None;
                }
                if !provider_enabled
                    .get(&format!("{id}:{provider}"))
                    .copied()
                    .unwrap_or(true)
                {
                    return None;
                }
                status
                    .get(provider)
                    .and_then(|item| item.get("authed"))
                    .and_then(serde_json::Value::as_bool)
                    .filter(|authed| *authed)
                    .map(|_| id.clone())
            })
        };
        let sent = if let Some(plugin_id) = selected {
            if let Some(sink) = self.senders.lock().await.get_mut(&plugin_id) {
                sink.send(Message::Text(json)).await?;
                true
            } else {
                false
            }
        } else if self.senders.lock().await.is_empty() {
            if let Some(sink) = self.sender.lock().await.as_mut() {
                sink.send(Message::Text(json)).await?;
                true
            } else {
                false
            }
        } else {
            false
        };
        if !sent {
            self.pending.lock().await.remove(&id);
            self.chunk_callbacks.lock().await.remove(&id);
            return Err(anyhow::anyhow!("没有可分配该供应商请求的已连接插件"));
        }

        tokio::select! {
            result = rx => {
                self.chunk_callbacks.lock().await.remove(&id);
                match result {
                    Ok(payload) => Ok(payload),
                    Err(_) => Err(anyhow::anyhow!("请求已取消")),
                }
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(timeout_ms)) => {
                self.pending.lock().await.remove(&id);
                self.chunk_callbacks.lock().await.remove(&id);
                Err(anyhow::anyhow!("请求超时"))
            }
        }
    }

    #[allow(dead_code)]
    pub async fn emit_chunk(&self, id: &str, payload: ChatChunkPayload) {
        if let Some(mut cb) = self.chunk_callbacks.lock().await.remove(id) {
            cb(payload);
        }
    }

    #[allow(dead_code)]
    pub async fn resolve_response(&self, id: &str, payload: ChatResponsePayload) {
        if let Some(sender) = self.pending.lock().await.remove(id) {
            let _ = sender.send(payload);
        }
        self.chunk_callbacks.lock().await.remove(id);
    }

    #[allow(dead_code)]
    pub async fn reject_response(&self, id: &str, error: &str) {
        if let Some(sender) = self.pending.lock().await.remove(id) {
            let payload = ChatResponsePayload {
                content: format!("错误: {}", error),
                model: "unknown".to_string(),
                usage: None,
                finished_reason: Some("error".to_string()),
                tool_calls: None,
            };
            let _ = sender.send(payload);
        }
        self.chunk_callbacks.lock().await.remove(id);
    }

    pub async fn run_listener(
        &self,
        mut stream: WsSplitStream,
        plugin_id: String,
    ) -> anyhow::Result<()> {
        let connected = self.connected.clone();
        let pending = self.pending.clone();
        let chunk_callbacks = self.chunk_callbacks.clone();
        let pending_execute = self.pending_execute.clone();
        let execute_chunk_callbacks = self.execute_chunk_callbacks.clone();
        let pending_capture = self.pending_capture.clone();
        let latest_status = self.latest_status.clone();
        let plugin_statuses = self.plugin_statuses.clone();
        let senders = self.senders.clone();
        let enabled_plugins = self.enabled_plugins.clone();
        let listener_plugin_id = plugin_id.clone();
        let credentials = self.credentials.clone();

        tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        if let Ok(text_str) = String::from_utf8(text.into()) {
                            handle_bridge_message(
                                &text_str,
                                &pending,
                                &chunk_callbacks,
                                &pending_execute,
                                &execute_chunk_callbacks,
                                &pending_capture,
                                &latest_status,
                                &plugin_statuses,
                                &listener_plugin_id,
                                &credentials,
                            )
                            .await;
                        }
                    }
                    Ok(Message::Close(_)) => {
                        plugin_statuses.write().await.remove(&listener_plugin_id);
                        senders.lock().await.remove(&listener_plugin_id);
                        enabled_plugins.write().await.remove(&listener_plugin_id);
                        plugin_statuses.write().await.remove(&listener_plugin_id);
                        *connected.write().await = !senders.lock().await.is_empty();
                        break;
                    }
                    Err(e) => {
                        error!("WebSocket 错误: {}", e);
                        plugin_statuses.write().await.remove(&listener_plugin_id);
                        senders.lock().await.remove(&listener_plugin_id);
                        enabled_plugins.write().await.remove(&listener_plugin_id);
                        *connected.write().await = !senders.lock().await.is_empty();
                        break;
                    }
                    _ => {}
                }
            }
            plugin_statuses.write().await.remove(&plugin_id);
            senders.lock().await.remove(&plugin_id);
            enabled_plugins.write().await.remove(&plugin_id);
            *connected.write().await = !senders.lock().await.is_empty();
        });
        Ok(())
    }

    /// 通过扩展在网页 context 执行一次请求（EXECUTE_REQUEST 下行 → 扩展 fetch → 回传）
    pub async fn send_execute(
        &self,
        payload: ExecuteRequestPayload,
        timeout_ms: u64,
        on_chunk: Option<Box<dyn FnMut(String) + Send + Sync>>,
    ) -> anyhow::Result<ExecuteResultPayload> {
        let id = Uuid::new_v4().to_string();
        // headers 以对象形式发送（与扩展协议一致：Record<string,string>）
        let headers_obj = serde_json::Map::from_iter(
            payload
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone()))),
        );
        // GET/HEAD 请求不携带 body：浏览器 fetch 禁止 GET/HEAD 携带 body（即使空字符串也报错），
        // 否则 fetch_history 等 GET 辅助接口会失败，导致 parent_message_id 无法获取
        let method_upper = payload.method.to_uppercase();
        let body_field = if method_upper == "GET" || method_upper == "HEAD" {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(payload.body)
        };
        let message = serde_json::json!({
            "id": id,
            "type": "EXECUTE_REQUEST",
            "timestamp": chrono::Utc::now().timestamp_millis(),
            "payload": {
                "provider": payload.provider,
                "method": payload.method,
                "url": payload.url,
                "headers": headers_obj,
                "body": body_field,
                "streaming": on_chunk.is_some(),
            }
        });

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending_execute.lock().await.insert(id.clone(), tx);
        if let Some(cb) = on_chunk {
            self.execute_chunk_callbacks
                .lock()
                .await
                .insert(id.clone(), cb);
        }

        let json = serde_json::to_string(&message)?;
        let target = {
            let statuses = self.plugin_statuses.read().await;
            let enabled = self.enabled_plugins.read().await;
            let provider_enabled = self.enabled_providers.read().await;
            statuses.iter().find_map(|(id, status)| {
                if !enabled.get(id).copied().unwrap_or(true) {
                    return None;
                }
                if !provider_enabled
                    .get(&format!("{id}:{}", payload.provider))
                    .copied()
                    .unwrap_or(true)
                {
                    return None;
                }
                status
                    .get(&payload.provider)
                    .and_then(|item| item.get("authed"))
                    .and_then(serde_json::Value::as_bool)
                    .filter(|authed| *authed)
                    .map(|_| id.clone())
            })
        };
        let sent = if let Some(id) = target {
            if let Some(sink) = self.senders.lock().await.get_mut(&id) {
                sink.send(Message::Text(json)).await?;
                true
            } else {
                false
            }
        } else if self.senders.lock().await.is_empty() {
            if let Some(sink) = self.sender.lock().await.as_mut() {
                sink.send(Message::Text(json)).await?;
                true
            } else {
                false
            }
        } else {
            false
        };
        if !sent {
            self.pending_execute.lock().await.remove(&id);
            self.execute_chunk_callbacks.lock().await.remove(&id);
            return Err(anyhow::anyhow!("没有可分配该供应商请求的已连接插件"));
        }

        tokio::select! {
            result = rx => {
                self.execute_chunk_callbacks.lock().await.remove(&id);
                match result {
                    Ok(payload) => Ok(payload),
                    Err(_) => Err(anyhow::anyhow!("请求已取消")),
                }
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(timeout_ms)) => {
                self.pending_execute.lock().await.remove(&id);
                self.execute_chunk_callbacks.lock().await.remove(&id);
                Err(anyhow::anyhow!("请求超时"))
            }
        }
    }

    /// 请求扩展捕获指定 provider 的凭证（CAPTURE_REQUEST 下行；扩展上报 CREDENTIAL_SYNC 后完成）
    pub async fn request_credentials(&self, provider: &str, timeout_ms: u64) -> anyhow::Result<()> {
        let id = provider.to_string();
        let message = serde_json::json!({
            "id": id,
            "type": "CAPTURE_REQUEST",
            "timestamp": chrono::Utc::now().timestamp_millis(),
            "payload": { "provider": provider }
        });

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending_capture.lock().await.insert(id.clone(), tx);

        let json = serde_json::to_string(&message)?;
        let target = {
            let statuses = self.plugin_statuses.read().await;
            statuses.iter().find_map(|(id, status)| {
                status
                    .get(provider)
                    .and_then(|item| item.get("connected"))
                    .and_then(serde_json::Value::as_bool)
                    .filter(|connected| *connected)
                    .map(|_| id.clone())
            })
        };
        let sent = if let Some(id) = target {
            if let Some(sink) = self.senders.lock().await.get_mut(&id) {
                sink.send(Message::Text(json)).await?;
                true
            } else {
                false
            }
        } else if let Some(sink) = self.sender.lock().await.as_mut() {
            sink.send(Message::Text(json)).await?;
            true
        } else {
            false
        };
        if !sent {
            self.pending_capture.lock().await.remove(&id);
            return Err(anyhow::anyhow!("没有可用的已连接插件"));
        }

        tokio::select! {
            _ = rx => Ok(()),
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(timeout_ms)) => {
                self.pending_capture.lock().await.remove(&id);
                Err(anyhow::anyhow!("凭证捕获超时"))
            }
        }
    }

    #[allow(dead_code)]
    pub async fn stop(&self) {
        *self.connected.write().await = false;
        if let Some(mut sink) = self.sender.lock().await.take() {
            let _ = sink.close().await;
        }
        self.pending.lock().await.clear();
        self.chunk_callbacks.lock().await.clear();
    }
}

impl Default for WebSocketBridge {
    fn default() -> Self {
        Self::new(Arc::new(CredentialStore::new()))
    }
}
