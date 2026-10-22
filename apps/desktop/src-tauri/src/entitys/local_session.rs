use sea_orm::entity::prelude::*;

/// 本地会话表：`local_sessions`。
///
/// 由会话管理同步生成：同一系统提示词 + 第一条用户提问得到基础会话 id；
/// 历史对话出现分叉时基于差异生成分支 id。`conversation` 保存去掉思考内容后的
/// 规范化对话（含已产生的助手回答），供后续请求做前缀比对。
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "local_sessions")]
pub struct Model {
    /// 会话 id（`sess_` + 确定性哈希或冲突标记哈希）。
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// 基础会话 id（仅由系统提示词 + 第一条用户提问哈希得到）。
    pub base_id: String,
    /// 生成该会话时使用的系统提示词。
    pub system_prompt: String,
    /// 第一条用户真实提问。
    pub first_user_prompt: String,
    /// 规范化后的完整对话（已剔除思考内容，含助手回答）。
    pub conversation: String,
    /// 无历史的新对话发生 id 冲突时写入的随机标记。
    pub collision_marker: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
