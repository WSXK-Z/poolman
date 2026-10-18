//! 网关模型路由：按**请求参数 + 调用方 token** 选择模型并执行请求。
//!
//! 位于 crate 根（与 API 格式、供应商实现都解耦）——各 API 格式（openai 的对话补全 /
//! 文本补全 / 响应接口等）都通过它执行推理；对渠道会话的访问也经由它。
//!
//! 选择流程（[`ChatRouter::route`]）：
//! 1. **token**：调用方 token 必须存在（鉴权上下文的入口）；
//! 2. **解析**：对外模型名（含别名/前缀）→ 承载它的渠道；
//! 3. **存在性**：该渠道已注册模型；
//! 4. **启停**：该渠道在桌面端启用；
//! 5. **能力**：请求要求的流式 / 工具能力被该模型支持。
//!
//! 注册表里目前只注册插件供应商（`plugin`）——它把浏览器插件连接封装成一个供应商，
//! 对外提供全部网页模型。以后接入官方 API 等其他类型的供应商时，只需在这里多注册一个。

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use tracing::info;

use crate::models::PluginModel;
use crate::providers::plugin::PLUGIN_PROVIDER_ID;
use crate::providers::provider_control::ProviderControl;
use crate::providers::{
    ChatProvider, InferRequest, ModelDescriptor, ModelResponse, PluginProvider, ProviderRegistry,
};

/// 路由输入：请求参数 + 调用方 token。
#[derive(Debug, Clone)]
pub struct RouteRequest<'a> {
    /// 请求的对外模型名。
    pub model: &'a str,
    /// 是否要求流式输出。
    pub stream: bool,
    /// 请求是否携带工具定义。
    pub tools: Option<&'a Value>,
    /// 调用方 token（API Key）。
    pub api_key: Option<&'a str>,
}

/// 路由决策：选中的模型与它所属的渠道 / 供应商。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteDecision {
    /// 对外模型名。
    pub model: String,
    /// 承载它的渠道。
    pub channel: String,
    /// 提供它的供应商 id。
    pub provider: String,
}

pub struct ChatRouter {
    /// 供应商注册表：模型名 → 供应商。
    registry: ProviderRegistry,
    /// 插件供应商（渠道模型与会话管理入口）。
    plugin: Arc<PluginProvider>,
    /// 渠道启停状态。
    provider_control: Arc<ProviderControl>,
}

impl ChatRouter {
    /// 组装注册表：把传入的插件模型交给插件供应商。
    pub fn new(models: Vec<Arc<dyn PluginModel>>, provider_control: Arc<ProviderControl>) -> Self {
        let plugin = Arc::new(PluginProvider::new(models, provider_control.clone()));
        let registry = ProviderRegistry::new(vec![plugin.clone() as Arc<dyn ChatProvider>]);
        Self {
            registry,
            plugin,
            provider_control,
        }
    }

    /// 按请求参数与调用方 token 选择模型。
    pub fn route(&self, request: &RouteRequest<'_>) -> Result<RouteDecision> {
        if request
            .api_key
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .is_none()
        {
            anyhow::bail!("缺少调用方 token，无法选择模型");
        }

        let channel = Self::map_model(request.model);
        let model = self.plugin.channel(&channel).ok_or_else(|| {
            anyhow::anyhow!("未知模型 {}：渠道 {} 未注册", request.model, channel)
        })?;

        if !self.is_provider_enabled(&channel) {
            anyhow::bail!("渠道 {} 已在桌面端停用", channel);
        }

        let capability = model.capability();
        if request.stream && !capability.stream {
            anyhow::bail!("模型 {} 不支持流式输出", request.model);
        }
        if request.tools.is_some() && !capability.tools {
            anyhow::bail!("模型 {} 不支持工具调用", request.model);
        }

        let provider = self
            .registry
            .provider_id_for(request.model)
            .unwrap_or_else(|| PLUGIN_PROVIDER_ID.to_string());

        Ok(RouteDecision {
            model: request.model.to_string(),
            channel,
            provider,
        })
    }

    /// 由 `InferRequest` 组装路由输入。
    fn route_request(request: &InferRequest) -> RouteRequest<'_> {
        RouteRequest {
            model: &request.model,
            stream: request.stream,
            tools: request.tools.as_ref(),
            api_key: request.api_key.as_deref(),
        }
    }

    /// 取某个渠道的模型（渠道会话管理入口）。
    pub fn channel(&self, platform: &str) -> Option<Arc<dyn PluginModel>> {
        self.plugin.channel(platform)
    }

    /// 列出当前可用的模型（已按渠道启停过滤）。
    pub fn list_models(&self) -> Vec<ModelDescriptor> {
        self.registry
            .list_models()
            .into_iter()
            .filter(|model| self.is_provider_enabled(&model.owned_by))
            .collect()
    }

    /// 该渠道是否在桌面端启用。
    pub fn is_provider_enabled(&self, provider: &str) -> bool {
        self.provider_control
            .is_enabled(Self::managed_provider_id(provider))
    }

    /// 将模型名、API 别名和内部 provider 名统一映射到桌面管理用 ID。
    fn managed_provider_id(provider: &str) -> &str {
        match provider {
            "openai" | "chatgpt" | "chatgpt-web" => "chatgpt-web",
            "claude" | "claude-web" => "claude-web",
            "gemini" | "gemini-web" => "gemini-web",
            "kimi" | "kimi-web" => "kimi-web",
            "qwen" | "qwen-web" => "qwen-web",
            "yuanbao" | "yuanbao-web" => "yuanbao-web",
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

    /// 将对外模型名映射到承载它的渠道标识。
    ///
    /// 与 `packages/shared/src/types/models.ts` 的 `ModelMapping` 同源，
    /// 一致性由 `crate::providers` 的单测直接读该 TS 文件逐条锁死。
    pub fn map_model(model: &str) -> String {
        crate::providers::platform_for(model).to_string()
    }

    /// 非流式对话：先路由（选模型 + 校验），再按模型分派给对应供应商。
    pub async fn chat_completions(&self, request: InferRequest) -> Result<ModelResponse> {
        let decision = self.route(&Self::route_request(&request))?;
        info!(
            "路由: model={} channel={} provider={}",
            decision.model, decision.channel, decision.provider
        );
        self.registry.chat_completions(request).await
    }

    /// 流式对话：先路由（选模型 + 校验），再按模型分派，增量经 `on_chunk` 实时送出。
    pub async fn chat_completions_stream(
        &self,
        request: InferRequest,
        on_chunk: Box<dyn FnMut(String) + Send + Sync>,
    ) -> Result<ModelResponse> {
        let decision = self.route(&Self::route_request(&request))?;
        info!(
            "路由(流式): model={} channel={} provider={}",
            decision.model, decision.channel, decision.provider
        );
        self.registry
            .chat_completions_stream(request, on_chunk)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::{ChatRouter, RouteDecision, RouteRequest};
    use crate::models::defs::{
        ChunkSink, Concurrency, InferRequest, ModelCapability, ModelResponse,
    };
    use crate::models::model::Model;
    use crate::models::PluginModel;
    use crate::providers::provider_control::ProviderControl;
    use anyhow::Result;
    use futures_util::future::BoxFuture;
    use serde_json::json;
    use std::sync::Arc;

    /// 可配置能力的渠道模型（渠道 `deepseek-web`）。
    struct MockModel {
        tools: bool,
    }

    impl Model for MockModel {
        fn id(&self) -> &str {
            "deepseek-web"
        }

        fn capability(&self) -> ModelCapability {
            ModelCapability {
                stream: true,
                tools: self.tools,
                vision: false,
                ..ModelCapability::basic()
            }
        }

        fn concurrency(&self) -> Concurrency {
            Concurrency::single()
        }

        fn infer<'a>(&'a self, _request: InferRequest) -> BoxFuture<'a, Result<ModelResponse>> {
            Box::pin(async move { Ok(ModelResponse::default()) })
        }

        fn infer_stream<'a>(
            &'a self,
            _request: InferRequest,
            _on_chunk: ChunkSink,
        ) -> BoxFuture<'a, Result<ModelResponse>> {
            Box::pin(async move { Ok(ModelResponse::default()) })
        }
    }

    impl PluginModel for MockModel {
        fn channel(&self) -> &str {
            "deepseek-web"
        }

        fn create_channel_session(&self) -> BoxFuture<'_, Result<Option<String>>> {
            Box::pin(async move { Ok(None) })
        }

        fn list_channel_sessions(&self) -> BoxFuture<'_, Result<serde_json::Value>> {
            Box::pin(async move { Ok(json!([])) })
        }

        fn delete_channel_session<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<bool>> {
            Box::pin(async move { Ok(true) })
        }

        fn rename_channel_session<'a>(
            &'a self,
            _id: &'a str,
            _title: &'a str,
        ) -> BoxFuture<'a, Result<bool>> {
            Box::pin(async move { Ok(true) })
        }

        fn channel_history<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<serde_json::Value>> {
            Box::pin(async move { Ok(json!({})) })
        }
    }

    fn router(tools: bool) -> (ChatRouter, Arc<ProviderControl>) {
        let control = Arc::new(ProviderControl::new(std::collections::HashMap::new()));
        let model = Arc::new(MockModel { tools }) as Arc<dyn PluginModel>;
        (ChatRouter::new(vec![model], control.clone()), control)
    }

    /// 正常路由：token + 请求参数齐备时选中该渠道的模型。
    #[test]
    fn routes_to_channel_with_token_and_params() {
        let (router, _control) = router(true);
        let decision = router
            .route(&RouteRequest {
                model: "deepseek-chat",
                stream: true,
                tools: Some(&json!([])),
                api_key: Some("sk-test"),
            })
            .expect("应能路由");
        assert_eq!(
            decision,
            RouteDecision {
                model: "deepseek-chat".to_string(),
                channel: "deepseek-web".to_string(),
                provider: "plugin".to_string(),
            }
        );
    }

    /// 缺少 token：拒绝路由。
    #[test]
    fn rejects_without_token() {
        let (router, _control) = router(true);
        let error = router
            .route(&RouteRequest {
                model: "deepseek-web",
                stream: false,
                tools: None,
                api_key: None,
            })
            .expect_err("缺 token 应拒绝");
        assert!(error.to_string().contains("token"));
    }

    /// 未知渠道：拒绝路由。
    #[test]
    fn rejects_unknown_channel() {
        let (router, _control) = router(true);
        let error = router
            .route(&RouteRequest {
                model: "gpt-4o",
                stream: false,
                tools: None,
                api_key: Some("sk-test"),
            })
            .expect_err("未注册渠道应拒绝");
        assert!(error.to_string().contains("未注册"));
    }

    /// 请求参数要求工具能力但模型不支持：拒绝路由。
    #[test]
    fn rejects_unsupported_capability() {
        let (router, _control) = router(false);
        let error = router
            .route(&RouteRequest {
                model: "deepseek-web",
                stream: false,
                tools: Some(&json!([])),
                api_key: Some("sk-test"),
            })
            .expect_err("不支持工具调用应拒绝");
        assert!(error.to_string().contains("工具调用"));
    }

    /// 渠道停用：拒绝路由。
    #[test]
    fn rejects_disabled_channel() {
        let (router, control) = router(true);
        control.set_enabled("deepseek-web", false).unwrap();
        let error = router
            .route(&RouteRequest {
                model: "deepseek-web",
                stream: false,
                tools: None,
                api_key: Some("sk-test"),
            })
            .expect_err("停用渠道应拒绝");
        assert!(error.to_string().contains("停用"));
    }

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
        assert_eq!(ChatRouter::managed_provider_id("moonshot-v1-8k"), "kimi-web");
        assert_eq!(ChatRouter::managed_provider_id("qwen-plus"), "qwen-web");
        assert_eq!(
            ChatRouter::managed_provider_id("deepseek-reasoner"),
            "deepseek-web"
        );
    }

    #[test]
    fn model_names_map_to_web_platforms() {
        assert_eq!(ChatRouter::map_model("gpt-4o"), "chatgpt-web");
        assert_eq!(ChatRouter::map_model("deepseek-reasoner"), "deepseek-web");
        assert_eq!(ChatRouter::map_model("custom-model"), "custom-model");
    }
}
