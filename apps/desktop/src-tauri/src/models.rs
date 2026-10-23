//! 模型模块：统一模型抽象与插件模型。
//!
//! - `defs`：公共定义（推理请求 / 结果、并发语义、能力声明、渠道执行契约）。
//! - `model`：模型统一抽象 [`Model`]（并发是模型基础属性）。
//! - `plugin`：插件模型抽象 [`PluginModel`]（一个模型对应一个 web 渠道，会话管理自行实现）。
//! - `pow`：DeepSeek 网页版 PoW 纯计算。
//! - `deepseek_web`：DeepSeek 网页版模型 [`DeepSeekWebModel`]。
//!
//! 提示词组装、接入平台转换与插件实例选择都不属于模型。

pub mod deepseek_web;
pub mod defs;
pub mod model;
pub mod plugin;
pub mod pow;
pub mod prompt;
pub mod yuanbao_web;
pub mod web_context;

pub use deepseek_web::DeepSeekWebModel;
pub use yuanbao_web::YuanbaoWebModel;
pub use defs::{Concurrency, InferRequest, ModelResponse, SessionPoolStore, WebContextStore};
pub use plugin::PluginModel;
pub use web_context::DbWebContextStore;