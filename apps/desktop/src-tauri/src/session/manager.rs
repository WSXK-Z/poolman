//! 会话管理器：维护会话列表（可按 API Key 分平台查看），
//! 记录原始对话（OpenAI 格式）与转换后的实际对话（转换器输出）。

use anyhow::Result;
use sea_orm::entity::prelude::*;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DeleteResult, EntityTrait, QueryFilter,
    QueryOrder, Set,
};
use serde::Serialize;

use super::entity::{converted_message, raw_message, session};

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

/// 会话详情（含原始/转换消息）
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
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&self.db)
        .await?;
        Ok(id)
    }

    /// 追加原始对话记录
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

    /// 获取会话详情（含原始/转换消息）
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
            Ok(Some(SessionDetail {
                id: s.id.clone(),
                web_session_id: s.web_session_id,
                title: s.title,
                platform: s.platform,
                api_key: s.api_key,
                created_at: s.created_at,
                updated_at: s.updated_at,
                raw_messages: raw
                    .into_iter()
                    .map(|m| MessageRecord {
                        role: m.role,
                        content: m.content,
                        created_at: m.created_at,
                    })
                    .collect(),
                converted_messages: conv
                    .into_iter()
                    .map(|m| MessageRecord {
                        role: m.role,
                        content: m.content,
                        created_at: m.created_at,
                    })
                    .collect(),
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
