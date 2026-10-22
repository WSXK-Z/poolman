use sea_orm::entity::prelude::*;

/// 本地请求记录表：`request_records`。
///
/// 每一次经过网关的对话请求一条记录：请求侧保存协议、请求头、API Key、日期、耗时、
/// 系统提示词、用户提问、可用工具与历史对话；回答侧保存回答类型（正文 / 工具调用）、
/// 正文、思考内容与工具调用原文。界面按字段展开查看。
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "request_records")]
pub struct Model {
    /// 请求 id（随机生成，对外展示与定位用）。
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// 请求的对外模型名。
    pub model: String,
    /// 是否流式请求。
    pub stream: bool,
    /// 处理状态：success / error。
    pub status: String,
    /// 协议类型（openai / deepseek 等）。
    pub protocol: Option<String>,
    /// HTTP 方法（当前均为 POST）。
    pub method: Option<String>,
    /// 请求路径（如 /v1/chat/completions）。
    pub endpoint: Option<String>,
    /// 请求头（小写名称 → 值，JSON 对象原文）。
    pub headers: Option<String>,
    /// 调用方使用的 API Key。
    pub api_key: Option<String>,
    /// 本次请求耗时（毫秒）。
    pub duration_ms: Option<i64>,
    /// 全部 system 消息拼接后的系统提示词。
    pub system_prompt: String,
    /// 最新一条 user 消息（用户提问）。
    pub user_prompt: String,
    /// 可用工具定义的 JSON 原文（没有工具时为空串）。
    pub tools: String,
    /// 历史对话消息数组的 JSON 原文（已剔除 system 与本次用户提问）。
    pub history: String,
    /// 完整非 system 消息数组的 JSON 原文（保留原始顺序，供会话内容表还原）。
    pub messages: Option<String>,
    /// 本地会话 id（会话管理同步后写入；未同步时为 NULL）。
    pub local_session_id: Option<String>,
    /// 会话被删除后把关联请求标记为忽略，全量同步时不再重新建会话。
    pub local_session_ignored: Option<bool>,
    /// 是否为客户端自动发起的「生成会话标题」请求（不参与本地会话归并）。
    pub is_title_request: Option<bool>,
    /// 回答分类：text（正文）/ tool_call（工具调用）/ none（无回答）。
    pub answer_type: String,
    /// 回答正文。
    pub answer_content: String,
    /// 思考内容（网页版推理过程）。
    pub answer_reasoning: String,
    /// 工具调用的 JSON 原文（没有工具调用时为空串）。
    pub answer_tool_calls: String,
    /// 失败原因（status = error 时）。
    pub error: String,
    /// 列表预览串（落库时生成，列表查询只读这些短字段，避免拉取完整原文）。
    pub system_prompt_preview: Option<String>,
    pub user_prompt_preview: Option<String>,
    pub tools_preview: Option<String>,
    pub history_preview: Option<String>,
    pub answer_preview: Option<String>,
    pub headers_preview: Option<String>,
    /// 创建时间（毫秒时间戳）。
    pub created_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
