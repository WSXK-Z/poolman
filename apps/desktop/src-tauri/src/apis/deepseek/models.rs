//! DeepSeek 模型列表（`GET /models`）。
//!
//! 字段对齐 DeepSeek 官方 `list-models` 文档，与 [`crate::apis::base`] 里那份
//! OpenAI 形状的 `/v1/models` 并存、互不影响：
//! - 官方用 `context_window`，OpenAI 用 `context_length`；
//! - 官方多 `name` / `input_modalities` / `output_modalities`。
//!
//! `effort` 与 `api_capabilities` 是上游模型的自述信息，网关没有对应数据，
//! 按「不确定就不写」处理（两者在官方 schema 里都是可选）——宁可缺字段，
//! 也不编造一个查不到出处的档位声明。

use std::sync::Arc;

use axum::{extract::State, response::IntoResponse, routing::get, Json, Router};
use serde_json::{json, Value};

use crate::apis::base::AppState;
use crate::chat_router::ChatRouter;
use crate::models::defs::ModelCapability;
use crate::providers::plugin::channel_label;

/// 把一条模型描述渲染成官方 `data[]` 里的一项。
///
/// 抽成纯函数便于单测：`AppState` 需要数据库与插件连接，构造代价太大，
/// 而这里关心的只是字段映射。
fn model_object(id: &str, owned_by: &str, channel: &str, capability: Option<ModelCapability>) -> Value {
    let mut item = json!({
        "id": id,
        "object": "model",
        "owned_by": owned_by,
        "name": channel_label(channel),
    });
    if let Some(capability) = capability {
        item["context_window"] = json!(capability.context_length);
        item["max_output_tokens"] = json!(capability.max_output_tokens);
        // 渠道支持图像输入时把 image 一并列出，否则只有 text。
        item["input_modalities"] = if capability.vision {
            json!(["text", "image"])
        } else {
            json!(["text"])
        };
        // 网关输出侧只产出文本。
        item["output_modalities"] = json!(["text"]);
    }
    item
}

/// 模型列表：结构与官方 `GET /models` 一致。
pub async fn models_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let data: Vec<Value> = state
        .chat_router
        .list_models()
        .into_iter()
        .map(|model| {
            let channel = ChatRouter::map_model(&model.id);
            // 渠道未注册时 capability 为 None，对应字段整体省略。
            let capability = state
                .chat_router
                .channel(&channel)
                .map(|channel| channel.capability());
            model_object(&model.id, &model.owned_by, &channel, capability)
        })
        .collect();
    Json(json!({ "object": "list", "data": data }))
}

/// 本格式的路由：`GET /models`（DeepSeek 官方路径，不带 `/v1` 前缀）。
pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/models", get(models_handler))
}

#[cfg(test)]
mod tests {
    use super::model_object;
    use crate::models::defs::ModelCapability;
    use serde_json::json;

    /// 基础模型：官方字段齐全，且用 `context_window` 而非 `context_length`。
    #[test]
    fn renders_official_model_shape() {
        let item = model_object(
            "deepseek-web",
            "deepseek-web",
            "deepseek-web",
            Some(ModelCapability::basic()),
        );
        assert_eq!(item["id"], "deepseek-web");
        assert_eq!(item["object"], "model");
        assert_eq!(item["owned_by"], "deepseek-web");
        assert_eq!(item["name"], "DeepSeek");
        assert_eq!(item["context_window"], 64 * 1024);
        assert_eq!(item["max_output_tokens"], 8 * 1024);
        assert_eq!(item["input_modalities"], json!(["text"]));
        assert_eq!(item["output_modalities"], json!(["text"]));
        assert!(
            item.get("context_length").is_none(),
            "官方字段名是 context_window：{item}"
        );
    }

    /// 支持图像输入的渠道，模态数组里要带 image。
    #[test]
    fn vision_channel_lists_image_modality() {
        let capability = ModelCapability {
            vision: true,
            ..ModelCapability::basic()
        };
        let item = model_object("deepseek-web", "deepseek-web", "deepseek-web", Some(capability));
        assert_eq!(item["input_modalities"], json!(["text", "image"]));
    }

    /// 渠道未注册时只保留必然可知的字段，不写任何能力数字。
    #[test]
    fn unknown_channel_omits_capability_fields() {
        let item = model_object("custom-model", "custom", "custom-model", None);
        assert_eq!(item["id"], "custom-model");
        assert_eq!(item["name"], "custom-model");
        for key in [
            "context_window",
            "max_output_tokens",
            "input_modalities",
            "output_modalities",
        ] {
            assert!(item.get(key).is_none(), "不该出现 {key}：{item}");
        }
    }

    /// `effort` / `api_capabilities` 没有数据来源，不能凭空产出。
    #[test]
    fn omits_unavailable_fields() {
        let item = model_object(
            "deepseek-web",
            "deepseek-web",
            "deepseek-web",
            Some(ModelCapability::basic()),
        );
        assert!(item.get("effort").is_none(), "effort 无数据来源：{item}");
        assert!(
            item.get("api_capabilities").is_none(),
            "api_capabilities 无数据来源：{item}"
        );
    }
}