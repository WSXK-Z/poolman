//! 供应商模块的公共定义。
//!
//! 这里放两类东西：统一对话接口 [`ChatProvider`]（含可覆写的默认实现）与
//! 模型描述 [`ModelDescriptor`]。请求与结果类型复用模型模块的
//! [`InferRequest`](crate::models::InferRequest) / [`ModelResponse`](crate::models::ModelResponse)：
//! 供应商只是模型的容器与分派入口，不另立契约。
//!
//! 具体供应商各自一个文件实现 [`ChatProvider`]（见同目录 `plugin.rs`），互不引用。

use anyhow::Result;
use futures_util::future::BoxFuture;

use crate::models::{InferRequest, ModelResponse};

/// 一条对外可用的模型。
#[derive(Debug, Clone)]
pub struct ModelDescriptor {
    /// 对外模型名。
    pub id: String,
    /// 提供它的供应商 id。
    pub provider: String,
    /// 实际承载它的来源标识（网页渠道，或官方 API 组织名）。
    pub owned_by: String,
}

/// 供应商统一接口：注册表只通过它与具体供应商对话。
pub trait ChatProvider: Send + Sync {
    /// 供应商稳定标识。
    fn id(&self) -> &str;

    /// 供应商展示名。默认与 [`ChatProvider::id`] 相同，供应商可覆写为更友好的名字。
    fn display_name(&self) -> &str {
        self.id()
    }

    /// 该供应商对外提供的全部模型。
    fn models(&self) -> Vec<ModelDescriptor>;

    /// 非流式对话。
    fn chat_completions<'a>(
        &'a self,
        request: InferRequest,
    ) -> BoxFuture<'a, Result<ModelResponse>>;

    /// 流式对话：增量经 `on_chunk` 实时送出。
    fn chat_completions_stream<'a>(
        &'a self,
        request: InferRequest,
        on_chunk: Box<dyn FnMut(String) + Send + Sync>,
    ) -> BoxFuture<'a, Result<ModelResponse>>;
}
