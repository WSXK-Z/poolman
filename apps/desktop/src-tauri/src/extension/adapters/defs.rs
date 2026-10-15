use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::extension::websocket::ChatResponsePayload;
pub trait BaseWebService {
    /// 获取供应商访问凭证
    async fn get_token(&self) -> Result<String>;
    /// 获取供应商设置凭证
    fn get_setting_token(&self) -> Option<String>;
    /// 执行一次 HTTP 请求，返回完整响应。
    async fn execute_once(
        &self,
        method: &str,
        url: &str,
        headers: Vec<(String, String)>,
        body: Option<String>,
        timeout_ms: u64,
    ) -> Result<crate::extension::websocket::ExecuteResultPayload>;
    /// 执行一次 HTTP 请求，返回流式响应。
    async fn execute_streaming(
        &self,
        method: &str,
        url: &str,
        headers: Vec<(String, String)>,
        body: String,
        on_chunk: Box<dyn FnMut(String) + Send + Sync>,
    ) -> Result<crate::extension::websocket::ExecuteResultPayload>;

    fn default_headers(&self, token: &str, settings_token: Option<&str>) -> Vec<(String, String)>;
}
pub trait SessionWebService {
    /// 创建会话
    async fn create_session(&self) -> Result<Option<String>>;
    /// 删除会话
    async fn delete_session(&self, session_id: &str) -> Result<bool>;
    /// 更新会话标题
    async fn update_session_title(&self, session_id: &str, title: &str) -> Result<bool>;
    /// 获取会话历史记录
    async fn get_session_history(&self, session_id: &str) -> Result<serde_json::Value>;
    /// 获取会话列表
    async fn get_session_list(&self) -> Result<serde_json::Value>;
}

pub trait ChatWebService {
    /// 进行聊天
    async fn chat(
        &self,
        messages: Vec<Value>,
        options: Option<Value>,
    ) -> Result<ChatResponsePayload>;
    /// 流式聊天
    async fn chat_stream<F: FnMut(String) + Send + Sync + 'static>(
        &self,
        messages: Vec<Value>,
        options: Option<Value>,
        on_chunk: F,
    ) -> Result<ChatResponsePayload>;
}

/// 平台回答的解析结果。
pub struct ParsedAnswer {
    /// 回答纯文本（RESPONSE）
    pub text: String,
    /// 解析出的工具调用（OpenAI 标准 tool_calls 数组）
    pub tool_calls: Option<Value>,
    /// 文本里留有「像工具调用但没被解析」的痕迹（模型可能未按提示词围栏格式输出）
    pub unparsed_tool_trace: bool,
}

/// 消息解析抽象：桌面消息与网页平台负载之间的双向转换。
pub trait ParseWebService {
    /// 桌面消息（OpenAI 格式）转换为发给网页平台的提示词；
    /// platform 指定提示词组装平台（如 vscode-copilot）。
    fn build_prompt(
        messages: &[Value],
        tools: Option<&Value>,
        is_first_turn: bool,
        platform: &str,
    ) -> Result<String>;
    /// 网页平台回答解析为桌面聊天内容。
    fn parse_answer(content: &Value) -> ParsedAnswer;
}

/// 供应商门面：链路（路由 / 调度 / API 编码）只认它。
///
/// 供应商特有的细节——凭证、PoW、签名、会话链、SSE 解析、422 自愈、工具调用解析——
/// 全部封在实现内部（见 `链路设计.md` 第 4.1 节）。
pub trait ProviderAdapter: Send + Sync {
    /// 供应商标识（对外模型名经模型映射后落到它）
    fn id(&self) -> &'static str;

    /// 非流式对话
    fn chat<'a>(
        &'a self,
        messages: Vec<Value>,
        options: Option<Value>,
    ) -> BoxFuture<'a, Result<ChatResponsePayload>>;

    /// 流式对话：增量经 `on_chunk` 实时送出
    /// （回调需 `'static`：它会跨 WebSocket 回调线程存活到本轮请求结束）
    fn chat_stream<'a>(
        &'a self,
        messages: Vec<Value>,
        options: Option<Value>,
        on_chunk: Box<dyn FnMut(String) + Send + Sync>,
    ) -> BoxFuture<'a, Result<ChatResponsePayload>>;
}
