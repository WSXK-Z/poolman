use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::info;

use crate::extension::adapters::deepseek::DeepSeekService;
use crate::extension::provider_control::ProviderControl;
use crate::extension::websocket::{ChatRequestPayload, ChatResponsePayload, WebSocketBridge};

pub struct ChatRouter {
    bridge: Arc<WebSocketBridge>,
    deepseek: Option<Arc<DeepSeekService>>,
    timeout_ms: u64,
    provider_control: Arc<ProviderControl>,
}

impl ChatRouter {
    pub fn new(
        bridge: Arc<WebSocketBridge>,
        deepseek: Option<Arc<DeepSeekService>>,
        timeout_ms: u64,
        provider_control: Arc<ProviderControl>,
    ) -> Self {
        Self {
            bridge,
            deepseek,
            timeout_ms,
            provider_control,
        }
    }

    fn check_provider_enabled(&self, provider: &str) -> anyhow::Result<()> {
        let managed_id = Self::managed_provider_id(provider);
        if !self.provider_control.is_enabled(managed_id) {
            anyhow::bail!("供应商 {} 已在桌面端停用", managed_id);
        }

        Ok(())
    }

    pub fn is_provider_enabled(&self, provider: &str) -> bool {
        self.provider_control
            .is_enabled(Self::managed_provider_id(provider))
    }

    fn managed_provider_id(provider: &str) -> &str {
        match provider {
            "openai" | "chatgpt" | "chatgpt-web" => "chatgpt-web",
            "claude" | "claude-web" => "claude-web",
            "gemini" | "gemini-web" => "gemini-web",
            "kimi" | "kimi-web" => "kimi-web",
            "qwen" | "qwen-web" => "qwen-web",
            "deepseek" | "deepseek-web" => "deepseek-web",
            model if model.starts_with("gpt-") => "chatgpt-web",
            model if model.starts_with("claude-") => "claude-web",
            model if model.starts_with("gemini-") => "gemini-web",
            model if model.starts_with("kimi") || model.starts_with("moonshot-") => "kimi-web",
            model if model.starts_with("qwen-") => "qwen-web",
            model if model.starts_with("deepseek-") => "deepseek-web",
            other => other,
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

        mapping
            .get(model)
            .map(|s| s.to_string())
            .unwrap_or_else(|| model.to_string())
    }

    pub async fn handle(
        &self,
        model: &str,
        messages: Vec<Value>,
        options: Option<Value>,
    ) -> anyhow::Result<ChatResponsePayload> {
        let provider = Self::map_model(model);
        self.check_provider_enabled(&provider)?;
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

        self.bridge
            .send_request(payload, self.timeout_ms, None)
            .await
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
        self.check_provider_enabled(&provider)?;
        let messages_to_use = if provider == "deepseek-web" {
            messages
        } else {
            messages.last().map(|m| vec![m.clone()]).unwrap_or_default()
        };

        info!("处理流式请求: model={} provider={}", model, provider);

        if provider == "deepseek-web" {
            if let Some(deepseek) = &self.deepseek {
                return deepseek
                    .chat_stream(messages_to_use, options, move |delta| {
                        on_chunk(delta);
                    })
                    .await;
            }
        }

        let payload = ChatRequestPayload {
            model: provider,
            messages: messages_to_use,
            options,
        };

        use std::sync::Arc;
        use std::sync::Mutex as StdMutex;
        let _callback: Arc<
            StdMutex<
                Option<Box<dyn Fn(crate::extension::websocket::ChatChunkPayload) + Send + Sync>>,
            >,
        > = Arc::new(StdMutex::new(None));

        let result = self
            .bridge
            .send_request(
                payload,
                self.timeout_ms,
                Some(Box::new(move |chunk| {
                    if let Some(delta) = chunk.delta {
                        on_chunk(delta);
                    }
                })),
            )
            .await;

        result
    }
}

#[cfg(test)]
mod tests {
    use super::ChatRouter;

    #[test]
    fn model_names_resolve_to_managed_provider_ids() {
        assert_eq!(ChatRouter::managed_provider_id("gpt-4o"), "chatgpt-web");
        assert_eq!(
            ChatRouter::managed_provider_id("claude-3-7-sonnet"),
            "claude-web"
        );
        assert_eq!(
            ChatRouter::managed_provider_id("gemini-2.5-pro"),
            "gemini-web"
        );
        assert_eq!(
            ChatRouter::managed_provider_id("moonshot-v1-8k"),
            "kimi-web"
        );
        assert_eq!(ChatRouter::managed_provider_id("qwen-plus"), "qwen-web");
        assert_eq!(
            ChatRouter::managed_provider_id("deepseek-reasoner"),
            "deepseek-web"
        );
    }
}
