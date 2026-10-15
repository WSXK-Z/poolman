use crate::extension::{
    adapters::{
        deepseek::{
            defs::{
                AUX_TIMEOUT, DEEPSEEK_API_URL, DEEPSEEK_HISTORY_URL, DEEPSEEK_SESSION_DELETE_URL,
                DEEPSEEK_SESSION_LIST_URL, DEEPSEEK_SESSION_URL, DEEPSEEK_TITLE_URL,
                POW_CHALLENGE_URL, PROVIDER_ID,
            },
            pow::{build_pow_header, solve_pow},
        },
        defs::{BaseWebService, SessionWebService},
    },
    credentials::CredentialStore,
    websocket::{ExecuteRequestPayload, WebSocketBridge},
};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex as StdMutex};
use tracing::{debug, info, warn};
/// deepseek web端供应商
pub struct DeepSeekWebService {
    bridge: Arc<WebSocketBridge>,
    credentials: Arc<CredentialStore>,
    timeout_ms: u64,
    /// 当前会话键（网页会话 id）：每次执行都带上它，
    /// 供调度层维持会话粘性（同一会话固定落到同一台插件）。
    current_session: StdMutex<Option<String>>,
}
impl DeepSeekWebService {
    pub fn new(
        bridge: Arc<WebSocketBridge>,
        credentials: Arc<CredentialStore>,
        timeout_ms: u64,
    ) -> Self {
        Self {
            bridge,
            credentials,
            timeout_ms,
            current_session: StdMutex::new(None),
        }
    }

    /// 记录当前会话键（由聊天服务在选定网页会话后写入）。
    pub fn set_current_session(&self, session_id: Option<String>) {
        *self.current_session.lock().unwrap() = session_id;
    }

    /// 取出当前会话键（无会话时返回 None，调度层则不启用粘性）。
    fn session_key(&self) -> Option<String> {
        self.current_session.lock().unwrap().clone()
    }
    /// 取得并求解一次 PoW 挑战，返回 x-ds-pow-response 头值；两轮都失败时返回 None。
    /// 挑战请求经 bridge 直发而不走 execute_once，避免「取头」与「注入头」互相调用。
    pub async fn get_pow_header(&self) -> Result<Option<String>> {
        for attempt in 1..=2u32 {
            let token = self.get_token().await?;
            let settings_token = self.get_setting_token();
            let payload = ExecuteRequestPayload {
                provider: PROVIDER_ID.to_string(),
                url: POW_CHALLENGE_URL.to_string(),
                method: "POST".to_string(),
                headers: self.default_headers(&token, settings_token.as_deref()),
                body: json!({ "target_path": "/api/v0/chat/completion" }).to_string(),
                session_key: self.session_key(),
            };
            let res = match self.bridge.send_execute(payload, AUX_TIMEOUT, None).await {
                Ok(r) => r,
                Err(e) => {
                    warn!("PoW 流程失败（第 {} 次）: {}", attempt, e);
                    continue;
                }
            };
            if !res.ok || res.body.is_empty() {
                warn!("PoW 挑战获取失败(HTTP {}) 第{}次", res.status, attempt);
                continue;
            }
            let json: Value = match serde_json::from_str(&res.body) {
                Ok(v) => v,
                Err(_) => {
                    warn!("PoW 挑战响应非 JSON");
                    continue;
                }
            };
            let ch = json
                .get("data")
                .and_then(|d| d.get("biz_data"))
                .and_then(|b| b.get("challenge"))
                .or_else(|| json.get("data").and_then(|d| d.get("challenge")))
                .or_else(|| json.get("challenge"))
                .cloned();
            let ch = match ch {
                Some(c) if c.is_object() => c,
                _ => {
                    warn!("PoW 挑战响应缺少 challenge 字段");
                    continue;
                }
            };
            let algorithm = ch
                .get("algorithm")
                .and_then(|v| v.as_str())
                .unwrap_or("DeepSeekHashV1")
                .to_string();
            let challenge = ch
                .get("challenge")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let salt = ch
                .get("salt")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let expire_at = ch
                .get("expire_at")
                .or_else(|| ch.get("expireAt"))
                .or_else(|| ch.get("expires_at"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let difficulty = ch
                .get("difficulty")
                .and_then(|v| v.as_u64())
                .unwrap_or(144000)
                .max(1);
            let signature = ch
                .get("signature")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let target_path = ch
                .get("target_path")
                .and_then(|v| v.as_str())
                .unwrap_or("/api/v0/chat/completion")
                .to_string();
            let start = std::time::Instant::now();
            let answer = solve_pow(&challenge, &salt, expire_at, difficulty);
            debug!(
                "PoW solved in {:?} difficulty={}",
                start.elapsed(),
                difficulty
            );
            if answer < 0 {
                warn!("PoW 求解失败（difficulty 范围内未找到答案）");
                continue;
            }
            return Ok(Some(build_pow_header(
                &algorithm,
                &challenge,
                &salt,
                answer,
                &signature,
                &target_path,
            )));
        }
        Ok(None)
    }
    /// 请求超时时间（毫秒）
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }
}
/// 实现基础网页服务接口，提供 DeepSeek 网页端的凭证获取、请求执行和默认请求头生成。
impl BaseWebService for DeepSeekWebService {
    /// 获取 DeepSeek 网页访问令牌，若未获取到则触发凭证捕获。
    async fn get_token(&self) -> Result<String> {
        if let Some(token) = self.credentials.get_token(PROVIDER_ID) {
            return Ok(token);
        }
        info!("未找到凭证，触发捕获");
        let _ = self.bridge.request_credentials(PROVIDER_ID, 10_000).await;
        self.credentials
            .get_token(PROVIDER_ID)
            .ok_or_else(|| anyhow!("未获取到 DeepSeek 访问凭证：请先在浏览器打开 chat.deepseek.com 并登录，且插件已连接"))
    }
    fn get_setting_token(&self) -> Option<String> {
        self.credentials.get_settings_token(PROVIDER_ID)
    }
    /// 执行一次 HTTP 请求，返回完整响应。
    /// 仅 completion 接口自动补充 PoW 头；其它接口不带该头，挑战请求自身也不再触发取头。
    async fn execute_once(
        &self,
        method: &str,
        url: &str,
        mut headers: Vec<(String, String)>,
        body: Option<String>,
        timeout_ms: u64,
    ) -> Result<crate::extension::websocket::ExecuteResultPayload> {
        if url == DEEPSEEK_API_URL {
            if let Some(pow) = self.get_pow_header().await? {
                headers.push(("x-ds-pow-response".to_string(), pow));
            }
        }
        let payload = ExecuteRequestPayload {
            provider: PROVIDER_ID.to_string(),
            url: url.to_string(),
            method: method.to_string(),
            headers,
            body: body.unwrap_or_default(),
            session_key: self.session_key(),
        };
        let res = self.bridge.send_execute(payload, timeout_ms, None).await?;
        if url != DEEPSEEK_API_URL && !res.ok {
            warn!(
                "辅助接口失败 {} {} -> {} body={}",
                method,
                url,
                res.status,
                res.body.chars().take(200).collect::<String>()
            );
        }
        Ok(res)
    }

    /// 执行一次 HTTP 请求，返回流式响应。
    async fn execute_streaming(
        &self,
        method: &str,
        url: &str,
        mut headers: Vec<(String, String)>,
        body: String,
        on_chunk: Box<dyn FnMut(String) + Send + Sync>,
    ) -> Result<crate::extension::websocket::ExecuteResultPayload> {
         if url == DEEPSEEK_API_URL {
            if let Some(pow) = self.get_pow_header().await? {
                headers.push(("x-ds-pow-response".to_string(), pow));
            }
        }
        let payload = ExecuteRequestPayload {
            provider: PROVIDER_ID.to_string(),
            url: url.to_string(),
            method: method.to_string(),
            headers,
            body,
            session_key: self.session_key(),
        };
        self.bridge
            .send_execute(payload, self.timeout_ms, Some(on_chunk))
            .await
    }
    /// DeepSeek 默认请求头
    fn default_headers(&self, token: &str, settings_token: Option<&str>) -> Vec<(String, String)> {
        let mut h = vec![
            (
                "x-client-bundle-id".to_string(),
                "com.deepseek.chat".to_string(),
            ),
            ("x-client-platform".to_string(), "web".to_string()),
            ("x-client-version".to_string(), "2.4.0".to_string()),
            ("x-client-locale".to_string(), "zh_CN".to_string()),
            ("x-client-timezone-offset".to_string(), "28800".to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Authorization".to_string(), format!("Bearer {}", token)),
        ];
        if let Some(st) = settings_token {
            h.push(("x-settings-token".to_string(), st.to_string()));
        }
        h
    }
}
/// 实现会话网页服务接口，提供 DeepSeek 网页端的会话管理功能。
impl SessionWebService for DeepSeekWebService {
    async fn create_session(&self) -> Result<Option<String>> {
        let token = self.get_token().await?;
        let settings_token = self.get_setting_token();
        let res = self
            .execute_once(
                "POST",
                DEEPSEEK_SESSION_URL,
                self.default_headers(&token, settings_token.as_deref()),
                Some("{}".to_string()),
                AUX_TIMEOUT,
            )
            .await?;
        if !res.ok || res.body.is_empty() {
            return Ok(None);
        }
        let json: Value = match serde_json::from_str(&res.body) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        let data = json.get("data");
        let candidates = [
            data.and_then(|d| d.get("biz_data"))
                .and_then(|b| b.get("chat_session"))
                .and_then(|s| s.get("id")),
            data.and_then(|d| d.get("biz_data"))
                .and_then(|b| b.get("chat_session_id")),
            data.and_then(|d| d.get("biz_data"))
                .and_then(|b| b.get("id")),
            data.and_then(|d| d.get("biz_data"))
                .and_then(|b| b.get("session_id")),
            data.and_then(|d| d.get("chat_session_id")),
            data.and_then(|d| d.get("id")),
            data.and_then(|d| d.get("session_id")),
            json.get("chat_session_id"),
        ];
        for c in candidates {
            if let Some(s) = c.and_then(|v| v.as_str()) {
                if s.len() > 10 {
                    debug!("创建会话 {}", s);
                    return Ok(Some(s.to_string()));
                }
            }
        }
        Ok(None)
    }
    async fn delete_session(&self, session_id: &str) -> Result<bool> {
        let token = self.get_token().await?;
        let settings_token = self.get_setting_token();
        let response = self
            .execute_once(
                "POST",
                DEEPSEEK_SESSION_DELETE_URL,
                self.default_headers(&token, settings_token.as_deref()),
                Some(json!({ "chat_session_id": session_id }).to_string()),
                AUX_TIMEOUT,
            )
            .await?;
        if !response.ok {
            anyhow::bail!("删除 DeepSeek 会话失败（HTTP {}）", response.status);
        }
        let body: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
        if body
            .get("code")
            .and_then(Value::as_i64)
            .is_some_and(|code| code != 0)
            || body
                .get("data")
                .and_then(|v| v.get("biz_code"))
                .and_then(Value::as_i64)
                .is_some_and(|code| code != 0)
        {
            anyhow::bail!("DeepSeek 拒绝删除会话：{}", response.body);
        }
        Ok(true)
    }
    /// 更新会话标题，返回是否成功；两次尝试均失败时返回 false 且不报错。
    async fn update_session_title(&self, session_id: &str, title: &str) -> Result<bool> {
        for attempt in 1..=2u32 {
            let token = self.get_token().await?;
            let settings_token = self.get_setting_token();
            let res = self
                .execute_once(
                    "POST",
                    DEEPSEEK_TITLE_URL,
                    self.default_headers(&token, settings_token.as_deref()),
                    Some(json!({ "chat_session_id": session_id, "title": title }).to_string()),
                    AUX_TIMEOUT,
                )
                .await;
            match res {
                Ok(r) if r.ok => {
                    let mut biz_ok = false;
                    if let Ok(j) = serde_json::from_str::<Value>(&r.body) {
                        biz_ok = j.get("code").and_then(|v| v.as_i64()) == Some(0)
                            && j.get("data")
                                .and_then(|d| d.get("biz_code"))
                                .and_then(|v| v.as_i64())
                                == Some(0);
                    }
                    if biz_ok || r.body.is_empty() {
                        info!("已将会话标题设为 {} session={}", title, session_id);
                        return Ok(true);
                    }
                    warn!(
                        "设置会话标题业务失败 session={} body={}",
                        session_id,
                        r.body.chars().take(200).collect::<String>()
                    );
                }
                Ok(r) => {
                    warn!("设置会话标题失败(HTTP {}) session={}", r.status, session_id);
                }
                Err(e) => {
                    warn!("设置会话标题异常（第 {} 次）: {}", attempt, e);
                }
            }
            if attempt < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
        }
        Ok(false)
    }
    /// 拉取指定会话的历史消息，返回平台原始响应体。
    async fn get_session_history(&self, session_id: &str) -> Result<serde_json::Value> {
        let token = self.get_token().await?;
        let settings_token = self.get_setting_token();
        let url = format!(
            "{}?chat_session_id={}&cache_version=8",
            DEEPSEEK_HISTORY_URL,
            encode_query(session_id)
        );
        let res = self
            .execute_once(
                "GET",
                &url,
                self.default_headers(&token, settings_token.as_deref()),
                None,
                AUX_TIMEOUT,
            )
            .await?;
        if !res.ok {
            anyhow::bail!(
                "获取 DeepSeek 会话历史失败（HTTP {}）：{}",
                res.status,
                res.body.chars().take(200).collect::<String>()
            );
        }
        Ok(serde_json::from_str(&res.body)?)
    }

    /// 拉取会话列表，返回平台原始响应体。
    async fn get_session_list(&self) -> Result<serde_json::Value> {
        let token = self.get_token().await?;
        let settings_token = self.get_setting_token();
        let url = format!("{DEEPSEEK_SESSION_LIST_URL}?lte_cursor.pinned=false");
        let res = self
            .execute_once(
                "GET",
                &url,
                self.default_headers(&token, settings_token.as_deref()),
                None,
                AUX_TIMEOUT,
            )
            .await?;
        if !res.ok {
            anyhow::bail!(
                "获取 DeepSeek 会话列表失败（HTTP {}）：{}",
                res.status,
                res.body.chars().take(200).collect::<String>()
            );
        }
        Ok(serde_json::from_str(&res.body)?)
    }
}

/// 按 RFC 3986 对查询参数值做百分号编码，仅保留非保留字符 `A-Za-z0-9-._~`。
fn encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}


