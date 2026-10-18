//! 插件供应商：把浏览器插件连接封装成一个供应商。
//!
//! 它对外提供 [`WEB_MODELS`] 里的全部模型（别名 → 渠道），内部按渠道持有插件模型
//! [`crate::models::PluginModel`] 并转交执行（会话管理由模型自行完成）。
//!
//! 具体由哪台插件实例执行，属于插件管理器（`extension::manager::ExtensionManager`）
//! 与桥接层调度的职责，供应商与模型都不感知实例。

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use futures_util::future::BoxFuture;

use super::defs::{ChatProvider, ModelDescriptor};
use crate::models::{Concurrency, InferRequest, ModelResponse, PluginModel};
use crate::providers::provider_control::ProviderControl;

/// 插件供应商的稳定标识。
pub const PLUGIN_PROVIDER_ID: &str = "plugin";

/// 插件供应商对外展示名。
pub const PLUGIN_PROVIDER_NAME: &str = "浏览器模型代理";

/// 渠道并发设置在模型行配置里的键（`models.config.concurrency`）。
pub const CHANNEL_CONCURRENCY_KEY: &str = "concurrency";

/// 渠道默认并发：网页渠道本身支持并发的不同会话，默认预建这个数量的会话。
pub const DEFAULT_CHANNEL_CONCURRENCY: u32 = 5;

/// 渠道并发上限：配置写错时避免一次性建出过多网页会话。
pub const MAX_CHANNEL_CONCURRENCY: u32 = 10;

/// 一条对外模型目录项：对外模型名（别名）→ 承载它的网页渠道。
#[derive(Debug, Clone, Copy)]
pub struct WebModel {
    /// 对外模型名（客户端请求 `model` 字段使用的名字）。
    pub id: &'static str,
    /// 承载它的网页渠道标识（渠道 id，如 `deepseek-web`）。
    pub platform: &'static str,
}

/// 插件供应商对外提供的模型目录（别名 → 渠道）。
///
/// 与 `packages/shared/src/types/models.ts` 的 `ModelMapping` 保持同源
/// （一致性由单测锁死）。
pub const WEB_MODELS: &[WebModel] = &[
    // ChatGPT 网页版
    WebModel { id: "gpt-4o", platform: "chatgpt-web" },
    WebModel { id: "gpt-4o-mini", platform: "chatgpt-web" },
    WebModel { id: "gpt-4", platform: "chatgpt-web" },
    WebModel { id: "gpt-3.5-turbo", platform: "chatgpt-web" },
    // Claude 网页版
    WebModel { id: "claude-3-7-sonnet", platform: "claude-web" },
    WebModel { id: "claude-3-5-sonnet", platform: "claude-web" },
    WebModel { id: "claude-3-opus", platform: "claude-web" },
    // DeepSeek 网页版：统一入口 + 已停用 API 模型名向后兼容
    WebModel { id: "deepseek-web", platform: "deepseek-web" },
    WebModel { id: "deepseek-chat", platform: "deepseek-web" },
    WebModel { id: "deepseek-reasoner", platform: "deepseek-web" },
    // Gemini 网页版
    WebModel { id: "gemini-2.5-pro", platform: "gemini-web" },
    WebModel { id: "gemini-2.0-flash", platform: "gemini-web" },
    WebModel { id: "gemini-pro", platform: "gemini-web" },
    // Kimi
    WebModel { id: "kimi", platform: "kimi-web" },
    WebModel { id: "moonshot-v1-8k", platform: "kimi-web" },
    // 通义千问
    WebModel { id: "qwen-max", platform: "qwen-web" },
    WebModel { id: "qwen-plus", platform: "qwen-web" },
    // 腾讯元宝
    WebModel { id: "yuanbao-web", platform: "yuanbao-web" },
];

/// 对外模型名 → 承载它的网页渠道。
///
/// 未收录的模型按原名当作渠道标识返回，兼容用户自定义的模型名。
pub fn platform_for(model: &str) -> &str {
    WEB_MODELS
        .iter()
        .find(|entry| entry.id == model)
        .map(|entry| entry.platform)
        .unwrap_or(model)
}

/// 渠道展示名（未知渠道回退为渠道 id）。
pub fn channel_label(channel: &str) -> &str {
    match channel {
        "deepseek-web" => "DeepSeek",
        "chatgpt-web" => "ChatGPT",
        "claude-web" => "Claude",
        "gemini-web" => "Gemini",
        "kimi-web" => "Kimi",
        "qwen-web" => "通义千问",
        "yuanbao-web" => "腾讯元宝",
        other => other,
    }
}

/// 渠道并发设置：取该渠道各模型行配置里的最大值，缺失用默认值，并夹到上限内。
pub fn channel_concurrency(rows: &[super::manager::Model], channel: &str) -> u32 {
    rows.iter()
        .filter(|row| row.channel == channel)
        .filter_map(|row| {
            row.config
                .get(CHANNEL_CONCURRENCY_KEY)
                .and_then(serde_json::Value::as_u64)
        })
        .max()
        .map(|value| value.clamp(1, MAX_CHANNEL_CONCURRENCY as u64) as u32)
        .unwrap_or(DEFAULT_CHANNEL_CONCURRENCY)
}

/// 插件供应商。
pub struct PluginProvider {
    /// 渠道标识 → 插件模型。
    channels: HashMap<String, Arc<dyn PluginModel>>,
    /// 供应商启停状态：决定某个渠道是否参与请求分配。
    control: Arc<ProviderControl>,
}

impl PluginProvider {
    pub fn new(channels: Vec<Arc<dyn PluginModel>>, control: Arc<ProviderControl>) -> Self {
        let channels = channels
            .into_iter()
            .map(|channel| (channel.channel().to_string(), channel))
            .collect();
        Self { channels, control }
    }

    /// 取某个渠道的模型（供渠道会话管理使用）。
    pub fn channel(&self, platform: &str) -> Option<Arc<dyn PluginModel>> {
        self.channels.get(platform).cloned()
    }

    /// 按对外模型名取到承载它的渠道模型，并检查该渠道是否启用。
    fn channel_for(&self, model: &str) -> Result<Arc<dyn PluginModel>> {
        let platform = platform_for(model);
        if !self.control.is_enabled(platform) {
            anyhow::bail!("供应商 {} 已在桌面端停用", platform);
        }
        self.channels.get(platform).cloned().ok_or_else(|| {
            anyhow::anyhow!("渠道 {} 尚未在桌面端注册，无法通过插件执行", platform)
        })
    }

    /// 某个对外模型声明的并发语义（转交其渠道模型的声明）。
    pub fn concurrency_for(&self, model: &str) -> Option<Concurrency> {
        self.channels
            .get(platform_for(model))
            .map(|channel| channel.concurrency())
    }
}

impl ChatProvider for PluginProvider {
    fn id(&self) -> &str {
        PLUGIN_PROVIDER_ID
    }

    fn display_name(&self) -> &str {
        PLUGIN_PROVIDER_NAME
    }

    fn models(&self) -> Vec<ModelDescriptor> {
        WEB_MODELS
            .iter()
            .map(|entry| ModelDescriptor {
                id: entry.id.to_string(),
                provider: PLUGIN_PROVIDER_ID.to_string(),
                owned_by: entry.platform.to_string(),
            })
            .collect()
    }

    fn chat_completions<'a>(
        &'a self,
        request: InferRequest,
    ) -> BoxFuture<'a, Result<ModelResponse>> {
        Box::pin(async move { self.channel_for(&request.model)?.infer(request).await })
    }

    fn chat_completions_stream<'a>(
        &'a self,
        request: InferRequest,
        on_chunk: Box<dyn FnMut(String) + Send + Sync>,
    ) -> BoxFuture<'a, Result<ModelResponse>> {
        Box::pin(async move {
            self.channel_for(&request.model)?
                .infer_stream(request, on_chunk)
                .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{platform_for, PluginProvider, WEB_MODELS};
    use crate::models::model::Model;
    use crate::models::defs::ModelCapability;
    use crate::models::{Concurrency, InferRequest, ModelResponse, PluginModel};
    use crate::providers::defs::ChatProvider;
    use crate::providers::plugin::{
        channel_concurrency, CHANNEL_CONCURRENCY_KEY, DEFAULT_CHANNEL_CONCURRENCY,
        MAX_CHANNEL_CONCURRENCY,
    };
    use crate::providers::provider_control::ProviderControl;
    use serde_json::json;
    use anyhow::Result;
    use futures_util::future::BoxFuture;
    use serde_json::Value;
    use std::sync::Arc;

    /// 最小插件模型：渠道 `deepseek-web`，回显消息条数，会话管理直接返回。
    struct MockPluginModel;

    impl Model for MockPluginModel {
        fn id(&self) -> &str {
            "deepseek-web"
        }

        fn display_name(&self) -> &str {
            "DeepSeek"
        }

        fn capability(&self) -> ModelCapability {
            ModelCapability::basic()
        }

        fn concurrency(&self) -> Concurrency {
            Concurrency::single()
        }

        fn infer<'a>(&'a self, request: InferRequest) -> BoxFuture<'a, Result<ModelResponse>> {
            Box::pin(async move {
                Ok(ModelResponse {
                    text: format!("model:{}", request.messages.len()),
                    is_complete: true,
                    ..Default::default()
                })
            })
        }

        fn infer_stream<'a>(
            &'a self,
            _request: InferRequest,
            mut on_chunk: crate::models::defs::ChunkSink,
        ) -> BoxFuture<'a, Result<ModelResponse>> {
            Box::pin(async move {
                on_chunk("chunk".to_string());
                Ok(ModelResponse {
                    text: "chunk".to_string(),
                    is_complete: true,
                    ..Default::default()
                })
            })
        }
    }

    impl PluginModel for MockPluginModel {
        fn channel(&self) -> &str {
            "deepseek-web"
        }

        fn create_channel_session(&self) -> BoxFuture<'_, Result<Option<String>>> {
            Box::pin(async move { Ok(Some("sess-1".to_string())) })
        }

        fn list_channel_sessions(&self) -> BoxFuture<'_, Result<Value>> {
            Box::pin(async move { Ok(serde_json::json!([])) })
        }

        fn delete_channel_session<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, Result<bool>> {
            Box::pin(async move { Ok(true) })
        }

        fn rename_channel_session<'a>(
            &'a self,
            _session_id: &'a str,
            _title: &'a str,
        ) -> BoxFuture<'a, Result<bool>> {
            Box::pin(async move { Ok(true) })
        }

        fn channel_history<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<Value>> {
            Box::pin(async move { Ok(serde_json::json!({ "id": session_id })) })
        }
    }

    fn deepseek_channel() -> Arc<dyn PluginModel> {
        Arc::new(MockPluginModel)
    }

    /// 模型目录与共享包单一来源一致：直接读 packages/shared 的 ModelMapping 逐条比对。
    #[test]
    fn web_models_match_shared_mapping() {
        let ts = include_str!("../../../../../packages/shared/src/types/models.ts");
        let mut checked = 0usize;
        for line in ts.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix('\'') else {
                continue;
            };
            let Some((model, rest)) = rest.split_once("': '") else {
                continue;
            };
            let Some((provider, _)) = rest.split_once('\'') else {
                continue;
            };
            assert_eq!(
                platform_for(model),
                provider,
                "模型 {} 的渠道映射与 packages/shared/src/types/models.ts 不一致",
                model
            );
            checked += 1;
        }
        assert!(
            checked >= 15,
            "未从 models.ts 解析到足够的模型映射（{} 条），解析逻辑可能失效",
            checked
        );
        assert_eq!(WEB_MODELS.len(), checked, "目录条数应与共享映射一致");
    }

    /// 未收录的模型按原名当作渠道标识（兼容自定义模型名）。
    #[test]
    fn unlisted_model_falls_back_to_itself() {
        assert_eq!(platform_for("custom-model"), "custom-model");
    }

    /// 渠道并发设置：取该渠道各模型行配置的最大值，缺失用默认值，并夹到上限内。
    #[test]
    fn channel_concurrency_reads_model_config() {
        fn row(channel: &str, config: serde_json::Value) -> crate::providers::manager::Model {
            crate::providers::manager::Model {
                id: format!("{}-{}", channel, config),
                provider_id: "plugin".to_string(),
                channel: channel.to_string(),
                display_name: channel.to_string(),
                enabled: true,
                status: "inactive".to_string(),
                config,
                created_at: 0,
                updated_at: 0,
            }
        }

        let rows = vec![
            row("deepseek-web", json!({})),
            row("deepseek-web", json!({ (CHANNEL_CONCURRENCY_KEY): 7 })),
            row("deepseek-web", json!({ (CHANNEL_CONCURRENCY_KEY): 2 })),
            row("chatgpt-web", json!({ (CHANNEL_CONCURRENCY_KEY): 9 })),
        ];
        // 同一渠道多行取最大
        assert_eq!(channel_concurrency(&rows, "deepseek-web"), 7);
        // 有配置的渠道按配置走
        assert_eq!(channel_concurrency(&rows, "chatgpt-web"), 9);
        // 没记录的渠道用默认值
        assert_eq!(
            channel_concurrency(&rows, "kimi-web"),
            DEFAULT_CHANNEL_CONCURRENCY
        );
        assert_eq!(DEFAULT_CHANNEL_CONCURRENCY, 5, "默认并发为 5");
        // 配置越界与非法值都被夹住
        assert_eq!(
            channel_concurrency(
                &[row("kimi-web", json!({ (CHANNEL_CONCURRENCY_KEY): 0 }))],
                "kimi-web"
            ),
            1
        );
        assert_eq!(
            channel_concurrency(
                &[row(
                    "kimi-web",
                    json!({ (CHANNEL_CONCURRENCY_KEY): MAX_CHANNEL_CONCURRENCY + 5 })
                )],
                "kimi-web"
            ),
            MAX_CHANNEL_CONCURRENCY
        );
        assert_eq!(
            channel_concurrency(
                &[row(
                    "kimi-web",
                    json!({ (CHANNEL_CONCURRENCY_KEY): "many" })
                )],
                "kimi-web"
            ),
            DEFAULT_CHANNEL_CONCURRENCY
        );
    }

    /// 插件供应商：别名路由到渠道模型执行、覆盖全部对外模型、遵守启停、缺渠道报错。
    #[tokio::test]
    async fn plugin_provider_routes_to_channel_models() {
        let control = Arc::new(ProviderControl::new(std::collections::HashMap::new()));
        let provider = PluginProvider::new(vec![deepseek_channel()], control.clone());

        assert_eq!(provider.models().len(), WEB_MODELS.len());
        assert_eq!(provider.id(), "plugin");

        // deepseek-chat 是该渠道的别名 → 路由到渠道模型执行
        let response = provider
            .chat_completions(InferRequest::new("deepseek-chat", vec![serde_json::json!({})]))
            .await
            .expect("已注册渠道的模型应可执行");
        assert_eq!(response.text, "model:1");

        // 并发语义来自渠道模型
        assert_eq!(
            provider.concurrency_for("deepseek-chat").unwrap().max_inflight,
            1
        );

        // 渠道会话管理经由渠道模型自行完成
        let channel = provider.channel("deepseek-web").expect("应有渠道模型");
        assert_eq!(
            channel.create_channel_session().await.unwrap().as_deref(),
            Some("sess-1")
        );

        // 停用该渠道后应拒绝
        control.set_enabled("deepseek-web", false).unwrap();
        assert!(provider
            .chat_completions(InferRequest::new("deepseek-chat", vec![]))
            .await
            .is_err());

        // 未注册渠道的模型应报错
        assert!(provider
            .chat_completions(InferRequest::new("gpt-4o", vec![]))
            .await
            .is_err());
    }
}
