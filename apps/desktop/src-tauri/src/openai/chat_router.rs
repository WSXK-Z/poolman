use std::collections::HashMap;
use std::sync::Arc;
use serde_json::Value;
use tracing::info;

use crate::extension::websocket::{WebSocketBridge, ChatRequestPayload, ChatResponsePayload};
use crate::extension::adapters::deepseek::DeepSeekService;

pub struct ChatRouter {
    bridge: Arc<WebSocketBridge>,
    deepseek: Option<Arc<DeepSeekService>>,
    timeout_ms: u64,
}

impl ChatRouter {
    pub fn new(bridge: Arc<WebSocketBridge>, deepseek: Option<Arc<DeepSeekService>>, timeout_ms: u64) -> Self {
        Self {
            bridge,
            deepseek,
            timeout_ms,
        }
    }

    pub fn map_model(model: &str) -> String {
        let mapping: HashMap<String, String> = [
            ("deepseek-chat", "deepseek-web"),
            ("deepseek-reasoner", "deepseek-web"),
            ("gpt-4", "openai"),
            ("gpt-3.5-turbo", "openai"),
            ("claude-3-opus", "claude"),
            ("claude-3-sonnet", "claude"),
            ("gemini-pro", "gemini"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

        mapping.get(model).map(|s| s.to_string()).unwrap_or_else(|| model.to_string())
    }

    pub async fn handle(
        &self,
        model: &str,
        messages: Vec<Value>,
        options: Option<Value>,
    ) -> anyhow::Result<ChatResponsePayload> {
        let provider = Self::map_model(model);
        let messages_to_use = if provider == "deepseek-web" {
            messages
        } else {
            messages.last().map(|m| vec![m.clone()]).unwrap_or_default()
        };

        info!("处理聊天请求: model={} provider={}", model, provider);

        if provider == "deepseek-web" {
            if let Some(deepseek) = &self.deepseek {
                return deepseek.chat(messages_to_use, options).await;
            }
        }

        let payload = ChatRequestPayload {
            model: provider.clone(),
            messages: messages_to_use,
            options,
        };

        self.bridge.send_request(payload, self.timeout_ms, None).await
    }

    pub async fn handle_stream<F>(
        &self,
        model: &str,
        messages: Vec<Value>,
        options: Option<Value>,
        mut on_chunk: F,
    ) -> anyhow::Result<ChatResponsePayload>
    where
        F: FnMut(String) + Send + Sync + 'static,
    {
        let provider = Self::map_model(model);
        let messages_to_use = if provider == "deepseek-web" {
            messages
        } else {
            messages.last().map(|m| vec![m.clone()]).unwrap_or_default()
        };

        info!("处理流式请求: model={} provider={}", model, provider);

        if provider == "deepseek-web" {
            if let Some(deepseek) = &self.deepseek {
                return deepseek.chat_stream(messages_to_use, options, move |delta| {
                    on_chunk(delta);
                }).await;
            }
        }

        let payload = ChatRequestPayload {
            model: provider,
            messages: messages_to_use,
            options,
        };

        use std::sync::Arc;
        use std::sync::Mutex as StdMutex;
        let _callback: Arc<StdMutex<Option<Box<dyn Fn(crate::extension::websocket::ChatChunkPayload) + Send + Sync>>>> = Arc::new(StdMutex::new(None));

        let result = self.bridge.send_request(
            payload,
            self.timeout_ms,
            Some(Box::new(move |chunk| {
                if let Some(delta) = chunk.delta {
                    on_chunk(delta);
                }
            })),
        ).await;

        result
    }
}
