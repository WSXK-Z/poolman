// DeepSeek 网页端聊天服务。
//
// 输入消息在此选择或创建网页会话、转换为供应商提示词并构造 completion 请求；
// 响应完成后提取文本/工具调用、估算 token 使用量并维护会话游标。

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use tracing::{error, info, warn};

use crate::extension::adapters::defs::{
    BaseWebService, ChatWebService, ParseWebService, ParsedAnswer, SessionWebService,
};
use crate::extension::adapters::deepseek::defs::{
    DEEPSEEK_API_URL, DEEPSEEK_MODEL_TYPE, DEEPSEEK_THINK, MAX_ATTEMPTS, PROVIDER_ID,
};
use crate::extension::adapters::deepseek::streaming::{
    extract_session_id_from_history, uuid, DeepSeekStreamDriver,
};
use crate::extension::adapters::deepseek::tool_parsing::{
    self, attach_answer_prefix, delta_text, estimate_tokens, format_ts_ms, guess_default,
    make_usage, parse_biz_error, parse_missing_field, value_text, DeepSeekParser,
};
use crate::extension::adapters::deepseek::web_service::DeepSeekWebService;
use crate::extension::websocket::ChatResponsePayload;

/// DeepSeek 网页端聊天服务
pub struct DeepSeekChatService {
    web_service: Arc<DeepSeekWebService>,
    /// 当前会话 id：追问轮次未显式指定会话时的回退
    current_session_id: StdMutex<Option<String>>,
    /// 会话 id → 最近一次响应消息 id（下轮请求的 parent_message_id）
    session_map: StdMutex<HashMap<String, i64>>,
    /// 会话持久化（未注入时不落库）
    sessions: Option<Arc<crate::session::manager::SessionManager>>,
}

/// 请求构造阶段产出的不可变快照，供执行和回答前缀共用。
struct BuiltRequest {
    headers: Vec<(String, String)>,
    body: Value,
    is_new_session: bool,
    /// 是否首次对话（没有 assistant 消息）：仅在首次回答加「回答时间 + 会话」前缀
    is_first_turn: bool,
    chat_session_id: String,
    /// 转换后的提示词（实际发给网页平台的对话）
    prompt: String,
}

impl DeepSeekChatService {
    pub fn new(
        web_service: Arc<DeepSeekWebService>,
        sessions: Option<Arc<crate::session::manager::SessionManager>>,
    ) -> Self {
        Self {
            web_service,
            current_session_id: StdMutex::new(None),
            session_map: StdMutex::new(HashMap::new()),
            sessions,
        }
    }

    /* ------------------------- 请求组装（提示词转换见 converter 模块） ------------------------- */

    /// 选择或创建目标会话，调用平台转换器生成提示词，并组合网页 completion 请求。
    async fn build_request(&self, messages: &[Value], options: &Value) -> Result<BuiltRequest> {
        let token = self.web_service.get_token().await?;
        let settings_token = self.web_service.get_setting_token();
        let tools = options.get("tools").cloned();
        // 是否首次对话（没有 assistant 消息）
        let is_first_turn = !messages
            .iter()
            .any(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"));
        let fixed_session = options
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(String::from);

        let chat_session_id: String;
        let is_new_session: bool;
        if let Some(sid) = fixed_session {
            chat_session_id = sid;
            is_new_session = false;
        } else if let Some(sid) = extract_session_id_from_history(messages) {
            chat_session_id = sid;
            is_new_session = false;
        } else if !is_first_turn {
            let fallback = self.current_session_id.lock().unwrap().clone();
            chat_session_id = match fallback {
                Some(s) => s,
                None => self
                    .web_service
                    .create_session()
                    .await?
                    .unwrap_or_else(uuid),
            };
            is_new_session = false;
        } else {
            chat_session_id = self
                .web_service
                .create_session()
                .await?
                .unwrap_or_else(uuid);
            is_new_session = true;
        }
        self.fetch_history(&chat_session_id).await;
        *self.current_session_id.lock().unwrap() = Some(chat_session_id.clone());
        // 让底层执行带上会话键：调度层据此维持会话粘性（同一会话固定落到同一台插件）
        self.web_service
            .set_current_session(Some(chat_session_id.clone()));

        // 中间转换器：按 API Key 绑定的平台选择（默认 vscode-copilot）
        let platform = options
            .get("converterPlatform")
            .and_then(|v| v.as_str())
            .unwrap_or(crate::prompts::vscode_copilot::PLATFORM);
        // 所有 Key 一律走平台转换器（按平台拆分重建提示词）
        let prompt = Self::build_prompt(messages, tools.as_ref(), is_first_turn, platform)?;

        let headers = self
            .web_service
            .default_headers(&token, settings_token.as_deref());

        let parent_message_id = if is_new_session {
            Value::Null
        } else {
            let id = self
                .session_map
                .lock()
                .unwrap()
                .get(&chat_session_id)
                .copied()
                .unwrap_or(0);
            if id == 0 {
                Value::Null
            } else {
                Value::Number(id.into())
            }
        };

        let body = json!({
            "chat_session_id": chat_session_id,
            "parent_message_id": parent_message_id,
            "model_type": DEEPSEEK_MODEL_TYPE,
            "prompt": prompt,
            "ref_file_ids": [],
            "thinking_enabled": DEEPSEEK_THINK,
            "search_enabled": false,
            "action": null,
            "preempt": false,
        });

        Ok(BuiltRequest {
            headers,
            body,
            is_new_session,
            is_first_turn,
            chat_session_id,
            prompt,
        })
    }

    /// 拉取会话历史的 current_message_id 写入 session_map（失败忽略，不影响主流程），
    /// 供后续请求的 parent_message_id 维持服务端消息链。
    async fn fetch_history(&self, session_id: &str) {
        if let Ok(json) = self.web_service.get_session_history(session_id).await {
            if let Some(id) = json
                .get("data")
                .and_then(|d| d.get("biz_data"))
                .and_then(|b| b.get("chat_session"))
                .and_then(|s| s.get("current_message_id"))
                .and_then(|v| v.as_i64())
            {
                self.session_map
                    .lock()
                    .unwrap()
                    .insert(session_id.to_string(), id);
            }
        }
    }

    /* ------------------------- 会话落库 ------------------------- */

    /// 一轮对话结束后落库：客户端原始消息 + 转换后实际发出的提示词。
    /// 以网页会话 id 作为幂等键 upsert，失败只告警不影响主流程。
    async fn persist_turn(
        &self,
        web_session_id: &str,
        messages: &[Value],
        converted_prompt: &str,
        options: &Value,
    ) {
        let Some(sessions) = &self.sessions else {
            return;
        };
        let platform = options
            .get("converterPlatform")
            .and_then(|v| v.as_str())
            .unwrap_or(crate::prompts::vscode_copilot::PLATFORM);
        let api_key = options
            .get("apiKey")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let session_id = match sessions
            .upsert_session(web_session_id, web_session_id, platform, api_key)
            .await
        {
            Ok(id) => id,
            Err(e) => {
                warn!("会话落库失败（upsert_session）: {}", e);
                return;
            }
        };
        // 原始消息：保持客户端发来的 OpenAI 兼容形式
        for m in messages {
            let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
            let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if content.is_empty() {
                continue;
            }
            if let Err(e) = sessions.add_raw_message(&session_id, role, content).await {
                warn!("会话落库失败（raw_message）: {}", e);
            }
        }
        // 转换后消息：实际发给网页平台的整段提示词
        if let Err(e) = sessions
            .add_converted_message(&session_id, "user", converted_prompt)
            .await
        {
            warn!("会话落库失败（converted_message）: {}", e);
        }
    }

    /* ------------------------- 执行 + 422 自愈 ------------------------- */

    /// 执行非流式请求（含 422 自愈 / 业务错误 / 空响应）
    async fn execute_with_retry(
        &self,
        headers: Vec<(String, String)>,
        body: &Value,
    ) -> Result<Value> {
        let mut current_body = body.clone();
        for attempt in 1..=MAX_ATTEMPTS {
            let body_str = serde_json::to_string(&current_body)?;
            let res = self
                .web_service
                .execute_once(
                    "POST",
                    DEEPSEEK_API_URL,
                    headers.clone(),
                    Some(body_str),
                    self.web_service.timeout_ms(),
                )
                .await?;

            if res.status == 401 || res.status == 403 {
                return Err(anyhow!(
                    "DeepSeek 登录状态失效或未登录：请先在浏览器打开 chat.deepseek.com 并登录"
                ));
            }
            if res.status == 422 {
                if let Some(missing) = parse_missing_field(&res.body) {
                    if attempt < MAX_ATTEMPTS {
                        let val = guess_default(&missing);
                        if let Some(obj) = current_body.as_object_mut() {
                            obj.insert(missing.clone(), val.clone());
                        }
                        warn!(
                            "自动补全字段 {}={} 后重试(第{}次)",
                            missing,
                            val,
                            attempt + 1
                        );
                        continue;
                    }
                }
                return Err(anyhow!(
                    "DeepSeek API 请求失败 (422): {}",
                    res.body.chars().take(300).collect::<String>()
                ));
            }
            if !res.ok {
                return Err(anyhow!(
                    "DeepSeek API 请求失败 ({}): {}",
                    res.status,
                    res.body.chars().take(200).collect::<String>()
                ));
            }
            if let Some((_, msg)) = parse_biz_error(&res.body) {
                warn!("DeepSeek 业务错误：{}", msg);
                return Err(anyhow!("DeepSeek 业务错误: {}", msg));
            }

            let mut parser = DeepSeekParser::new();
            let mut line_buf = String::new();
            line_buf.push_str(&res.body);
            while let Some(nl) = line_buf.find('\n') {
                let line = line_buf[..nl].to_string();
                parser.process_line(&line);
                line_buf.drain(..nl + 1);
            }
            if !line_buf.trim().is_empty() {
                parser.process_line(&line_buf);
            }

            // hint 错误识别（rate limit 等）→ 明确报错，避免误报空响应
            if let Some(hint) = parser.take_hint_error() {
                return Err(anyhow!("DeepSeek 提示: {}", hint));
            }
            let content = parser.answer();
            if content.is_null()
                || content.as_str().map_or(false, |s| s.is_empty())
                || content.as_object().map_or(false, |o| o.is_empty())
            {
                error!(
                    "DeepSeek 解析结果为空（deepseek_empty_response） status={}",
                    res.status
                );
                return Err(anyhow!(
                    "DeepSeek 未返回内容（status={}，原始响应: {}）",
                    res.status,
                    res.body.chars().take(200).collect::<String>()
                ));
            }
            // 捕获响应消息 id → 更新 session_map，供下轮 parent_message_id 使用
            if let Some(mid) = parser.message_id() {
                if mid > 0 {
                    if let Some(sid) = current_body.get("chat_session_id").and_then(|v| v.as_str())
                    {
                        self.session_map
                            .lock()
                            .unwrap()
                            .insert(sid.to_string(), mid);
                    }
                }
            }
            return Ok(content);
        }
        Err(anyhow!("DeepSeek 请求重试次数用尽"))
    }
}

impl ParseWebService for DeepSeekChatService {
    fn build_prompt(
        messages: &[Value],
        tools: Option<&Value>,
        _is_first_turn: bool,
        platform: &str,
    ) -> Result<String> {
        let converter = crate::prompts::resolve(platform)
            .ok_or_else(|| anyhow!("未知转换平台: {}", platform))?;
        // 先过 API 转换器：入站消息 → 内部输入参数格式；再交提示词转换器 → poolman 提示词。
        // 首轮与否由 AgentInput 自身推导（历史里有没有 assistant 回合），不再单独传参。
        let input = crate::converters::api::openai::to_agent_input(
            messages,
            tools.cloned(),
            Default::default(),
        );
        Ok(converter.convert(&input))
    }

    fn parse_answer(content: &Value) -> ParsedAnswer {
        let text = tool_parsing::extract_response_text(&content.to_string());
        let tool_calls = tool_parsing::parse_tool_calls(&text);
        let unparsed_tool_trace =
            tool_calls.is_none() && tool_parsing::looks_like_unparsed_tool_call(&text);
        ParsedAnswer {
            text,
            tool_calls,
            unparsed_tool_trace,
        }
    }
}

impl ChatWebService for DeepSeekChatService {
    /// 执行非流式对话，解析网页响应并构造网关统一的聊天结果。
    async fn chat(
        &self,
        messages: Vec<Value>,
        options: Option<Value>,
    ) -> Result<ChatResponsePayload> {
        let options = options.unwrap_or_else(|| json!({}));
        // 是否带工具（用于告警观察模型是否未按提示词围栏格式输出）
        let has_tools = options
            .get("tools")
            .and_then(|t| t.as_array())
            .map_or(false, |a| !a.is_empty());
        let built = self.build_request(&messages, &options).await?;
        let ts = format_ts_ms();
        let session_id = built.chat_session_id.clone();

        let content = self
            .execute_with_retry(built.headers.clone(), &built.body)
            .await?;
        // 会话落库：客户端原始消息 + 转换后实际发出的提示词
        self.persist_turn(&session_id, &messages, &built.prompt, &options)
            .await;
        // DeepSeek 网页版不返回 usage → 估算：输入=发给平台的提示词，输出=模型生成的全部内容
        let usage = {
            let prompt_tokens = estimate_tokens(&built.prompt);
            let completion_tokens = estimate_tokens(&value_text(&content));
            info!(session = %session_id, prompt_tokens, completion_tokens, total = prompt_tokens + completion_tokens, "估算 token 用量（DeepSeek 网页版无真实 usage）");
            Some(make_usage(prompt_tokens, completion_tokens))
        };
        if built.is_new_session {
            let _ = self
                .web_service
                .update_session_title(&session_id, &session_id)
                .await;
        }
        let parsed = Self::parse_answer(&content);
        if let Some(tool_calls) = parsed.tool_calls {
            info!("检测到工具调用");
            return Ok(ChatResponsePayload {
                content: String::new(),
                model: PROVIDER_ID.to_string(),
                usage: usage.clone(),
                finished_reason: Some("tool_calls".to_string()),
                tool_calls: Some(tool_calls),
            });
        }
        if has_tools && parsed.unparsed_tool_trace {
            warn!(
                "本次请求带工具，但未解析出工具调用（模型可能未按提示词围栏格式输出）: {}",
                parsed.text.chars().take(300).collect::<String>()
            );
        }
        // 只在首次回答添加「回答时间 + 会话」前缀；追问通过反向查找历史里的前缀定位会话
        let final_content = if built.is_first_turn {
            attach_answer_prefix(&parsed.text, &ts, &session_id)
        } else {
            parsed.text
        };
        Ok(ChatResponsePayload {
            content: final_content,
            model: PROVIDER_ID.to_string(),
            usage,
            finished_reason: Some("stop".to_string()),
            tool_calls: None,
        })
    }

    /// 流式聊天
    async fn chat_stream<F: FnMut(String) + Send + Sync + 'static>(
        &self,
        messages: Vec<Value>,
        options: Option<Value>,
        on_chunk: F,
    ) -> Result<ChatResponsePayload> {
        let options = options.unwrap_or_else(|| json!({}));
        let built = self.build_request(&messages, &options).await?;
        let ts = format_ts_ms();
        let session_id = built.chat_session_id.clone();
        // 只在首次回答带前缀；追问不再带（通过反向查找历史前缀定位会话）
        let prefix = if built.is_first_turn {
            format!("回答时间: {}\n会话: {}\n", ts, session_id)
        } else {
            String::new()
        };
        let has_tools = options
            .get("tools")
            .map_or(false, |t| t.as_array().map_or(false, |a| !a.is_empty()));
        // 累计本次流式实际输出的文本，用于估算输出 token（DeepSeek 网页版无真实 usage）
        let completion_buf = Arc::new(StdMutex::new(String::new()));

        let mut current_body = built.body.clone();
        let mut deltas_out: Vec<String> = Vec::new();
        let mut attempt = 0usize;
        // 共享 on_chunk：WS 回调线程实时转发增量（THINK/RESPONSE 逐块流式），
        // 流结束再用同一入口转发收尾增量（工具调用 / 嗅探回退补发），避免重复发送
        let on_chunk = Arc::new(StdMutex::new(on_chunk));

        // 流式执行 + 422 自愈：每次重试重置 driver（避免残留增量）
        loop {
            attempt += 1;
            let driver = Arc::new(StdMutex::new(DeepSeekStreamDriver::new(
                has_tools,
                prefix.clone(),
            )));

            let on_chunk_cb: Box<dyn FnMut(String) + Send + Sync> = {
                let driver_cb = driver.clone();
                let on_chunk_cb = on_chunk.clone();
                let completion_cb = Arc::clone(&completion_buf);
                Box::new(move |data: String| {
                    let deltas = {
                        let mut d = driver_cb.lock().unwrap();
                        d.feed(&data)
                    };
                    // 实时转发到客户端（THINK / 普通回答逐字流式），同时累计输出文本用于估算 token
                    if !deltas.is_empty() {
                        let mut oc = on_chunk_cb.lock().unwrap();
                        for d in deltas {
                            completion_cb.lock().unwrap().push_str(&delta_text(&d));
                            oc(d);
                        }
                    }
                })
            };

            let body_str = serde_json::to_string(&current_body)?;
            let res = self
                .web_service
                .execute_streaming(
                    "POST",
                    DEEPSEEK_API_URL,
                    built.headers.clone(),
                    body_str,
                    on_chunk_cb,
                )
                .await?;

            if res.status == 401 || res.status == 403 {
                return Err(anyhow!("DeepSeek 登录状态失效或未登录"));
            }
            // 422 自愈：补缺失字段后重试
            if res.status == 422 && attempt < MAX_ATTEMPTS {
                if let Some(missing) = parse_missing_field(&res.body) {
                    let val = guess_default(&missing);
                    if let Some(obj) = current_body.as_object_mut() {
                        obj.insert(missing.clone(), val.clone());
                    }
                    warn!(
                        "流式自动补全字段 {}={} 后重试(第{}次)",
                        missing,
                        val,
                        attempt + 1
                    );
                    continue;
                }
            }
            if !res.ok {
                return Err(anyhow!(
                    "DeepSeek API 请求失败 ({}): {}",
                    res.status,
                    res.body
                ));
            }

            // 收尾增量（工具模式解析 / 嗅探补发）
            let final_deltas = driver.lock().unwrap().finish();
            // 转发收尾增量（工具调用 → tool_calls delta；嗅探回退 → 补发文本）
            if !final_deltas.is_empty() {
                let mut oc = on_chunk.lock().unwrap();
                for d in &final_deltas {
                    completion_buf.lock().unwrap().push_str(&delta_text(d));
                    oc(d.clone());
                }
            }
            deltas_out.extend(final_deltas);
            // 捕获响应消息 id → 更新 session_map，供下轮 parent_message_id 使用
            if let Some(mid) = driver.lock().unwrap().message_id() {
                if mid > 0 {
                    self.session_map
                        .lock()
                        .unwrap()
                        .insert(session_id.clone(), mid);
                }
            }
            // hint 错误识别（rate limit 等）→ 明确报错
            if let Some(hint) = driver.lock().unwrap().take_hint_error() {
                return Err(anyhow!("DeepSeek 提示: {}", hint));
            }
            break;
        }

        // 会话落库：客户端原始消息 + 转换后实际发出的提示词
        self.persist_turn(&session_id, &messages, &built.prompt, &options)
            .await;

        let mut tool_calls: Option<Value> = None;
        for d in deltas_out {
            if let Ok(v) = serde_json::from_str::<Value>(&d) {
                if let Some(tc) = v.get("tool_calls").cloned() {
                    tool_calls = Some(tc);
                }
            }
        }

        if built.is_new_session {
            let _ = self
                .web_service
                .update_session_title(&session_id, &session_id)
                .await;
        }

        // 估算 token：输入=发给平台的提示词，输出=本次流式实际输出的全部文本
        let prompt_tokens = estimate_tokens(&built.prompt);
        let completion_tokens = estimate_tokens(&completion_buf.lock().unwrap());
        info!(session = %session_id, prompt_tokens, completion_tokens, total = prompt_tokens + completion_tokens, "估算 token 用量（DeepSeek 网页版无真实 usage）");
        let usage = Some(make_usage(prompt_tokens, completion_tokens));

        if let Some(tc) = tool_calls {
            return Ok(ChatResponsePayload {
                content: String::new(),
                model: PROVIDER_ID.to_string(),
                usage,
                finished_reason: Some("tool_calls".to_string()),
                tool_calls: Some(tc),
            });
        }
        Ok(ChatResponsePayload {
            content: String::new(),
            model: PROVIDER_ID.to_string(),
            usage,
            finished_reason: Some("stop".to_string()),
            tool_calls: None,
        })
    }
}
