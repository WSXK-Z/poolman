use anyhow::Result;


pub enum TextRecognizerResult{
    Finded(usize,usize),
    NotFinded,
    Maybe(usize,usize),
}

/// 从 target 中识别 text：整段范围内找，不要求从头开始。
///
/// - `Finded(start, end)`：text 完整出现在 target 的 `[start, end)`；
/// - `Maybe(start, end)`：target 的结尾只对上了 text 的开头（内容还没收全），
///   `[start, end)` 是已经对上的那一段；
/// - `NotFinded`：任何位置都找不到 text 的开头。
pub fn text_recognizer(text: &str, target: &str) -> Result<TextRecognizerResult> {

    if let Some(start) = target.find(text) {
        return Ok(TextRecognizerResult::Finded(start, start + text.len()));
    }
    // 没有完整命中：比 text 长的后缀不可能是 text 的前缀，只需看结尾这一段里有没有对上开头
    let from = target.len().saturating_sub(text.len());
    for (start, _) in target.char_indices() {
        if start < from {
            continue;
        }
        let suffix = &target[start..];
        if text.starts_with(suffix) {
            return Ok(TextRecognizerResult::Maybe(start, start + suffix.len()));
        }
    }
    Ok(TextRecognizerResult::NotFinded)
}