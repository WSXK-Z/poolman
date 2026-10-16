//! 网页渠道会话池：按并发设置预建若干网页会话，请求轮转独占，槽位持久化。
//!
//! 网页会话是有状态的（同一会话不能并发），而网页渠道本身允许不同会话并行，
//! 因此按渠道并发设置为每个渠道预建若干会话：
//! - 池容量 = 渠道并发设置，槽位序号从 0 起；
//! - 会话标题统一为 `poolman_<序号>`（序号从 1 起），便于在网页端辨认与回收；
//!   网页端在会话还没有内容时拒绝改名（`EMPTY_CHAT_SESSION`），所以命名推迟到该会话首轮对话成功之后；
//! - 槽位表持久化：再次启动先与网页会话列表核对，仍然存在的直接复用、并按标题认领
//!   丢失 id 的槽位，都不会重复创建；列表取不到（渠道未连接等）时保守保留已登记槽位。
//!
//! 请求是无状态的（每轮把整段对话拼进提示词），所以「哪个槽位对应哪段对话」不参与路由：
//! 每个在途请求独占一个槽位即可，取到哪个槽位不影响结果。
//!
//! 并发闸门保证同时在途的请求数不超过池容量，而槽位游标单调递增，
//! 因此同时在途的请求必然落在互不相同的槽位上（N 个连续整数对 N 取模互不相同）。

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use anyhow::{anyhow, Result};
use serde_json::Value;
use tracing::{info, warn};

use super::{is_biz_ok, DeepSeekWebModel, CHANNEL};
use crate::models::defs::PooledSession;
use crate::models::plugin::PluginModel;

/// 池内会话标题前缀：`poolman_<槽位序号>`（序号从 1 起）。
pub const POOL_TITLE_PREFIX: &str = "poolman_";

/// 池内会话在网页端的核对结果。
enum SessionCheck {
    /// 会话有效；`Option<i64>` 是服务端的当前消息 id（会话还没有消息时为 None）。
    Valid(Option<i64>),
    /// 网页端不认这个会话 id（已被删除 / 已过期）。
    Missing,
    /// 核对不到（插件不可用、网络失败等）：无法判断，按有效处理。
    Unknown,
}

/// 槽位序号对应的会话标题。
fn pool_title(slot: u32) -> String {
    format!("{POOL_TITLE_PREFIX}{}", slot + 1)
}

/// 会话列表响应 → 会话 id 与标题（id → title）。
fn listed_sessions(list: &Value) -> HashMap<String, String> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item.get("id").and_then(Value::as_str)?;
            let title = item.get("title").and_then(Value::as_str).unwrap_or("");
            Some((id.to_string(), title.to_string()))
        })
        .collect()
}

impl DeepSeekWebModel {
    /// 预热会话池：按并发设置预建会话（幂等，可与请求并发调用）。
    pub async fn prepare_session_pool(&self) -> Result<()> {
        self.ensure_pool().await
    }

    /// 取一个槽位的网页会话。
    ///
    /// 调用方必须先持有并发闸门：否则同时取槽位会撞到同一个会话。
    pub(super) async fn acquire_pool_slot(&self) -> Result<String> {
        self.ensure_pool().await?;
        let size = self.pool_size.max(1) as usize;
        let slot = self.slot_cursor.fetch_add(1, Ordering::Relaxed) % size;
        let pool = self.pool.lock().await;
        pool.get(slot)
            .cloned()
            .ok_or_else(|| anyhow!("会话池槽位 {} 尚未就绪", slot))
    }

    /// 确保会话池已与渠道对齐：读已登记槽位 → 与网页会话列表核对 → 补建缺失 → 落库。
    ///
    /// 只有全流程成功才置「已对齐」，中途失败时下一次请求会重试。
    async fn ensure_pool(&self) -> Result<()> {
        if self.pool_ready.load(Ordering::Acquire) {
            return Ok(());
        }
        let _guard = self.pool_lock.lock().await;
        if self.pool_ready.load(Ordering::Acquire) {
            return Ok(());
        }
        let size = self.pool_size.max(1) as usize;
        let mut slots = self.load_pool(size).await;
        self.adopt_listed_sessions(&mut slots).await;
        let mut created = Vec::new();
        for slot in 0..size {
            if slots[slot].is_none() {
                let session_id = self.create_pool_session(slot as u32).await?;
                slots[slot] = Some(session_id.clone());
                created.push(session_id);
            }
        }
        let ready: Vec<String> = slots.iter().flatten().cloned().collect();
        *self.pool.lock().await = ready;
        self.save_current_pool().await?;
        self.pool_ready.store(true, Ordering::Release);
        if !created.is_empty() {
            info!(
                "会话池已就绪：渠道={} 容量={} 新建会话 {} 个",
                CHANNEL,
                size,
                created.len()
            );
        }
        Ok(())
    }

    /// 读取已登记的槽位：存储不可用或未注入存储时全部为空（等于全量新建）。
    async fn load_pool(&self, size: usize) -> Vec<Option<String>> {
        let mut slots = vec![None; size];
        let Some(store) = &self.pool_store else {
            return slots;
        };
        match store.load(CHANNEL).await {
            Ok(saved) => {
                for item in saved {
                    if let Some(slot) = slots.get_mut(item.slot as usize) {
                        *slot = Some(item.session_id);
                    }
                }
            }
            Err(error) => warn!("读取会话池失败（按未登记处理）: {}", error),
        }
        slots
    }

    /// 与网页会话列表核对已登记槽位：
    /// - 列表里仍然存在的会话保留（重启后直接复用，不重复创建）；
    /// - 已消失的槽位清空后重取，避免继续用一个已删除的空会话；
    /// - 空槽位按标题 `poolman_<序号>` 认领网页端已有的会话（列表分页看不到旧 id 时的兜底）。
    ///
    /// 顺带记下标题已经是池内命名的会话：它们不必再改名。
    async fn adopt_listed_sessions(&self, slots: &mut [Option<String>]) {
        let listed = match self.list_channel_sessions().await {
            Ok(list) => listed_sessions(&list),
            Err(error) => {
                warn!("核对会话池失败（保留已登记槽位）: {}", error);
                return;
            }
        };
        let mut titled = Vec::new();
        for (slot, current) in slots.iter_mut().enumerate() {
            let title = pool_title(slot as u32);
            match current {
                // 会话还在：标题已是池内命名则无需再改
                Some(id) if listed.get(id) == Some(&title) => titled.push(id.clone()),
                Some(id) if listed.contains_key(id) => {}
                Some(_) => *current = None,
                None => {}
            }
            if current.is_none() {
                if let Some((id, _)) = listed.iter().find(|(_, name)| **name == title) {
                    *current = Some(id.clone());
                    titled.push(id.clone());
                }
            }
        }
        self.pool_titled.lock().unwrap().extend(titled);
    }

    /// 新建一个池内会话。
    ///
    /// 这里不设标题：网页端在会话还没有内容时拒绝改名（`EMPTY_CHAT_SESSION`），
    /// 命名由该会话首轮对话成功后的 [`Self::ensure_pool_title`] 完成。
    async fn create_pool_session(&self, slot: u32) -> Result<String> {
        let session_id = self
            .create_channel_session()
            .await?
            .ok_or_else(|| anyhow!("创建会话池会话失败（槽位 {}）", slot))?;
        info!("会话池新建会话 {}（槽位 {}）", session_id, slot);
        Ok(session_id)
    }

    /// 给池内会话改名为 `poolman_<槽位序号>`，每个会话只做一次；不在池内的会话（外部会话）不动。
    ///
    /// 网页端在会话还没有内容时拒绝改名（`EMPTY_CHAT_SESSION`），所以在本会话首轮对话成功后才调用；
    /// 改名没成功就不记「已命名」，下一轮对话成功后再试。
    pub(super) async fn ensure_pool_title(&self, session_id: &str) {
        if self.pool_titled.lock().unwrap().contains(session_id) {
            return;
        }
        let Some(slot) = self.pool_slot_of(session_id).await else {
            return;
        };
        let title = pool_title(slot as u32);
        match self.rename_channel_session(session_id, &title).await {
            Ok(true) => {
                info!("会话池已将会话 {} 标题设为 {}", session_id, title);
                self.pool_titled
                    .lock()
                    .unwrap()
                    .insert(session_id.to_string());
            }
            Ok(false) => warn!("会话 {} 标题设置未生效（期望 {}）", session_id, title),
            Err(error) => warn!("会话 {} 标题设置失败: {}", session_id, error),
        }
    }

    /// 会话在池中的槽位序号（不在池内时为 None）。
    async fn pool_slot_of(&self, session_id: &str) -> Option<usize> {
        self.pool
            .lock()
            .await
            .iter()
            .position(|id| id == session_id)
    }

    /// 用之前确认池内会话在网页端还有效：失效就补建一个顶上，返回本次实际使用的会话。
    ///
    /// 启动时插件可能还没连上（会话列表取不到），那时的已登记槽位只能保守保留——它们可能
    /// 早已在网页端被删除，不核对就会一直拿到「invalid chat session id」的空回答。
    pub(super) async fn ensure_pool_session(&self, session_id: String) -> Result<String> {
        // 已经拿到过消息 id 的会话不再核对：既省一次请求，也避免每轮都查历史
        if self.session_map.lock().unwrap().contains_key(&session_id) {
            return Ok(session_id);
        }
        match self.check_pool_session(&session_id).await {
            SessionCheck::Valid(message_id) => {
                self.remember_message_id(&session_id, message_id);
                Ok(session_id)
            }
            // 核对不了（插件不可用等）：按有效处理，别把好会话换掉
            SessionCheck::Unknown => Ok(session_id),
            SessionCheck::Missing => {
                let replacement = self.replace_pool_slot(&session_id).await?;
                info!(
                    "会话池槽位会话已失效，补建新会话 {}（原 {}）",
                    replacement, session_id
                );
                Ok(replacement)
            }
        }
    }

    /// 核对一个池内会话：读会话历史，顺带拿到服务端的当前消息 id。
    async fn check_pool_session(&self, session_id: &str) -> SessionCheck {
        let Ok(json) = self.channel_history(session_id).await else {
            return SessionCheck::Unknown;
        };
        if !is_biz_ok(&json) {
            // 网页端明确表示不认这个会话 id：槽位上的会话已经没了
            let message = json
                .get("data")
                .and_then(|data| data.get("biz_msg"))
                .and_then(Value::as_str)
                .unwrap_or("");
            return if message.contains("invalid chat session id") {
                SessionCheck::Missing
            } else {
                SessionCheck::Unknown
            };
        }
        let message_id = json
            .get("data")
            .and_then(|data| data.get("biz_data"))
            .and_then(|biz| biz.get("chat_session"))
            .and_then(|session| session.get("current_message_id"))
            .and_then(Value::as_i64);
        SessionCheck::Valid(message_id)
    }

    /// 给失效的槽位补建会话：新会话写回池并落库，旧会话的消息 id 与「已命名」记录一并清掉。
    async fn replace_pool_slot(&self, session_id: &str) -> Result<String> {
        let slot = self
            .pool_slot_of(session_id)
            .await
            .ok_or_else(|| anyhow!("会话 {} 不在会话池内", session_id))?;
        let created = self.create_pool_session(slot as u32).await?;
        {
            let mut pool = self.pool.lock().await;
            if let Some(entry) = pool.get_mut(slot) {
                *entry = created.clone();
            }
        }
        self.pool_titled.lock().unwrap().remove(session_id);
        self.session_map.lock().unwrap().remove(session_id);
        self.save_current_pool().await?;
        Ok(created)
    }

    /// 网页端说这个会话到长度上限了：删掉它，按同一个标题补建一个新会话顶上。
    ///
    /// 池内会话是长期复用的，累积的历史会把网页上下文顶到上限；换个新会话等于清空那段历史，
    /// 后续请求才有地方落。新会话沿用原槽位标题（`poolman_<序号>`），方便在网页端辨认和回收。
    /// 不在池内的会话（外部会话）不动，返回 None。
    pub(super) async fn recycle_pool_session(&self, session_id: &str) -> Result<Option<String>> {
        let Some(slot) = self.pool_slot_of(session_id).await else {
            return Ok(None);
        };
        let title = pool_title(slot as u32);
        match self.delete_channel_session(session_id).await {
            Ok(true) => info!("会话达到长度上限，已删除网页会话 {}", session_id),
            Ok(false) => warn!("会话达到长度上限，删除网页会话 {} 未生效", session_id),
            Err(error) => warn!("会话达到长度上限，删除网页会话 {} 失败: {}", session_id, error),
        }
        let created = self.create_pool_session(slot as u32).await?;
        // 网页端在会话还没有内容时拒绝改名（EMPTY_CHAT_SESSION）：改不动就留给首轮对话成功后命名
        match self.rename_channel_session(&created, &title).await {
            Ok(true) => {
                self.pool_titled.lock().unwrap().insert(created.clone());
            }
            Ok(false) => warn!("新会话 {} 标题设置未生效（期望 {}）", created, title),
            Err(error) => warn!("新会话 {} 标题设置失败: {}", created, error),
        }
        {
            let mut pool = self.pool.lock().await;
            if let Some(entry) = pool.get_mut(slot) {
                *entry = created.clone();
            }
        }
        self.pool_titled.lock().unwrap().remove(session_id);
        self.session_map.lock().unwrap().remove(session_id);
        self.save_current_pool().await?;
        info!("会话池槽位已换新会话 {}（原 {}）", created, session_id);
        Ok(Some(created))
    }

    /// 当前池的槽位表落库（未注入存储时跳过）：下标即槽位序号。
    async fn save_current_pool(&self) -> Result<()> {
        let Some(store) = &self.pool_store else {
            return Ok(());
        };
        let pool = self.pool.lock().await.clone();
        let sessions: Vec<PooledSession> = pool
            .iter()
            .enumerate()
            .map(|(slot, session_id)| PooledSession {
                slot: slot as u32,
                session_id: session_id.clone(),
            })
            .collect();
        store.save(CHANNEL, &sessions).await
    }
}
