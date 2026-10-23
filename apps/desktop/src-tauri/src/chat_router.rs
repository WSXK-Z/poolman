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
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::apis::base::DeltaChunk;
use crate::local_session::{LocalSessionStore, SessionAnswer, SessionPlan};
use crate::models::{InferRequest, ModelResponse, PluginModel};
use crate::providers::plugin::PLUGIN_PROVIDER_ID;
use crate::providers::provider_control::ProviderControl;
use crate::providers::{ChatProvider, ModelDescriptor, PluginProvider, ProviderRegistry};
use crate::request_record::{RequestMeta, RequestRecordDraft, RequestRecordStore};

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

/// 一次请求的待落库快照：记录库未注入时为 None。
pub(crate) type PendingRecord = Option<(Arc<RequestRecordStore>, RequestRecordDraft)>;

pub struct ChatRouter {
    /// 供应商注册表：模型名 → 供应商。
    registry: ProviderRegistry,
    /// 插件供应商（渠道模型与会话管理入口）。
    plugin: Arc<PluginProvider>,
    /// 渠道启停状态。
    provider_control: Arc<ProviderControl>,
    /// 本地请求记录（未注入时不记录）。
    request_records: Option<Arc<RequestRecordStore>>,
    /// 本地会话管理（未注入时不生成会话与标记）。
    local_sessions: Option<Arc<LocalSessionStore>>,
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
            request_records: None,
            local_sessions: None,
        }
    }

    /// 注入本地请求记录：所有经过路由的对话请求都会写入一条记录。
    pub fn with_request_records(mut self, store: Arc<RequestRecordStore>) -> Self {
        self.request_records = Some(store);
        self
    }

    /// 注入本地会话管理：每次请求归入会话，新会话在回复里写入标记。
    pub fn with_local_sessions(mut self, store: Arc<LocalSessionStore>) -> Self {
        self.local_sessions = Some(store);
        self
    }

    /// 删除网页渠道的远端会话；渠道模型未实例化时返回错误。
    pub async fn delete_remote_session(
        &self,
        channel: &str,
        session_id: &str,
    ) -> Result<bool> {
        let model = self
            .plugin
            .channel(channel)
            .ok_or_else(|| anyhow::anyhow!("渠道 {channel} 未实例化，无法删除远端会话"))?;
        model.delete_channel_session(session_id).await
    }

    /// 修改网页渠道的远端会话标题；渠道模型未实例化时返回错误。
    pub async fn rename_remote_session(
        &self,
        channel: &str,
        session_id: &str,
        title: &str,
    ) -> Result<bool> {
        let model = self
            .plugin
            .channel(channel)
            .ok_or_else(|| anyhow::anyhow!("渠道 {channel} 未实例化，无法修改远端会话标题"))?;
        model.rename_channel_session(session_id, title).await
    }

    /// 在请求交给渠道前拍快照；未注入记录库时返回 None。
    pub(crate) fn begin_record(&self, request: &InferRequest, meta: &RequestMeta) -> PendingRecord {
        self.request_records
            .as_ref()
            .map(|store| (store.clone(), RequestRecordDraft::from_request(request, meta)))
    }

    /// 工作流节点用：决定本地会话。
    pub(crate) async fn plan_session_for(&self, request: &InferRequest) -> Option<SessionPlan> {
        self.begin_session_plan(request).await
    }

    /// 工作流节点用：路由（不直接执行）。
    pub(crate) fn resolve_route_for(&self, request: &InferRequest) -> Result<RouteDecision> {
        self.route(&Self::route_request(request))
    }

    /// 工作流节点用：只把请求交给渠道，不处理会话标记。
    pub(crate) async fn dispatch_channel(
        &self,
        request: InferRequest,
        decision: Result<RouteDecision>,
        on_chunk: Option<Box<dyn FnMut(String) + Send + Sync>>,
    ) -> Result<ModelResponse> {
        let decision = decision?;
        info!(
            "工作流渠道发送: model={} channel={} provider={} stream={}",
            decision.model,
            decision.channel,
            decision.provider,
            on_chunk.is_some()
        );
        match on_chunk {
            Some(sink) => {
                self.registry
                    .chat_completions_stream(request, sink)
                    .await
            }
            None => self.registry.chat_completions(request).await,
        }
    }

    /// 工作流节点用：会话、请求记录、内容表收尾。
    pub(crate) async fn finish_prepared(
        &self,
        pending: PendingRecord,
        session_plan: Option<SessionPlan>,
        result: &Result<ModelResponse>,
    ) {
        let answer = Self::session_answer(result);
        Self::finish_record(
            pending,
            self.local_sessions.clone(),
            session_plan,
            answer,
            result,
        )
        .await;
    }

    /// 请求交给渠道前决定本地会话；未注入会话库或查询失败时返回 None。
    async fn begin_session_plan(&self, request: &InferRequest) -> Option<SessionPlan> {
        let store = self.local_sessions.as_ref()?;
        match store.plan(&request.messages).await {
            Ok(plan) => Some(plan),
            Err(error) => {
                warn!("生成本地会话计划失败: {}", error);
                None
            }
        }
    }

    /// 结果返回后写入会话、请求记录，并刷新该会话的内容表。
    async fn finish_record(
        pending: PendingRecord,
        local_store: Option<Arc<LocalSessionStore>>,
        session_plan: Option<SessionPlan>,
        answer: Option<SessionAnswer>,
        result: &Result<ModelResponse>,
    ) {
        let content_store = local_store.clone();
        let entry = pending.map(|(store, draft)| {
            let mut entry = draft.finish(result);
            if let Some(plan) = &session_plan {
                entry.local_session_id = Some(plan.session_id.clone());
            }
            (store, entry)
        });
        if entry.is_none() && (local_store.is_none() || session_plan.is_none()) {
            return;
        }
        if let (Some(store), Some(plan)) = (local_store, session_plan) {
            if let Err(error) = store.finalize(&plan, answer.as_ref()).await {
                warn!("写入本地会话失败 id={} err={}", plan.session_id, error);
            }
        }
        if let Some((store, entry)) = entry {
            let id = entry.id.clone();
            let session_id = entry.local_session_id.clone();
            if let Err(error) = store.insert(entry).await {
                warn!("写入本地请求记录失败 id={} err={}", id, error);
            }
            if let (Some(content_store), Some(session_id)) = (content_store, session_id) {
                if let Err(error) = content_store.rebuild_session_content(&session_id).await {
                    warn!("重建本地会话内容失败 id={} err={}", session_id, error);
                }
            }
        }
    }

    /// 本轮回答快照；失败请求没有回答内容。
    fn session_answer(result: &Result<ModelResponse>) -> Option<SessionAnswer> {
        match result {
            Ok(response) => Some(SessionAnswer {
                text: response.text.clone(),
                tool_calls: response.tool_calls.clone(),
            }),
            Err(_) => None,
        }
    }

    /// 新会话标记写入回复正文最前面。
    pub(crate) fn prepend_marker(text: &str, marker: &str) -> String {
        if text.starts_with(marker) {
            text.to_string()
        } else if text.is_empty() {
            marker.to_string()
        } else {
            format!("{marker}\n\n{text}")
        }
    }

    /// 流式输出前先送出标记增量，客户端随后会把它作为助手内容放回历史。
    pub(crate) fn marker_frame(marker: &str) -> String {
        json!({ DeltaChunk::RESPONSE: format!("{marker}\n\n") }).to_string()
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
    pub async fn chat_completions(
        &self,
        request: InferRequest,
        meta: RequestMeta,
    ) -> Result<ModelResponse> {
        let mut request = request;
        let title_request = crate::request_record::detect_title_request(
            &request.messages,
            request.tools.as_ref(),
        );
        let session_plan = if title_request {
            None
        } else {
            self.begin_session_plan(&request).await
        };
        if let Some(plan) = &session_plan {
            request.web_context_key = Some(plan.session_id.clone());
        }
        let marker = session_plan.as_ref().and_then(|plan| plan.marker.clone());
        let pending = self.begin_record(&request, &meta);
        let request_id = pending
            .as_ref()
            .map(|(_, draft)| draft.id.as_str())
            .unwrap_or("-");
        let routed = self.route(&Self::route_request(&request));
        let mut result = match routed {
            Ok(decision) => {
                info!(
                    "路由: request_id={} model={} channel={} provider={}",
                    request_id, decision.model, decision.channel, decision.provider
                );
                self.registry.chat_completions(request).await
            }
            Err(error) => Err(error),
        };
        if let (Some(marker), Ok(response)) = (marker.as_deref(), &mut result) {
            response.text = Self::prepend_marker(&response.text, marker);
        }
        let answer = Self::session_answer(&result);
        Self::finish_record(
            pending,
            self.local_sessions.clone(),
            session_plan,
            answer,
            &result,
        )
        .await;
        result
    }

    /// 流式对话：先路由（选模型 + 校验），再按模型分派，增量经 `on_chunk` 实时送出。
    pub async fn chat_completions_stream(
        &self,
        request: InferRequest,
        meta: RequestMeta,
        on_chunk: Box<dyn FnMut(String) + Send + Sync>,
    ) -> Result<ModelResponse> {
        let mut request = request;
        let title_request = crate::request_record::detect_title_request(
            &request.messages,
            request.tools.as_ref(),
        );
        let session_plan = if title_request {
            None
        } else {
            self.begin_session_plan(&request).await
        };
        if let Some(plan) = &session_plan {
            request.web_context_key = Some(plan.session_id.clone());
        }
        let marker = session_plan.as_ref().and_then(|plan| plan.marker.clone());
        let pending = self.begin_record(&request, &meta);
        let request_id = pending
            .as_ref()
            .map(|(_, draft)| draft.id.as_str())
            .unwrap_or("-");
        let routed = self.route(&Self::route_request(&request));
        let mut on_chunk = on_chunk;
        let mut result = match routed {
            Ok(decision) => {
                info!(
                    "路由(流式): request_id={} model={} channel={} provider={}",
                    request_id, decision.model, decision.channel, decision.provider
                );
                if let Some(marker) = marker.as_deref() {
                    on_chunk(Self::marker_frame(marker));
                }
                self.registry
                    .chat_completions_stream(request, on_chunk)
                    .await
            }
            Err(error) => Err(error),
        };
        if let (Some(marker), Ok(response)) = (marker.as_deref(), &mut result) {
            response.text = Self::prepend_marker(&response.text, marker);
        }
        let answer = Self::session_answer(&result);
        Self::finish_record(
            pending,
            self.local_sessions.clone(),
            session_plan,
            answer,
            &result,
        )
        .await;
        result
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
    use crate::request_record::RequestMeta;
    use crate::local_session::LocalSessionStore;
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

    /// 删除远端会话应转交给对应渠道模型。
    #[tokio::test]
    async fn deletes_remote_session_through_channel_model() {
        let (router, _control) = router(true);
        assert!(router
            .delete_remote_session("deepseek-web", "sess-1")
            .await
            .expect("删除应成功"));
        assert!(router
            .delete_remote_session("missing-web", "sess-1")
            .await
            .is_err());
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

    /// 注入记录库后，每次对话请求都会落一条数据库记录（请求侧字段与回答状态都完整）。
    #[tokio::test]
    async fn records_each_chat_request_to_database() {
        let path = std::env::temp_dir().join(format!(
            "poolman-chat-router-record-{}.db",
            uuid::Uuid::new_v4()
        ));
        let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
        let db = sea_orm::Database::connect(&url)
            .await
            .expect("初始化测试数据库失败");
        let store = Arc::new(
            crate::request_record::RequestRecordStore::new(db)
                .await
                .expect("同步请求记录表失败"),
        );
        let control = Arc::new(ProviderControl::new(std::collections::HashMap::new()));
        let model = Arc::new(MockModel { tools: true }) as Arc<dyn PluginModel>;
        let router = ChatRouter::new(vec![model], control).with_request_records(store.clone());

        let mut request = InferRequest::new(
            "deepseek-web",
            vec![
                json!({ "role": "system", "content": "系统提示" }),
                json!({ "role": "user", "content": "用户提问" }),
            ],
        );
        request.api_key = Some("sk-test".to_string());
        let meta = RequestMeta {
            protocol: "openai".to_string(),
            method: "POST".to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            headers: json!({ "authorization": "Bearer sk-test" }),
        };
        router
            .chat_completions(request, meta)
            .await
            .expect("请求应完成");

        // 记录写入是异步的（不阻塞响应），这里等它落库。
        let mut records = Vec::new();
        for _ in 0..100 {
            records = store.list(10).await.expect("读取请求记录失败");
            if !records.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(records.len(), 1, "每次请求应恰好一条记录");
        let detail = store
            .get(&records[0].id)
            .await
            .expect("读取详情失败")
            .expect("记录应存在");
        assert_eq!(detail.system_prompt, "系统提示");
        assert_eq!(detail.user_prompt, "用户提问");
        assert_eq!(detail.status, "success");
        assert_eq!(detail.protocol, "openai");
        assert_eq!(detail.endpoint, "/v1/chat/completions");
        assert_eq!(detail.api_key, "sk-test");
        assert!(detail.headers.contains("authorization"));
        assert!(detail.duration_ms >= 0);
        let _ = std::fs::remove_file(path);
    }

    /// 新请求的回复应带会话标记；带标记的后续请求归入同一本地会话。
    #[tokio::test]
    async fn marks_new_session_and_reuses_it_on_follow_up() {
        let path = std::env::temp_dir().join(format!(
            "poolman-chat-router-session-{}.db",
            uuid::Uuid::new_v4()
        ));
        let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
        let db = sea_orm::Database::connect(&url)
            .await
            .expect("初始化测试数据库失败");
        let sessions = Arc::new(
            LocalSessionStore::new(db)
                .await
                .expect("同步本地会话表失败"),
        );
        let control = Arc::new(ProviderControl::new(std::collections::HashMap::new()));
        let model = Arc::new(MockModel { tools: true }) as Arc<dyn PluginModel>;
        let router = ChatRouter::new(vec![model], control).with_local_sessions(sessions.clone());
        let meta = RequestMeta {
            protocol: "openai".to_string(),
            method: "POST".to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            headers: json!({ "authorization": "Bearer sk-test" }),
        };

        let mut first = InferRequest::new(
            "deepseek-web",
            vec![json!({ "role": "user", "content": "第一问" })],
        );
        first.api_key = Some("sk-test".to_string());
        let first_result = router
            .chat_completions(first, meta.clone())
            .await
            .expect("首次请求应完成");
        assert!(
            first_result.text.starts_with(crate::local_session::MARKER_PREFIX),
            "新会话回复应带标记：{}",
            first_result.text
        );

        let mut stored = Vec::new();
        for _ in 0..100 {
            stored = sessions.list_sessions().await.expect("读取会话失败");
            if !stored.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(stored.len(), 1);
        let marker = stored[0].collision_marker.clone().expect("会话应带标记");
        assert!(first_result.text.contains(&marker));

        let mut second = InferRequest::new(
            "deepseek-web",
            vec![
                json!({ "role": "user", "content": "第一问" }),
                json!({ "role": "assistant", "content": first_result.text }),
                json!({ "role": "user", "content": "第二问" }),
            ],
        );
        second.api_key = Some("sk-test".to_string());
        router
            .chat_completions(second, meta)
            .await
            .expect("后续请求应完成");
        let stored = sessions.list_sessions().await.expect("读取会话失败");
        assert_eq!(stored.len(), 1, "带标记的后续请求不应新建会话");

        let _ = std::fs::remove_file(path);
    }

    /// 客户端的自动标题请求不生成会话，也不在回复里写标记。
    #[tokio::test]
    async fn title_generation_request_does_not_create_session() {
        let path = std::env::temp_dir().join(format!(
            "poolman-chat-router-title-{}.db",
            uuid::Uuid::new_v4()
        ));
        let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
        let db = sea_orm::Database::connect(&url)
            .await
            .expect("初始化测试数据库失败");
        let sessions = Arc::new(
            LocalSessionStore::new(db)
                .await
                .expect("同步本地会话表失败"),
        );
        let control = Arc::new(ProviderControl::new(std::collections::HashMap::new()));
        let model = Arc::new(MockModel { tools: true }) as Arc<dyn PluginModel>;
        let router = ChatRouter::new(vec![model], control).with_local_sessions(sessions.clone());
        let meta = RequestMeta {
            protocol: "openai".to_string(),
            method: "POST".to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            headers: json!({}),
        };

        let mut request = InferRequest::new(
            "deepseek-web",
            vec![
                json!({
                    "role": "system",
                    "content": "Create a concise title for an AI coding-assistant session from the supplied human messages."
                }),
                json!({
                    "role": "user",
                    "content": "Generate the session title from this JSON array of human messages:\n[]"
                }),
            ],
        );
        request.api_key = Some("sk-test".to_string());
        let result = router
            .chat_completions(request, meta)
            .await
            .expect("标题请求应完成");
        assert!(result.text.is_empty(), "标题请求不应注入会话标记：{}", result.text);
        assert!(
            sessions
                .list_sessions()
                .await
                .expect("读取会话失败")
                .is_empty(),
            "标题请求不应创建本地会话"
        );

        let _ = std::fs::remove_file(path);
    }
}
