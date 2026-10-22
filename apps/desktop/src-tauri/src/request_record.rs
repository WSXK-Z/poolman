//! 本地请求管理：把每一次对话请求的输入与回答落库，供桌面端按字段查看。
//!
//! 记录点在 [`crate::chat_router::ChatRouter`]：所有 API 格式（OpenAI / DeepSeek）最终都经由它
//! 分派给渠道模型，因此在这里快照请求、在结果返回后写入记录，能覆盖流式与非流式两种调用。
//! HTTP 元数据（协议 / 方法 / 路径 / 请求头）由各 API 入口采集后随请求传入。
//!
//! 列表只回传预览串（单行展示用），完整原文按 id 单独读取：历史对话里可能带很长的工具结果，
//! 一次拉全量会让请求管理页变慢。

use std::time::Instant;

use anyhow::Result;
use sea_orm::{
    ActiveModelTrait, DatabaseConnection, EntityTrait, FromQueryResult, PaginatorTrait, QueryOrder,
    QuerySelect, Set,
};
use serde::Serialize;
use serde_json::Value;

use crate::entitys::request_record::{ActiveModel, Column, Entity, Model};
use crate::models::{InferRequest, ModelResponse};

/// 列表预览的最大字符数（表格单行展示，超出由前端省略号截断）。
const PREVIEW_CHARS: usize = 200;

/// 历史对话预览最多取多少条消息。
const HISTORY_PREVIEW_ITEMS: usize = 8;

/// 单条历史消息在预览里最多保留多少字符。
const HISTORY_PREVIEW_MESSAGE_CHARS: usize = 80;

/// 列表一次最多返回多少条：界面按最近记录展示，避免库无限增长时一次拉爆。
const MAX_LIST_LIMIT: u64 = 5_000;

/// 生成随机请求 id（`req_` + UUIDv4）。
pub fn new_request_id() -> String {
    format!("req_{}", uuid::Uuid::new_v4().simple())
}

/// 一次 HTTP 请求的元数据：由各 API 协议入口采集，随推理请求交给路由层落库。
#[derive(Debug, Clone, Default)]
pub struct RequestMeta {
    /// 协议类型：`openai` / `deepseek`。
    pub protocol: String,
    /// HTTP 方法。
    pub method: String,
    /// 请求路径。
    pub endpoint: String,
    /// 请求头（小写名称 → 值）。
    pub headers: Value,
}

/// 请求侧快照：在请求交给渠道之前提取，避免结果返回后拿不到原始输入。
#[derive(Debug, Clone)]
pub struct RequestRecordDraft {
    /// 随机请求 id。
    pub id: String,
    pub model: String,
    pub stream: bool,
    /// 协议类型。
    pub protocol: String,
    /// HTTP 方法。
    pub method: String,
    /// 请求路径。
    pub endpoint: String,
    /// 请求头 JSON 原文。
    pub headers: String,
    /// 调用方使用的 API Key。
    pub api_key: String,
    /// 全部 system 消息拼接后的系统提示词。
    pub system_prompt: String,
    /// 最新一条 user 消息（用户提问）。
    pub user_prompt: String,
    /// 可用工具定义的 JSON 原文。
    pub tools: String,
    /// 历史对话消息数组的 JSON 原文（已剔除 system 与本次用户提问）。
    pub history: String,
    /// 完整非 system 消息数组的 JSON 原文（保留原始顺序）。
    pub messages: String,
    /// 是否为客户端自动发起的「生成会话标题」请求。
    pub is_title_request: bool,
    /// 请求开始时间：用于计算本次请求耗时。
    started_at: Instant,
}

impl RequestRecordDraft {
    /// 从推理请求与 HTTP 元数据提取记录字段。
    pub fn from_request(request: &InferRequest, meta: &RequestMeta) -> Self {
        let system_prompt = request
            .messages
            .iter()
            .filter(|message| role_of(message) == Some("system"))
            .map(message_text)
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");

        // system 消息单独展示；其余消息里最后一条 user 是本次提问，剩下的都算历史上下文。
        // 这样即使本次请求以工具结果收尾，最后一次提问与后续的工具往返也都不会丢。
        let turns: Vec<&Value> = request
            .messages
            .iter()
            .filter(|message| role_of(message) != Some("system"))
            .collect();
        let last_user = turns
            .iter()
            .rposition(|message| role_of(message) == Some("user"));
        let user_prompt = last_user
            .map(|index| message_text(turns[index]))
            .unwrap_or_default();
        let history = turns
            .iter()
            .enumerate()
            .filter(|(index, _)| Some(*index) != last_user)
            .map(|(_, message)| sanitize_message(message))
            .collect::<Vec<_>>();
        let messages = turns
            .iter()
            .map(|message| sanitize_message(message))
            .collect::<Vec<_>>();
        let tools = request
            .tools
            .as_ref()
            .filter(|value| !value.is_null())
            .map(Value::to_string)
            .unwrap_or_default();
        let is_title_request = detect_title_request(&request.messages, request.tools.as_ref());

        Self {
            id: new_request_id(),
            model: request.model.clone(),
            stream: request.stream,
            protocol: meta.protocol.clone(),
            method: meta.method.clone(),
            endpoint: meta.endpoint.clone(),
            headers: if meta.headers.is_null() {
                "{}".to_string()
            } else {
                meta.headers.to_string()
            },
            api_key: request.api_key.clone().unwrap_or_default(),
            system_prompt,
            user_prompt,
            tools,
            history: Value::Array(history).to_string(),
            messages: Value::Array(messages).to_string(),
            is_title_request,
            started_at: Instant::now(),
        }
    }

    /// 结合模型结果（或错误）生成一条可落库的记录。
    pub fn finish(self, result: &Result<ModelResponse>) -> RequestRecordEntry {
        let created_at = chrono::Utc::now().timestamp_millis();
        let duration_ms = i64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(i64::MAX);
        match result {
            Ok(response) => {
                let tool_calls = response
                    .tool_calls
                    .as_ref()
                    .filter(|value| !value.is_null());
                let has_tool_calls = tool_calls
                    .and_then(Value::as_array)
                    .is_some_and(|calls| !calls.is_empty());
                let has_text = !response.text.trim().is_empty();
                let has_reasoning = response
                    .reasoning
                    .as_deref()
                    .is_some_and(|text| !text.trim().is_empty());
                let answer_type = if has_tool_calls {
                    "tool_call"
                } else if has_text || has_reasoning {
                    "text"
                } else {
                    "none"
                };
                RequestRecordEntry {
                    id: self.id,
                    model: self.model,
                    stream: self.stream,
                    status: "success".to_string(),
                    protocol: self.protocol,
                    method: self.method,
                    endpoint: self.endpoint,
                    headers: self.headers,
                    api_key: self.api_key,
                    duration_ms,
                    system_prompt: self.system_prompt,
                    user_prompt: self.user_prompt,
                    tools: self.tools,
                    history: self.history,
                    answer_type: answer_type.to_string(),
                    answer_content: response.text.clone(),
                    answer_reasoning: response.reasoning.clone().unwrap_or_default(),
                    answer_tool_calls: tool_calls.map(Value::to_string).unwrap_or_default(),
                    error: String::new(),
                    local_session_id: None,
                    local_session_ignored: None,
                    messages: self.messages,
                    is_title_request: self.is_title_request,
                    created_at,
                }
            }
            Err(error) => RequestRecordEntry {
                id: self.id,
                model: self.model,
                stream: self.stream,
                status: "error".to_string(),
                protocol: self.protocol,
                method: self.method,
                endpoint: self.endpoint,
                headers: self.headers,
                api_key: self.api_key,
                duration_ms,
                system_prompt: self.system_prompt,
                user_prompt: self.user_prompt,
                tools: self.tools,
                history: self.history,
                answer_type: "none".to_string(),
                answer_content: String::new(),
                answer_reasoning: String::new(),
                answer_tool_calls: String::new(),
                error: format!("{error:#}"),
                local_session_id: None,
                local_session_ignored: None,
                messages: self.messages,
                is_title_request: self.is_title_request,
                created_at,
            },
        }
    }
}

/// 待写入的一条请求记录。
#[derive(Debug, Clone)]
pub struct RequestRecordEntry {
    pub id: String,
    pub model: String,
    pub stream: bool,
    pub status: String,
    pub protocol: String,
    pub method: String,
    pub endpoint: String,
    pub headers: String,
    pub api_key: String,
    pub duration_ms: i64,
    pub system_prompt: String,
    pub user_prompt: String,
    pub tools: String,
    pub history: String,
    pub answer_type: String,
    pub answer_content: String,
    pub answer_reasoning: String,
    pub answer_tool_calls: String,
    pub error: String,
    /// 归属的本地会话 id；由会话管理在请求处理/同步时写入。
    pub local_session_id: Option<String>,
    /// 会话被删除后置为 true，全量同步时跳过该请求。
    pub local_session_ignored: Option<bool>,
    /// 完整非 system 消息数组 JSON（用于会话内容表还原顺序）。
    pub messages: String,
    /// 是否为客户端自动发起的标题生成请求。
    pub is_title_request: bool,
    pub created_at: i64,
}

/// 列表行：内容都压缩成预览串，完整原文按 id 读取。
#[derive(Debug, Clone, Serialize)]
pub struct RequestRecordSummary {
    pub id: String,
    pub model: String,
    pub stream: bool,
    pub status: String,
    pub protocol: String,
    pub method: String,
    pub endpoint: String,
    pub api_key: String,
    pub headers: String,
    pub duration_ms: i64,
    pub created_at: i64,
    pub system_prompt: String,
    pub user_prompt: String,
    pub tools: String,
    pub history: String,
    pub answer_type: String,
    pub answer_preview: String,
}

/// 详情：请求与原回答的完整内容，供右侧抽屉的「原文 / 格式化」两个页签展示。
#[derive(Debug, Clone, Serialize)]
pub struct RequestRecordDetail {
    pub id: String,
    pub model: String,
    pub stream: bool,
    pub status: String,
    pub protocol: String,
    pub method: String,
    pub endpoint: String,
    pub headers: String,
    pub api_key: String,
    pub duration_ms: i64,
    pub created_at: i64,
    pub system_prompt: String,
    pub user_prompt: String,
    pub tools: String,
    pub history: String,
    pub answer_type: String,
    pub answer_content: String,
    pub answer_reasoning: String,
    pub answer_tool_calls: String,
    pub error: String,
}

/// 列表查询的投影行：只读预览短字段，不把完整原文从数据库拉出来。
#[derive(Debug, FromQueryResult)]
struct RequestRecordSummaryRow {
    id: String,
    model: String,
    stream: bool,
    status: String,
    protocol: Option<String>,
    method: Option<String>,
    endpoint: Option<String>,
    api_key: Option<String>,
    headers_preview: Option<String>,
    duration_ms: Option<i64>,
    created_at: i64,
    system_prompt_preview: Option<String>,
    user_prompt_preview: Option<String>,
    tools_preview: Option<String>,
    history_preview: Option<String>,
    answer_type: String,
    answer_preview: Option<String>,
}

impl From<RequestRecordSummaryRow> for RequestRecordSummary {
    fn from(row: RequestRecordSummaryRow) -> Self {
        Self {
            id: row.id,
            model: row.model,
            stream: row.stream,
            status: row.status,
            protocol: row.protocol.unwrap_or_default(),
            method: row.method.unwrap_or_default(),
            endpoint: row.endpoint.unwrap_or_default(),
            api_key: row.api_key.unwrap_or_default(),
            headers: row.headers_preview.unwrap_or_default(),
            duration_ms: row.duration_ms.unwrap_or_default(),
            created_at: row.created_at,
            system_prompt: row.system_prompt_preview.unwrap_or_default(),
            user_prompt: row.user_prompt_preview.unwrap_or_default(),
            tools: row.tools_preview.unwrap_or_default(),
            history: row.history_preview.unwrap_or_default(),
            answer_type: row.answer_type,
            answer_preview: row.answer_preview.unwrap_or_default(),
        }
    }
}

impl From<Model> for RequestRecordDetail {
    fn from(model: Model) -> Self {
        Self {
            id: model.id,
            model: model.model,
            stream: model.stream,
            status: model.status,
            protocol: model.protocol.unwrap_or_default(),
            method: model.method.unwrap_or_default(),
            endpoint: model.endpoint.unwrap_or_default(),
            headers: model.headers.unwrap_or_default(),
            api_key: model.api_key.unwrap_or_default(),
            duration_ms: model.duration_ms.unwrap_or_default(),
            created_at: model.created_at,
            system_prompt: model.system_prompt,
            user_prompt: model.user_prompt,
            tools: model.tools,
            history: model.history,
            answer_type: model.answer_type,
            answer_content: model.answer_content,
            answer_reasoning: model.answer_reasoning,
            answer_tool_calls: model.answer_tool_calls,
            error: model.error,
        }
    }
}

/// 请求记录存储：SQLite 表 `request_records` 的唯一读写入口。
pub struct RequestRecordStore {
    db: DatabaseConnection,
}

impl RequestRecordStore {
    /// 初始化连接并幂等同步表结构。
    pub async fn new(db: DatabaseConnection) -> Result<Self> {
        db.get_schema_builder().register(Entity).sync(&db).await?;
        Ok(Self { db })
    }

    /// 写入一条记录。
    pub async fn insert(&self, entry: RequestRecordEntry) -> Result<()> {
        // 预览串在落库时生成：列表查询只读这些短字段，无需把完整原文搬进内存。
        let system_prompt_preview = preview(&entry.system_prompt);
        let user_prompt_preview = preview(&entry.user_prompt);
        let tools_preview_value = tools_preview(&entry.tools);
        let history_preview_value = history_preview(&entry.history);
        let answer_preview_value = answer_preview(&entry);
        let headers_preview_value = headers_preview(&entry.headers);
        ActiveModel {
            id: Set(entry.id),
            model: Set(entry.model),
            stream: Set(entry.stream),
            status: Set(entry.status),
            protocol: Set(Some(entry.protocol)),
            method: Set(Some(entry.method)),
            endpoint: Set(Some(entry.endpoint)),
            headers: Set(Some(entry.headers)),
            api_key: Set(Some(entry.api_key)),
            duration_ms: Set(Some(entry.duration_ms)),
            system_prompt: Set(entry.system_prompt),
            user_prompt: Set(entry.user_prompt),
            tools: Set(entry.tools),
            history: Set(entry.history),
            answer_type: Set(entry.answer_type),
            answer_content: Set(entry.answer_content),
            answer_reasoning: Set(entry.answer_reasoning),
            answer_tool_calls: Set(entry.answer_tool_calls),
            error: Set(entry.error),
            local_session_id: Set(entry.local_session_id),
            local_session_ignored: Set(entry.local_session_ignored),
            messages: Set(Some(entry.messages)),
            is_title_request: Set(Some(entry.is_title_request)),
            system_prompt_preview: Set(Some(system_prompt_preview)),
            user_prompt_preview: Set(Some(user_prompt_preview)),
            tools_preview: Set(Some(tools_preview_value)),
            history_preview: Set(Some(history_preview_value)),
            answer_preview: Set(Some(answer_preview_value)),
            headers_preview: Set(Some(headers_preview_value)),
            created_at: Set(entry.created_at),
        }
        .insert(&self.db)
        .await?;
        Ok(())
    }

    /// 按创建时间倒序列出最近记录（只查预览短字段）。
    pub async fn list(&self, limit: u64) -> Result<Vec<RequestRecordSummary>> {
        let rows = Entity::find()
            .select_only()
            .column(Column::Id)
            .column(Column::Model)
            .column(Column::Stream)
            .column(Column::Status)
            .column(Column::Protocol)
            .column(Column::Method)
            .column(Column::Endpoint)
            .column(Column::ApiKey)
            .column(Column::HeadersPreview)
            .column(Column::DurationMs)
            .column(Column::CreatedAt)
            .column(Column::SystemPromptPreview)
            .column(Column::UserPromptPreview)
            .column(Column::ToolsPreview)
            .column(Column::HistoryPreview)
            .column(Column::AnswerType)
            .column(Column::AnswerPreview)
            .order_by_desc(Column::CreatedAt)
            .limit(limit.clamp(1, MAX_LIST_LIMIT))
            .into_model::<RequestRecordSummaryRow>()
            .all(&self.db)
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// 读取一条记录的完整原文。
    pub async fn get(&self, id: &str) -> Result<Option<RequestRecordDetail>> {
        let row = Entity::find_by_id(id.to_string()).one(&self.db).await?;
        Ok(row.map(Into::into))
    }

    /// 清空全部记录，返回删除条数。
    pub async fn clear(&self) -> Result<u64> {
        let result = Entity::delete_many().exec(&self.db).await?;
        Ok(result.rows_affected)
    }

    /// 记录总数（供界面提示展示范围）。
    pub async fn count(&self) -> Result<u64> {
        Ok(Entity::find().count(&self.db).await?)
    }
}

/// 消息角色。
fn role_of(message: &Value) -> Option<&str> {
    message.get("role").and_then(Value::as_str)
}

/// 消息正文 → 纯文本：字符串原样取，多模态数组只保留文本片段，其余片段留占位标记。
pub(crate) fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    return text.to_string();
                }
                let kind = part
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("未知片段");
                format!("[{kind}]")
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// 历史落库前去掉多模态内容里的图片 / 文件 base64：只保留可读文本与占位标记。
fn sanitize_message(message: &Value) -> Value {
    let mut sanitized = message.clone();
    if let Some(object) = sanitized.as_object_mut() {
        if matches!(object.get("content"), Some(Value::Array(_))) {
            object.insert(
                "content".to_string(),
                Value::String(message_text(message)),
            );
        }
    }
    sanitized
}

/// 判断一次推理请求是否为客户端自动发起的「生成会话标题」请求。
///
/// 这类请求的典型特征：没有工具、没有历史，system prompt 明确要求生成标题，
/// 或 user prompt 要求从会话/对话生成标题。真实用户第一问即使包含“标题”二字，
/// 只要没有会话/对话上下文，也不会命中。
pub fn detect_title_request(messages: &[Value], tools: Option<&Value>) -> bool {
    if !tools_empty(tools) {
        return false;
    }
    // 标题请求只有当前一条用户消息；出现历史消息就不是。
    if messages
        .iter()
        .filter(|message| role_of(message) != Some("system"))
        .count()
        > 1
    {
        return false;
    }
    let system_prompt = messages
        .iter()
        .filter(|message| role_of(message) == Some("system"))
        .map(message_text)
        .collect::<Vec<_>>()
        .join("\n\n");
    let user_prompt = messages
        .iter()
        .rev()
        .find(|message| role_of(message) == Some("user"))
        .map(message_text)
        .unwrap_or_default();
    looks_like_title_request(&system_prompt, &user_prompt)
}

/// 从落库字段判断历史请求是否为标题生成请求。
pub(crate) fn is_title_generation_record(
    system_prompt: &str,
    user_prompt: &str,
    history: &str,
    tools: &str,
) -> bool {
    if !stored_tools_empty(tools) || !stored_history_empty(history) {
        return false;
    }
    looks_like_title_request(system_prompt, user_prompt)
}

fn looks_like_title_request(system_prompt: &str, user_prompt: &str) -> bool {
    let system = system_prompt.to_lowercase();
    let user = user_prompt.to_lowercase();

    const SYSTEM_PATTERNS: &[&str] = &[
        "title for an ai",
        "session title",
        "conversation title",
        "concise title",
        "short title",
        "return only the title",
        "generate a title",
        "generate a short title",
        "create a title",
        "create a short title",
        "title for the following",
        "title generator",
        "name the conversation",
        "summarize the conversation",
        "summarise the conversation",
    ];
    if SYSTEM_PATTERNS.iter().any(|pattern| system.contains(pattern)) {
        return true;
    }
    // system prompt 同时提到 title 与生成动作，也能覆盖不同客户端的措辞。
    if system.contains("title")
        && (system.contains("generate")
            || system.contains("create")
            || system.contains("return only")
            || system.contains("concise")
            || system.contains("short"))
    {
        return true;
    }
    // 中文标题指令：system prompt 同时提到标题与生成动作。
    if system.contains("标题")
        && (system.contains("生成")
            || system.contains("命名")
            || system.contains("起")
            || system.contains("概括")
            || system.contains("总结"))
    {
        return true;
    }

    // user prompt 必须同时包含「会话/对话」+「标题」+「生成动作」，
    // 避免把“如何给文章起标题”这类真实问题误判。
    if user.contains("name this conversation")
        || user.contains("name the conversation")
        || user.contains("为这个对话命名")
        || user.contains("为这个会话命名")
    {
        return true;
    }
    let subject = ["session", "conversation", "chat", "对话", "会话"]
        .iter()
        .any(|word| user.contains(word));
    let title = user.contains("title") || user.contains("标题");
    let action = [
        "generate", "create", "make", "name", "summar", "生成", "命名", "起", "概括", "总结",
    ]
    .iter()
    .any(|word| user.contains(word));
    subject && title && action
}

fn tools_empty(tools: Option<&Value>) -> bool {
    match tools {
        None | Some(Value::Null) => true,
        Some(Value::Array(items)) => items.is_empty(),
        Some(_) => false,
    }
}

fn stored_tools_empty(raw: &str) -> bool {
    if raw.trim().is_empty() {
        return true;
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Null) => true,
        Ok(Value::Array(items)) => items.is_empty(),
        _ => false,
    }
}

fn stored_history_empty(raw: &str) -> bool {
    if raw.trim().is_empty() {
        return true;
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Array(items)) => items.is_empty(),
        _ => false,
    }
}

/// 截断到指定字符数，超出补省略号。
fn truncate(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((index, _)) => format!("{}…", &text[..index]),
        None => text.to_string(),
    }
}

/// 列表预览串。
fn preview(text: &str) -> String {
    truncate(text.trim(), PREVIEW_CHARS)
}

/// 请求头预览：`name: value / name: value`，只取前几条。
fn headers_preview(raw: &str) -> String {
    if raw.trim().is_empty() {
        return "（无）".to_string();
    }
    let Ok(Value::Object(headers)) = serde_json::from_str::<Value>(raw) else {
        return preview(raw);
    };
    if headers.is_empty() {
        return "（无）".to_string();
    }
    let parts = headers
        .iter()
        .take(8)
        .map(|(name, value)| {
            let text = value
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| value.to_string());
            format!("{name}: {text}")
        })
        .collect::<Vec<_>>();
    let joined = if headers.len() > 8 {
        format!("{} / …共 {} 个", parts.join(" / "), headers.len())
    } else {
        parts.join(" / ")
    };
    truncate(&joined, PREVIEW_CHARS)
}

/// 工具列表预览：`name1、name2（N 个）`；解析失败时退回原文。
fn tools_preview(raw: &str) -> String {
    if raw.trim().is_empty() {
        return "（无）".to_string();
    }
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return preview(raw);
    };
    let Some(items) = value.as_array() else {
        return preview(raw);
    };
    if items.is_empty() {
        return "（无）".to_string();
    }
    let names = items
        .iter()
        .filter_map(|item| {
            let function = item.get("function").unwrap_or(item);
            function.get("name").and_then(Value::as_str)
        })
        .collect::<Vec<_>>();
    if names.is_empty() {
        return preview(raw);
    }
    truncate(&format!("{}（{} 个）", names.join("、"), items.len()), PREVIEW_CHARS)
}

/// 历史对话预览：`用户: … / AI: …`，只取前几条，避免单行过长。
fn history_preview(raw: &str) -> String {
    if raw.trim().is_empty() {
        return "（无）".to_string();
    }
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(raw) else {
        return preview(raw);
    };
    if items.is_empty() {
        return "（无）".to_string();
    }
    let mut parts = Vec::new();
    for message in items.iter().take(HISTORY_PREVIEW_ITEMS) {
        let role = role_label(role_of(message).unwrap_or(""));
        let text = truncate(message_text(message).trim(), HISTORY_PREVIEW_MESSAGE_CHARS);
        if text.is_empty() {
            parts.push(role.to_string());
        } else {
            parts.push(format!("{role}: {text}"));
        }
    }
    if items.len() > HISTORY_PREVIEW_ITEMS {
        parts.push(format!("…共 {} 条", items.len()));
    }
    truncate(&parts.join(" / "), PREVIEW_CHARS)
}

/// 回答预览：正文取开头，工具调用取「工具名(参数)」，失败取错误原因。
fn answer_preview(entry: &RequestRecordEntry) -> String {
    if entry.status == "error" {
        return if entry.error.trim().is_empty() {
            "请求失败".to_string()
        } else {
            preview(&entry.error)
        };
    }
    if entry.answer_type == "tool_call" {
        if let Ok(Value::Array(calls)) = serde_json::from_str::<Value>(&entry.answer_tool_calls) {
            let labels = calls
                .iter()
                .map(|call| {
                    let function = call.get("function").unwrap_or(call);
                    let name = function
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("未知工具");
                    let arguments = function
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    format!("{name}({arguments})")
                })
                .collect::<Vec<_>>();
            if !labels.is_empty() {
                return truncate(
                    &format!("工具调用：{}", labels.join("、")),
                    PREVIEW_CHARS,
                );
            }
        }
    }
    if entry.answer_content.trim().is_empty() && !entry.answer_reasoning.trim().is_empty() {
        return preview(&format!("（仅思考）{}", entry.answer_reasoning));
    }
    if entry.answer_content.trim().is_empty() {
        return "（无回答）".to_string();
    }
    preview(&entry.answer_content)
}

/// 角色中文名（历史预览用）。
fn role_label(role: &str) -> &str {
    match role {
        "system" => "系统",
        "user" => "用户",
        "assistant" => "AI",
        "tool" => "工具",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        detect_title_request, headers_preview, history_preview, is_title_generation_record,
        new_request_id, tools_preview, RequestMeta, RequestRecordDraft, RequestRecordEntry,
        RequestRecordStore,
    };
    use crate::models::{InferRequest, ModelResponse};
    use serde_json::json;

    /// 测试用 HTTP 元数据。
    fn meta() -> RequestMeta {
        RequestMeta {
            protocol: "openai".to_string(),
            method: "POST".to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            headers: json!({ "content-type": "application/json", "authorization": "Bearer sk-test" }),
        }
    }

    /// 客户端自动生成标题的请求应被识别，真实用户提问不应被误判。
    #[test]
    fn detects_title_generation_requests() {
        let title_system = "Create a concise title for an AI coding-assistant session from the supplied human messages.";
        let title_user = "Generate the session title from this JSON array of human messages:\n[]";
        let title_messages = vec![
            json!({ "role": "system", "content": title_system }),
            json!({ "role": "user", "content": title_user }),
        ];
        assert!(detect_title_request(&title_messages, None));
        assert!(is_title_generation_record(
            title_system,
            title_user,
            "[]",
            ""
        ));

        // 有历史或有工具时不是标题请求。
        let with_history = vec![
            json!({ "role": "system", "content": title_system }),
            json!({ "role": "user", "content": "第一问" }),
            json!({ "role": "assistant", "content": "第一答" }),
            json!({ "role": "user", "content": title_user }),
        ];
        assert!(!detect_title_request(&with_history, None));
        assert!(!is_title_generation_record(
            title_system,
            title_user,
            r#"[{"role":"user","content":"第一问"}]"#,
            ""
        ));
        assert!(!detect_title_request(
            &title_messages,
            Some(&json!([{ "type": "function", "function": { "name": "read" } }]))
        ));
        assert!(!is_title_generation_record(
            title_system,
            title_user,
            "[]",
            r#"[{"type":"function","function":{"name":"read"}}]"#
        ));

        // 普通用户问“怎么写标题”不是标题生成请求。
        let real = vec![
            json!({ "role": "system", "content": "You are a coding agent." }),
            json!({ "role": "user", "content": "How do I write a title for my essay?" }),
        ];
        assert!(!detect_title_request(&real, None));
    }

    /// 请求侧字段：系统提示词、最后一次用户提问、工具清单与历史对话各自提取正确。
    #[test]
    fn draft_extracts_request_fields() {
        let mut request = InferRequest::new(
            "deepseek-web",
            vec![
                json!({ "role": "system", "content": "你是助手" }),
                json!({ "role": "user", "content": "第一问" }),
                json!({ "role": "assistant", "content": "第一答" }),
                json!({ "role": "user", "content": "第二问" }),
            ],
        );
        request.tools = Some(json!([{ "type": "function", "function": { "name": "read_file" } }]));

        let draft = RequestRecordDraft::from_request(&request, &meta());
        assert!(draft.id.starts_with("req_"), "{}", draft.id);
        assert_eq!(draft.system_prompt, "你是助手");
        assert_eq!(draft.user_prompt, "第二问");
        assert!(draft.tools.contains("read_file"));
        assert_eq!(draft.protocol, "openai");
        assert_eq!(draft.endpoint, "/v1/chat/completions");
        assert!(draft.headers.contains("content-type"));

        let history: Vec<serde_json::Value> = serde_json::from_str(&draft.history).unwrap();
        assert_eq!(history.len(), 2, "历史应包含第一问与第一答");
        assert_eq!(history[0]["content"], "第一问");
        assert_eq!(history[1]["content"], "第一答");
    }

    /// 历史里的多模态数组落库时去掉 base64，只留文本与占位标记。
    #[test]
    fn draft_sanitizes_image_parts_in_history() {
        let request = InferRequest::new(
            "deepseek-web",
            vec![
                json!({
                    "role": "user",
                    "content": [
                        { "type": "text", "text": "看图" },
                        { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } }
                    ]
                }),
                json!({ "role": "assistant", "content": "收到" }),
                json!({ "role": "user", "content": "继续" }),
            ],
        );
        let draft = RequestRecordDraft::from_request(&request, &meta());
        let history: Vec<serde_json::Value> = serde_json::from_str(&draft.history).unwrap();
        assert_eq!(history[0]["content"], "看图\n[image_url]");
        assert!(!draft.history.contains("data:image/png"), "不应把 base64 落库");
    }

    /// 有工具调用时回答分类为 tool_call，并保留思考内容与调用原文。
    #[test]
    fn answer_is_classified_as_tool_call() {
        let request = InferRequest::new("deepseek-web", vec![json!({ "role": "user", "content": "读文件" })]);
        let draft = RequestRecordDraft::from_request(&request, &meta());
        let response = ModelResponse {
            text: String::new(),
            reasoning: Some("先调用工具".to_string()),
            tool_calls: Some(json!([{ "id": "c1", "type": "function", "function": { "name": "read_file", "arguments": "{}" } }])),
            usage: None,
            finish_reason: Some("tool_calls".to_string()),
            is_complete: true,
        };
        let entry = draft.finish(&Ok(response));
        assert_eq!(entry.status, "success");
        assert_eq!(entry.answer_type, "tool_call");
        assert_eq!(entry.answer_reasoning, "先调用工具");
        assert!(entry.answer_tool_calls.contains("read_file"));
    }

    /// 请求失败也要留一条记录，回答类型记为 none，错误信息落库。
    #[test]
    fn failure_is_recorded() {
        let request = InferRequest::new("deepseek-web", vec![json!({ "role": "user", "content": "在吗" })]);
        let entry =
            RequestRecordDraft::from_request(&request, &meta()).finish(&Err(anyhow::anyhow!("渠道不可用")));
        assert_eq!(entry.status, "error");
        assert_eq!(entry.answer_type, "none");
        assert!(entry.error.contains("渠道不可用"));
    }

    /// 预览串只保留摘要，工具名与历史角色可读。
    #[test]
    fn previews_are_compact() {
        let tools = json!([
            { "type": "function", "function": { "name": "read_file" } },
            { "type": "function", "function": { "name": "bash" } }
        ])
        .to_string();
        assert_eq!(tools_preview(&tools), "read_file、bash（2 个）");
        assert_eq!(tools_preview(""), "（无）");

        let history = json!([
            { "role": "user", "content": "你好" },
            { "role": "assistant", "content": "你好，有什么可以帮你？" }
        ])
        .to_string();
        assert!(history_preview(&history).contains("用户: 你好"));
        assert!(history_preview(&history).contains("AI: 你好"));

        let headers = json!({
            "content-type": "application/json",
            "authorization": "Bearer sk-test"
        })
        .to_string();
        assert!(headers_preview(&headers).contains("content-type: application/json"));
        assert_eq!(headers_preview(""), "（无）");
    }

    /// 请求 id 是随机且带前缀的。
    #[test]
    fn request_ids_are_random_and_prefixed() {
        let first = new_request_id();
        let second = new_request_id();
        assert!(first.starts_with("req_"));
        assert_ne!(first, second);
    }

    /// 记录写入、列表读取、详情读取与清空形成闭环。
    #[tokio::test]
    async fn store_roundtrip() {
        let path = std::env::temp_dir().join(format!(
            "poolman-request-log-{}.db",
            uuid::Uuid::new_v4()
        ));
        let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
        let db = sea_orm::Database::connect(&url)
            .await
            .expect("初始化测试数据库失败");
        let store = RequestRecordStore::new(db).await.expect("同步表结构失败");

        let entry = RequestRecordEntry {
            id: new_request_id(),
            model: "deepseek-web".to_string(),
            stream: false,
            status: "success".to_string(),
            protocol: "deepseek".to_string(),
            method: "POST".to_string(),
            endpoint: "/chat/completions".to_string(),
            headers: json!({ "content-type": "application/json" }).to_string(),
            api_key: "sk-test".to_string(),
            duration_ms: 12,
            system_prompt: "系统".to_string(),
            user_prompt: "用户".to_string(),
            tools: "[]".to_string(),
            history: "[]".to_string(),
            answer_type: "text".to_string(),
            answer_content: "回答".to_string(),
            answer_reasoning: "思考".to_string(),
            answer_tool_calls: String::new(),
            error: String::new(),
            local_session_id: None,
            local_session_ignored: None,
            messages: "[]".to_string(),
            is_title_request: false,
            created_at: chrono::Utc::now().timestamp_millis(),
        };
        store.insert(entry.clone()).await.expect("写入记录失败");

        assert_eq!(store.count().await.expect("统计失败"), 1);
        let list = store.list(10).await.expect("读取列表失败");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, entry.id);
        assert_eq!(list[0].answer_preview, "回答");
        assert_eq!(list[0].protocol, "deepseek");
        assert_eq!(list[0].endpoint, "/chat/completions");
        assert_eq!(list[0].duration_ms, 12);
        assert!(list[0].headers.contains("content-type"));

        let detail = store
            .get(&entry.id)
            .await
            .expect("读取详情失败")
            .expect("记录应存在");
        assert_eq!(detail.answer_reasoning, "思考");
        assert_eq!(detail.answer_content, "回答");
        assert_eq!(detail.api_key, "sk-test");

        assert_eq!(store.clear().await.expect("清空失败"), 1);
        assert_eq!(store.count().await.expect("统计失败"), 0);
        let _ = std::fs::remove_file(path);
    }
}
