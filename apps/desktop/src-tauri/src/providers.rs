//! 供应商（Provider）抽象与注册表。
//!
//! 网关不直接认识任何模型来源，只通过统一的「chat_completions 形状」接口与供应商对话：
//! 请求带上对外模型名，注册表找到声明了该模型的供应商并转交。
//!
//! 模块结构（本文件是模块入口，子模块各自一个文件）：
//! - `defs`：模块内公共定义（统一接口 [`ChatProvider`]、[`ModelDescriptor`]）。
//! - `plugin`：插件供应商 [`PluginProvider`]——把浏览器插件连接封装成一个供应商，
//!   内部按渠道持有 [`crate::models::PluginModel`]。具体由哪台插件实例执行
//!   由插件管理器负责，供应商与模型都不感知实例。
//! - `entity` / `manager`：供应商与模型的持久化（sea-orm + SQLite）与 [`ProviderManager`]。
//! - `seed`：启动装配——把插件供应商与其网页模型幂等写入持久化表。
//!
//! 以后接入官方 API 等其他类型的模型，只需在本文件旁新增一个实现 [`ChatProvider`] 的文件，
//! 在下面声明并注册即可，不必改动网关或 HTTP 层。
//!
//! 「按模型分派」的编排入口见 crate 根 [`crate::chat_router::ChatRouter`]。

pub mod defs;
pub mod manager;
pub mod plugin;
pub mod provider_control;
pub mod seed;

pub use defs::{ChatProvider, ModelDescriptor};
pub use manager::ProviderManager;
pub use plugin::{platform_for, PluginProvider};

/// 供应商与模型共用的请求 / 结果类型（契约在模型模块定义）。
pub use crate::models::{InferRequest, ModelResponse};

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use futures_util::future::BoxFuture;
use tracing::{info, warn};

/// 供应商注册表：按模型建立索引，提供列模型与按模型分派的统一入口。
pub struct ProviderRegistry {
    providers: Vec<Arc<dyn ChatProvider>>,
    /// 对外模型名 → `providers` 下标（重复声明以先注册者为准）。
    model_index: HashMap<String, usize>,
}

impl ProviderRegistry {
    pub fn new(providers: Vec<Arc<dyn ChatProvider>>) -> Self {
        let mut model_index: HashMap<String, usize> = HashMap::new();
        for (index, provider) in providers.iter().enumerate() {
            for model in provider.models() {
                match model_index.get(&model.id) {
                    Some(first) => warn!(
                        "模型 {} 被多个供应商声明，保留先注册的供应商下标 {}（忽略 {}）",
                        model.id, first, index
                    ),
                    None => {
                        model_index.insert(model.id, index);
                    }
                }
            }
        }
        Self {
            providers,
            model_index,
        }
    }

    /// 列出所有供应商提供的模型。
    pub fn list_models(&self) -> Vec<ModelDescriptor> {
        self.providers
            .iter()
            .flat_map(|provider| provider.models())
            .collect()
    }

    /// 找到声明了该模型的供应商。
    fn provider_for(&self, model: &str) -> Result<Arc<dyn ChatProvider>> {
        let index = *self.model_index.get(model).ok_or_else(|| {
            anyhow::anyhow!("未知模型 {}：没有任何已注册供应商提供它", model)
        })?;
        Ok(self.providers[index].clone())
    }

    /// 声明了该模型的供应商 id（未注册时为 None）。
    pub fn provider_id_for(&self, model: &str) -> Option<String> {
        self.model_index
            .get(model)
            .map(|index| self.providers[*index].id().to_string())
    }

    /// 按模型分派一次非流式对话。
    pub fn chat_completions<'a>(
        &'a self,
        request: InferRequest,
    ) -> BoxFuture<'a, Result<ModelResponse>> {
        Box::pin(async move {
            let provider = self.provider_for(&request.model)?;
            info!("供应商 {} 处理模型 {}", provider.id(), request.model);
            provider.chat_completions(request).await
        })
    }

    /// 按模型分派一次流式对话。
    pub fn chat_completions_stream<'a>(
        &'a self,
        request: InferRequest,
        on_chunk: Box<dyn FnMut(String) + Send + Sync>,
    ) -> BoxFuture<'a, Result<ModelResponse>> {
        Box::pin(async move {
            let provider = self.provider_for(&request.model)?;
            info!("供应商 {} 处理流式模型 {}", provider.id(), request.model);
            provider.chat_completions_stream(request, on_chunk).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ChatProvider, InferRequest, ModelDescriptor, ModelResponse, ProviderRegistry};
    use futures_util::future::BoxFuture;
    use std::sync::Arc;

    /// 一个最小供应商：声明一个模型，回显模型名。
    struct MockProvider;

    impl ChatProvider for MockProvider {
        fn id(&self) -> &str {
            "mock"
        }

        fn models(&self) -> Vec<ModelDescriptor> {
            vec![ModelDescriptor {
                id: "mock-model".to_string(),
                provider: "mock".to_string(),
                owned_by: "mock".to_string(),
            }]
        }

        fn chat_completions<'a>(
            &'a self,
            request: InferRequest,
        ) -> BoxFuture<'a, anyhow::Result<ModelResponse>> {
            Box::pin(async move {
                Ok(ModelResponse {
                    text: format!("echo:{}", request.model),
                    reasoning: None,
                    tool_calls: None,
                    usage: None,
                    finish_reason: Some("stop".to_string()),
                    is_complete: true,
                })
            })
        }

        fn chat_completions_stream<'a>(
            &'a self,
            _request: InferRequest,
            mut on_chunk: Box<dyn FnMut(String) + Send + Sync>,
        ) -> BoxFuture<'a, anyhow::Result<ModelResponse>> {
            Box::pin(async move {
                on_chunk("chunk".to_string());
                Ok(ModelResponse {
                    text: "chunk".to_string(),
                    reasoning: None,
                    tool_calls: None,
                    usage: None,
                    finish_reason: None,
                    is_complete: true,
                })
            })
        }
    }

    /// 展示名默认与 id 相同。
    #[test]
    fn display_name_defaults_to_id() {
        assert_eq!(MockProvider.display_name(), "mock");
    }

    /// 注册表按模型找到声明的供应商并转交；未知模型报错。
    #[tokio::test]
    async fn registry_dispatches_by_model() {
        let registry = ProviderRegistry::new(vec![Arc::new(MockProvider)]);

        assert_eq!(registry.list_models().len(), 1);
        assert_eq!(registry.list_models()[0].id, "mock-model");

        let response = registry
            .chat_completions(InferRequest::new("mock-model", vec![]))
            .await
            .expect("已声明的模型应可派发");
        assert_eq!(response.text, "echo:mock-model");

        let error = registry
            .chat_completions(InferRequest::new("unknown", vec![]))
            .await
            .expect_err("未知模型应报错");
        assert!(error.to_string().contains("未知模型"));
    }

    /// 流式分派同样按模型走。
    #[tokio::test]
    async fn registry_dispatches_stream_by_model() {
        let registry = ProviderRegistry::new(vec![Arc::new(MockProvider)]);
        let chunks = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = chunks.clone();
        let response = registry
            .chat_completions_stream(
                InferRequest::new("mock-model", vec![]),
                Box::new(move |delta| sink.lock().unwrap().push(delta)),
            )
            .await
            .expect("流式应可派发");
        assert_eq!(response.text, "chunk");
        assert_eq!(chunks.lock().unwrap().clone(), vec!["chunk".to_string()]);
    }
}
