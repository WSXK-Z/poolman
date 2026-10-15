//! 模型统一抽象。
//!
//! 模型是桌面端执行对话的最小单元：对外提供标识与能力，声明自己的并发语义，
//! 并负责一次（流式 / 非流式）推理。
//!
//! 不属于模型的内容：提示词组装、接入平台转换、插件实例选择（由入口/编排层与
//! 插件管理器负责）。

use anyhow::Result;
use futures_util::future::BoxFuture;

use super::defs::{ChunkSink, Concurrency, InferRequest, ModelCapability, ModelResponse};

/// 模型统一抽象。
pub trait Model: Send + Sync {
    /// 对外模型名。
    fn id(&self) -> &str;

    /// 展示名；默认与 [`Model::id`] 相同。
    fn display_name(&self) -> &str {
        self.id()
    }

    /// 能力声明。
    fn capability(&self) -> ModelCapability;

    /// 并发语义：调度层据此限制在途数与同会话串行。
    fn concurrency(&self) -> Concurrency;

    /// 非流式推理。
    fn infer<'a>(&'a self, request: InferRequest) -> BoxFuture<'a, Result<ModelResponse>>;

    /// 流式推理：增量经 `on_chunk` 实时送出。
    fn infer_stream<'a>(
        &'a self,
        request: InferRequest,
        on_chunk: ChunkSink,
    ) -> BoxFuture<'a, Result<ModelResponse>>;
}
