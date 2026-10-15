use crate::models::deepseek_web::sse_frame_parser::SseFrameParser;


pub enum ChatStreamEvent {
    /// 新增的（片段类型, 内容）
    NewContent(String),
    /// 新增思考内容
    NewThink(String),
    /// 新增工具调用
    NewToolCalls(serde_json::Value),
    /// SSE 流结束
    End,
}



/// 处理 deepseek-web 的 SSE 流式增量，解析为对应的新增内容
pub struct ChatStreamProcessor {
    sse_parser: SseFrameParser,
    raw_buffer: String,
    cache_buffer: String,
    line_buffer: Vec<String>,
}

impl ChatStreamProcessor {
    pub fn new() -> Self {
        Self {
            sse_parser: SseFrameParser::new(),
            raw_buffer: String::new(),
            cache_buffer: String::new(),
            line_buffer: Vec::new(),
        }
    }
    /// 处理一段 SSE 流式增量，返回本次新增的事件。
    pub fn push(&mut self, raw: &str) {
        self.raw_buffer.push_str(raw);
        self.cache_buffer.push_str(raw);
        while let Some(pos)=self.cache_buffer.find('\n'){
            let line = self.cache_buffer[..pos].trim_end_matches('\r').to_string();
            self.cache_buffer.drain(..=pos);
            self.line_buffer.push(line);
            
        }

    }
    /// 消费一行 SSE 流式增量，返回本次新增的事件。
    fn consume_line(&mut self) -> Option<String> {
        if self.line_buffer.is_empty() {
            return None;
        }
        Some(self.line_buffer.remove(0))
    }
}
