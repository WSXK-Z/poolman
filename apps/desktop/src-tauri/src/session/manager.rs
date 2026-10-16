//! 会话管理器：维护会话列表（可按 API Key 分平台查看），
//! 记录原始对话（OpenAI 格式）与转换后的实际对话（转换器输出）。

use anyhow::Result;
use sea_orm::entity::prelude::*;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DeleteResult, EntityTrait, QueryFilter,
    QueryOrder, Set,
};
use serde::Serialize;

use crate::entitys::{converted_message, raw_message, session};

/// 会话（列表用）
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub web_session_id: String,
    pub title: String,
    pub platform: String,
    pub api_key: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub raw_count: u64,
    pub converted_count: u64,
}

/// 消息记录
#[derive(Debug, Clone, Serialize)]
pub struct MessageRecord {
    pub role: String,
    pub content: String,
    pub created_at: i64,
}

/// 合并后的对话条目：每条消息同时携带客户端原始内容与转换器输出内容。
#[derive(Debug, Clone, Serialize)]
pub struct ConversationItem {
    /// 角色：system/user/assistant/tool
    pub role: String,
    /// 客户端发送的原始内容
    pub raw_content: String,
    /// 转换器输出的实际发给网页平台的内容
    pub converted_content: String,
    pub created_at: i64,
}

/// 会话详情（含原始/转换消息与合并对话视图）
#[derive(Debug, Clone, Serialize)]
pub struct SessionDetail {
    pub id: String,
    pub web_session_id: String,
    pub title: String,
    pub platform: String,
    pub api_key: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub raw_messages: Vec<MessageRecord>,
    pub converted_messages: Vec<MessageRecord>,
    /// 合并对话列表：按对话顺序排列，每条包含原始内容与转换后内容
    pub conversation: Vec<ConversationItem>,
}

pub struct SessionManager {
    db: DatabaseConnection,
}

impl SessionManager {
    /// 初始化连接并同步三张表（幂等）
    pub async fn new(db: DatabaseConnection) -> Result<Self> {
        db.get_schema_builder()
            .register(session::Entity)
            .register(raw_message::Entity)
            .register(converted_message::Entity)
            .sync(&db)
            .await?;
        Ok(Self { db })
    }

    /// 按网页平台会话 id 创建或更新会话，返回工具自身会话 id（时间序列 UUIDv7）
    /// 以网页会话 ID 作为幂等键；已存在时更新标题和活动时间。
    pub async fn upsert_session(
        &self,
        web_session_id: &str,
        title: &str,
        platform: &str,
        api_key: &str,
    ) -> Result<String> {
        let now = chrono::Utc::now().timestamp_millis();
        if let Some(existing) = session::Entity::find()
            .filter(session::Column::WebSessionId.eq(web_session_id))
            .one(&self.db)
            .await?
        {
            let mut am: session::ActiveModel = existing.clone().into();
            am.title = Set(title.to_string());
            am.platform = Set(platform.to_string());
            am.updated_at = Set(now);
            am.update(&self.db).await?;
            return Ok(existing.id);
        }
        // 新建：工具自身 id 用时间序列 UUIDv7
        let id = uuid::Uuid::now_v7().simple().to_string();
        session::ActiveModel {
            id: Set(id.clone()),
            web_session_id: Set(web_session_id.to_string()),
            title: Set(title.to_string()),
            platform: Set(platform.to_string()),
            api_key: Set(api_key.to_string()),
            // 新会话还没有调度记录
            plugin_id: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&self.db)
        .await?;
        Ok(id)
    }

    /// 记录会话最近使用的插件（调度粘性的落库副本）。
    /// 会话尚未落库时静默忽略：等适配器写会话时会自然补上。
    pub async fn set_session_plugin(
        &self,
        web_session_id: &str,
        plugin_id: &str,
    ) -> Result<()> {
        if let Some(existing) = session::Entity::find()
            .filter(session::Column::WebSessionId.eq(web_session_id))
            .one(&self.db)
            .await?
        {
            let mut am: session::ActiveModel = existing.into();
            am.plugin_id = Set(Some(plugin_id.to_string()));
            am.updated_at = Set(chrono::Utc::now().timestamp_millis());
            am.update(&self.db).await?;
        }
        Ok(())
    }

    /// 追加原始对话记录
    /// 保存客户端输入消息，内容保持 OpenAI 兼容 API 接收时的原始形式。
    pub async fn add_raw_message(&self, session_id: &str, role: &str, content: &str) -> Result<()> {
        raw_message::ActiveModel {
            id: Default::default(),
            session_id: Set(session_id.to_string()),
            role: Set(role.to_string()),
            content: Set(content.to_string()),
            created_at: Set(chrono::Utc::now().timestamp_millis()),
        }
        .insert(&self.db)
        .await?;
        Ok(())
    }

    /// 追加转换后的实际对话记录
    /// 保存供应商适配器最终生成并发送的提示词或对话片段。
    pub async fn add_converted_message(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
    ) -> Result<()> {
        converted_message::ActiveModel {
            id: Default::default(),
            session_id: Set(session_id.to_string()),
            role: Set(role.to_string()),
            content: Set(content.to_string()),
            created_at: Set(chrono::Utc::now().timestamp_millis()),
        }
        .insert(&self.db)
        .await?;
        Ok(())
    }

    /// 列出会话（可按 API Key 过滤）
    pub async fn list_sessions(&self, api_key: Option<&str>) -> Result<Vec<SessionInfo>> {
        let mut q = session::Entity::find();
        if let Some(k) = api_key {
            if !k.is_empty() {
                q = q.filter(session::Column::ApiKey.eq(k));
            }
        }
        let rows = q
            .order_by_desc(session::Column::CreatedAt)
            .all(&self.db)
            .await?;
        let mut out = Vec::new();
        for m in rows {
            let raw_count =
                raw_message::Entity::find()
                    .filter(raw_message::Column::SessionId.eq(&m.id))
                    .count(&self.db)
                    .await?;
            let converted_count =
                converted_message::Entity::find()
                    .filter(converted_message::Column::SessionId.eq(&m.id))
                    .count(&self.db)
                    .await?;
            out.push(SessionInfo {
                id: m.id.clone(),
                web_session_id: m.web_session_id,
                title: m.title,
                platform: m.platform,
                api_key: m.api_key,
                created_at: m.created_at,
                updated_at: m.updated_at,
                raw_count,
                converted_count,
            });
        }
        Ok(out)
    }

    /// 获取会话详情（含原始/转换消息与合并对话视图）
    pub async fn get_session(&self, id: &str) -> Result<Option<SessionDetail>> {
        if let Some(s) = session::Entity::find_by_id(id.to_string()).one(&self.db).await? {
            let raw = raw_message::Entity::find()
                .filter(raw_message::Column::SessionId.eq(&s.id))
                .order_by_asc(raw_message::Column::Id)
                .all(&self.db)
                .await?;
            let conv = converted_message::Entity::find()
                .filter(converted_message::Column::SessionId.eq(&s.id))
                .order_by_asc(converted_message::Column::Id)
                .all(&self.db)
                .await?;

            let raw_msgs: Vec<MessageRecord> = raw
                .iter()
                .map(|m| MessageRecord {
                    role: m.role.clone(),
                    content: m.content.clone(),
                    created_at: m.created_at,
                })
                .collect();
            let conv_msgs: Vec<MessageRecord> = conv
                .iter()
                .map(|m| MessageRecord {
                    role: m.role.clone(),
                    content: m.content.clone(),
                    created_at: m.created_at,
                })
                .collect();

            // 合并对话：把原始消息与转换后消息按记录顺序交错排列。
            // 每次请求会先批量写入该轮的全部原始消息，再写入一条转换后消息；
            // 因此以转换后消息作为轮次分界，把同轮的原始消息与该轮转换后内容配对。
            // 同一 (role, content) 在历史中重复出现时只保留首次出现（去重后即为完整对话流）。
            enum Entry {
                Raw(MessageRecord),
                Conv(MessageRecord),
            }
            let mut timeline: Vec<(i64, u8, Entry)> = Vec::new();
            for m in &raw_msgs {
                timeline.push((m.created_at, 0, Entry::Raw(m.clone())));
            }
            for m in &conv_msgs {
                timeline.push((m.created_at, 1, Entry::Conv(m.clone())));
            }
            timeline.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

            let mut conversation: Vec<ConversationItem> = Vec::new();
            let mut seen = std::collections::HashSet::new();
            let mut turn_raws: Vec<MessageRecord> = Vec::new();
            for (_, _, entry) in timeline {
                match entry {
                    Entry::Raw(m) => turn_raws.push(m),
                    Entry::Conv(m) => {
                        for r in &turn_raws {
                            let key = (r.role.clone(), r.content.clone());
                            if !seen.contains(&key) {
                                seen.insert(key);
                                conversation.push(ConversationItem {
                                    role: r.role.clone(),
                                    raw_content: r.content.clone(),
                                    converted_content: m.content.clone(),
                                    created_at: r.created_at,
                                });
                            }
                        }
                        turn_raws.clear();
                    }
                }
            }
            // 处理末尾未落盘转换后消息的原始消息
            for r in &turn_raws {
                let key = (r.role.clone(), r.content.clone());
                if !seen.contains(&key) {
                    seen.insert(key);
                    conversation.push(ConversationItem {
                        role: r.role.clone(),
                        raw_content: r.content.clone(),
                        converted_content: String::new(),
                        created_at: r.created_at,
                    });
                }
            }

            Ok(Some(SessionDetail {
                id: s.id.clone(),
                web_session_id: s.web_session_id,
                title: s.title,
                platform: s.platform,
                api_key: s.api_key,
                created_at: s.created_at,
                updated_at: s.updated_at,
                raw_messages: raw_msgs,
                converted_messages: conv_msgs,
                conversation,
            }))
        } else {
            Ok(None)
        }
    }

    /// 删除会话（含其原始/转换消息）
    pub async fn delete_session(&self, id: &str) -> Result<bool> {
        let sid = id.to_string();
        raw_message::Entity::delete_many()
            .filter(raw_message::Column::SessionId.eq(&sid))
            .exec(&self.db)
            .await?;
        converted_message::Entity::delete_many()
            .filter(converted_message::Column::SessionId.eq(&sid))
            .exec(&self.db)
            .await?;
        let res: DeleteResult = session::Entity::delete_by_id(sid).exec(&self.db).await?;
        Ok(res.rows_affected > 0)
    }
}
