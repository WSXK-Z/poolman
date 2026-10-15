//! 供应商门面：把聊天、网页会话能力收成一个入口，供链路调用。
//!
//! 适配器边界（见 `链路设计.md` 第 4.1 节）：
//! - 下行：poolman 格式提示词 → 该网页平台的请求（凭证、PoW、会话链都在这里）
//! - 上行：网页响应原文 → poolman 返回类型 `ChatResponsePayload`
//!
//! 上层（对话组装 / 调度 / API 编码）只与本门面打交道，不知道 PoW 等平台细节。

use std::sync::Arc;

use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::extension::adapters::defs::{ChatWebService, ProviderAdapter, SessionWebService};
use crate::extension::adapters::deepseek::adapter::DeepSeekChatService;
use crate::extension::adapters::deepseek::connector::DeepSeekWebService;
use crate::extension::adapters::deepseek::defs::PROVIDER_ID;
use crate::extension::credentials::CredentialStore;
use crate::extension::websocket::{ChatResponsePayload, WebSocketBridge};
use crate::session::manager::SessionManager;

/// DeepSeek 网页版供应商门面。
pub struct DeepSeekService {
    /// 聊天执行（会话选择、PoW、422 自愈、SSE 解析、工具调用解析）
    chat: DeepSeekChatService,
    /// 网页端接口（会话列表 / 删除 / 历史）
    web: Arc<DeepSeekWebService>,
}

impl DeepSeekService {
    pub fn new(
        bridge: Arc<WebSocketBridge>,
        credentials: Arc<CredentialStore>,
        timeout_ms: u64,
        sessions: Option<Arc<SessionManager>>,
    ) -> Self {
        let web = Arc::new(DeepSeekWebService::new(bridge, credentials, timeout_ms));
        Self {
            chat: DeepSeekChatService::new(web.clone(), sessions),
            web,
        }
    }

    /// 列出网页端会话（原始响应 → 会话数组）
    pub async fn list_provider_sessions(&self) -> Result<Vec<Value>> {
        let json = self.web.get_session_list().await?;
        let list = json
            .get("data")
            .and_then(|d| d.get("biz_data"))
            .and_then(|b| b.get("chat_sessions"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(list)
    }

    /// 删除网页端会话
    pub async fn delete_provider_session(&self, session_id: &str) -> Result<()> {
        self.web.delete_session(session_id).await.map(|_| ())
    }
}

/// 供应商门面实现：链路只通过这个 trait 使用 DeepSeek，
/// 看不到 PoW、凭证、SSE 解析、会话链等内部细节。
impl ProviderAdapter for DeepSeekService {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn chat<'a>(
        &'a self,
        messages: Vec<Value>,
        options: Option<Value>,
    ) -> BoxFuture<'a, Result<ChatResponsePayload>> {
        Box::pin(async move { self.chat.chat(messages, options).await })
    }

    fn chat_stream<'a>(
        &'a self,
        messages: Vec<Value>,
        options: Option<Value>,
        on_chunk: Box<dyn FnMut(String) + Send + Sync>,
    ) -> BoxFuture<'a, Result<ChatResponsePayload>> {
        Box::pin(async move { self.chat.chat_stream(messages, options, on_chunk).await })
    }
}
