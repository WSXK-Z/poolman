//! 插件渠道执行器：把模型侧的渠道执行端口 [`ChannelExecutor`] 接到浏览器插件连接。
//!
//! 模型只认端口；本实现把渠道请求经插件连接（`EXECUTE_REQUEST`）送到已登录网页执行，
//! 并把返回映射为渠道响应。凭证按渠道从桌面端凭证库读取。
//!
//! 具体由哪台插件实例执行由调度层决定，本执行器不选择实例，只发请求。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use futures_util::future::BoxFuture;
use serde_json::Value;
use tracing::{debug, warn};

use crate::extension::credentials::CredentialStore;
use crate::extension::websocket::{ExecuteRequestPayload, ExecuteResultPayload, WebSocketBridge};
use crate::models::defs::{ChannelExecutor, ChannelRequest, ChannelResponse, ChunkSink};

/// 插件渠道执行器：绑定一个渠道（如 `deepseek-web`）。
pub struct PluginChannelExecutor {
    bridge: Arc<WebSocketBridge>,
    credentials: Arc<CredentialStore>,
    /// 渠道标识（同时作为 `EXECUTE_REQUEST` 的 provider 与凭证键）。
    channel: String,
    /// 单次请求超时（毫秒）：非流式是总超时，流式是静默超时。
    timeout_ms: u64,
    /// 流式请求的静默容忍（毫秒）：这么久没有任何分块才算上游挂死。
    stream_idle_timeout_ms: u64,
}

impl PluginChannelExecutor {
    pub fn new(
        bridge: Arc<WebSocketBridge>,
        credentials: Arc<CredentialStore>,
        channel: impl Into<String>,
        timeout_ms: u64,
    ) -> Self {
        Self {
            bridge,
            credentials,
            channel: channel.into(),
            timeout_ms,
            stream_idle_timeout_ms: timeout_ms,
        }
    }

    /// 单独指定流式请求的静默容忍：网页端长思考 / 长回答期间可以长时间不出内容，
    /// 用非流式的总超时会把它掐断。
    pub fn with_stream_idle_timeout(mut self, stream_idle_timeout_ms: u64) -> Self {
        self.stream_idle_timeout_ms = stream_idle_timeout_ms;
        self
    }

    /// 请求插件重新捕获本渠道凭证：CAPTURE_REQUEST 下行，插件上报 CREDENTIAL_SYNC 后返回。
    #[allow(dead_code)]
    pub async fn request_credentials(&self) -> Result<()> {
        self.bridge
            .request_credentials(&self.channel, self.timeout_ms)
            .await
    }

    /// 组装发送给插件的执行负载（GET/HEAD 无体由桥接层处理）。
    fn payload(&self, request: ChannelRequest) -> ExecuteRequestPayload {
        ExecuteRequestPayload {
            provider: self.channel.clone(),
            url: request.url,
            method: request.method,
            headers: request.headers,
            body: request.body.unwrap_or_default(),
            session_key: request.session_key,
        }
    }

    /// 校验渠道当前可用（有插件在线且该渠道已连接）。
    fn check_available(&self) -> Result<()> {
        let status = self.bridge.manager.latest_status();
        match availability_error(&status, &self.channel) {
            Some(message) => Err(anyhow!(message)),
            None => Ok(()),
        }
    }
}

impl ChannelExecutor for PluginChannelExecutor {
    fn token(&self) -> Option<String> {
        self.credentials.get_token(&self.channel)
    }

    fn settings_token(&self) -> Option<String> {
        self.credentials.get_settings_token(&self.channel)
    }

    fn execute(&self, request: ChannelRequest) -> BoxFuture<'_, Result<ChannelResponse>> {
        Box::pin(async move {
            let url = request.url.clone();
            if let Err(error) = self.check_available() {
                return Err(log_failure(&self.channel, &url, error));
            }
            let result = self
                .bridge
                .send_execute(self.payload(request), self.timeout_ms, None)
                .await;
            log_result(&self.channel, &url, None, &result);
            Ok(to_response(result?))
        })
    }

    fn execute_stream(
        &self,
        request: ChannelRequest,
        on_chunk: ChunkSink,
    ) -> BoxFuture<'_, Result<ChannelResponse>> {
        Box::pin(async move {
            let url = request.url.clone();
            if let Err(error) = self.check_available() {
                return Err(log_failure(&self.channel, &url, error));
            }
            // 流式结果里没有完整正文（最终响应体为空），故记录分块数与本轮首个分块：
            // 结果为空时靠它们区分「插件没回正文」与「正文被解析掉」。
            let chunks = Arc::new(AtomicUsize::new(0));
            let first_chunk = Arc::new(Mutex::new(String::new()));
            let counter = chunks.clone();
            let first_sink = first_chunk.clone();
            let mut on_chunk = on_chunk;
            let counting: ChunkSink = Box::new(move |delta: String| {
                counter.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut first) = first_sink.lock() {
                    if first.is_empty() {
                        *first = head(&delta, 200);
                    }
                }
                on_chunk(delta);
            });
            let result = self
                .bridge
                .send_execute(
                    self.payload(request),
                    self.stream_idle_timeout_ms,
                    Some(counting),
                )
                .await;
            let stream = (
                chunks.load(Ordering::Relaxed).to_string(),
                first_chunk.lock().unwrap().clone(),
            );
            log_result(&self.channel, &url, Some(stream), &result);
            Ok(to_response(result?))
        })
    }
}

/// 记录插件调用结果（状态、正文长度、分块数）。
///
/// 正常的调用是高频常规事件，降到 `debug` 级别：默认日志里不出现，排查时才打开；
/// 失败仍然在 `warn`，绝不会被静默。正文不进日志（可能含对话内容），只在需要时看分块数。
fn log_result(
    channel: &str,
    url: &str,
    stream: Option<(String, String)>,
    result: &Result<ExecuteResultPayload>,
) {
    let (chunks, first_chunk) = match stream {
        Some((chunks, first)) => (chunks, non_empty_or(&first)),
        None => ("-".to_string(), "-".to_string()),
    };
    match result {
        Ok(payload) => debug!(
            "插件调用完成 channel={} url={} status={} ok={} body_len={} chunks={} first_chunk={}",
            channel,
            url,
            payload.status,
            payload.ok,
            payload.body.len(),
            chunks,
            first_chunk,
        ),
        Err(error) => warn!("插件调用失败 channel={} url={} err={}", channel, url, error),
    }
}

/// 空串用 `-` 占位，便于日志里一眼看出「没有分块」。
fn non_empty_or(text: &str) -> String {
    if text.is_empty() {
        "-".to_string()
    } else {
        text.to_string()
    }
}

/// 记录插件不可用并原样返回错误。
fn log_failure(channel: &str, url: &str, error: anyhow::Error) -> anyhow::Error {
    warn!("插件调用失败 channel={} url={} err={}", channel, url, error);
    error
}

/// 取正文开头（按字符截断，不切断多字节字符）。
fn head(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// 执行结果 → 渠道响应。
fn to_response(payload: ExecuteResultPayload) -> ChannelResponse {
    ChannelResponse {
        ok: payload.ok,
        status: payload.status,
        body: payload.body,
    }
}

/// 渠道不可用原因；可用时返回 None。
fn availability_error(status: &Value, channel: &str) -> Option<String> {
    let connected = status
        .get(channel)
        .and_then(|entry| entry.get("connected"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if connected {
        None
    } else {
        Some(format!(
            "渠道 {} 未连接：请在浏览器插件中打开并登录对应网页",
            channel
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{availability_error, head, to_response, PluginChannelExecutor};
    use crate::extension::credentials::CredentialStore;
    use crate::extension::manager::ExtensionManager;
    use crate::extension::websocket::{
        ExecuteRequestPayload, ExecuteResultPayload, WebSocketBridge,
    };
    use crate::models::defs::{ChannelExecutor, ChannelRequest};
    use futures_util::{SinkExt, StreamExt};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    type PluginWs =
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

    /// 执行结果映射到渠道响应。
    #[test]
    fn result_maps_to_channel_response() {
        let response = to_response(ExecuteResultPayload {
            ok: true,
            status: 200,
            body: "body".to_string(),
        });
        assert!(response.ok);
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "body");
    }

    /// 日志里的正文摘要：短正文原样，长正文按字符截断（不切断多字节字符）。
    #[test]
    fn head_truncates_on_char_boundary() {
        assert_eq!(head("abc", 5), "abc");
        assert_eq!(head("", 3), "");
        assert_eq!(head("中文中文", 2), "中文…");
        assert_eq!(head("中文中文", 4), "中文中文");
    }

    /// 渠道可用性判定：未连接给出原因，已连接放行。
    #[test]
    fn availability_reports_disconnected_channel() {
        assert!(availability_error(&serde_json::json!({}), "deepseek-web").is_some());
        assert!(
            availability_error(
                &serde_json::json!({ "deepseek-web": { "connected": false } }),
                "deepseek-web"
            )
            .is_some()
        );
        assert!(availability_error(
            &serde_json::json!({ "deepseek-web": { "connected": true } }),
            "deepseek-web"
        )
        .is_none());
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("poolman-executor-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 取一个空闲端口并启动桥接层监听。
    async fn start_bridge(bridge: &Arc<WebSocketBridge>) -> String {
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);
        let bridge = bridge.clone();
        tokio::spawn(async move {
            let _ = bridge.listen(&addr.to_string()).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        format!("ws://{}", addr)
    }

    /// mock 插件：上报渠道已连接，然后应答一次执行请求。
    async fn connect_plugin(addr: &str) -> PluginWs {
        let (mut ws, _) = connect_async(addr).await.expect("mock 插件应能连上桥接层");
        ws.send(Message::Text(
            serde_json::json!({
                "id": "mock-status",
                "type": "STATUS_SYNC",
                "timestamp": 1,
                "payload": { "deepseek-web": { "connected": true, "authed": true } }
            })
            .to_string(),
        ))
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        ws
    }

    /// 端到端：执行器经插件连接下发 EXECUTE_REQUEST，插件回 EXECUTE_RESULT，
    /// 执行器映射为渠道响应；并核对下发的 provider 为渠道 id。
    #[tokio::test]
    async fn execute_roundtrips_through_plugin_connection() {
        let dir = temp_dir();
        let manager = Arc::new(ExtensionManager::new(&dir.to_string_lossy()));
        let credentials = Arc::new(CredentialStore::new());
        let bridge = Arc::new(WebSocketBridge::new(
            credentials.clone(),
            manager.clone(),
            None,
        ));
        let addr = start_bridge(&bridge).await;
        let mut ws = connect_plugin(&addr).await;

        let executor = PluginChannelExecutor::new(
            bridge.clone(),
            credentials.clone(),
            "deepseek-web",
            5_000,
        );
        assert!(executor.token().is_none(), "未捕获凭证时为 None");

        // 插件侧：读请求并应答
        let responder = tokio::spawn(async move {
            let incoming = tokio::time::timeout(std::time::Duration::from_secs(3), ws.next())
                .await
                .expect("应收到执行请求")
                .expect("连接内有消息")
                .expect("消息解析成功");
            let text = match incoming {
                Message::Text(text) => text.to_string(),
                other => panic!("应收到文本消息，实际: {:?}", other),
            };
            let request: serde_json::Value = serde_json::from_str(&text).expect("请求应为 JSON");
            assert_eq!(request["type"], "EXECUTE_REQUEST");
            assert_eq!(request["payload"]["provider"], "deepseek-web");
            let id = request["id"].as_str().expect("请求带 id").to_string();
            ws.send(Message::Text(
                serde_json::json!({
                    "id": id,
                    "type": "EXECUTE_RESULT",
                    "timestamp": 2,
                    "payload": { "status": 200, "ok": true, "body": "data: hi\n\n" }
                })
                .to_string(),
            ))
            .await
            .unwrap();
            request["payload"]["url"].as_str().unwrap().to_string()
        });

        // 捕获本用例期间的日志：插件调用结果必须落日志，否则「没走下去」无从排查
        let logs = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || SinkWriter(sink.clone()))
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let response = executor
            .execute(ChannelRequest {
                url: "https://chat.deepseek.com/api/v0/chat/completion".to_string(),
                method: "POST".to_string(),
                headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                body: Some("{}".to_string()),
                session_key: Some("sess-1".to_string()),
            })
            .await
            .expect("执行应成功");

        assert_eq!(
            responder.await.unwrap(),
            "https://chat.deepseek.com/api/v0/chat/completion"
        );
        assert!(response.ok);
        assert_eq!(response.status, 200);
        assert!(response.body.contains("hi"));

        let logged = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        // 调用结果是 debug 级：默认日志不打印，排查时打开才看得到
        assert!(logged.contains("插件调用完成"), "{}", logged);
        assert!(logged.contains("status=200"), "{}", logged);
        assert!(
            logged.contains("https://chat.deepseek.com/api/v0/chat/completion"),
            "{}",
            logged
        );
        assert!(
            !logged.contains("data: hi"),
            "响应正文不该进日志：{}",
            logged
        );

        std::fs::remove_file(dir.join("plugins.json")).ok();
        std::fs::remove_dir(dir).ok();
    }

    /// 日志写入内存缓冲（用例断言日志内容用）。
    struct SinkWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SinkWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 流式往返：分块经回调送达客户端，并在日志里记下分块数与本轮首个分块
    /// （流式的最终响应体为空，只有这两项能说明插件到底回了什么）。
    #[tokio::test]
    async fn stream_result_logs_chunk_count_and_first_chunk() {
        let dir = temp_dir();
        let manager = Arc::new(ExtensionManager::new(&dir.to_string_lossy()));
        let credentials = Arc::new(CredentialStore::new());
        let bridge = Arc::new(WebSocketBridge::new(
            credentials.clone(),
            manager.clone(),
            None,
        ));
        let addr = start_bridge(&bridge).await;
        let mut ws = connect_plugin(&addr).await;

        let executor = PluginChannelExecutor::new(
            bridge.clone(),
            credentials.clone(),
            "deepseek-web",
            5_000,
        );

        let responder = tokio::spawn(async move {
            let incoming = tokio::time::timeout(std::time::Duration::from_secs(3), ws.next())
                .await
                .expect("应收到执行请求")
                .expect("连接内有消息")
                .expect("消息解析成功");
            let Message::Text(text) = incoming else {
                panic!("应收到文本消息");
            };
            let request: serde_json::Value = serde_json::from_str(&text).expect("请求应为 JSON");
            let id = request["id"].as_str().expect("请求带 id").to_string();
            for chunk in ["data: {\"p\":\"response/content\",\"v\":\"你\"}\n", "data: [DONE]\n"] {
                ws.send(Message::Text(
                    serde_json::json!({
                        "id": id,
                        "type": "EXECUTE_CHUNK",
                        "timestamp": 2,
                        "payload": { "data": chunk }
                    })
                    .to_string(),
                ))
                .await
                .unwrap();
            }
            ws.send(Message::Text(
                serde_json::json!({
                    "id": id,
                    "type": "EXECUTE_RESULT",
                    "timestamp": 3,
                    "payload": { "status": 200, "ok": true, "body": null }
                })
                .to_string(),
            ))
            .await
            .unwrap();
        });

        let logs = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || SinkWriter(sink.clone()))
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);

        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let collected = seen.clone();
        let response = executor
            .execute_stream(
                ChannelRequest {
                    url: "https://chat.deepseek.com/api/v0/chat/completion".to_string(),
                    method: "POST".to_string(),
                    headers: vec![],
                    body: Some("{}".to_string()),
                    session_key: None,
                },
                Box::new(move |delta| collected.lock().unwrap().push(delta)),
            )
            .await
            .expect("流式执行应成功");
        join_responder(responder).await;

        assert!(response.ok);
        assert!(response.body.is_empty(), "流式最终响应体为空");
        assert_eq!(seen.lock().unwrap().len(), 2, "两个分块都应送达");
        drop(guard);

        let logged = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        assert!(logged.contains("插件调用完成"), "{}", logged);
        assert!(logged.contains("chunks=2"), "应记下分块数：{}", logged);
        assert!(
            logged.contains("first_chunk=data: {\"p\":\"response/content\""),
            "应记下首个分块：{}",
            logged
        );

        std::fs::remove_file(dir.join("plugins.json")).ok();
        std::fs::remove_dir(dir).ok();
    }

    /// 等待后台应答任务结束（任务 panic 时原样抛出，避免被静默）。
    async fn join_responder(handle: tokio::task::JoinHandle<()>) {
        if let Err(error) = handle.await {
            panic!("应答任务失败: {}", error);
        }
    }

    /// 渠道未连接时，执行在本地即被拒绝（不下发请求）。
    #[tokio::test]
    async fn execute_rejects_when_channel_disconnected() {
        let dir = temp_dir();
        let manager = Arc::new(ExtensionManager::new(&dir.to_string_lossy()));
        let credentials = Arc::new(CredentialStore::new());
        let bridge = Arc::new(WebSocketBridge::new(credentials.clone(), manager.clone(), None));
        let executor =
            PluginChannelExecutor::new(bridge, credentials, "deepseek-web", 500);

        let error = executor
            .execute(ChannelRequest {
                url: "https://example.test/x".to_string(),
                method: "POST".to_string(),
                headers: vec![],
                body: None,
                session_key: None,
            })
            .await
            .expect_err("渠道不可用时应拒绝");
        assert!(error.to_string().contains("未连接"));

        std::fs::remove_dir(dir).ok();
    }

    /// 执行负载映射：body 为空时传空串，session_key 透传。
    #[test]
    fn payload_maps_body_and_session_key() {
        let bridge = Arc::new(WebSocketBridge::default());
        let executor = PluginChannelExecutor::new(
            bridge,
            Arc::new(CredentialStore::new()),
            "deepseek-web",
            1_000,
        );
        let payload: ExecuteRequestPayload = executor.payload(ChannelRequest {
            url: "https://example.test/x".to_string(),
            method: "GET".to_string(),
            headers: vec![],
            body: None,
            session_key: Some("s".to_string()),
        });
        assert_eq!(payload.provider, "deepseek-web");
        assert_eq!(payload.body, "");
        assert_eq!(payload.session_key.as_deref(), Some("s"));
    }
}
