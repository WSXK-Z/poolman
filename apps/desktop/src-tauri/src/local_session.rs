//! 本地会话管理：把请求按「系统提示词 + 第一条用户提问」归并成会话。
//!
//! 每次新请求（没有历史记录）都会生成一条会话，并在回复中带上
//! `开始新的一轮对话：{随机串}` 标记。后续请求只要在整段对话里能找到该标记，
//! 就归入同一会话；找不到标记的请求按新会话处理，并生成新的标记写入回复。
//!
//! 标记参与会话 id 的哈希，因此同一基础 id 下也不会冲突。历史比对会剔除
//! `reasoning_content` / `reasoning`，因为 API 侧历史不携带思考内容。

use std::collections::BTreeMap;

use anyhow::Result;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, Set,
};
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::entitys::local_session::{ActiveModel, Column, Entity, Model};
use crate::entitys::local_session_content;
use crate::entitys::request_record;
use crate::entitys::web_context_binding;
use crate::models::defs::WebContextBinding;
use crate::request_record::{is_title_generation_record, message_text};

/// 回复中的会话标记前缀。
pub const MARKER_PREFIX: &str = "开始新的一轮对话：";

/// 标记随机串长度。
const MARKER_RANDOM_CHARS: usize = 8;

/// 会话列表条目。界面左侧只展示 `id`。
#[derive(Debug, Clone, Serialize)]
pub struct LocalSessionInfo {
    pub id: String,
    pub base_id: String,
    pub first_user_prompt: String,
    pub request_count: u64,
    pub collision_marker: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 会话关联的一条请求。
#[derive(Debug, Clone, Serialize)]
pub struct LocalSessionRequestInfo {
    pub request_id: String,
    pub model: String,
    pub status: String,
    pub user_prompt: String,
    pub answer_type: String,
    pub duration_ms: i64,
    pub created_at: i64,
}

/// 会话内容表的一行。
#[derive(Debug, Clone, Serialize)]
pub struct LocalSessionContentInfo {
    pub id: i64,
    pub session_id: String,
    pub kind: String,
    pub content: String,
    pub request_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_call_id: Option<String>,
    pub token_count: i64,
    pub created_at: i64,
}

/// 会话内容类型。
pub const CONTENT_SYSTEM_PROMPT: &str = "system_prompt";
pub const CONTENT_TOOLS: &str = "tools";
pub const CONTENT_USER: &str = "user";
pub const CONTENT_REASONING: &str = "reasoning";
pub const CONTENT_ASSISTANT_TEXT: &str = "assistant_text";
pub const CONTENT_ASSISTANT_TOOL_CALLS: &str = "assistant_tool_calls";
pub const CONTENT_TOOL_RESULT: &str = "tool_result";

/// 一次同步的结果。
#[derive(Debug, Clone, Default, Serialize)]
pub struct LocalSessionSyncResult {
    pub added_sessions: u64,
    pub linked_requests: u64,
    /// 本次为请求新生成的会话标记数。
    pub collision_sessions: u64,
    /// 本次生成的随机标记，供界面反馈。
    pub collision_markers: Vec<String>,
}

/// 请求处理前生成的会话计划：决定归入已有会话还是开启带标记的新会话。
#[derive(Debug, Clone)]
pub struct SessionPlan {
    pub session_id: String,
    pub base_id: String,
    pub system_prompt: String,
    pub first_user_prompt: String,
    /// 需要写进回复的标记；已有会话或历史里已带标记时为 None。
    pub marker: Option<String>,
    /// 需要写进 `local_sessions.collision_marker` 的标记。
    pub session_marker: Option<String>,
    pub existing: bool,
    /// 规范化后的本次输入（不含 system）。
    pub input: Vec<Value>,
}

/// 助手回答快照：用于把本轮回答写入会话对话。
#[derive(Debug, Clone, Default)]
pub struct SessionAnswer {
    pub text: String,
    pub tool_calls: Option<Value>,
}

/// 本地会话存储：SQLite 表 `local_sessions` 的唯一读写入口。
pub struct LocalSessionStore {
    db: DatabaseConnection,
    /// 同步互斥：按钮连续点击时只让一个同步任务扫描请求列表。
    sync_lock: tokio::sync::Mutex<()>,
}

impl LocalSessionStore {
    pub async fn new(db: DatabaseConnection) -> Result<Self> {
        db.get_schema_builder()
            .register(Entity)
            .register(local_session_content::Entity)
            .register(request_record::Entity)
            .register(web_context_binding::Entity)
            .sync(&db)
            .await?;
        Ok(Self {
            db,
            sync_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// 列出全部本地会话（按创建时间倒序）。
    pub async fn list_sessions(&self) -> Result<Vec<LocalSessionInfo>> {
        let rows = Entity::find()
            .order_by_desc(Column::CreatedAt)
            .all(&self.db)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let request_count = request_record::Entity::find()
                .filter(request_record::Column::LocalSessionId.eq(&row.id))
                .count(&self.db)
                .await?;
            out.push(LocalSessionInfo {
                id: row.id,
                base_id: row.base_id,
                first_user_prompt: row.first_user_prompt,
                request_count,
                collision_marker: row.collision_marker,
                created_at: row.created_at,
                updated_at: row.updated_at,
            });
        }
        Ok(out)
    }

    /// 列出某个会话关联的请求（按时间正序）。
    pub async fn list_session_requests(
        &self,
        session_id: &str,
    ) -> Result<Vec<LocalSessionRequestInfo>> {
        let rows = request_record::Entity::find()
            .filter(request_record::Column::LocalSessionId.eq(session_id))
            .order_by_asc(request_record::Column::CreatedAt)
            .all(&self.db)
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| LocalSessionRequestInfo {
                request_id: row.id,
                model: row.model,
                status: row.status,
                user_prompt: row.user_prompt,
                answer_type: row.answer_type,
                duration_ms: row.duration_ms.unwrap_or_default(),
                created_at: row.created_at,
            })
            .collect())
    }

    /// 读取一组本地会话的远端网页会话绑定；删除会话前用它清理远端对话。
    pub async fn bindings_for(&self, session_ids: &[String]) -> Result<Vec<WebContextBinding>> {
        if session_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = web_context_binding::Entity::find()
            .filter(web_context_binding::Column::ContextKey.is_in(session_ids.to_vec()))
            .all(&self.db)
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| WebContextBinding {
                channel: row.channel,
                context_key: row.context_key,
                remote_session_id: row.remote_session_id,
                parent_message_id: row.parent_message_id,
                primed: row.primed,
                system_hash: row.system_hash,
                tools_hash: row.tools_hash,
                approx_tokens: row.approx_tokens,
                summary: row.summary.unwrap_or_default(),
            })
            .collect())
    }

    /// 列出某个会话的内容表（按写入顺序）。
    pub async fn list_session_contents(
        &self,
        session_id: &str,
    ) -> Result<Vec<LocalSessionContentInfo>> {        let rows = local_session_content::Entity::find()
            .filter(local_session_content::Column::SessionId.eq(session_id))
            .order_by_asc(local_session_content::Column::Id)
            .all(&self.db)
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| LocalSessionContentInfo {
                id: row.id,
                session_id: row.session_id,
                kind: row.kind,
                content: row.content,
                request_id: row.request_id,
                tool_name: row.tool_name,
                tool_call_id: row.tool_call_id,
                token_count: row.token_count,
                created_at: row.created_at,
            })
            .collect())
    }

    /// 按会话的请求记录重建该会话内容表；系统提示词和工具表只写一份。
    pub async fn rebuild_session_content(&self, session_id: &str) -> Result<()> {
        let records = request_record::Entity::find()
            .filter(request_record::Column::LocalSessionId.eq(session_id))
            .order_by_asc(request_record::Column::CreatedAt)
            .order_by_asc(request_record::Column::Id)
            .all(&self.db)
            .await?;
        self.write_session_content(session_id, &records).await
    }

    /// 全量重建所有会话内容表。
    async fn rebuild_all_contents(&self) -> Result<()> {
        local_session_content::Entity::delete_many()
            .exec(&self.db)
            .await?;
        let records = request_record::Entity::find()
            .filter(request_record::Column::LocalSessionId.is_not_null())
            .order_by_asc(request_record::Column::CreatedAt)
            .order_by_asc(request_record::Column::Id)
            .all(&self.db)
            .await?;
        let mut groups: BTreeMap<String, Vec<request_record::Model>> = BTreeMap::new();
        for record in records {
            if let Some(session_id) = record.local_session_id.clone() {
                groups.entry(session_id).or_default().push(record);
            }
        }
        for (session_id, records) in groups {
            self.write_session_content(&session_id, &records).await?;
        }
        Ok(())
    }

    /// 用一组按时间排序的请求记录生成一个会话的完整内容。
    async fn write_session_content(
        &self,
        session_id: &str,
        records: &[request_record::Model],
    ) -> Result<()> {
        local_session_content::Entity::delete_many()
            .filter(local_session_content::Column::SessionId.eq(session_id))
            .exec(&self.db)
            .await?;

        let mut rows: Vec<local_session_content::ActiveModel> = Vec::new();
        let mut system_written = false;
        let mut tools_written = false;
        let mut known: Vec<Value> = Vec::new();

        for record in records {
            if record.is_title_request == Some(true)
                || is_title_generation_record(
                    &record.system_prompt,
                    &record.user_prompt,
                    &record.history,
                    &record.tools,
                )
            {
                continue;
            }
            if !system_written && !record.system_prompt.trim().is_empty() {
                rows.push(content_row(
                    session_id,
                    CONTENT_SYSTEM_PROMPT,
                    &record.system_prompt,
                    record,
                    None,
                    None,
                ));
                system_written = true;
            }
            if !tools_written {
                if let Some(tools) = tools_payload(&record.tools) {
                    rows.push(content_row(
                        session_id,
                        CONTENT_TOOLS,
                        &tools,
                        record,
                        None,
                        None,
                    ));
                    tools_written = true;
                }
            }

            let messages = full_messages_for_record(record);
            let canonical = messages
                .iter()
                .filter_map(canonical_message)
                .collect::<Vec<_>>();
            let common = common_prefix_len(&known, &canonical);
            for message in canonical.iter().skip(common) {
                append_message_content(&mut rows, session_id, record, message);
            }

            if !record.answer_reasoning.trim().is_empty() {
                rows.push(content_row(
                    session_id,
                    CONTENT_REASONING,
                    &record.answer_reasoning,
                    record,
                    None,
                    None,
                ));
            }
            if !record.answer_content.is_empty() {
                rows.push(content_row(
                    session_id,
                    CONTENT_ASSISTANT_TEXT,
                    &record.answer_content,
                    record,
                    None,
                    None,
                ));
            }
            if let Some(calls) = tool_calls_payload(&record.answer_tool_calls) {
                let name = tool_names(&calls);
                rows.push(content_row(
                    session_id,
                    CONTENT_ASSISTANT_TOOL_CALLS,
                    &calls,
                    record,
                    name.as_deref(),
                    None,
                ));
            }

            known = canonical;
            if let Some(answer) = assistant_from_record(record) {
                known.push(answer);
            }
        }

        if !rows.is_empty() {
            local_session_content::Entity::insert_many(rows)
                .exec(&self.db)
                .await?;
        }
        Ok(())
    }

    /// 请求处理前决定会话：对话里已有标记且会话存在则归入旧会话；
    /// 否则生成新标记，开启新会话。
    pub async fn plan(&self, messages: &[Value]) -> Result<SessionPlan> {
        let (system_prompt, input) = split_messages(messages);
        let first_user = first_user_prompt(&input);
        let base_id = session_id_from_parts(&[&system_prompt, &first_user]);

        if let Some(marker) = find_marker(messages) {
            if let Some(row) = Entity::find()
                .filter(Column::CollisionMarker.eq(marker.as_str()))
                .one(&self.db)
                .await?
            {
                return Ok(SessionPlan {
                    session_id: row.id,
                    base_id: row.base_id,
                    system_prompt: row.system_prompt,
                    first_user_prompt: row.first_user_prompt,
                    marker: None,
                    session_marker: None,
                    existing: true,
                    input,
                });
            }
            // 历史里已经有标记，说明会话生成过；即使本地会话行缺失，
            // 也用历史里的标记恢复会话，不再生成第二个标记。
            let session_id = session_id_from_parts(&[&system_prompt, &first_user, &marker]);
            return Ok(SessionPlan {
                session_id,
                base_id,
                system_prompt,
                first_user_prompt: first_user,
                marker: None,
                session_marker: Some(marker),
                existing: false,
                input,
            });
        }

        let marker = new_marker();
        let session_id = session_id_from_parts(&[&system_prompt, &first_user, &marker]);
        Ok(SessionPlan {
            session_id,
            base_id,
            system_prompt,
            first_user_prompt: first_user,
            marker: Some(marker.clone()),
            session_marker: Some(marker),
            existing: false,
            input,
        })
    }

    /// 回答产生后落库新会话；已有会话只刷新更新时间。
    pub async fn finalize(&self, plan: &SessionPlan, answer: Option<&SessionAnswer>) -> Result<()> {
        // 与全量同步互斥，避免同步删除会话时并发插入。
        let _guard = self.sync_lock.lock().await;
        let now = chrono::Utc::now().timestamp_millis();
        if plan.existing {
            if let Some(row) = Entity::find_by_id(plan.session_id.clone())
                .one(&self.db)
                .await?
            {
                let mut active: ActiveModel = row.into();
                active.updated_at = Set(now);
                active.update(&self.db).await?;
            }
            return Ok(());
        }

        let conversation = conversation_with_answer(&plan.input, answer);
        ActiveModel {
            id: Set(plan.session_id.clone()),
            base_id: Set(plan.base_id.clone()),
            system_prompt: Set(plan.system_prompt.clone()),
            first_user_prompt: Set(plan.first_user_prompt.clone()),
            conversation: Set(conversation),
            collision_marker: Set(plan.session_marker.clone()),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&self.db)
        .await?;
        Ok(())
    }

    /// 删除一个或多个本地会话：关联请求标记为忽略并断开归属，避免全量同步重新建会话。
    pub async fn delete_sessions(&self, ids: &[String]) -> Result<u64> {
        let _guard = self.sync_lock.lock().await;
        let mut deleted = 0;
        for id in ids {
            let records = request_record::Entity::find()
                .filter(request_record::Column::LocalSessionId.eq(id))
                .all(&self.db)
                .await?;
            for record in records {
                let mut active: request_record::ActiveModel = record.into();
                active.local_session_id = Set(None);
                active.local_session_ignored = Set(Some(true));
                active.update(&self.db).await?;
            }
            let result = Entity::delete_by_id(id.clone()).exec(&self.db).await?;
            local_session_content::Entity::delete_many()
                .filter(local_session_content::Column::SessionId.eq(id))
                .exec(&self.db)
                .await?;
            web_context_binding::Entity::delete_many()
                .filter(web_context_binding::Column::ContextKey.eq(id))
                .exec(&self.db)
                .await?;
            if result.rows_affected > 0 {
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    /// 全量重建本地会话：重新扫描请求列表，按回复标记归并；没有标记的旧请求
    /// 先用历史前缀兼容归并，仍无法归并时用请求 id 派生稳定标记。
    pub async fn sync(&self) -> Result<LocalSessionSyncResult> {
        let _guard = self.sync_lock.lock().await;
        let mut result = LocalSessionSyncResult::default();
        let records = request_record::Entity::find()
            .order_by_asc(request_record::Column::CreatedAt)
            .order_by_asc(request_record::Column::Id)
            .all(&self.db)
            .await?;
        Entity::delete_many().exec(&self.db).await?;
        let mut sessions: Vec<Model> = Vec::new();

        for record in records {
            if record.local_session_ignored == Some(true) {
                continue;
            }
            let title_record = record.is_title_request == Some(true)
                || is_title_generation_record(
                    &record.system_prompt,
                    &record.user_prompt,
                    &record.history,
                    &record.tools,
                );
            if title_record {
                // 旧记录可能已经被错误地归入会话，这里顺手清掉并标记类型。
                if record.local_session_id.is_some() || record.is_title_request != Some(true) {
                    let mut active: request_record::ActiveModel = record.into();
                    active.local_session_id = Set(None);
                    active.is_title_request = Set(Some(true));
                    active.update(&self.db).await?;
                }
                continue;
            }
            let (system_prompt, input) = split_history(&record);
            let first_user = first_user_prompt(&input);
            let base_id = session_id_from_parts(&[&system_prompt, &first_user]);
            let marker = find_marker_in_record(&record);

            let (session_id, new_marker) = if let Some(marker) = marker {
                if let Some(session) = sessions
                    .iter()
                    .find(|session| session.collision_marker.as_deref() == Some(marker.as_str()))
                {
                    (session.id.clone(), None)
                } else {
                    let id = session_id_from_parts(&[&system_prompt, &first_user, &marker]);
                    (id, Some(marker))
                }
            } else if input.len() > 1 {
                // 兼容旧记录：没有标记时先按同一基础 id 下的历史前缀归并。
                let mut matched: Option<(usize, bool)> = None;
                for (index, session) in sessions
                    .iter()
                    .enumerate()
                    .filter(|(_, session)| session.base_id == base_id)
                {
                    let existing = parse_conversation(&session.conversation);
                    let common = common_prefix_len(&existing, &input);
                    if common == existing.len() {
                        matched = Some((index, existing.len() < input.len()));
                        break;
                    }
                    if common == input.len() {
                        matched = Some((index, false));
                        break;
                    }
                }
                if let Some((index, extend)) = matched {
                    if extend {
                        let conversation = conversation_with_record_answer(&input, &record);
                        let mut active: ActiveModel = sessions[index].clone().into();
                        active.conversation = Set(conversation.clone());
                        active.updated_at = Set(record.created_at);
                        active.update(&self.db).await?;
                        sessions[index].conversation = conversation;
                        sessions[index].updated_at = record.created_at;
                    }
                    (sessions[index].id.clone(), None)
                } else {
                    let marker = marker_from_record(&record);
                    let id = session_id_from_parts(&[&system_prompt, &first_user, &marker]);
                    (id, Some(marker))
                }
            } else {
                // 无历史的新请求：每条都开启新会话，用稳定标记参与 id 哈希。
                let marker = marker_from_record(&record);
                let id = session_id_from_parts(&[&system_prompt, &first_user, &marker]);
                (id, Some(marker))
            };

            if let Some(marker) = new_marker {
                let conversation = conversation_with_record_answer(&input, &record);
                let model = Model {
                    id: session_id.clone(),
                    base_id: base_id.clone(),
                    system_prompt: system_prompt.clone(),
                    first_user_prompt: first_user.clone(),
                    conversation,
                    collision_marker: Some(marker.clone()),
                    created_at: record.created_at,
                    updated_at: record.created_at,
                };
                ActiveModel {
                    id: Set(model.id.clone()),
                    base_id: Set(model.base_id.clone()),
                    system_prompt: Set(model.system_prompt.clone()),
                    first_user_prompt: Set(model.first_user_prompt.clone()),
                    conversation: Set(model.conversation.clone()),
                    collision_marker: Set(model.collision_marker.clone()),
                    created_at: Set(model.created_at),
                    updated_at: Set(model.updated_at),
                }
                .insert(&self.db)
                .await?;
                sessions.push(model);
                result.collision_sessions += 1;
                result.collision_markers.push(marker);
            }

            let mut active: request_record::ActiveModel = record.into();
            active.local_session_id = Set(Some(session_id));
            active.update(&self.db).await?;
            result.linked_requests += 1;
        }
        result.added_sessions = sessions.len() as u64;
        let valid_ids = sessions
            .iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>();
        // 标题/摘要专用会话不属于本地会话，不能被同步清理掉。
        let auxiliary_ids = vec![
            "__poolman_title__".to_string(),
            "__poolman_summary__".to_string(),
        ];
        if valid_ids.is_empty() {
            web_context_binding::Entity::delete_many()
                .filter(web_context_binding::Column::ContextKey.is_not_in(auxiliary_ids))
                .exec(&self.db)
                .await?;
        } else {
            web_context_binding::Entity::delete_many()
                .filter(web_context_binding::Column::ContextKey.is_not_in(valid_ids))
                .filter(web_context_binding::Column::ContextKey.is_not_in(auxiliary_ids))
                .exec(&self.db)
                .await?;
        }
        self.rebuild_all_contents().await?;
        Ok(result)
    }
}

/// 请求消息 → (系统提示词, 规范化对话输入)。
fn split_messages(messages: &[Value]) -> (String, Vec<Value>) {
    let system_prompt = messages
        .iter()
        .filter(|message| role_of(message) == Some("system"))
        .map(message_text)
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let input = messages
        .iter()
        .filter(|message| role_of(message) != Some("system"))
        .filter_map(canonical_message)
        .collect::<Vec<_>>();
    (system_prompt, input)
}

/// 构造一行会话内容。
fn content_row(
    session_id: &str,
    kind: &str,
    content: &str,
    record: &request_record::Model,
    tool_name: Option<&str>,
    tool_call_id: Option<&str>,
) -> local_session_content::ActiveModel {
    local_session_content::ActiveModel {
        id: sea_orm::ActiveValue::NotSet,
        session_id: Set(session_id.to_string()),
        kind: Set(kind.to_string()),
        content: Set(content.to_string()),
        request_id: Set(Some(record.id.clone())),
        tool_name: Set(tool_name.map(str::to_string)),
        tool_call_id: Set(tool_call_id.map(str::to_string)),
        token_count: Set(i64::from(crate::tokenizer::count(content))),
        created_at: Set(record.created_at),
    }
}

/// 工具表：空工具（`""`、`[]`、`null`）不写入内容表。
fn tools_payload(raw: &str) -> Option<String> {
    if raw.trim().is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Null) => None,
        Ok(Value::Array(items)) if items.is_empty() => None,
        Ok(Value::Array(items)) => Some(Value::Array(items).to_string()),
        Ok(value) => Some(value.to_string()),
        Err(_) => None,
    }
}

/// 回答中的工具调用：空数组不写入内容表。
fn tool_calls_payload(raw: &str) -> Option<String> {
    if raw.trim().is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Null) => None,
        Ok(Value::Array(items)) if items.is_empty() => None,
        Ok(value) => Some(value.to_string()),
        Err(_) => None,
    }
}

/// 从工具调用数组里提取函数名，便于在内容表上展示。
fn tool_names(raw: &str) -> Option<String> {
    let Value::Array(items) = serde_json::from_str::<Value>(raw).ok()? else {
        return None;
    };
    let names = items
        .iter()
        .filter_map(|call| {
            call.pointer("/function/name")
                .and_then(Value::as_str)
                .or_else(|| call.get("name").and_then(Value::as_str))
        })
        .collect::<Vec<_>>();
    if names.is_empty() {
        None
    } else {
        Some(names.join(", "))
    }
}

/// 请求的完整非 system 消息；旧记录没有 `messages` 时退化为 history + 当前提问。
fn full_messages_for_record(record: &request_record::Model) -> Vec<Value> {
    if let Some(raw) = record.messages.as_deref() {
        if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(raw) {
            return items;
        }
    }
    let mut messages = Vec::new();
    if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&record.history) {
        messages.extend(items);
    }
    messages.push(json!({ "role": "user", "content": record.user_prompt }));
    messages
}

/// 把一条历史消息拆成内容表行。
fn append_message_content(
    rows: &mut Vec<local_session_content::ActiveModel>,
    session_id: &str,
    record: &request_record::Model,
    message: &Value,
) {
    match role_of(message) {
        Some("user") => {
            let text = message_text(message);
            if !text.trim().is_empty() {
                rows.push(content_row(session_id, CONTENT_USER, &text, record, None, None));
            }
        }
        Some("assistant") => {
            if let Some(reasoning) = message
                .get("reasoning_content")
                .or_else(|| message.get("reasoning"))
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
            {
                rows.push(content_row(
                    session_id,
                    CONTENT_REASONING,
                    reasoning,
                    record,
                    None,
                    None,
                ));
            }
            let text = message_text(message);
            if !text.trim().is_empty() {
                rows.push(content_row(
                    session_id,
                    CONTENT_ASSISTANT_TEXT,
                    &text,
                    record,
                    None,
                    None,
                ));
            }
            if let Some(calls) = message
                .get("tool_calls")
                .filter(|value| value.as_array().is_some_and(|items| !items.is_empty()))
            {
                let raw = calls.to_string();
                let name = tool_names(&raw);
                rows.push(content_row(
                    session_id,
                    CONTENT_ASSISTANT_TOOL_CALLS,
                    &raw,
                    record,
                    name.as_deref(),
                    None,
                ));
            }
        }
        Some("tool") | Some("function") => {
            let text = message_text(message);
            if text.trim().is_empty() {
                return;
            }
            let name = message
                .get("name")
                .or_else(|| message.get("tool_name"))
                .and_then(Value::as_str);
            let call_id = message.get("tool_call_id").and_then(Value::as_str);
            rows.push(content_row(
                session_id,
                CONTENT_TOOL_RESULT,
                &text,
                record,
                name,
                call_id,
            ));
        }
        _ => {}
    }
}

/// 请求记录 → (系统提示词, 规范化对话输入)。
fn split_history(record: &request_record::Model) -> (String, Vec<Value>) {
    let mut messages = Vec::new();
    if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&record.history) {
        messages.extend(items);
    }
    messages.push(json!({ "role": "user", "content": record.user_prompt }));
    split_messages(&messages)
}

/// 单条消息规范化：只保留参与对话比对的字段，跳过思考内容。
fn canonical_message(message: &Value) -> Option<Value> {
    let role = role_of(message)?;
    let content = message_text(message);
    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .filter(|calls| !calls.is_empty());
    // 纯思考、没有正文与工具调用的助手消息不参与比较。
    if role == "assistant" && content.trim().is_empty() && tool_calls.is_none() {
        return None;
    }
    let mut object = Map::new();
    object.insert("role".to_string(), json!(role));
    object.insert(
        "content".to_string(),
        if content.is_empty() {
            Value::Null
        } else {
            json!(content)
        },
    );
    if let Some(calls) = tool_calls {
        object.insert("tool_calls".to_string(), Value::Array(calls.clone()));
    }
    for key in ["tool_call_id", "name"] {
        if let Some(value) = message.get(key) {
            object.insert(key.to_string(), value.clone());
        }
    }
    Some(Value::Object(object))
}

fn role_of(message: &Value) -> Option<&str> {
    message.get("role").and_then(Value::as_str)
}

/// 第一条用户真实提问。
fn first_user_prompt(messages: &[Value]) -> String {
    messages
        .iter()
        .find(|message| role_of(message) == Some("user"))
        .map(message_text)
        .unwrap_or_default()
}

/// 在整段对话消息里从后往前查找标记。
fn find_marker(messages: &[Value]) -> Option<String> {
    for message in messages.iter().rev() {
        let text = message_text(message);
        if let Some(index) = text.rfind(MARKER_PREFIX) {
            let rest = &text[index + MARKER_PREFIX.len()..];
            let random: String = rest.chars().take(MARKER_RANDOM_CHARS).collect();
            if random.chars().count() == MARKER_RANDOM_CHARS {
                return Some(format!("{MARKER_PREFIX}{random}"));
            }
        }
    }
    None
}

/// 从请求记录（历史 + 本轮提问 + 本轮回答）里查找标记。
fn find_marker_in_record(record: &request_record::Model) -> Option<String> {
    let mut messages = vec![json!({ "role": "system", "content": record.system_prompt })];
    if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&record.history) {
        messages.extend(items);
    }
    messages.push(json!({ "role": "user", "content": record.user_prompt }));
    if let Some(answer) = assistant_from_record(record) {
        messages.push(answer);
    }
    find_marker(&messages)
}

/// 会话对话：本次输入 + 本轮回答（回答文本应已包含标记）。
fn conversation_with_answer(input: &[Value], answer: Option<&SessionAnswer>) -> String {
    let mut messages = input.to_vec();
    if let Some(answer) = answer {
        let tool_calls = answer
            .tool_calls
            .as_ref()
            .filter(|value| value.as_array().is_some_and(|calls| !calls.is_empty()));
        if !answer.text.trim().is_empty() || tool_calls.is_some() {
            let mut object = Map::new();
            object.insert("role".to_string(), json!("assistant"));
            object.insert(
                "content".to_string(),
                if answer.text.is_empty() {
                    Value::Null
                } else {
                    json!(answer.text)
                },
            );
            if let Some(calls) = tool_calls {
                object.insert("tool_calls".to_string(), calls.clone());
            }
            messages.push(Value::Object(object));
        }
    }
    Value::Array(messages).to_string()
}

/// 会话对话：本次输入 + 请求记录里的回答。
fn conversation_with_record_answer(input: &[Value], record: &request_record::Model) -> String {
    let mut messages = input.to_vec();
    if let Some(answer) = assistant_from_record(record) {
        messages.push(answer);
    }
    Value::Array(messages).to_string()
}

fn assistant_from_record(record: &request_record::Model) -> Option<Value> {
    if record.status == "error" {
        return None;
    }
    let tool_calls = serde_json::from_str::<Value>(&record.answer_tool_calls)
        .ok()
        .filter(|value| value.as_array().is_some_and(|calls| !calls.is_empty()));
    if record.answer_content.trim().is_empty() && tool_calls.is_none() {
        return None;
    }
    let mut object = Map::new();
    object.insert("role".to_string(), json!("assistant"));
    object.insert(
        "content".to_string(),
        if record.answer_content.is_empty() {
            Value::Null
        } else {
            json!(record.answer_content)
        },
    );
    if let Some(calls) = tool_calls {
        object.insert("tool_calls".to_string(), calls);
    }
    Some(Value::Object(object))
}

fn parse_conversation(raw: &str) -> Vec<Value> {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Array(messages)) => messages,
        _ => Vec::new(),
    }
}

fn common_prefix_len(left: &[Value], right: &[Value]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count()
}

/// 会话 id：`sess_` + UUIDv5（SHA-1）确定性哈希。
fn session_id_from_parts(parts: &[&str]) -> String {
    let mut material = String::new();
    for part in parts {
        material.push_str(&part.len().to_string());
        material.push('#');
        material.push_str(part);
        material.push('\u{1f}');
    }
    let hash = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, material.as_bytes())
        .simple()
        .to_string();
    format!("sess_{hash}")
}

/// 新会话标记：`开始新的一轮对话：` + 8 位随机串。
pub fn new_marker() -> String {
    let random = uuid::Uuid::new_v4().simple().to_string();
    format!("{MARKER_PREFIX}{}", &random[..MARKER_RANDOM_CHARS])
}

/// 旧请求没有标记时，用请求 id 派生稳定标记，保证重复同步不会换会话。
fn marker_from_record(record: &request_record::Model) -> String {
    let suffix = record.id.rsplit('_').next().unwrap_or(record.id.as_str());
    let random: String = suffix.chars().take(MARKER_RANDOM_CHARS).collect();
    format!("{MARKER_PREFIX}{random:0<8}")
}

#[cfg(test)]
mod tests {
    use super::{
        canonical_message, find_marker, first_user_prompt, new_marker, session_id_from_parts,
        LocalSessionStore, SessionAnswer, MARKER_PREFIX,
    };
    use sea_orm::Database;
    use serde_json::json;

    /// 思考内容不参与比对；只含思考的助手消息被跳过。
    #[test]
    fn canonical_message_skips_reasoning() {
        let message = json!({
            "role": "assistant",
            "content": "回答",
            "reasoning_content": "思考",
            "tool_calls": [{ "id": "c1", "function": { "name": "read", "arguments": "{}" } }]
        });
        let canonical = canonical_message(&message).expect("应保留消息");
        assert!(canonical.get("reasoning_content").is_none());
        assert_eq!(canonical["content"], "回答");
        assert_eq!(canonical["tool_calls"][0]["id"], "c1");

        let thinking_only = json!({ "role": "assistant", "reasoning_content": "只有思考" });
        assert!(canonical_message(&thinking_only).is_none());
    }

    #[test]
    fn hash_is_deterministic_and_distinct() {
        let first = session_id_from_parts(&["系统", "第一问"]);
        let second = session_id_from_parts(&["系统", "第一问"]);
        let other = session_id_from_parts(&["系统", "第二问"]);
        assert_eq!(first, second);
        assert_ne!(first, other);
        assert!(first.starts_with("sess_"));
    }

    #[test]
    fn first_user_is_the_earliest_user_message() {
        let messages = vec![
            json!({ "role": "assistant", "content": "旧回答" }),
            json!({ "role": "user", "content": "真正的第一问" }),
            json!({ "role": "user", "content": "第二问" }),
        ];
        assert_eq!(first_user_prompt(&messages), "真正的第一问");
    }

    /// 标记可以从整段对话中识别出来。
    #[test]
    fn marker_is_found_in_conversation() {
        let marker = new_marker();
        assert!(marker.starts_with(MARKER_PREFIX));
        let messages = vec![
            json!({ "role": "user", "content": "第一问" }),
            json!({ "role": "assistant", "content": format!("{marker}\n\n第一答") }),
            json!({ "role": "user", "content": "第二问" }),
        ];
        assert_eq!(find_marker(&messages), Some(marker));
    }

    /// 历史里已有标记但本地会话行缺失时，仍沿用历史标记恢复会话，不生成第二个标记。
    #[tokio::test]
    async fn history_marker_recovers_missing_session_without_new_marker() {
        let path = std::env::temp_dir().join(format!(
            "poolman-local-session-recover-{}.db",
            uuid::Uuid::new_v4()
        ));
        let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
        let db = Database::connect(&url)
            .await
            .expect("初始化测试数据库失败");
        let store = LocalSessionStore::new(db).await.expect("同步表结构失败");
        let marker = new_marker();
        let messages = vec![
            json!({ "role": "system", "content": "系统" }),
            json!({ "role": "user", "content": "第一问" }),
            json!({ "role": "assistant", "content": format!("{marker}\n\n第一答") }),
            json!({ "role": "user", "content": "第二问" }),
        ];
        let plan = store.plan(&messages).await.expect("生成会话计划失败");
        assert!(!plan.existing);
        assert!(plan.marker.is_none(), "不应再生成第二个回复标记");
        assert_eq!(plan.session_marker.as_deref(), Some(marker.as_str()));
        assert_eq!(
            plan.session_id,
            session_id_from_parts(&["系统", "第一问", &marker])
        );

        let _ = std::fs::remove_file(path);
    }

    /// 有标记的后续请求归入同一会话；没有标记的请求各自开启新会话。
    #[tokio::test]
    async fn sync_groups_by_marker() {
        let path = std::env::temp_dir().join(format!(
            "poolman-local-session-{}.db",
            uuid::Uuid::new_v4()
        ));
        let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
        let db = Database::connect(&url)
            .await
            .expect("初始化测试数据库失败");
        let store = LocalSessionStore::new(db).await.expect("同步表结构失败");
        let base_time = chrono::Utc::now().timestamp_millis();
        let marker = new_marker();

        // 第一部分请求的回答里带着标记，模拟实时请求注入标记后的记录。
        insert_request(
            &store,
            "[]",
            "第一问",
            &format!("{marker}\n\n第一答"),
            base_time,
        )
        .await;
        let first = store.sync().await.expect("首次同步失败");
        assert_eq!(first.added_sessions, 1);
        let sessions = store.list_sessions().await.expect("读取会话失败");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].collision_marker.as_deref(), Some(marker.as_str()));

        // 第二条请求的助手历史里带着该标记，应归入同一会话。
        let history = json!([
            { "role": "user", "content": "第一问" },
            { "role": "assistant", "content": format!("{marker}\n\n第一答") }
        ])
        .to_string();
        insert_request(&store, &history, "第二问", "第二答", base_time + 1).await;
        let second = store.sync().await.expect("第二次同步失败");
        assert_eq!(second.added_sessions, 1);
        assert_eq!(second.linked_requests, 2);
        let sessions = store.list_sessions().await.expect("读取会话失败");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].request_count, 2);

        let _ = std::fs::remove_file(path);
    }

    /// 兼容旧记录：没有标记时用历史前缀归并同一会话。
    #[tokio::test]
    async fn sync_groups_legacy_continuation_without_marker() {
        let path = std::env::temp_dir().join(format!(
            "poolman-local-session-legacy-{}.db",
            uuid::Uuid::new_v4()
        ));
        let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
        let db = Database::connect(&url)
            .await
            .expect("初始化测试数据库失败");
        let store = LocalSessionStore::new(db).await.expect("同步表结构失败");
        let base_time = chrono::Utc::now().timestamp_millis();
        insert_request(&store, "[]", "第一问", "第一答", base_time).await;
        let history = json!([
            { "role": "user", "content": "第一问" },
            { "role": "assistant", "content": "第一答" }
        ])
        .to_string();
        insert_request(&store, &history, "第二问", "第二答", base_time + 1).await;

        let result = store.sync().await.expect("同步失败");
        assert_eq!(result.added_sessions, 1);
        assert_eq!(result.linked_requests, 2);
        let sessions = store.list_sessions().await.expect("读取会话失败");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].request_count, 2);

        let _ = std::fs::remove_file(path);
    }

    /// 会话删除前能读到远端网页会话绑定。
    #[tokio::test]
    async fn bindings_for_reads_remote_conversation() {
        let path = std::env::temp_dir().join(format!(
            "poolman-local-session-binding-{}.db",
            uuid::Uuid::new_v4()
        ));
        let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
        let db = Database::connect(&url)
            .await
            .expect("初始化测试数据库失败");
        let store = LocalSessionStore::new(db.clone())
            .await
            .expect("同步表结构失败");
        let bindings = crate::models::web_context::DbWebContextStore::new(db)
            .await
            .expect("初始化绑定表失败");
        use crate::models::defs::{WebContextBinding, WebContextStore};
        let binding = WebContextBinding {
            channel: "deepseek-web".to_string(),
            context_key: "sess_test".to_string(),
            remote_session_id: "remote_test".to_string(),
            parent_message_id: Some(7),
            primed: true,
            system_hash: "s".to_string(),
            tools_hash: "t".to_string(),
            approx_tokens: 10,
            summary: String::new(),
        };
        bindings.save(&binding).await.expect("保存绑定失败");
        let rows = store
            .bindings_for(&["sess_test".to_string()])
            .await
            .expect("读取绑定失败");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].channel, "deepseek-web");
        assert_eq!(rows[0].remote_session_id, "remote_test");

        let _ = std::fs::remove_file(path);
    }

    /// 删除会话后请求保留但被忽略，再次同步不会把会话建回来。
    #[tokio::test]
    async fn delete_sessions_keeps_requests_ignored() {
        let path = std::env::temp_dir().join(format!(
            "poolman-local-session-delete-{}.db",
            uuid::Uuid::new_v4()
        ));
        let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
        let db = Database::connect(&url)
            .await
            .expect("初始化测试数据库失败");
        let store = LocalSessionStore::new(db).await.expect("同步表结构失败");
        insert_request(
            &store,
            "[]",
            "第一问",
            "第一答",
            chrono::Utc::now().timestamp_millis(),
        )
        .await;
        store.sync().await.expect("首次同步失败");
        let sessions = store.list_sessions().await.expect("读取会话失败");
        assert_eq!(sessions.len(), 1);

        let deleted = store
            .delete_sessions(&[sessions[0].id.clone()])
            .await
            .expect("删除会话失败");
        assert_eq!(deleted, 1);
        assert!(store.list_sessions().await.expect("读取会话失败").is_empty());

        let result = store.sync().await.expect("再次同步失败");
        assert_eq!(result.linked_requests, 0);
        assert!(store.list_sessions().await.expect("读取会话失败").is_empty());

        let _ = std::fs::remove_file(path);
    }

    /// 会话内容表按顺序保存一份内容，系统提示词不会重复。
    #[tokio::test]
    async fn session_content_keeps_one_copy_and_order() {
        let path = std::env::temp_dir().join(format!(
            "poolman-local-session-content-{}.db",
            uuid::Uuid::new_v4()
        ));
        let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
        let db = Database::connect(&url)
            .await
            .expect("初始化测试数据库失败");
        let store = LocalSessionStore::new(db).await.expect("同步表结构失败");
        let base_time = chrono::Utc::now().timestamp_millis();
        insert_request(&store, "[]", "第一问", "第一答", base_time).await;
        store.sync().await.expect("首次同步失败");
        let sessions = store.list_sessions().await.expect("读取会话失败");
        let session_id = sessions[0].id.clone();

        let contents = store
            .list_session_contents(&session_id)
            .await
            .expect("读取会话内容失败");
        let kinds = contents
            .iter()
            .map(|row| row.kind.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                crate::local_session::CONTENT_SYSTEM_PROMPT,
                crate::local_session::CONTENT_USER,
                crate::local_session::CONTENT_ASSISTANT_TEXT
            ]
        );

        let history = json!([
            { "role": "user", "content": "第一问" },
            { "role": "assistant", "content": "第一答" }
        ])
        .to_string();
        insert_request(&store, &history, "第二问", "第二答", base_time + 1).await;
        store.sync().await.expect("第二次同步失败");
        let contents = store
            .list_session_contents(&session_id)
            .await
            .expect("读取会话内容失败");
        let kinds = contents
            .iter()
            .map(|row| row.kind.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                crate::local_session::CONTENT_SYSTEM_PROMPT,
                crate::local_session::CONTENT_USER,
                crate::local_session::CONTENT_ASSISTANT_TEXT,
                crate::local_session::CONTENT_USER,
                crate::local_session::CONTENT_ASSISTANT_TEXT
            ]
        );
        assert_eq!(
            contents
                .iter()
                .filter(|row| row.kind == crate::local_session::CONTENT_SYSTEM_PROMPT)
                .count(),
            1
        );
        assert_eq!(
            contents
                .iter()
                .filter(|row| row.kind == crate::local_session::CONTENT_USER)
                .map(|row| row.content.as_str())
                .collect::<Vec<_>>(),
            vec!["第一问", "第二问"]
        );
        assert!(contents.iter().all(|row| row.token_count > 0));

        let _ = std::fs::remove_file(path);
    }

    /// 请求处理：无标记时生成计划，finalize 落库；后续带标记的请求归入同一会话。
    #[tokio::test]
    async fn plan_and_finalize_roundtrip() {
        let path = std::env::temp_dir().join(format!(
            "poolman-local-session-plan-{}.db",
            uuid::Uuid::new_v4()
        ));
        let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
        let db = Database::connect(&url)
            .await
            .expect("初始化测试数据库失败");
        let store = LocalSessionStore::new(db).await.expect("同步表结构失败");
        let messages = vec![
            json!({ "role": "system", "content": "系统提示" }),
            json!({ "role": "user", "content": "第一问" }),
        ];
        let plan = store.plan(&messages).await.expect("生成计划失败");
        assert!(!plan.existing);
        let marker = plan.marker.clone().expect("新计划应生成标记");
        let answer = SessionAnswer {
            text: format!("{marker}\n\n第一答"),
            tool_calls: None,
        };
        store
            .finalize(&plan, Some(&answer))
            .await
            .expect("落库会话失败");

        let follow_up = vec![
            json!({ "role": "system", "content": "系统提示" }),
            json!({ "role": "user", "content": "第一问" }),
            json!({ "role": "assistant", "content": format!("{marker}\n\n第一答") }),
            json!({ "role": "user", "content": "第二问" }),
        ];
        let next = store.plan(&follow_up).await.expect("生成后续计划失败");
        assert!(next.existing);
        assert_eq!(next.session_id, plan.session_id);
        assert!(next.marker.is_none());

        let _ = std::fs::remove_file(path);
    }

    async fn insert_request(
        store: &LocalSessionStore,
        history: &str,
        user_prompt: &str,
        answer: &str,
        created_at: i64,
    ) {
        use crate::entitys::request_record;
        use sea_orm::{ActiveModelTrait, Set};
        let mut messages: Vec<serde_json::Value> =
            serde_json::from_str(history).unwrap_or_default();
        messages.push(json!({ "role": "user", "content": user_prompt }));
        request_record::ActiveModel {
            id: Set(format!("req_{}", uuid::Uuid::new_v4().simple())),
            model: Set("deepseek-web".to_string()),
            stream: Set(false),
            status: Set("success".to_string()),
            protocol: Set(Some("deepseek".to_string())),
            method: Set(Some("POST".to_string())),
            endpoint: Set(Some("/chat/completions".to_string())),
            headers: Set(Some("{}".to_string())),
            api_key: Set(Some("sk-test".to_string())),
            duration_ms: Set(Some(1)),
            local_session_id: Set(None),
            local_session_ignored: Set(None),
            is_title_request: Set(None),
            system_prompt: Set("系统提示".to_string()),
            user_prompt: Set(user_prompt.to_string()),
            tools: Set(String::new()),
            history: Set(history.to_string()),
            messages: Set(Some(serde_json::Value::Array(messages).to_string())),
            answer_type: Set("text".to_string()),
            answer_content: Set(answer.to_string()),
            answer_reasoning: Set(String::new()),
            answer_tool_calls: Set(String::new()),
            error: Set(String::new()),
            system_prompt_preview: Set(Some("系统提示".to_string())),
            user_prompt_preview: Set(Some(user_prompt.to_string())),
            tools_preview: Set(Some("（无）".to_string())),
            history_preview: Set(Some("（无）".to_string())),
            answer_preview: Set(Some(answer.to_string())),
            headers_preview: Set(Some("（无）".to_string())),
            created_at: Set(created_at),
        }
        .insert(&store.db)
        .await
        .expect("写入测试请求失败");
    }
}
