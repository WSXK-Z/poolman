//! 官方 DeepSeek 分词器：按 `deepseek_v4_tokenizer/tokenizer.json` 精确统计 token。
//!
//! 网页渠道不会回传「本轮」用量（`accumulated_token_usage` 是整段会话的累计值），
//! 因此 `/v1/chat/completions` 与 `/v1/responses` 的 usage 只能由网关自己算。
//! 早先按「中文每字 1、英文每 4 字符 1」粗估，与真实分词结果偏差很大；
//! 这里直接加载官方 BPE 词表，口径与上游一致。
//!
//! `tokenizer.json` 在编译期内嵌（约 6 MB），运行时不依赖外部文件；
//! 词表只在首次使用时解析一次，之后复用。

use std::sync::OnceLock;

use tokenizers::Tokenizer;

/// 官方分词器定义：编译期嵌入，避免运行时找不到文件。
static TOKENIZER_JSON: &[u8] =
    include_bytes!("../../../../deepseek_v4_tokenizer/tokenizer.json");

/// 进程内共享的分词器实例。
static TOKENIZER: OnceLock<Option<Tokenizer>> = OnceLock::new();

/// 取分词器；解析失败时返回 `None`（调用方回落到字符估算，不让计数问题拖垮请求）。
fn tokenizer() -> Option<&'static Tokenizer> {
    TOKENIZER
        .get_or_init(|| match Tokenizer::from_bytes(TOKENIZER_JSON) {
            Ok(tokenizer) => Some(tokenizer),
            Err(error) => {
                tracing::error!("加载官方分词器失败，token 统计将回落到字符估算：{}", error);
                None
            }
        })
        .as_ref()
}

/// 统计一段文本的 token 数（不计 BOS/EOS 等特殊 token）。
///
/// 与 OpenAI 的 usage 口径一致：只数内容本身。分词器不可用时按
/// 「非 ASCII 每字 1、ASCII 每 4 字符 1」估算，保证始终有值。
pub fn count(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    match tokenizer() {
        Some(tokenizer) => tokenizer
            .encode(text, false)
            .map(|encoding| encoding.len() as u32)
            .unwrap_or_else(|error| {
                tracing::warn!("分词失败，回落到字符估算：{}", error);
                estimate(text)
            }),
        None => estimate(text),
    }
}

/// 多条消息拼接后的 token 数：按消息边界累加，并计入每条消息的固定开销。
///
/// 聊天请求的 prompt 不是简单拼接：上游会插入角色标记与分隔符。这里给每条消息
/// 加一个保守的固定开销，使估算贴近真实 prompt，而不是只数正文。
pub fn count_messages<'a>(messages: impl IntoIterator<Item = &'a str>) -> u32 {
    /// 每条消息的角色标记与分隔符开销（与官方 chat_template 的量级相当）。
    const PER_MESSAGE_OVERHEAD: u32 = 4;
    messages
        .into_iter()
        .map(|text| count(text) + PER_MESSAGE_OVERHEAD)
        .sum()
}

/// 字符估算兜底：非 ASCII 字符计 1，ASCII 字符 4 个计 1。
fn estimate(text: &str) -> u32 {
    let mut wide = 0usize;
    let mut narrow = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            narrow += 1;
        } else {
            wide += 1;
        }
    }
    (wide + narrow.div_ceil(4)) as u32
}

#[cfg(test)]
mod tests {
    use super::{count, count_messages, estimate};

    /// 中文按字近似：一个汉字通常就是一个 token。
    #[test]
    fn counts_chinese_close_to_one_per_char() {
        let text = "你好世界";
        let tokens = count(text);
        assert_eq!(tokens, 4, "四个汉字应约为四个 token，实际 {tokens}");
    }

    /// 英文单词不会被拆成一个字母一个 token。
    #[test]
    fn counts_english_words_compactly() {
        let tokens = count("hello world");
        assert!(tokens <= 3, "两个常见单词应不超过 3 个 token，实际 {tokens}");
        assert!(tokens >= 2, "至少两个 token，实际 {tokens}");
    }

    /// 空文本没有 token，也不会触发分词器初始化。
    #[test]
    fn empty_text_counts_zero() {
        assert_eq!(count(""), 0);
    }

    /// 长文本的 token 数随内容增长，且明显少于字符数。
    #[test]
    fn longer_text_yields_more_tokens() {
        let short = count("hello");
        let long = count("hello world, this is a longer sentence for counting");
        assert!(long > short, "更长的文本应有更多 token");
        assert!(long < 60, "不应退化成一个字符一个 token，实际 {long}");
    }

    /// 多消息按条累加并计入每条消息的固定开销。
    #[test]
    fn messages_add_per_message_overhead() {
        let single = count_messages(["你好"]);
        let doubled = count_messages(["你好", "你好"]);
        assert_eq!(doubled, single * 2, "两条相同消息应是单条的两倍");
        assert!(single > count("你好"), "应含消息开销");
    }

    /// 兜底估算的口径：中文每字 1，英文每 4 字符 1。
    #[test]
    fn fallback_estimate_keeps_ratio() {
        assert_eq!(estimate("你好"), 2);
        assert_eq!(estimate("abcd"), 1);
        assert_eq!(estimate("abcde"), 2);
        assert_eq!(estimate(""), 0);
    }
}