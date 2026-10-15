// 渠道id
pub const PROVIDER_ID: &str = "deepseek-web";
// DeepSeek 相关接口地址
pub const DEEPSEEK_API_URL: &str = "https://chat.deepseek.com/api/v0/chat/completion";
// DeepSeek 会话接口地址
pub const DEEPSEEK_SESSION_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/create";
// DeepSeek 历史消息接口地址
pub const DEEPSEEK_HISTORY_URL: &str = "https://chat.deepseek.com/api/v0/chat/history_messages";
// DeepSeek 会话标题接口地址
pub const DEEPSEEK_TITLE_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/update_title";
// DeepSeek 会话列表接口地址
pub const DEEPSEEK_SESSION_LIST_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/fetch_page";
// DeepSeek 会话删除接口地址
pub const DEEPSEEK_SESSION_DELETE_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/delete";
// DeepSeek POW 挑战接口地址
pub const POW_CHALLENGE_URL: &str = "https://chat.deepseek.com/api/v0/chat/create_pow_challenge";
// DeepSeek 模型类型
pub const DEEPSEEK_MODEL_TYPE: &str = "default";
// DeepSeek 思考模式
pub const DEEPSEEK_THINK: bool = true;
// 最大尝试次数
pub const MAX_ATTEMPTS: usize = 6;
// 辅助超时时间
pub const AUX_TIMEOUT: u64 = 10_000;