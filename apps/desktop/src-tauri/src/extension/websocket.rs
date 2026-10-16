//! 桌面端 WebSocket bridge：进程内插件唯一的连接通道，维护多个扩展连接并承接网页执行结果。
//!
//! 插件实例的登记、状态与启用选择的唯一归属是 [`super::manager::ExtensionManager`]；
//! 下行请求按扩展及供应商启用/登录状态分配；上行流块按请求 ID
//! 转交等待中的适配器回调，最终结果则完成对应 oneshot 请求。

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, RwLock};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::{accept_hdr_async, connect_async, tungstenite::Message};
use tracing::{error, info, warn};
use uuid::Uuid;

use super::credentials::CredentialStore;
use super::handler::handle_bridge_message;
use super::manager::{ClientInfo, ExtensionManager};
use super::scheduler;
use crate::session::manager::SessionManager;

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
pub struct ExecuteRequestPayload {
    pub provider: String,
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    /// 本次请求所属的会话键（网页会话 id）。调度层用它维持会话粘性，
    /// 让同一会话固定落到同一台插件上（避免网页端会话状态分裂）。
    pub session_key: Option<String>,
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

/// 单台插件的最大在途请求数：超过后不再把新请求分给它，排队等一个空位，
/// 避免把一台插件的浏览器标签页打满。
///
/// 与渠道默认并发（`providers::plugin::DEFAULT_CHANNEL_CONCURRENCY`）对齐：
/// 单台插件要能承接默认并发数的在途请求，否则渠道侧的并发设置达不到。
const MAX_INFLIGHT_PER_PLUGIN: u32 = 5;

pub struct WebSocketBridge {
    pub sender: Arc<Mutex<Option<WsSink>>>,
    pub senders: Arc<Mutex<HashMap<String, WsSink>>>,
    pub pending_execute: PendingExecute,
    pub execute_chunk_callbacks: ExecuteChunkCallbacks,
    pub pending_capture: PendingCapture,
    /// 会话粘性：会话键 → 上次成功执行它的插件 id
    pub sticky_sessions: Arc<RwLock<HashMap<String, String>>>,
    /// 在途请求计数：插件 id → 未完成请求数（流控用）
    pub inflight: Arc<RwLock<HashMap<String, u32>>>,
    pub connected: Arc<RwLock<bool>>,
    /// 插件实例管理器：插件注册表、状态与启用选择的唯一归属
    pub manager: Arc<ExtensionManager>,
    credentials: Arc<CredentialStore>,
    /// 会话库（可空）：用于把调度粘性落库，重启后仍能复用
    sessions: Option<Arc<SessionManager>>,
}

impl WebSocketBridge {
    pub fn new(
        credentials: Arc<CredentialStore>,
        manager: Arc<ExtensionManager>,
        sessions: Option<Arc<SessionManager>>,
    ) -> Self {
        Self {
            sender: Arc::new(Mutex::new(None)),
            senders: Arc::new(Mutex::new(HashMap::new())),
            pending_execute: Arc::new(Mutex::new(HashMap::new())),
            execute_chunk_callbacks: Arc::new(Mutex::new(HashMap::new())),
            pending_capture: Arc::new(Mutex::new(HashMap::new())),
            sticky_sessions: Arc::new(RwLock::new(HashMap::new())),
            inflight: Arc::new(RwLock::new(HashMap::new())),
            connected: Arc::new(RwLock::new(false)),
            manager,
            credentials,
            sessions,
        }
    }

    pub async fn list_plugins(&self) -> Vec<serde_json::Value> {
        self.manager.list()
    }

    pub async fn set_plugin_enabled(&self, id: &str, value: bool) -> anyhow::Result<()> {
        self.manager.set_enabled(id, value)
    }

    pub async fn set_plugin_channel_enabled(
        &self,
        id: &str,
        channel: &str,
        value: bool,
    ) -> anyhow::Result<()> {
        self.manager.set_channel_enabled(id, channel, value)
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
            let client_info = Arc::new(std::sync::Mutex::new(ClientInfo::default()));
            let handshake_info = client_info.clone();
            let handshake =
                move |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
                    if let Some(query) = request.uri().query() {
                        if let Ok(mut slot) = handshake_info.lock() {
                            *slot = parse_client_info(query);
                        }
                    }
                    Ok(response)
                };
            match accept_hdr_async(tokio_tungstenite::MaybeTlsStream::Plain(tcp), handshake).await {
                Ok(ws) => {
                    let (sink, stream) = ws.split();
                    let client = client_info
                        .lock()
                        .map(|info| info.clone())
                        .unwrap_or_default();
                    // 登记插件实例：自报 clientId 时复用同一实例并保留其启用选择
                    let plugin_id = self.manager.register(&client);
                    self.senders.lock().await.insert(plugin_id.clone(), sink);
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

    pub async fn run_listener(
        &self,
        mut stream: WsSplitStream,
        plugin_id: String,
    ) -> anyhow::Result<()> {
        let connected = self.connected.clone();
        let pending_execute = self.pending_execute.clone();
        let execute_chunk_callbacks = self.execute_chunk_callbacks.clone();
        let pending_capture = self.pending_capture.clone();
        let manager = self.manager.clone();
        let senders = self.senders.clone();
        let listener_plugin_id = plugin_id.clone();
        let credentials = self.credentials.clone();

        tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        if let Ok(text_str) = String::from_utf8(text.into()) {
                            handle_bridge_message(
                                &text_str,
                                &pending_execute,
                                &execute_chunk_callbacks,
                                &pending_capture,
                                &manager,
                                &listener_plugin_id,
                                &credentials,
                            )
                            .await;
                        }
                    }
                    Ok(Message::Close(_)) => {
                        senders.lock().await.remove(&listener_plugin_id);
                        *connected.write().await = !senders.lock().await.is_empty();
                        break;
                    }
                    Err(e) => {
                        error!("WebSocket 错误: {}", e);
                        senders.lock().await.remove(&listener_plugin_id);
                        *connected.write().await = !senders.lock().await.is_empty();
                        break;
                    }
                    _ => {}
                }
            }
            // 连接结束：登记为离线，其持久化连接信息与启用选择保留
            manager.disconnect(&plugin_id);
            senders.lock().await.remove(&plugin_id);
            *connected.write().await = !senders.lock().await.is_empty();
        });
        Ok(())
    }

    /// 通过扩展在网页 context 执行一次请求（EXECUTE_REQUEST 下行 → 扩展 fetch → 回传）
    /// 按供应商登录与启用状态选择一个插件执行请求，并等待最终结果或超时。
    /// 流式正文通过 `on_chunk` 逐块送达；完成时由 EXECUTE_RESULT 解除等待。
    ///
    /// `timeout_ms` 的含义按是否流式区分：流式是「静默超时」（连续这么久没有任何分块才算挂死，
    /// 有分块就继续等，网页端长思考 / 长回答可以远超它），非流式是整次请求的总超时。
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

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        self.pending_execute.lock().await.insert(id.clone(), tx);
        // 最近一次收到分块的时间：流式请求靠它把超时从「总耗时」改成「静默时长」。
        let last_chunk_at = Arc::new(AtomicU64::new(now_millis()));
        if let Some(mut cb) = on_chunk {
            let activity = last_chunk_at.clone();
            let tracked: Box<dyn FnMut(String) + Send + Sync> = Box::new(move |delta: String| {
                activity.store(now_millis(), Ordering::Relaxed);
                cb(delta);
            });
            self.execute_chunk_callbacks
                .lock()
                .await
                .insert(id.clone(), tracked);
        }

        let json = serde_json::to_string(&message)?;
        // 选择插件：全部达到并发上限时排队等一个空位（而不是直接失败），直到本次超时
        let deadline =
            tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);
        let mut waited_for_slot = false;
        let target = loop {
            let snapshot = self.manager.snapshot();
            let statuses = &snapshot.statuses;
            let enabled = &snapshot.enabled;
            let provider_enabled = &snapshot.provider_enabled;
            // 该会话上次成功用过的插件（会话粘性）
            let sticky_id = {
                let sticky = self.sticky_sessions.read().await;
                payload
                    .session_key
                    .as_ref()
                    .and_then(|key| sticky.get(key).cloned())
            };
            // 各插件当前在途数（流控）：超过上限的插件不参与本次分配
            let inflight_snapshot = self.inflight.read().await.clone();
            // 把每台插件的实时状态组装成候选快照，交给调度器决策
            // （选择算法是纯函数，见 `extension::scheduler`）
            let candidates: Vec<scheduler::Candidate> = statuses
                .iter()
                .map(|(id, status)| {
                    let runtime = status.get(&payload.provider);
                    // 注意：借用不能穿过 and_then 的闭包（闭包返回值不得借用其参数），
                    // 因此先取出 &Value，再在它上面取字段。
                    let flag = |key: &str| match runtime {
                        Some(item) => item
                            .get(key)
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false),
                        None => false,
                    };
                    let models: Vec<String> = match runtime {
                        Some(item) => item
                            .get("models")
                            .and_then(serde_json::Value::as_array)
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|m| m.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        None => Vec::new(),
                    };
                    let current_inflight = inflight_snapshot.get(id).copied().unwrap_or(0);
                    scheduler::Candidate {
                        plugin_id: id.clone(),
                        // enabled 同时承载「开关打开」与「未超过并发上限」两个条件
                        enabled: enabled.get(id).copied().unwrap_or(true)
                            && current_inflight < MAX_INFLIGHT_PER_PLUGIN,
                        provider_enabled: provider_enabled
                            .get(&format!("{id}:{}", payload.provider))
                            .copied()
                            .unwrap_or(true),
                        connected: flag("connected"),
                        authed: flag("authed"),
                        models,
                        inflight: current_inflight,
                        sticky: sticky_id.as_deref() == Some(id.as_str()),
                        last_ok_at: 0,
                    }
                })
                .collect();
            // 模型能力过滤待接线：EXECUTE_REQUEST 目前不带对外模型名，先不按模型过滤
            let picked = scheduler::pick_plugin(&candidates, "").map(|c| c.plugin_id.clone());
            let Some(picked) = picked else {
                // 没有可分配的插件。要看是「插件都在忙」还是「根本没得用」：
                // 前者排队等空位，后者立即失败（避免白白等满超时）。
                let all_busy = candidates.iter().any(|c| {
                    c.connected
                        && c.authed
                        && c.provider_enabled
                        && c.inflight >= MAX_INFLIGHT_PER_PLUGIN
                });
                if !all_busy {
                    break None;
                }
                if tokio::time::Instant::now() >= deadline {
                    waited_for_slot = true;
                    warn!(
                        "等待插件空位超时：{} 的可用插件在途均已达到上限 {}",
                        payload.provider, MAX_INFLIGHT_PER_PLUGIN
                    );
                    break None;
                }
                waited_for_slot = true;
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                continue;
            };
            // 记录本会话与所选插件的绑定，供后续请求复用（同一会话不跨插件）
            if let Some(key) = payload.session_key.clone() {
                self.sticky_sessions
                    .write()
                    .await
                    .insert(key.clone(), picked.clone());
                // 落库副本：失败不影响本次调度（内存粘性已生效）
                if let Some(sessions) = &self.sessions {
                    if let Err(e) = sessions.set_session_plugin(&key, &picked).await {
                        warn!("会话粘性落库失败: {}", e);
                    }
                }
            }
            break Some(picked);
        };
        // 等空位等到超时：把原因明确带出去（区别于「压根没有可用插件」）
        if target.is_none() && waited_for_slot {
            return Err(anyhow::anyhow!(
                "等待插件空位超时：在途均已达到上限 {}",
                MAX_INFLIGHT_PER_PLUGIN
            ));
        }
        // 占位：选中即计入在途，请求结束（成功 / 失败 / 超时）时释放
        if let Some(id) = &target {
            *self.inflight.write().await.entry(id.clone()).or_insert(0) += 1;
        }
        let sent = if let Some(id) = &target {
            if let Some(sink) = self.senders.lock().await.get_mut(id) {
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

        let outcome = loop {
            let idle_ms = now_millis().saturating_sub(last_chunk_at.load(Ordering::Relaxed));
            if idle_ms >= timeout_ms {
                self.pending_execute.lock().await.remove(&id);
                self.execute_chunk_callbacks.lock().await.remove(&id);
                break Err(anyhow::anyhow!(
                    "请求超时：上游已静默 {} 毫秒没有任何分块",
                    idle_ms
                ));
            }
            tokio::select! {
                result = &mut rx => {
                    self.execute_chunk_callbacks.lock().await.remove(&id);
                    break match result {
                        Ok(payload) => Ok(payload),
                        Err(_) => Err(anyhow::anyhow!("请求已取消")),
                    };
                }
                _ = tokio::time::sleep(tokio::time::Duration::from_millis(timeout_ms - idle_ms)) => {}
            }
        };
        // 释放在途计数
        if let Some(plugin_id) = &target {
            if let Some(n) = self.inflight.write().await.get_mut(plugin_id) {
                *n = n.saturating_sub(1);
            }
        }
        outcome
    }

    /// 请求在线插件重新读取网页凭证（CAPTURE_REQUEST 下行）：
    /// CREDENTIAL_SYNC 到达时按 provider 唤醒等待方，超时则报错。
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
            let snapshot = self.manager.snapshot();
            snapshot
                .statuses
                .iter()
                .find_map(|(id, status)| {
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
    }
}

impl Default for WebSocketBridge {
    fn default() -> Self {
        Self::new(
            Arc::new(CredentialStore::new()),
            Arc::new(ExtensionManager::new(".")),
            None,
        )
    }
}

/// 当前时间（毫秒）：只用于比较先后，不参与展示。
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// 从握手 URL 的查询串解析插件自报身份；未提供的字段保持 `None`。
fn parse_client_info(query: &str) -> ClientInfo {
    let mut info = ClientInfo::default();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let value = value.into_owned();
        match key.as_ref() {
            "clientId" => info.client_id = Some(value),
            "name" => info.name = Some(value),
            "version" => info.version = Some(value),
            "platform" => info.platform = Some(value),
            _ => {}
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use super::{ExecuteRequestPayload, WebSocketBridge, MAX_INFLIGHT_PER_PLUGIN};
    use crate::extension::credentials::CredentialStore;
    use futures_util::{SinkExt, StreamExt};
    use std::sync::Arc;
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    /// mock 插件的连接类型
    type PluginWs =
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

    /// 单插件在途上限必须能承接渠道默认并发，否则渠道侧的并发设置达不到。
    #[test]
    fn inflight_cap_covers_default_channel_concurrency() {
        assert!(
            MAX_INFLIGHT_PER_PLUGIN >= crate::providers::plugin::DEFAULT_CHANNEL_CONCURRENCY,
            "单插件在途上限 {} 低于渠道默认并发 {}",
            MAX_INFLIGHT_PER_PLUGIN,
            crate::providers::plugin::DEFAULT_CHANNEL_CONCURRENCY
        );
    }

    /// 构造使用临时数据目录的桥接实例（插件注册表写入临时目录）。
    fn test_bridge() -> Arc<WebSocketBridge> {
        let dir = std::env::temp_dir().join(format!("poolman-bridge-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(WebSocketBridge::new(
            Arc::new(CredentialStore::new()),
            Arc::new(crate::extension::manager::ExtensionManager::new(
                &dir.to_string_lossy(),
            )),
            None,
        ))
    }

    /// Mock 插件端到端：桥接层按状态选中插件 → 下发 EXECUTE_REQUEST →
    /// 插件回 EXECUTE_RESULT → send_execute 返回结果，并记下会话粘性。
    #[tokio::test]
    async fn mock_plugin_roundtrip() {
        let bridge = test_bridge();

        // 取一个空闲端口再让桥接层监听它（listen 是无限循环，放到后台任务）
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);
        {
            let bridge = bridge.clone();
            tokio::spawn(async move {
                let _ = bridge.listen(&addr.to_string()).await;
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // mock 插件：连上来并上报「deepseek-web 已连接且已登录」
        let (mut ws, _) = connect_async(format!("ws://{}", addr))
            .await
            .expect("mock 插件应能连上桥接层");
        ws.send(Message::Text(
            serde_json::json!({
                "id": "mock-status",
                "type": "STATUS_SYNC",
                "timestamp": 1,
                "payload": {
                    "deepseek-web": { "connected": true, "authed": true, "models": ["deepseek-web"] }
                }
            })
            .to_string(),
        ))
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // 一边发起执行，一边由 mock 插件应答
        let exec = {
            let bridge = bridge.clone();
            tokio::spawn(async move {
                bridge
                    .send_execute(
                        ExecuteRequestPayload {
                            provider: "deepseek-web".to_string(),
                            url: "https://example.test/v0/chat/completion".to_string(),
                            method: "POST".to_string(),
                            headers: vec![],
                            body: "{}".to_string(),
                            session_key: Some("sess-e2e".to_string()),
                        },
                        5_000,
                        None,
                    )
                    .await
            })
        };

        let incoming = tokio::time::timeout(std::time::Duration::from_secs(3), ws.next())
            .await
            .expect("应收到下发的执行请求")
            .expect("连接内有消息")
            .expect("消息解析成功");
        let text = match incoming {
            Message::Text(t) => t.to_string(),
            other => panic!("应收到文本消息，实际: {:?}", other),
        };
        let request: serde_json::Value = serde_json::from_str(&text).expect("请求应为 JSON");
        assert_eq!(request["type"], "EXECUTE_REQUEST");
        assert_eq!(request["payload"]["provider"], "deepseek-web");
        let request_id = request["id"].as_str().expect("请求带 id").to_string();

        ws.send(Message::Text(
            serde_json::json!({
                "id": request_id,
                "type": "EXECUTE_RESULT",
                "timestamp": 2,
                "payload": { "status": 200, "ok": true, "body": "data: hello\n\n" }
            })
            .to_string(),
        ))
        .await
        .unwrap();

        let outcome = exec.await.expect("执行任务未 panic").expect("执行应成功");
        assert!(outcome.ok, "状态应为成功");
        assert_eq!(outcome.status, 200);
        assert!(outcome.body.contains("hello"), "应带回响应体");

        // 会话粘性已记录（插件 id 由桥接层生成，此处只校验键存在）
        let sticky = bridge.sticky_sessions.read().await;
        assert!(
            sticky.contains_key("sess-e2e"),
            "应记录会话与插件的绑定: {:?}",
            sticky
        );
    }

    /// 启动桥接层并等它就绪，返回监听地址；同时连一台 mock 插件并上报「已连接且已登录」。
    async fn setup_bridge_with_plugin(bridge: &Arc<WebSocketBridge>) -> (String, PluginWs) {
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);
        {
            let bridge = bridge.clone();
            tokio::spawn(async move {
                let _ = bridge.listen(&addr.to_string()).await;
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let (mut ws, _) = connect_async(format!("ws://{}", addr))
            .await
            .expect("mock 插件应能连上桥接层");
        ws.send(Message::Text(
            serde_json::json!({
                "id": "mock-status",
                "type": "STATUS_SYNC",
                "timestamp": 1,
                "payload": {
                    "deepseek-web": { "connected": true, "authed": true, "models": [] }
                }
            })
            .to_string(),
        ))
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        (format!("ws://{}", addr), ws)
    }

    /// 从插件侧读一条消息并取出它的 id（用于回 EXECUTE_CHUNK / EXECUTE_RESULT）。
    async fn read_request_id(ws: &mut PluginWs) -> String {
        let incoming = tokio::time::timeout(std::time::Duration::from_secs(3), ws.next())
            .await
            .expect("应收到下发的执行请求")
            .expect("连接内有消息")
            .expect("消息解析成功");
        let text = match incoming {
            Message::Text(t) => t.to_string(),
            other => panic!("应收到文本消息，实际: {:?}", other),
        };
        let request: serde_json::Value = serde_json::from_str(&text).expect("请求应为 JSON");
        assert_eq!(request["type"], "EXECUTE_REQUEST");
        request["id"].as_str().expect("请求带 id").to_string()
    }

    fn execute_payload() -> ExecuteRequestPayload {
        ExecuteRequestPayload {
            provider: "deepseek-web".to_string(),
            url: "https://example.test/v0/chat/completion".to_string(),
            method: "POST".to_string(),
            headers: vec![],
            body: "{}".to_string(),
            session_key: None,
        }
    }

    /// 流式分块：插件逐块回 EXECUTE_CHUNK，回调按序收到；完整响应体原样带回。
    #[tokio::test]
    async fn streams_chunks_in_order() {
        let bridge = test_bridge();
        let (addr, mut ws) = setup_bridge_with_plugin(&bridge).await;

        let chunks: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = chunks.clone();
        let exec = {
            let bridge = bridge.clone();
            tokio::spawn(async move {
                bridge
                    .send_execute(
                        execute_payload(),
                        5_000,
                        Some(Box::new(move |chunk: String| {
                            sink.lock().unwrap().push(chunk);
                        })),
                    )
                    .await
            })
        };

        let request_id = read_request_id(&mut ws).await;

        for chunk in ["第一块", "第二块"] {
            ws.send(Message::Text(
                serde_json::json!({
                    "id": request_id,
                    "type": "EXECUTE_CHUNK",
                    "timestamp": 2,
                    "payload": { "data": chunk }
                })
                .to_string(),
            ))
            .await
            .unwrap();
        }

        let body = "<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"filePath\": \"a.rs\"}}\n</tool_call>";
        ws.send(Message::Text(
            serde_json::json!({
                "id": request_id,
                "type": "EXECUTE_RESULT",
                "timestamp": 3,
                "payload": { "status": 200, "ok": true, "body": body }
            })
            .to_string(),
        ))
        .await
        .unwrap();

        let outcome = exec.await.expect("任务未 panic").expect("执行应成功");
        // 完整响应体原样带回（围栏等细节由上层解析，桥接层不改动内容）
        assert!(outcome.body.contains("<tool_call>"));
        assert_eq!(
            chunks.lock().unwrap().clone(),
            vec!["第一块".to_string(), "第二块".to_string()],
            "回调应按序收到每个增量块"
        );
        assert!(
            bridge.inflight.read().await.values().all(|n| *n == 0),
            "结束后应释放在途计数 (addr={})",
            addr
        );
    }

    /// 超时：插件收到请求却沉默时，send_execute 应超时失败，并释放在途计数。
    #[tokio::test]
    async fn times_out_when_plugin_silent() {
        let bridge = test_bridge();
        let (_addr, mut ws) = setup_bridge_with_plugin(&bridge).await;

        let exec = {
            let bridge = bridge.clone();
            tokio::spawn(async move { bridge.send_execute(execute_payload(), 200, None).await })
        };

        // 收到请求但故意不回结果
        let _ = read_request_id(&mut ws).await;

        let outcome = exec.await.expect("任务未 panic");
        assert!(outcome.is_err(), "插件沉默时应失败");
        let message = format!("{}", outcome.expect_err("应为 Err"));
        assert!(message.contains("超时"), "错误信息应说明超时: {}", message);
        assert!(
            bridge.inflight.read().await.values().all(|n| *n == 0),
            "超时后应释放在途计数"
        );
    }

    /// 流式：分块持续到达时按「静默」判超时，整轮耗时超过 timeout_ms 也不中断。
    #[tokio::test]
    async fn streaming_survives_past_total_timeout() {
        let bridge = test_bridge();
        let (_addr, mut ws) = setup_bridge_with_plugin(&bridge).await;

        let chunks = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = chunks.clone();
        let exec = {
            let bridge = bridge.clone();
            tokio::spawn(async move {
                bridge
                    .send_execute(
                        execute_payload(),
                        200,
                        Some(Box::new(move |chunk: String| {
                            sink.lock().unwrap().push(chunk);
                        })),
                    )
                    .await
            })
        };

        let request_id = read_request_id(&mut ws).await;
        // 每 100ms 回一块，持续 800ms（远超 200ms 的总超时）：静默始终小于 200ms
        for index in 0..8 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            ws.send(Message::Text(
                serde_json::json!({
                    "id": request_id,
                    "type": "EXECUTE_CHUNK",
                    "timestamp": 2,
                    "payload": { "data": format!("块{}", index) }
                })
                .to_string(),
            ))
            .await
            .unwrap();
        }
        ws.send(Message::Text(
            serde_json::json!({
                "id": request_id,
                "type": "EXECUTE_RESULT",
                "timestamp": 3,
                "payload": { "status": 200, "ok": true, "body": "" }
            })
            .to_string(),
        ))
        .await
        .unwrap();

        let outcome = exec.await.expect("任务未 panic").expect("持续出块时不应超时");
        assert!(outcome.ok);
        assert_eq!(chunks.lock().unwrap().len(), 8, "8 个分块都应送达");
    }

    /// 流式：收到分块后再次静默超过 timeout_ms，仍应超时失败（静默判定不是无超时）。
    #[tokio::test]
    async fn streaming_times_out_when_upstream_goes_silent() {
        let bridge = test_bridge();
        let (_addr, mut ws) = setup_bridge_with_plugin(&bridge).await;

        let exec = {
            let bridge = bridge.clone();
            tokio::spawn(async move {
                bridge
                    .send_execute(execute_payload(), 200, Some(Box::new(|_delta: String| {})))
                    .await
            })
        };

        let request_id = read_request_id(&mut ws).await;
        ws.send(Message::Text(
            serde_json::json!({
                "id": request_id,
                "type": "EXECUTE_CHUNK",
                "timestamp": 2,
                "payload": { "data": "第一块" }
            })
            .to_string(),
        ))
        .await
        .unwrap();

        let outcome = exec.await.expect("任务未 panic");
        let message = format!("{}", outcome.expect_err("静默超过静默容忍时应失败"));
        assert!(message.contains("静默"), "{}", message);
        assert!(message.contains("超时"), "{}", message);
    }

    /// 并发上限：占满后新请求不是立刻失败，而是排队等空位，直到本次超时。
    #[tokio::test]
    async fn queues_when_all_plugins_are_busy() {
        let bridge = test_bridge();
        let (_addr, mut ws) = setup_bridge_with_plugin(&bridge).await;

        // 用长超时把在途占满（插件读到请求但不回结果，请求会一直挂着）
        let mut holders = Vec::new();
        for _ in 0..MAX_INFLIGHT_PER_PLUGIN {
            let bridge = bridge.clone();
            holders.push(tokio::spawn(async move {
                let _ = bridge.send_execute(execute_payload(), 30_000, None).await;
            }));
            let _ = read_request_id(&mut ws).await;
        }
        // 等在途计数确实达到上限
        for _ in 0..40 {
            let full = bridge
                .inflight
                .read()
                .await
                .values()
                .any(|n| *n >= MAX_INFLIGHT_PER_PLUGIN);
            if full {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            bridge
                .inflight
                .read()
                .await
                .values()
                .any(|n| *n >= MAX_INFLIGHT_PER_PLUGIN),
            "应已占满在途"
        );

        // 再来一个：应排队等空位，超时后说明是「等空位」而非「没有插件」
        let err = bridge
            .send_execute(execute_payload(), 300, None)
            .await
            .expect_err("空位不足时应超时失败");
        let message = format!("{}", err);
        assert!(message.contains("等待插件空位"), "应说明在等空位: {}", message);

        for holder in holders {
            holder.abort();
        }
    }
}
