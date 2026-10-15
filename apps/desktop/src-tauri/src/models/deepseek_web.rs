//! DeepSeek 网页版模型：实现插件模型抽象，一个模型对应 `deepseek-web` 渠道。
//!
//! 会话管理（创建 / 列表 / 删除 / 改标题 / 历史）与聊天请求组装、PoW 应答、流式分块
//! 全部在模型内部完成；对外的请求发送与凭证读取经 [`ChannelExecutor`]（由传输层实现）。
//!
//! 渠道细节（URL、PoW、SSE 分行）不外露；具体由哪台插件实例执行由插件管理器调度。
pub mod sse_frame_parser;
pub mod chat_stream_processor;
pub mod sse_frame_processor;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use futures_util::future::BoxFuture;
use serde::Serialize;
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use super::defs::{
    ChannelExecutor, ChannelRequest, ChunkSink, Concurrency, InferRequest, ModelCapability,
    ModelResponse,
};
use super::model::Model;
use super::plugin::PluginModel;
use super::pow::{build_pow_header, solve_pow};
use crate::dump::DumpConfig;
use crate::models::deepseek_web::sse_frame_processor::{
    SseFrameProcessor, FRAGMENT_RESPONSE, FRAGMENT_THINK,
};
use crate::stream_processor::{
    CloseSpec, Fence, FenceRecognizer, Node, ProcessorOptions, Recognized, StreamEvent,
    StreamProcessor,
};

/// 渠道标识。
pub const CHANNEL: &str = "deepseek-web";

/// 拼装提示词时置顶的前缀：网页会话本身带着历史对话，要求模型只依据本次内容作答。
const PROMPT_PREFIX: &str = "你只需要根据以下内容回答问题，上面的对话内容不要考虑：";

/// 系统提示词段标题：正文取自全部 system 消息。
const SYSTEM_HEADING: &str = "以下是【系统提示词】：";

/// 历史对话段标题：正文是除最新一条记录以外的消息。
const HISTORY_HEADING: &str = "以下是【历史对话内容】：";

/// 工具调用格式与限定规则段标题。
const TOOL_RULE_HEADING: &str = "以下是【工具调用格式和限定规则】：";

/// 可用工具段标题。
const TOOLS_HEADING: &str = "以下是【可用工具】：";

/// 工具调用围栏标签名。
const TOOL_CALL_TAG: &str = "tool_call";

/// 工具调用围栏开边。
const TOOL_CALL_OPEN: &str = "<tool_call>";

/// 工具调用围栏闭边。
const TOOL_CALL_CLOSE: &str = "</tool_call>";

/// 工具调用格式与限定规则：网页版没有原生工具调用，靠这段文字约束模型按固定围栏输出调用。
const TOOL_RULE: &str = r#"1. 需要调用工具时，一次调用写成一个独立的 <tool_call> ... </tool_call> 围栏，围栏内只放一个 JSON 对象：name 为工具名，arguments 为参数对象，例如：
<tool_call>
{
  "name": "read_file",
  "arguments": {
    "path": "d:/project/src/main.rs"
  }
}
</tool_call>
2. 开标签与闭标签必须逐字符写成 <tool_call> 与 </tool_call>，成对出现，不能改写、不能漏写、不能嵌套。
3. 围栏内除这一个 JSON 对象外不写任何内容，不要套 Markdown 代码块；JSON 的键与字符串值必须用双引号。
4. 需要调用多个工具时，一个工具一个围栏，围栏之间换行分隔。
5. 决定调用工具时只输出围栏，围栏之外不写解释、前言或总结；不调用工具时只输出普通回答，不要输出任何围栏。
6. 工具名与参数名必须与工具定义完全一致，必填参数不能省略；只能调用下面列出的工具。"#;

const COMPLETION_URL: &str = "https://chat.deepseek.com/api/v0/chat/completion";
const SESSION_CREATE_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/create";
const SESSION_LIST_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/fetch_page";
const SESSION_DELETE_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/delete";
const SESSION_TITLE_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/update_title";
const HISTORY_URL: &str = "https://chat.deepseek.com/api/v0/chat/history_messages";
const POW_CHALLENGE_URL: &str = "https://chat.deepseek.com/api/v0/chat/create_pow_challenge";

/// 网页版默认模型类型与思考开关。
const MODEL_TYPE: &str = "default";
const THINKING_ENABLED: bool = true;

/// DeepSeek 网页版模型。
pub struct DeepSeekWebModel {
    /// 展示名。
    display_name: String,
    /// 渠道执行器（发送请求 + 读取凭证）。
    executor: Arc<dyn ChannelExecutor>,
    /// 当前渠道会话（网页 chat_session_id）。
    current_session: Mutex<Option<String>>,
    /// 并发语义（网页渠道：同会话串行）。
    concurrency: Concurrency,
    /// 分阶段落盘工具（响应）；None 表示不落盘。
    dump: Option<Arc<DumpConfig>>,
}

impl DeepSeekWebModel {
    pub fn new(display_name: impl Into<String>, executor: Arc<dyn ChannelExecutor>) -> Self {
        Self {
            display_name: display_name.into(),
            executor,
            current_session: Mutex::new(None),
            concurrency: Concurrency::single(),
            dump: None,
        }
    }

    /// 注入分阶段落盘工具：响应按开关落盘。
    pub fn with_dump(mut self, dump: Arc<DumpConfig>) -> Self {
        self.dump = Some(dump);
        self
    }

    /// 记录当前渠道会话（供后续请求沿用）。
    pub fn set_current_session(&self, session_id: Option<String>) {
        *self.current_session.lock().unwrap() = session_id;
    }

    /// 当前渠道会话键（无会话时不启用传输层粘性）。
    fn session_key(&self) -> Option<String> {
        self.current_session.lock().unwrap().clone()
    }

    /// DeepSeek 网页默认请求头。
    fn default_headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![
            (
                "x-client-bundle-id".to_string(),
                "com.deepseek.chat".to_string(),
            ),
            ("x-client-platform".to_string(), "web".to_string()),
            ("x-client-version".to_string(), "2.4.0".to_string()),
            ("x-client-locale".to_string(), "zh_CN".to_string()),
            ("x-client-timezone-offset".to_string(), "28800".to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
        ];
        if let Some(token) = self.executor.token() {
            headers.push(("Authorization".to_string(), format!("Bearer {}", token)));
        }
        if let Some(settings_token) = self.executor.settings_token() {
            headers.push(("x-settings-token".to_string(), settings_token));
        }
        headers
    }

    /// 取访问令牌；渠道未捕获凭证时报错。
    fn token(&self) -> Result<String> {
        self.executor.token().ok_or_else(|| {
            anyhow!(
                "未获取到 DeepSeek 访问凭证：请先在浏览器打开 chat.deepseek.com 并登录，且插件已连接"
            )
        })
    }

    /// 发送一次渠道请求并解析 JSON 响应体。
    async fn send(&self, method: &str, url: &str, body: Option<String>) -> Result<Value> {
        let response = self
            .executor
            .execute(ChannelRequest {
                url: url.to_string(),
                method: method.to_string(),
                headers: self.default_headers(),
                body,
                session_key: self.session_key(),
            })
            .await?;
        if !response.ok {
            anyhow::bail!(
                "渠道请求失败（HTTP {}）：{}",
                response.status,
                response.body.chars().take(200).collect::<String>()
            );
        }
        Ok(serde_json::from_str(&response.body).unwrap_or(Value::Null))
    }

    /// 取得并求解一次 PoW 挑战，返回 `x-ds-pow-response` 头值；失败时返回 None。
    async fn pow_header(&self) -> Option<String> {
        let response = self
            .executor
            .execute(ChannelRequest {
                url: POW_CHALLENGE_URL.to_string(),
                method: "POST".to_string(),
                headers: self.default_headers(),
                body: Some(json!({ "target_path": "/api/v0/chat/completion" }).to_string()),
                session_key: self.session_key(),
            })
            .await
            .ok()?;
        if !response.ok || response.body.is_empty() {
            warn!("PoW 挑战获取失败（HTTP {}）", response.status);
            return None;
        }
        let json: Value = serde_json::from_str(&response.body).ok()?;
        let challenge = json
            .get("data")
            .and_then(|d| d.get("biz_data"))
            .and_then(|b| b.get("challenge"))
            .or_else(|| json.get("data").and_then(|d| d.get("challenge")))
            .or_else(|| json.get("challenge"))?;
        if !challenge.is_object() {
            return None;
        }
        let algorithm = challenge
            .get("algorithm")
            .and_then(Value::as_str)
            .unwrap_or("DeepSeekHashV1");
        let challenge_hex = challenge
            .get("challenge")
            .and_then(Value::as_str)
            .unwrap_or("");
        let salt = challenge.get("salt").and_then(Value::as_str).unwrap_or("");
        let expire_at = challenge
            .get("expire_at")
            .or_else(|| challenge.get("expireAt"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let difficulty = challenge
            .get("difficulty")
            .and_then(Value::as_u64)
            .unwrap_or(144_000)
            .max(1);
        let signature = challenge
            .get("signature")
            .and_then(Value::as_str)
            .unwrap_or("");
        let target_path = challenge
            .get("target_path")
            .and_then(Value::as_str)
            .unwrap_or("/api/v0/chat/completion");
        let answer = solve_pow(challenge_hex, salt, expire_at, difficulty);
        if answer < 0 {
            warn!("PoW 求解失败（difficulty 范围内未找到答案）");
            return None;
        }
        Some(build_pow_header(
            algorithm,
            challenge_hex,
            salt,
            answer,
            signature,
            target_path,
        ))
    }

    /// 确保存在渠道会话，返回会话 id。
    async fn ensure_session(&self) -> Result<String> {
        if let Some(session) = self.session_key() {
            return Ok(session);
        }
        let session = self
            .create_channel_session()
            .await?
            .ok_or_else(|| anyhow!("创建 DeepSeek 会话失败"))?;
        self.set_current_session(Some(session.clone()));
        Ok(session)
    }

    /// 把推理请求拼成渠道所需的 prompt，顺序固定为：
    /// 固定前缀 → 系统提示词 → 历史对话 → 工具调用格式与限定规则 → 可用工具 → 最新一条记录。
    ///
    /// 提示词组装不属于模型；这里只做渠道所需的最小拼接。
    fn prompt_of(request: &InferRequest) -> String {
        let mut sections: Vec<String> = vec![PROMPT_PREFIX.to_string()];

        let system = Self::render_system(&request.messages);
        if !system.is_empty() {
            sections.push(format!("{}\n{}", SYSTEM_HEADING, system));
        }

        // system 消息已单独成段，其余消息里末尾那条是最新记录，之前的都算历史对话。
        let turns: Vec<&Value> = request
            .messages
            .iter()
            .filter(|message| !Self::is_system(message))
            .collect();
        let Some((latest, history)) = turns.split_last() else {
            return sections.join("\n\n");
        };
        let history = history
            .iter()
            .filter_map(|message| Self::render_message(message))
            .collect::<Vec<_>>();
        if !history.is_empty() {
            sections.push(format!("{}\n{}", HISTORY_HEADING, history.join("\n\n")));
        }
        // 工具段只在确实有工具时出现：格式规则与工具清单成对给出。
        if let Some(tools) = Self::render_tools(request.tools.as_ref()) {
            sections.push(format!("{}\n{}", TOOL_RULE_HEADING, TOOL_RULE));
            sections.push(format!("{}\n{}", TOOLS_HEADING, tools));
        }
        if let Some(latest) = Self::render_message(latest) {
            sections.push(latest);
        }
        sections.join("\n\n")
    }

    /// 是否为 system 消息。
    fn is_system(message: &Value) -> bool {
        message.get("role").and_then(Value::as_str) == Some("system")
    }

    /// 系统提示词段：取全部 system 消息正文，多条之间空行分隔。
    fn render_system(messages: &[Value]) -> String {
        messages
            .iter()
            .filter(|message| Self::is_system(message))
            .map(Self::message_content)
            .filter(|content| !content.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// 取消息正文：字符串，或多模态内容数组里的文本片段；非文本片段只留占位标记，
    /// 避免把图片的 base64 塞进提示词。
    fn message_content(message: &Value) -> String {
        match message.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .map(Self::render_part)
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        }
    }

    /// 单条消息 → `【角色】正文`；助手消息里的历史工具调用按 `<tool_call>` 围栏接在正文之后。
    fn render_message(message: &Value) -> Option<String> {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        let mut content = Self::message_content(message);
        for call in Self::render_tool_calls(message) {
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str(&call);
        }
        if role.is_empty() && content.is_empty() {
            return None;
        }
        if content.is_empty() {
            return Some(format!("【{}】", role));
        }
        Some(format!("【{}】\n{}", role, content))
    }

    /// 助手消息里的 `tool_calls` → `<tool_call>` 围栏，多个之间换行分隔。
    ///
    /// 参数是字符串时按 JSON 解析，解析不了就原样保留。
    fn render_tool_calls(message: &Value) -> Vec<String> {
        let Some(calls) = message.get("tool_calls").and_then(Value::as_array) else {
            return Vec::new();
        };
        let tag = TOOL_CALL_TAG;
        calls
            .iter()
            .filter_map(|call| {
                let function = call.get("function").unwrap_or(call);
                let name = function.get("name").and_then(Value::as_str)?;
                let arguments = match function.get("arguments") {
                    Some(Value::String(raw)) => {
                        serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.clone()))
                    }
                    Some(other) => other.clone(),
                    None => json!({}),
                };
                Some(format!(
                    "<{tag}>\n{}\n</{tag}>",
                    json!({ "name": name, "arguments": arguments })
                ))
            })
            .collect()
    }

    /// 多模态内容片段 → 文本；非文本片段给占位标记（如 `[image_url]`）。
    fn render_part(part: &Value) -> String {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            return text.to_string();
        }
        let kind = part.get("type").and_then(Value::as_str).unwrap_or("未知片段");
        format!("[{}]", kind)
    }

    /// 工具清单：每项一行 `- 名称：描述`，参数以紧凑 JSON 附在其后；没有可用工具时返回 None。
    fn render_tools(tools: Option<&Value>) -> Option<String> {
        let items = tools
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())?;
        let lines = items
            .iter()
            .filter_map(|item| {
                let function = item.get("function").unwrap_or(item);
                let name = function.get("name").and_then(Value::as_str).unwrap_or("");
                if name.is_empty() {
                    return None;
                }
                let mut line = format!("- {}", name);
                if let Some(description) = function.get("description").and_then(Value::as_str) {
                    if !description.is_empty() {
                        line.push_str(&format!("：{}", description));
                    }
                }
                if let Some(parameters) = function.get("parameters") {
                    line.push_str(&format!("\n  参数：{}", parameters));
                }
                Some(line)
            })
            .collect::<Vec<_>>();
        if lines.is_empty() {
            return None;
        }
        Some(lines.join("\n"))
    }

    /// 聊天完成请求体。
    fn completion_body(&self, session_id: &str, prompt: &str) -> String {
        json!({
            "chat_session_id": session_id,
            "parent_message_id": Value::Null,
            "model_type": MODEL_TYPE,
            "prompt": prompt,
            "thinking_enabled": THINKING_ENABLED,
            "ref_file_ids": [],
            "search_enabled": false,
        })
        .to_string()
    }

    /// 聊天头（含 PoW 应答，取不到则不附带）。
    async fn completion_headers(&self) -> Vec<(String, String)> {
        let mut headers = self.default_headers();
        if let Some(pow) = self.pow_header().await {
            headers.push(("x-ds-pow-response".to_string(), pow));
        }
        headers
    }

    /// 聊天共用实现：`on_chunk` 为 Some 时流式回吐正文增量，工具调用在收尾时解析。
    async fn chat(
        &self,
        request: InferRequest,
        on_chunk: Option<ChunkSink>,
    ) -> Result<ModelResponse> {
        self.token()?;
        let prompt = Self::prompt_of(&request);
        // 请求没带工具时不识别围栏：模型若引用围栏只是正文。
        let recognize_tools = request
            .tools
            .as_ref()
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty());
        let session_id = self.ensure_session().await?;
        info!(
            "DeepSeek 网页推理 session={} prompt_len={}",
            session_id,
            prompt.len()
        );
        let channel_request = ChannelRequest {
            url: COMPLETION_URL.to_string(),
            method: "POST".to_string(),
            headers: self.completion_headers().await,
            body: Some(self.completion_body(&session_id, &prompt)),
            session_key: Some(session_id.clone()),
        };

        let (text, reasoning, tool_calls, status, raw_body) = match on_chunk {
            Some(user_sink) => {
                // 帧处理器与驱动器各自独立：闭包是 FnMut，跨 chunk 的状态用 Arc<Mutex> 共享。
                let frames = Arc::new(Mutex::new(SseFrameProcessor::new()));
                let lines = Arc::new(Mutex::new(String::new()));
                let driver = Arc::new(Mutex::new(ChatStreamDriver::new(
                    user_sink,
                    recognize_tools,
                )));
                let feeder_frames = frames.clone();
                let feeder_lines = lines.clone();
                let feeder_driver = driver.clone();
                let response = self
                    .executor
                    .execute_stream(
                        channel_request,
                        Box::new(move |raw| {
                            let (Ok(mut frames), Ok(mut lines), Ok(mut driver)) = (
                                feeder_frames.lock(),
                                feeder_lines.lock(),
                                feeder_driver.lock(),
                            ) else {
                                return;
                            };
                            lines.push_str(&raw);
                            while let Some(index) = lines.find('\n') {
                                let line = lines[..index].trim_end_matches('\r').to_string();
                                lines.drain(..=index);
                                feed_line(&mut frames, &mut driver, &line);
                            }
                        }),
                    )
                    .await?;
                if !response.ok {
                    anyhow::bail!("DeepSeek 网页推理失败（HTTP {}）", response.status);
                }
                // 最后一行可能没等到换行符：收尾时补处理，再让驱动器闭合围栏。
                let tail = lines.lock().unwrap().trim_end_matches('\r').to_string();
                let mut driver = driver.lock().unwrap();
                if !tail.trim().is_empty() {
                    feed_line(&mut frames.lock().unwrap(), &mut driver, &tail);
                }
                driver.finish();
                let text = driver.text().to_string();
                let reasoning = driver.reasoning().to_string();
                let tool_calls = driver.tool_calls().to_vec();
                (text, reasoning, tool_calls, response.status, response.body)
            }
            None => {
                let response = self.executor.execute(channel_request).await?;
                if !response.ok {
                    anyhow::bail!("DeepSeek 网页推理失败（HTTP {}）", response.status);
                }
                let driver = parse_sse_text(&response.body, recognize_tools);
                (
                    driver.text().to_string(),
                    driver.reasoning().to_string(),
                    driver.tool_calls().to_vec(),
                    response.status,
                    response.body,
                )
            }
        };

        let has_calls = !tool_calls.is_empty();
        let finish_reason = if has_calls { "tool_calls" } else { "stop" };
        // 响应落盘（开发期排查）：落的是解析后的内容，不影响返回给客户端的结果。
        if let Some(dump) = &self.dump {
            if dump.response_enabled() {
                let payload = ResponseDump {
                    received_at: chrono::Utc::now().to_rfc3339(),
                    stage: "response",
                    model: request.model.clone(),
                    channel: CHANNEL,
                    web_session: Some(session_id.clone()),
                    status,
                    raw_len: raw_body.len(),
                    reasoning: reasoning.clone(),
                    reasoning_len: reasoning.chars().count(),
                    text: text.clone(),
                    text_len: text.chars().count(),
                    tool_calls: Value::Array(tool_calls.clone()),
                    finish_reason,
                };
                let path = dump.write_response(&payload)?;
                info!("响应已落盘：{}", path.display());
            }
        }
        if text.trim().is_empty() && !has_calls {
            warn!(
                "网页回答为空：session={} status={} 未解析出正文与工具调用，原始响应：{}",
                session_id,
                status,
                raw_body.chars().take(300).collect::<String>()
            );
        }
        Ok(ModelResponse {
            text,
            reasoning: if reasoning.is_empty() {
                None
            } else {
                Some(reasoning)
            },
            tool_calls: if has_calls {
                Some(Value::Array(tool_calls))
            } else {
                None
            },
            usage: None,
            finish_reason: Some(finish_reason.to_string()),
            is_complete: true,
        })
    }
}

/// 响应阶段落盘快照（开发期排查用）：网页返回经 `SseFrameProcessor` 解析后的内容。
#[derive(Debug, Serialize)]
struct ResponseDump {
    received_at: String,
    /// 阶段标识，便于与请求阶段的快照区分。
    stage: &'static str,
    /// 对外模型名。
    model: String,
    /// 承载它的渠道。
    channel: &'static str,
    /// 执行本次请求的渠道会话。
    web_session: Option<String>,
    /// 网页返回的 HTTP 状态。
    status: u16,
    /// 网页原始返回文本长度（流式下原始文本不落盘，只记长度）。
    raw_len: usize,
    /// 思考内容（THINK 片段）。
    reasoning: String,
    reasoning_len: usize,
    /// 正文（不含思考与工具围栏）。
    text: String,
    text_len: usize,
    /// 解析出的工具调用（OpenAI 标准 tool_calls 数组，无调用时为空数组）。
    tool_calls: Value,
    finish_reason: &'static str,
}

impl Model for DeepSeekWebModel {
    fn id(&self) -> &str {
        CHANNEL
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    fn capability(&self) -> ModelCapability {
        ModelCapability::basic()
    }

    fn concurrency(&self) -> Concurrency {
        self.concurrency
    }

    fn infer<'a>(&'a self, request: InferRequest) -> BoxFuture<'a, Result<ModelResponse>> {
        Box::pin(async move { self.chat(request, None).await })
    }

    fn infer_stream<'a>(
        &'a self,
        request: InferRequest,
        on_chunk: ChunkSink,
    ) -> BoxFuture<'a, Result<ModelResponse>> {
        Box::pin(async move { self.chat(request, Some(on_chunk)).await })
    }
}

impl PluginModel for DeepSeekWebModel {
    fn channel(&self) -> &str {
        CHANNEL
    }

    fn create_channel_session(&self) -> BoxFuture<'_, Result<Option<String>>> {
        Box::pin(async move {
            let json = self
                .send("POST", SESSION_CREATE_URL, Some("{}".to_string()))
                .await?;
            let data = json.get("data");
            let candidates = [
                data.and_then(|d| d.get("biz_data"))
                    .and_then(|b| b.get("chat_session"))
                    .and_then(|s| s.get("id")),
                data.and_then(|d| d.get("biz_data"))
                    .and_then(|b| b.get("chat_session_id")),
                data.and_then(|d| d.get("biz_data")).and_then(|b| b.get("id")),
                data.and_then(|d| d.get("chat_session_id")),
                data.and_then(|d| d.get("id")),
                json.get("chat_session_id"),
            ];
            for candidate in candidates {
                if let Some(id) = candidate.and_then(Value::as_str) {
                    if id.len() > 10 {
                        debug!("已创建 DeepSeek 会话 {}", id);
                        return Ok(Some(id.to_string()));
                    }
                }
            }
            Ok(None)
        })
    }

    fn list_channel_sessions(&self) -> BoxFuture<'_, Result<Value>> {
        Box::pin(async move {
            let url = format!("{SESSION_LIST_URL}?lte_cursor.pinned=false");
            let json = self.send("GET", &url, None).await?;
            Ok(extract_sessions(&json))
        })
    }

    fn delete_channel_session<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let body = json!({ "chat_session_id": session_id }).to_string();
            let json = self.send("POST", SESSION_DELETE_URL, Some(body)).await?;
            Ok(is_biz_ok(&json))
        })
    }

    fn rename_channel_session<'a>(
        &'a self,
        session_id: &'a str,
        title: &'a str,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let body = json!({ "chat_session_id": session_id, "title": title }).to_string();
            let response = self
                .executor
                .execute(ChannelRequest {
                    url: SESSION_TITLE_URL.to_string(),
                    method: "POST".to_string(),
                    headers: self.default_headers(),
                    body: Some(body),
                    session_key: self.session_key(),
                })
                .await?;
            if !response.ok {
                warn!("设置会话标题失败（HTTP {}）", response.status);
                return Ok(false);
            }
            if response.body.is_empty() {
                return Ok(true);
            }
            let json: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
            Ok(is_biz_ok(&json))
        })
    }

    fn channel_history<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let url = format!(
                "{}?chat_session_id={}&cache_version=8",
                HISTORY_URL,
                encode_query(session_id)
            );
            self.send("GET", &url, None).await
        })
    }
}

/// 从会话列表响应里抽取会话数组；无法识别时原样返回。
fn extract_sessions(json: &Value) -> Value {
    json.get("data")
        .and_then(|data| data.get("biz_data"))
        .and_then(|biz| biz.get("chat_sessions"))
        .cloned()
        .filter(Value::is_array)
        .unwrap_or_else(|| json.clone())
}

/// 会话接口业务成功判定：`code == 0` 且 `data.biz_code == 0`（缺失按 0 计）。
fn is_biz_ok(json: &Value) -> bool {    let code_ok = json.get("code").and_then(Value::as_i64).unwrap_or(0) == 0;
    let biz_ok = json
        .get("data")
        .and_then(|d| d.get("biz_code"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
        == 0;
    code_ok && biz_ok
}

/// 工具调用围栏识别器：开边 `<tool_call>`、闭边 `</tool_call>`。
///
/// `enabled` 为 false 时不认领任何围栏，整段输出都按普通正文处理。
struct ToolCallRecognizer {
    enabled: bool,
}

impl FenceRecognizer for ToolCallRecognizer {
    fn recognize(&self, buf: &str) -> Recognized {
        if !self.enabled {
            return Recognized::NotMine;
        }
        if buf.starts_with(TOOL_CALL_OPEN) {
            return Recognized::FenceOpen {
                consume: TOOL_CALL_OPEN.len(),
                edge: TOOL_CALL_OPEN.to_string(),
                close: Some(CloseSpec::exact(TOOL_CALL_CLOSE)),
            };
        }
        // 开边可能被切在多个 chunk 里（如 "<tool_c"）。
        if TOOL_CALL_OPEN.starts_with(buf) {
            return Recognized::NeedMore;
        }
        Recognized::NotMine
    }
}

/// 聊天流驱动器：片段增量 → 正文增量 + 工具调用。
///
/// 正文增量交给统一流式处理器：围栏外的内容实时回吐，
/// `<tool_call>` 围栏内的内容由处理器累积，收尾时统一解析成工具调用。
struct ChatStreamDriver {
    sink: ChunkSink,
    processor: StreamProcessor,
    text: String,
    reasoning: String,
    tool_calls: Vec<Value>,
}

impl ChatStreamDriver {
    fn new(sink: ChunkSink, recognize_tools: bool) -> Self {
        Self {
            sink,
            processor: StreamProcessor::new(
                ToolCallRecognizer {
                    enabled: recognize_tools,
                },
                ProcessorOptions::default(),
            ),
            text: String::new(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
        }
    }

    /// 接收一个片段增量：正文送进围栏处理器，思考直接回吐，其它片段丢弃。
    fn feed(&mut self, ftype: &str, delta: &str) {
        match ftype {
            FRAGMENT_RESPONSE => {
                for event in self.processor.feed(delta) {
                    self.emit(event);
                }
            }
            // 思考不计入正文，按思考增量回吐（调用方会映射成 reasoning_content）
            FRAGMENT_THINK => {
                self.reasoning.push_str(delta);
                (self.sink)(json!({ "THINK": delta }).to_string());
            }
            _ => {}
        }
    }

    /// 收尾：放出残留内容、强制闭合未闭合围栏，最后把围栏内容解析成工具调用。
    /// 结果经 [`Self::text`] / [`Self::reasoning`] / [`Self::tool_calls`] 读取。
    fn finish(&mut self) {
        let (events, nodes) = self.processor.finish();
        for event in events {
            self.emit(event);
        }
        for node in &nodes {
            let Node::Fence(fence) = node else {
                continue;
            };
            match parse_tool_call(&fence.content) {
                Some(call) => self.tool_calls.push(call),
                None => {
                    // 不是工具调用就原样当正文回吐，不吞掉模型输出。
                    let raw = reconstruct_fence(fence);
                    warn!(
                        "工具围栏内容无法解析为工具调用，按正文回吐：{}",
                        raw.chars().take(200).collect::<String>()
                    );
                    self.push_text(&raw);
                }
            }
        }
        // 工具调用以增量回吐：流式调用方只认增量，不看最终结果。
        if !self.tool_calls.is_empty() {
            (self.sink)(json!({ "tool_calls": self.tool_calls }).to_string());
        }
    }

    /// 正文（不含思考与工具围栏）。
    fn text(&self) -> &str {
        &self.text
    }

    /// 思考原文（不含正文）。
    fn reasoning(&self) -> &str {
        &self.reasoning
    }

    /// 解析出的工具调用。
    fn tool_calls(&self) -> &[Value] {
        &self.tool_calls
    }

    /// 处理器事件：只关心围栏外的正文，围栏内容留给 `finish` 统一解析。
    fn emit(&mut self, event: StreamEvent) {
        if let StreamEvent::Content(content) = event {
            self.push_text(&content);
        }
    }

    /// 围栏外正文：累计进结果并实时交给调用方。
    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.text.push_str(text);
        (self.sink)(json!({ "RESPONSE": text }).to_string());
    }
}

/// 还原围栏原文（开边 + 内容 + 子围栏 + 规范闭边）。
fn reconstruct_fence(fence: &Fence) -> String {
    let mut out = fence.open_edge.clone();
    out.push_str(&fence.content);
    for child in &fence.children {
        out.push_str(&reconstruct_fence(child));
    }
    out.push_str(TOOL_CALL_CLOSE);
    out
}

/// 围栏内容 → OpenAI 工具调用；缺工具名、或内容不是 JSON 对象时返回 None。
fn parse_tool_call(content: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(content.trim()).ok()?;
    let function = value.get("function").unwrap_or(&value);
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())?;
    // OpenAI 契约里 arguments 是 JSON 字符串：本来已是字符串就沿用，否则序列化对象。
    let arguments = match function.get("arguments") {
        Some(Value::String(raw)) => raw.clone(),
        Some(other) => other.to_string(),
        None => "{}".to_string(),
    };
    Some(json!({
        "id": format!("call_{}", uuid::Uuid::new_v4().simple()),
        "type": "function",
        "function": { "name": name, "arguments": arguments },
    }))
}

/// 一行 SSE 交给帧处理器，产出的增量按片段类型转交驱动器。
fn feed_line(frames: &mut SseFrameProcessor, driver: &mut ChatStreamDriver, line: &str) {
    for (ftype, delta) in frames.process_line(line) {
        driver.feed(ftype.as_str(), &delta);
    }
}

/// 解析整段响应文本（非流式情形）：返回收尾后的驱动器，供读取正文 / 思考 / 工具调用。
fn parse_sse_text(body: &str, recognize_tools: bool) -> ChatStreamDriver {
    let mut frames = SseFrameProcessor::new();
    let mut driver = ChatStreamDriver::new(Box::new(|_| {}), recognize_tools);
    for line in body.lines() {
        feed_line(&mut frames, &mut driver, line.trim_end_matches('\r'));
    }
    driver.finish();
    driver
}

/// 按 RFC 3986 对查询参数值做百分号编码。
fn encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{parse_sse_text, DeepSeekWebModel, CHANNEL};
    use crate::models::defs::{
        ChannelExecutor, ChannelRequest, ChannelResponse, ChunkSink, InferRequest,
    };
    use crate::models::model::Model;
    use crate::models::plugin::PluginModel;
    use futures_util::future::BoxFuture;
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};

    /// 记录请求并按 URL 返回预设响应。
    struct MockExecutor {
        requests: Mutex<Vec<ChannelRequest>>,
        completion_body: String,
        /// 流式分块粒度（按字符数切分）；0 表示整段一次送出。
        stream_chunk: usize,
    }

    impl ChannelExecutor for MockExecutor {
        fn token(&self) -> Option<String> {
            Some("tok".to_string())
        }

        fn settings_token(&self) -> Option<String> {
            None
        }

        fn execute(
            &self,
            request: ChannelRequest,
        ) -> BoxFuture<'_, anyhow::Result<ChannelResponse>> {
            Box::pin(async move {
                let body = if request.url.contains("chat_session/create") {
                    json!({ "data": { "biz_data": { "chat_session": { "id": "session-abcdefghij" } } } })
                        .to_string()
                } else if request.url.contains("create_pow_challenge") {
                    json!({ "data": {} }).to_string()
                } else if request.url.contains("fetch_page") {
                    json!({ "data": { "biz_data": { "chat_sessions": [{ "id": "s1" }] } } })
                        .to_string()
                } else if request.url.contains("history_messages") {
                    json!({ "data": { "biz_data": { "chat_messages": [] } } }).to_string()
                } else if request.url.contains("delete") || request.url.contains("update_title") {
                    json!({ "code": 0, "data": { "biz_code": 0 } }).to_string()
                } else {
                    self.completion_body.clone()
                };
                self.requests.lock().unwrap().push(request);
                Ok(ChannelResponse {
                    ok: true,
                    status: 200,
                    body,
                })
            })
        }

        fn execute_stream(
            &self,
            request: ChannelRequest,
            on_chunk: ChunkSink,
        ) -> BoxFuture<'_, anyhow::Result<ChannelResponse>> {
            Box::pin(async move {
                let body = self.completion_body.clone();
                let mut on_chunk = on_chunk;
                if self.stream_chunk == 0 {
                    on_chunk(body.clone());
                } else {
                    let chars: Vec<char> = body.chars().collect();
                    for piece in chars.chunks(self.stream_chunk) {
                        on_chunk(piece.iter().collect());
                    }
                }
                self.requests.lock().unwrap().push(request);
                Ok(ChannelResponse {
                    ok: true,
                    status: 200,
                    body,
                })
            })
        }
    }

    fn model(completion_body: &str) -> DeepSeekWebModel {
        model_split(completion_body, 0)
    }

    /// 带分块粒度的模型：验证被任意边界切开的围栏。
    fn model_split(completion_body: &str, stream_chunk: usize) -> DeepSeekWebModel {
        DeepSeekWebModel::new(
            "DeepSeek",
            Arc::new(MockExecutor {
                requests: Mutex::new(Vec::new()),
                completion_body: completion_body.to_string(),
                stream_chunk,
            }),
        )
    }

    fn user_request() -> InferRequest {
        InferRequest::new("deepseek-web", vec![json!({ "role": "user", "content": "hi" })])
    }

    /// 带工具的请求。
    fn tool_request() -> InferRequest {
        let mut request = user_request();
        request.tools = Some(json!([{
            "type": "function",
            "function": { "name": "read_file", "description": "读取文件" }
        }]));
        request
    }

    /// 一个 `response/content` SSE 帧。
    fn content_frame(text: &str) -> String {
        format!(
            "data: {}\n",
            json!({ "p": "response/content", "v": text })
        )
    }

    /// Model 基础：标识、能力、并发，以及非流式推理抽取正文。
    #[tokio::test]
    async fn infer_extracts_content_and_is_single_concurrency() {
        let body = concat!(
            "data: {\"p\":\"response/thinking_content\",\"v\":\"think\"}\n",
            "data: {\"p\":\"response/content\",\"v\":\"你好\"}\n",
            "data: {\"p\":\"response/content\",\"v\":\"世界\"}\n",
        );
        let model = model(body);

        assert_eq!(model.id(), CHANNEL);
        assert_eq!(model.display_name(), "DeepSeek");
        assert_eq!(model.channel(), "deepseek-web");
        assert!(model.capability().stream);
        assert!(model.concurrency().per_session_serial);

        let response = model.infer(user_request()).await.expect("推理应成功");
        assert_eq!(response.text, "你好世界");
        assert!(response.is_complete);
    }

    /// 流式：正文增量按序回吐，最终结果累计完整。
    #[tokio::test]
    async fn infer_stream_emits_deltas() {
        let body = concat!(
            "data: {\"p\":\"response/content\",\"v\":\"第一\"}\n",
            "data: {\"p\":\"response/content\",\"v\":\"第二\"}\n",
        );
        let model = model(body);
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let response = model
            .infer_stream(
                user_request(),
                Box::new(move |delta| sink.lock().unwrap().push(delta)),
            )
            .await
            .expect("流式推理应成功");
        assert_eq!(response.text, "第一第二");
        let contents: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|delta| crate::apis::base::parse_delta(delta).content)
            .collect();
        assert_eq!(contents, vec!["第一".to_string(), "第二".to_string()]);
    }

    /// 非流式：正文里的工具围栏解析成工具调用，围栏外的文字保留为正文。
    #[tokio::test]
    async fn infer_extracts_tool_calls_from_fence() {
        let body = content_frame("先读文件")
            + &content_frame(
                "<tool_call>\n{\"name\":\"read_file\",\"arguments\":{\"path\":\"a.rs\"}}\n</tool_call>",
            )
            + &content_frame("读完就答");
        let response = model(&body)
            .infer(tool_request())
            .await
            .expect("推理应成功");

        assert_eq!(response.text, "先读文件读完就答", "围栏文字不应混进正文");
        assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
        let calls = response.tool_calls.expect("应解析出工具调用");
        assert_eq!(calls.as_array().unwrap().len(), 1);
        let call = &calls[0];
        assert!(call["id"].as_str().unwrap().starts_with("call_"));
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "read_file");
        assert_eq!(call["function"]["arguments"], "{\"path\":\"a.rs\"}");
    }

    /// 请求没带工具时，同样的围栏按普通正文返回。
    #[tokio::test]
    async fn infer_keeps_fence_as_text_without_tools() {
        let fence = "<tool_call>\n{\"name\":\"read_file\",\"arguments\":{}}\n</tool_call>";
        let response = model(&content_frame(fence))
            .infer(user_request())
            .await
            .expect("推理应成功");

        assert_eq!(response.text, fence);
        assert!(response.tool_calls.is_none());
        assert_eq!(response.finish_reason.as_deref(), Some("stop"));
    }

    /// 围栏内容不是合法工具调用时按正文回吐，不吞掉模型输出。
    #[tokio::test]
    async fn infer_falls_back_when_fence_is_not_tool_call() {
        let fence = "<tool_call>\n这不是 JSON\n</tool_call>";
        let response = model(&content_frame(fence))
            .infer(tool_request())
            .await
            .expect("推理应成功");

        assert!(response.text.contains("这不是 JSON"), "{}", response.text);
        assert!(response.tool_calls.is_none());
    }

    /// 多个围栏 → 多个工具调用。
    #[tokio::test]
    async fn infer_extracts_multiple_tool_calls() {
        let body = content_frame("<tool_call>{\"name\":\"a\",\"arguments\":{}}</tool_call>")
            + &content_frame("<tool_call>{\"name\":\"b\",\"arguments\":{\"n\":1}}</tool_call>");
        let response = model(&body)
            .infer(tool_request())
            .await
            .expect("推理应成功");

        let calls = response.tool_calls.expect("应解析出工具调用");
        let names: Vec<&str> = calls
            .as_array()
            .unwrap()
            .iter()
            .map(|call| call["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["a", "b"]);
        assert_eq!(calls[1]["function"]["arguments"], "{\"n\":1}");
    }

    /// 流式：围栏被任意边界切开也不外泄给客户端，收尾增量里给出工具调用。
    #[tokio::test]
    async fn infer_stream_hides_fence_and_reports_tool_calls() {
        let fence = "<tool_call>\n{\"name\":\"read_file\",\"arguments\":{\"path\":\"a.rs\"}}\n</tool_call>";
        let body = content_frame("先读文件") + &content_frame(fence);
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let response = model_split(&body, 7)
            .infer_stream(
                tool_request(),
                Box::new(move |delta| sink.lock().unwrap().push(delta)),
            )
            .await
            .expect("流式推理应成功");

        let deltas = seen.lock().unwrap().clone();
        let contents: String = deltas
            .iter()
            .map(|delta| crate::apis::base::parse_delta(delta).content)
            .collect();
        assert_eq!(contents, "先读文件", "围栏文字不应作为正文流出");
        assert!(
            deltas.iter().any(|delta| delta.contains("read_file")),
            "应有承载工具调用的增量：{:?}",
            deltas
        );
        assert_eq!(response.text, "先读文件");
        assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(response.tool_calls.unwrap()[0]["function"]["name"], "read_file");

        // 增量经 API 层解析器后必须还原成工具调用（客户端拿到的那一层）
        let parsed: Vec<Value> = deltas
            .iter()
            .filter_map(|delta| crate::apis::base::parse_delta(delta).tool_calls)
            .flat_map(|calls| calls.as_array().cloned().unwrap_or_default())
            .collect();
        assert_eq!(parsed.len(), 1, "{:?}", deltas);
        assert_eq!(parsed[0]["function"]["name"], "read_file");
        assert_eq!(parsed[0]["function"]["arguments"], "{\"path\":\"a.rs\"}");
    }

    /// 流式：最后一行没有换行符也要解析（分块边界不可预期）。
    #[tokio::test]
    async fn infer_stream_parses_trailing_line_without_newline() {
        let body = "data: {\"p\":\"response/content\",\"v\":\"尾\"}";
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let response = model_split(body, 3)
            .infer_stream(
                user_request(),
                Box::new(move |delta| sink.lock().unwrap().push(delta)),
            )
            .await
            .expect("流式推理应成功");

        assert_eq!(response.text, "尾");
        let contents: String = seen
            .lock()
            .unwrap()
            .iter()
            .map(|delta| crate::apis::base::parse_delta(delta).content)
            .collect();
        assert_eq!(contents, "尾");
    }

    /// 流式：围栏跨多个真实协议帧拼接，围栏前后的正文都照常流出。
    #[tokio::test]
    async fn infer_stream_joins_fence_across_frames() {
        let body = concat!(
            "data: {\"v\":{\"response\":{\"message_id\":8,\"fragments\":[{\"type\":\"RESPONSE\",\"content\":\"\"}]}}}\n",
            "data: {\"p\":\"response/fragments/0/content\",\"o\":\"APPEND\",\"v\":\"前\"}\n",
            "data: {\"p\":\"response/fragments/0/content\",\"o\":\"APPEND\",\"v\":\"<tool_call>{\\\"name\\\":\\\"read\"}\n",
            "data: {\"p\":\"response/fragments/0/content\",\"o\":\"APPEND\",\"v\":\"_file\\\",\\\"arguments\\\":{\\\"path\\\":\\\"a.rs\\\"}}</tool_call>\"}\n",
            "data: {\"p\":\"response/fragments/0/content\",\"o\":\"APPEND\",\"v\":\"后\"}\n",
        );
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let response = model_split(body, 5)
            .infer_stream(
                tool_request(),
                Box::new(move |delta| sink.lock().unwrap().push(delta)),
            )
            .await
            .expect("流式推理应成功");

        let contents: String = seen
            .lock()
            .unwrap()
            .iter()
            .map(|delta| crate::apis::base::parse_delta(delta).content)
            .collect();
        assert_eq!(contents, "前后", "围栏应在正文中消失且不打断顺序");
        assert_eq!(response.text, "前后");
        let calls = response.tool_calls.expect("应解析出工具调用");
        assert_eq!(calls[0]["function"]["name"], "read_file");
        assert_eq!(calls[0]["function"]["arguments"], "{\"path\":\"a.rs\"}");
    }

    /// 空回答必须把原始响应打进日志：否则「请求走完了但客户端什么都没有」无从排查。
    #[tokio::test]
    async fn empty_answer_logs_raw_response() {
        let body = concat!(
            "data: {\"p\":\"response/status\",\"v\":\"x\"}\n",
            "data: {\"code\":1,\"msg\":\"会话无效\"}\n",
        );
        let logs = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || LogSink(sink.clone()))
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let response = model(body).infer(user_request()).await.expect("推理应成功");
        assert!(response.text.is_empty());
        assert!(response.tool_calls.is_none());

        let logged = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        assert!(logged.contains("网页回答为空"), "{}", logged);
        assert!(logged.contains("会话无效"), "应打出原始响应：{}", logged);
    }

    /// 日志写入内存缓冲（用例断言日志内容用）。
    struct LogSink(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 围栏解析规则：arguments 为字符串时沿用，缺工具名或内容不是对象时拒绝。
    #[test]
    fn tool_call_parsing_rules() {
        let call = super::parse_tool_call("{\"name\":\"a\",\"arguments\":{\"n\":1}}").unwrap();
        assert_eq!(call["function"]["arguments"], "{\"n\":1}");

        let call = super::parse_tool_call("{\"function\":{\"name\":\"b\",\"arguments\":\"{}\"}}")
            .unwrap();
        assert_eq!(call["function"]["name"], "b");
        assert_eq!(call["function"]["arguments"], "{}");

        // 缺 arguments：补空对象，保持 OpenAI 契约里 arguments 恒为字符串
        let call = super::parse_tool_call("{\"name\":\"c\"}").unwrap();
        assert_eq!(call["function"]["arguments"], "{}");

        assert!(super::parse_tool_call("not json").is_none());
        assert!(super::parse_tool_call("{\"arguments\":{}}").is_none());
        assert!(super::parse_tool_call("{\"name\":\"\"}").is_none());
        assert!(super::parse_tool_call("\"text\"").is_none());
    }

    /// 提示词拼接：系统提示词单独置顶，其余消息里只有最新一条不算历史，工具段在最新记录之前。
    #[test]
    fn prompt_orders_system_history_rules_tools_and_latest() {
        let mut request = InferRequest::new(
            "deepseek-web",
            vec![
                json!({ "role": "system", "content": "你是助手" }),
                json!({ "role": "user", "content": "第一个问题" }),
                json!({ "role": "assistant", "content": "第一个回答" }),
                json!({ "role": "tool", "tool_call_id": "c1", "content": "工具结果" }),
                json!({ "role": "user", "content": "第二个问题" }),
            ],
        );
        request.tools = Some(json!([
            {
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "读取文件",
                    "parameters": { "type": "object", "properties": { "path": { "type": "string" } } }
                }
            }
        ]));

        let prompt = DeepSeekWebModel::prompt_of(&request);
        // 段标题由常量给出，顺序固定
        let expected_order = [
            super::PROMPT_PREFIX,
            super::SYSTEM_HEADING,
            super::HISTORY_HEADING,
            "【user】\n第一个问题",
            "【assistant】\n第一个回答",
            "【tool】\n工具结果",
            super::TOOL_RULE_HEADING,
            super::TOOLS_HEADING,
            "- read_file：读取文件",
        ];
        let mut cursor = 0usize;
        for fragment in expected_order {
            let at = prompt.find(fragment).unwrap_or_else(|| {
                panic!("提示词缺少片段 {:?}：\n{}", fragment, prompt)
            });
            assert!(at >= cursor, "片段顺序不对：{:?}\n{}", fragment, prompt);
            cursor = at;
        }
        // 系统提示词段紧跟标题，且不带角色标注
        assert!(
            prompt.contains(&format!("{}\n你是助手", super::SYSTEM_HEADING)),
            "{}",
            prompt
        );
        assert!(!prompt.contains("【system】"), "{}", prompt);
        // 规则正文由模型自己给出：围栏格式 + 限定规则
        assert!(prompt.contains("<tool_call>"), "{}", prompt);
        assert!(prompt.contains("必须逐字符写成 <tool_call>"), "{}", prompt);
        assert!(prompt.contains("只能调用下面列出的工具"), "{}", prompt);
        // 参数为紧凑 JSON
        assert!(prompt.contains(r#"参数：{"properties""#), "{}", prompt);
        // 最新一条记录拼在最末尾
        assert!(prompt.ends_with("【user】\n第二个问题"), "{}", prompt);
    }

    /// 最新一条记录是工具返回时同样拼在末尾；历史里助手的旧工具调用还原成围栏。
    #[test]
    fn prompt_appends_latest_tool_result_and_restores_history_tool_calls() {
        let request = InferRequest::new(
            "deepseek-web",
            vec![
                json!({ "role": "user", "content": "读文件" }),
                json!({
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": "c1",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{\"path\":\"a.rs\"}" }
                    }]
                }),
                json!({ "role": "tool", "tool_call_id": "c1", "content": "文件内容" }),
            ],
        );

        let prompt = DeepSeekWebModel::prompt_of(&request);
        // 助手历史消息：正文为空也不丢围栏，字符串参数按 JSON 还原
        assert!(prompt.contains("【assistant】\n<tool_call>"), "{}", prompt);
        assert!(prompt.contains(r#""name":"read_file""#), "{}", prompt);
        assert!(prompt.contains(r#""arguments":{"path":"a.rs"}"#), "{}", prompt);
        assert!(prompt.contains("</tool_call>"), "{}", prompt);
        // 围栏属于历史段
        assert!(
            prompt.find("<tool_call>").unwrap() > prompt.find(super::HISTORY_HEADING).unwrap(),
            "{}",
            prompt
        );
        // 最新记录（工具返回）在最后
        assert!(prompt.ends_with("【tool】\n文件内容"), "{}", prompt);
        // 没有工具时不给格式规则段
        assert!(!prompt.contains(super::TOOL_RULE_HEADING), "{}", prompt);
    }

    /// 多模态正文取文本片段；非文本片段只留占位标记，不把 base64 塞进提示词。
    #[test]
    fn prompt_renders_multimodal_parts_without_payload() {
        let request = InferRequest::new(
            "deepseek-web",
            vec![json!({
                "role": "user",
                "content": [
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } },
                    { "type": "text", "text": "图片背景是什么颜色" }
                ]
            })],
        );

        let prompt = DeepSeekWebModel::prompt_of(&request);
        assert!(prompt.contains("图片背景是什么颜色"), "{}", prompt);
        assert!(prompt.contains("[image_url]"), "非文本片段应有占位：{}", prompt);
        assert!(!prompt.contains("base64"), "不应带上图片负载：{}", prompt);
        assert!(
            prompt.starts_with("你只需要根据以下内容回答问题，上面的对话内容不要考虑："),
            "固定前缀不应被多模态处理影响：{}",
            prompt
        );
    }

    /// 无工具或空工具数组时不拼接工具段与规则段。
    #[test]
    fn prompt_skips_tool_section_when_absent() {
        let mut request =
            InferRequest::new("deepseek-web", vec![json!({ "role": "user", "content": "hi" })]);
        let expected = "你只需要根据以下内容回答问题，上面的对话内容不要考虑：\n\n【user】\nhi";
        assert_eq!(DeepSeekWebModel::prompt_of(&request), expected);

        request.tools = Some(json!([]));
        assert_eq!(
            DeepSeekWebModel::prompt_of(&request),
            expected,
            "空工具数组不应产生工具段"
        );
    }

    /// 只有系统提示词与最新一条记录、没有历史时，不产生历史段标题。
    #[test]
    fn prompt_skips_history_heading_without_history() {
        let request = InferRequest::new(
            "deepseek-web",
            vec![
                json!({ "role": "system", "content": "系统" }),
                json!({ "role": "user", "content": "问题" }),
            ],
        );
        let prompt = DeepSeekWebModel::prompt_of(&request);
        assert_eq!(
            prompt,
            format!(
                "{}\n\n{}\n系统\n\n【user】\n问题",
                super::PROMPT_PREFIX,
                super::SYSTEM_HEADING
            )
        );
    }

    /// 固定前缀必须在最开头（其后才是各段）。
    #[test]
    fn prompt_starts_with_fixed_prefix() {
        let mut request = InferRequest::new(
            "deepseek-web",
            vec![
                json!({ "role": "system", "content": "系统" }),
                json!({ "role": "user", "content": "问题" }),
            ],
        );
        request.tools = Some(json!([{ "function": { "name": "t", "description": "工具" } }]));

        let prompt = DeepSeekWebModel::prompt_of(&request);
        assert!(prompt.starts_with(super::PROMPT_PREFIX), "{}", prompt);
        let prefix_at = 0usize;
        for later in [
            super::SYSTEM_HEADING,
            super::TOOL_RULE_HEADING,
            super::TOOLS_HEADING,
            "【user】\n问题",
        ] {
            assert!(prompt.find(later).unwrap() > prefix_at, "{} 应在前缀之后", later);
        }
    }

    /// 围栏标签名与开/闭边字面量必须一致（提示词、识别器、解析器共用同一约定）。
    #[test]
    fn tool_call_fence_literals_agree() {
        assert_eq!(super::TOOL_CALL_OPEN, format!("<{}>", super::TOOL_CALL_TAG));
        assert_eq!(super::TOOL_CALL_CLOSE, format!("</{}>", super::TOOL_CALL_TAG));
        // 提示词里的规则与历史围栏都用这套字面量
        let mut request = InferRequest::new("deepseek-web", vec![json!({ "role": "user", "content": "hi" })]);
        request.tools = Some(json!([{ "function": { "name": "t" } }]));
        let prompt = DeepSeekWebModel::prompt_of(&request);
        assert!(prompt.contains(super::TOOL_CALL_OPEN), "{}", prompt);
        assert!(prompt.contains(super::TOOL_CALL_CLOSE), "{}", prompt);
    }

    /// 响应落盘：落一份解析结果快照，且不打断链路，客户端拿到正常结果。
    #[tokio::test]
    async fn response_dump_writes_file_and_continues() {
        let dir = std::env::temp_dir().join(format!("poolman-dump-{}", uuid::Uuid::new_v4()));
        let dump = Arc::new(crate::dump::DumpConfig::new(
            true,
            dir.to_string_lossy().to_string(),
        ));
        let body = "data: {\"p\":\"response/content\",\"v\":\"答\"}\n";
        let executor = Arc::new(MockExecutor {
            requests: Mutex::new(Vec::new()),
            completion_body: body.to_string(),
            stream_chunk: 0,
        });
        let model =
            DeepSeekWebModel::new("DeepSeek", executor.clone() as Arc<dyn ChannelExecutor>)
                .with_dump(dump);

        let response = model.infer(user_request()).await.expect("落盘不应打断链路");
        assert_eq!(response.text, "答");
        assert!(
            !executor.requests.lock().unwrap().is_empty(),
            "落盘后仍应发起渠道请求"
        );

        let files: Vec<_> = std::fs::read_dir(&dir)
            .expect("落盘目录应已创建")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect();
        assert_eq!(files.len(), 1, "一次请求落一份响应快照");
        assert!(
            files[0]
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("response-")),
            "快照文件名应带阶段前缀：{:?}",
            files[0]
        );

        let doc: Value = serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
        assert_eq!(doc["stage"], "response");
        assert_eq!(doc["channel"], CHANNEL);
        assert_eq!(doc["model"], "deepseek-web");
        assert_eq!(doc["web_session"], "session-abcdefghij");
        assert_eq!(doc["status"], 200);
        assert_eq!(doc["text"], "答");
        assert_eq!(doc["text_len"], 1);
        assert_eq!(doc["reasoning"], "", "该次没有思考片段");
        assert_eq!(doc["tool_calls"], json!([]));
        assert_eq!(doc["finish_reason"], "stop");
        assert_eq!(doc["raw_len"], body.len() as u64, "记下网页原始返回文本长度");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 响应快照落的是解析后的三类内容：思考、正文、工具调用。
    #[tokio::test]
    async fn response_dump_records_parsed_content() {
        let dir = std::env::temp_dir().join(format!("poolman-dump-{}", uuid::Uuid::new_v4()));
        let dump = Arc::new(crate::dump::DumpConfig::new(
            true,
            dir.to_string_lossy().to_string(),
        ));
        let body = concat!(
            "data: {\"v\":{\"response\":{\"message_id\":6,\"fragments\":[{\"type\":\"THINK\",\"content\":\"想\"}]}}}\n",
            "data: {\"v\":\"一下\"}\n",
            "data: {\"p\":\"response/fragments\",\"o\":\"APPEND\",\"v\":[{\"type\":\"RESPONSE\",\"content\":\"<tool_call>{\\\"name\\\":\\\"read_file\\\",\\\"arguments\\\":{\\\"path\\\":\\\"a.rs\\\"}}</tool_call>\"}]}\n",
        );
        let executor = Arc::new(MockExecutor {
            requests: Mutex::new(Vec::new()),
            completion_body: body.to_string(),
            stream_chunk: 0,
        });
        let model = DeepSeekWebModel::new("DeepSeek", executor as Arc<dyn ChannelExecutor>)
            .with_dump(dump);
        model.infer(tool_request()).await.expect("推理应成功");

        let file = std::fs::read_dir(&dir)
            .expect("落盘目录应已创建")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("response-"))
            })
            .expect("应落一份响应快照");
        let doc: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();

        assert_eq!(doc["stage"], "response");
        assert_eq!(doc["reasoning"], "想一下", "思考原文");
        assert_eq!(doc["reasoning_len"], 3);
        assert_eq!(doc["text"], "", "围栏不算正文");
        assert_eq!(doc["tool_calls"][0]["function"]["name"], "read_file");
        assert_eq!(doc["tool_calls"][0]["function"]["arguments"], "{\"path\":\"a.rs\"}");
        assert_eq!(doc["finish_reason"], "tool_calls");
        assert!(doc["raw_len"].as_u64().unwrap() > 0, "记下原始文本长度");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 渠道会话管理由模型自行完成。
    #[tokio::test]
    async fn channel_sessions_are_model_owned() {
        let model = model("");
        assert_eq!(
            model.create_channel_session().await.unwrap().as_deref(),
            Some("session-abcdefghij")
        );
        assert!(model.list_channel_sessions().await.unwrap().is_array());
        assert_eq!(
            model
                .list_channel_sessions()
                .await
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(model.delete_channel_session("s1").await.unwrap());
        assert!(model.rename_channel_session("s1", "t").await.unwrap());
        assert!(model.channel_history("s1").await.is_ok());
    }

    /// SSE 帧解析：真实协议（快照 / 追加片段 / 追加片段内容）与简写正文帧都能取到正文。
    #[test]
    fn sse_frames_yield_response_deltas() {
        let mut parser = super::SseFrameProcessor::new();
        // 简写正文帧
        assert_eq!(
            parser.process_line("data: {\"p\":\"response/content\",\"v\":\"a\"}"),
            vec![("RESPONSE".to_string(), "a".to_string())]
        );
        assert_eq!(
            parser.process_line("data: {\"p\":\"response/content\",\"v\":\"b\"}"),
            vec![("RESPONSE".to_string(), "b".to_string())]
        );
        assert!(parser.process_line("data: [DONE]").is_empty());
        assert!(parser.process_line("").is_empty());
        assert!(parser.process_line("data: 不是 JSON").is_empty());
        // 无路径的纯文本帧按正文收下
        assert_eq!(
            parser.process_line("data: {\"v\":\"裸文本\"}"),
            vec![("RESPONSE".to_string(), "裸文本".to_string())]
        );

        // 真实协议：快照 → 只放出新增部分；追加片段；按下标追加内容
        let mut parser = super::SseFrameProcessor::new();
        let snapshot = "data: {\"v\":{\"response\":{\"message_id\":8,\"fragments\":[\
            {\"type\":\"THINK\",\"content\":\"想一想\"},{\"type\":\"RESPONSE\",\"content\":\"你好\"}]}}}";
        assert_eq!(
            parser.process_line(snapshot),
            vec![
                ("THINK".to_string(), "想一想".to_string()),
                ("RESPONSE".to_string(), "你好".to_string()),
            ],
            "快照按片段类型给出内容"
        );
        // 同一快照重发不重复输出（sent_length 去重）
        assert!(parser.process_line(snapshot).is_empty());
        // 追加片段
        assert_eq!(
            parser.process_line("data: {\"p\":\"response/fragments\",\"o\":\"APPEND\",\"v\":[{\"type\":\"RESPONSE\",\"content\":\"世界\"}]}"),
            vec![("RESPONSE".to_string(), "世界".to_string())]
        );
        // 按下标追加内容；-1 表示最后一个片段
        assert_eq!(
            parser.process_line("data: {\"p\":\"response/fragments/1/content\",\"o\":\"APPEND\",\"v\":\"！\"}"),
            vec![("RESPONSE".to_string(), "！".to_string())],
            "只放新增部分"
        );
        assert_eq!(
            parser.process_line("data: {\"p\":\"response/fragments/-1/content\",\"o\":\"APPEND\",\"v\":\"啦\"}"),
            vec![("RESPONSE".to_string(), "啦".to_string())]
        );
        // SET 覆盖成更长的内容：只放新增部分
        assert_eq!(
            parser.process_line("data: {\"p\":\"response/fragments/1/content\",\"o\":\"SET\",\"v\":\"你好！呀\"}"),
            vec![("RESPONSE".to_string(), "呀".to_string())]
        );
        // SET 覆盖成更短的内容：增量表达不了替换，只把游标退回去，后续追加仍能出增量
        assert!(parser
            .process_line("data: {\"p\":\"response/fragments/1/content\",\"o\":\"SET\",\"v\":\"重写\"}")
            .is_empty());
        assert_eq!(
            parser.process_line("data: {\"p\":\"response/fragments/1/content\",\"o\":\"APPEND\",\"v\":\"好\"}"),
            vec![("RESPONSE".to_string(), "好".to_string())]
        );
    }

    /// 真实网页数据：无路径续帧属于当前片段（思考的续帧不能被当成正文），参数帧一律忽略。
    #[test]
    fn sse_frames_attribute_bare_continuation_to_current_fragment() {
        let mut parser = super::SseFrameProcessor::new();
        parser.process_line(
            "data: {\"v\":{\"response\":{\"message_id\":6,\"fragments\":[{\"id\":2,\"type\":\"THINK\",\"content\":\"The\"}]}}}",
        );
        assert_eq!(
            parser.process_line("data: {\"p\":\"response/fragments/-1/content\",\"o\":\"APPEND\",\"v\":\" user\"}"),
            vec![("THINK".to_string(), " user".to_string())],
            "思考片段的下标续帧仍是思考"
        );
        assert_eq!(
            parser.process_line("data: {\"v\":\" just\"}"),
            vec![("THINK".to_string(), " just".to_string())],
            "无路径续帧沿用当前片段类型"
        );
        // 参数 / 状态帧（路径不是正文）不当作正文
        assert!(parser
            .process_line("data: {\"p\":\"response/fragments/-1/elapsed_secs\",\"o\":\"SET\",\"v\":0.83}")
            .is_empty());
        assert!(parser
            .process_line("data: {\"p\":\"response/status\",\"o\":\"SET\",\"v\":\"FINISHED\"}")
            .is_empty());
        assert!(parser
            .process_line("data: {\"p\":\"response\",\"o\":\"BATCH\",\"v\":[{\"p\":\"quasi_status\",\"v\":\"FINISHED\"}]}")
            .is_empty());
        // 新片段（正文）与它的续帧
        assert_eq!(
            parser.process_line("data: {\"p\":\"response/fragments\",\"o\":\"APPEND\",\"v\":[{\"id\":3,\"type\":\"RESPONSE\",\"content\":\"你好\"}]}"),
            vec![("RESPONSE".to_string(), "你好".to_string())]
        );
        assert_eq!(
            parser.process_line("data: {\"p\":\"response/fragments/-1/content\",\"v\":\"！\"}"),
            vec![("RESPONSE".to_string(), "！".to_string())],
            "缺 o 字段按追加处理"
        );
        assert_eq!(
            parser.process_line("data: {\"v\":\"有什么\"}"),
            vec![("RESPONSE".to_string(), "有什么".to_string())]
        );
    }

    /// 真实网页数据（流式）：思考走 reasoning、正文走 content，状态帧两边都不进。
    #[tokio::test]
    async fn infer_stream_emits_thinking_and_answer_separately() {
        let body = concat!(
            "event: ready\n",
            "data: {\"request_message_id\":5,\"response_message_id\":6,\"model_type\":\"default\"}\n",
            "\n",
            "data: {\"v\":{\"response\":{\"message_id\":6,\"status\":\"WIP\",\"fragments\":[{\"id\":2,\"type\":\"THINK\",\"content\":\"The\"}]}}}\n",
            "\n",
            "data: {\"p\":\"response/fragments/-1/content\",\"o\":\"APPEND\",\"v\":\" user\"}\n",
            "data: {\"v\":\" thinks\"}\n",
            "data: {\"p\":\"response/fragments/-1/elapsed_secs\",\"o\":\"SET\",\"v\":0.83}\n",
            "data: {\"p\":\"response/fragments\",\"o\":\"APPEND\",\"v\":[{\"id\":3,\"type\":\"RESPONSE\",\"content\":\"你好\"}]}\n",
            "data: {\"p\":\"response/fragments/-1/content\",\"v\":\"！\"}\n",
            "data: {\"v\":\"有什么可以帮你的吗\"}\n",
            "data: {\"p\":\"response/status\",\"o\":\"SET\",\"v\":\"FINISHED\"}\n",
            "event: close\n",
            "data: {\"click_behavior\":\"none\"}\n",
        );
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let response = model_split(body, 7)
            .infer_stream(
                user_request(),
                Box::new(move |delta| sink.lock().unwrap().push(delta)),
            )
            .await
            .expect("流式推理应成功");

        let deltas = seen.lock().unwrap().clone();
        let mut reasoning = String::new();
        let mut content = String::new();
        for delta in &deltas {
            let chunk = crate::apis::base::parse_delta(delta);
            reasoning.push_str(&chunk.reasoning);
            content.push_str(&chunk.content);
        }
        assert_eq!(reasoning, "The user thinks");
        assert_eq!(content, "你好！有什么可以帮你的吗");
        assert_eq!(response.text, "你好！有什么可以帮你的吗", "正文不含思考");
        assert_eq!(response.reasoning.as_deref(), Some("The user thinks"), "思考单独返回");
        assert!(response.tool_calls.is_none());
        assert_eq!(response.finish_reason.as_deref(), Some("stop"));
    }

    /// 真实网页数据（流式）：工具围栏由无路径续帧逐块送达，既不外泄也能解析成调用。
    #[tokio::test]
    async fn infer_stream_reads_tool_call_from_bare_continuation_frames() {
        let body = concat!(
            "data: {\"v\":{\"response\":{\"message_id\":6,\"fragments\":[]}}}\n",
            "data: {\"p\":\"response/fragments\",\"o\":\"APPEND\",\"v\":[{\"id\":3,\"type\":\"RESPONSE\",\"content\":\"\"}]}\n",
            "data: {\"v\":\"<tool_call>\\n{\\\"name\\\":\\\"read\"}\n",
            "data: {\"v\":\"_file\\\",\\\"arguments\\\":{\\\"path\\\":\\\"a.rs\\\"}}\\n\"}\n",
            "data: {\"v\":\"</tool_call>\"}\n",
            "data: {\"p\":\"response/status\",\"o\":\"SET\",\"v\":\"FINISHED\"}\n",
        );
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let response = model_split(body, 6)
            .infer_stream(
                tool_request(),
                Box::new(move |delta| sink.lock().unwrap().push(delta)),
            )
            .await
            .expect("流式推理应成功");

        let deltas = seen.lock().unwrap().clone();
        let content: String = deltas
            .iter()
            .map(|delta| crate::apis::base::parse_delta(delta).content)
            .collect();
        assert_eq!(content, "", "围栏不能作为正文流出");
        assert!(response.text.is_empty(), "{}", response.text);
        let calls = response.tool_calls.expect("应解析出工具调用");
        assert_eq!(calls[0]["function"]["name"], "read_file");
        assert_eq!(calls[0]["function"]["arguments"], "{\"path\":\"a.rs\"}");
        assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
    }

    /// 真实协议下正文与工具调用都要解析出来（含思考片段被忽略）。
    #[tokio::test]
    async fn infer_parses_real_protocol_frames() {
        let body = concat!(
            "data: {\"v\":{\"response\":{\"message_id\":8,\"fragments\":[{\"type\":\"THINK\",\"content\":\"想\"},{\"type\":\"RESPONSE\",\"content\":\"\"}]}}}\n",
            "data: {\"p\":\"response/fragments/1/content\",\"o\":\"APPEND\",\"v\":\"先读文件\"}\n",
            "data: {\"p\":\"response/fragments/1/content\",\"o\":\"APPEND\",\"v\":\"<tool_call>\"}\n",
            "data: {\"p\":\"response/fragments/1/content\",\"o\":\"APPEND\",\"v\":\"{\\\"name\\\":\\\"read_file\\\",\\\"arguments\\\":{\\\"path\\\":\\\"a.rs\\\"}}\"}\n",
            "data: {\"p\":\"response/fragments/1/content\",\"o\":\"APPEND\",\"v\":\"</tool_call>\"}\n",
        );
        let response = model(body)
            .infer(tool_request())
            .await
            .expect("推理应成功");

        assert_eq!(response.text, "先读文件", "思考片段与围栏都不进正文");
        assert_eq!(response.reasoning.as_deref(), Some("想"), "思考单独返回");
        let calls = response.tool_calls.expect("应解析出工具调用");
        assert_eq!(calls[0]["function"]["name"], "read_file");
        assert_eq!(calls[0]["function"]["arguments"], "{\"path\":\"a.rs\"}");
        assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
    }
}
