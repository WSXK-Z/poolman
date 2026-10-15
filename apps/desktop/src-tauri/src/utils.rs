use anyhow::Result;


pub enum TextRecognizerResult{
    Finded(usize,usize),
    NotFinded,
    Maybe(usize,usize),
}

/// 从target中识别text.返回识别结果
pub fn text_recognizer(text:&str,target:&str)->Result<TextRecognizerResult>{
        if target.starts_with(text) {
            return Ok(TextRecognizerResult::Finded (0, text.len()));
        }
        if text.starts_with(target) {
            return Ok(TextRecognizerResult::Maybe(0, text.len()));
        }
        Ok(TextRecognizerResult::NotFinded)
}