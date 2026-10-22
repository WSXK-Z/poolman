use sea_orm::entity::prelude::*;

/// 本地会话内容表：`local_session_contents`。
///
/// 一个会话内按 `id` 顺序保存一份完整内容：
/// 系统提示词、工具表、用户提问、思考、正文、工具调用、工具调用结果。
/// 系统提示词与工具表每个会话只写一次；后续请求只追加新解析出的内容。
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "local_session_contents")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    /// 所属本地会话 id。
    pub session_id: String,
    /// 内容类型：system_prompt / tools / user / reasoning / assistant_text /
    /// assistant_tool_calls / tool_result。
    pub kind: String,
    /// 内容原文；工具表、工具调用、工具结果为 JSON。
    pub content: String,
    /// 产生该内容的请求 id（系统提示词与工具表取会话首个请求）。
    pub request_id: Option<String>,
    /// 工具名（工具结果行）。
    pub tool_name: Option<String>,
    /// 工具调用 id（工具结果行）。
    pub tool_call_id: Option<String>,
    /// token 数。
    pub token_count: i64,
    /// 该内容产生时间（毫秒时间戳）。
    pub created_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
