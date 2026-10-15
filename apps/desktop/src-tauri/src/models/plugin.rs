//! 插件模型抽象。
//!
//! 插件供应商提供的模型统一实现本 trait：在 [`Model`] 之上补上「渠道」身份与
//! **渠道会话管理**（创建 / 列表 / 删除 / 改标题 / 历史）。会话管理由各模型
//! **自行实现**，不注入外部服务。
//!
//! 具体由哪台插件实例执行由插件管理器与桥接调度决定，模型不感知实例。

use anyhow::Result;
use futures_util::future::BoxFuture;
use serde_json::Value;

use super::model::Model;

/// 插件模型：一个模型对应一个 web 渠道，并自行完成渠道会话管理。
pub trait PluginModel: Model {
    /// 渠道标识（web 平台 id，如 `deepseek-web`）。
    fn channel(&self) -> &str;

    /// 在渠道创建会话，返回渠道会话 id。
    fn create_channel_session(&self) -> BoxFuture<'_, Result<Option<String>>>;

    /// 列出渠道会话。
    fn list_channel_sessions(&self) -> BoxFuture<'_, Result<Value>>;

    /// 删除渠道会话。
    fn delete_channel_session<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<bool>>;

    /// 修改渠道会话标题。
    fn rename_channel_session<'a>(
        &'a self,
        session_id: &'a str,
        title: &'a str,
    ) -> BoxFuture<'a, Result<bool>>;

    /// 拉取渠道会话历史。
    fn channel_history<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<Value>>;
}
