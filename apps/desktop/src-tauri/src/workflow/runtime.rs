//! 工作流运行时：提交请求、排队执行、持久化运行与节点状态。

use std::sync::Arc;

use anyhow::{anyhow, Result};
use tokio::sync::{oneshot, Semaphore};
use tracing::{info, warn};
use uuid::Uuid;

use crate::chat_router::ChatRouter;
use crate::models::defs::{ChunkSink, InferRequest, ModelResponse};
use crate::request_record::RequestMeta;
use crate::title_engine::{TitleSettings, TitleSource};
use crate::workflow::context::WorkflowContext;
use crate::workflow::graph::WorkflowGraph;
use crate::workflow::graph_store::WorkflowGraphStore;
use crate::workflow::node::{NodeDescriptor, NodeOutcome};
use crate::workflow::nodes;
use crate::workflow::registry::NodeRegistry;
use crate::workflow::store::{WorkflowNodeRunInfo, WorkflowRunInfo, WorkflowRunStore};

struct RuntimeInner {
    router: Arc<ChatRouter>,
    /// 标题生成设置：来源策略与本地模型目录。
    title: Arc<TitleSettings>,
    registry: Arc<NodeRegistry>,
    graph_store: Arc<WorkflowGraphStore>,
    run_store: Arc<WorkflowRunStore>,
    permits: Arc<Semaphore>,
}

pub struct WorkflowRuntime {
    inner: Arc<RuntimeInner>,
}

impl WorkflowRuntime {
    pub fn new(
        router: Arc<ChatRouter>,
        graph_store: Arc<WorkflowGraphStore>,
        run_store: Arc<WorkflowRunStore>,
        concurrency: usize,
    ) -> Self {
        let registry = Arc::new(nodes::build_registry());
        let concurrency = concurrency.max(1);
        Self {
            inner: Arc::new(RuntimeInner {
                router,
                // 默认只做启发式：测试与未配置场景都走这条最轻的路。
                title: TitleSettings::in_memory(TitleSource::Heuristic, ""),
                registry,
                graph_store,
                run_store,
                permits: Arc::new(Semaphore::new(concurrency)),
            }),
        }
    }

    /// 注入标题生成设置（来源策略与本地模型目录）。
    pub fn with_title_settings(mut self, title: Arc<TitleSettings>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("运行时尚未共享，可安全替换")
            .title = title;
        self
    }

    /// 当前标题生成设置。
    pub fn title_settings(&self) -> Arc<TitleSettings> {
        self.inner.title.clone()
    }

    /// 非流式入口：与旧 ChatRouter 方法保持同名，API 层可无感切换。
    pub async fn chat_completions(
        &self,
        request: InferRequest,
        meta: RequestMeta,
    ) -> Result<ModelResponse> {
        self.submit(request, meta, None).await
    }

    /// 流式入口：增量通过传入的 sink 实时送出。
    pub async fn chat_completions_stream(
        &self,
        request: InferRequest,
        meta: RequestMeta,
        on_chunk: Box<dyn FnMut(String) + Send + Sync>,
    ) -> Result<ModelResponse> {
        self.submit(request, meta, Some(on_chunk)).await
    }

    async fn submit(
        &self,
        request: InferRequest,
        meta: RequestMeta,
        sink: Option<ChunkSink>,
    ) -> Result<ModelResponse> {
        let graph = self.inner.graph_store.load()?;
        graph.validate(&self.inner.registry)?;
        let run_id = format!("wf_{}", Uuid::new_v4().simple());
        let graph_json = serde_json::to_string(&graph)?;
        let version = format!("v{}", graph.version);
        self.inner
            .run_store
            .create_run(
                &run_id,
                &meta.protocol,
                &request.model,
                request.stream,
                &graph_json,
                &version,
            )
            .await?;

        let (tx, rx) = oneshot::channel();
        let inner = self.inner.clone();
        let task_run_id = run_id.clone();
        tokio::spawn(async move {
            let permit = match inner.permits.acquire().await {
                Ok(permit) => permit,
                Err(_) => {
                    let _ = tx.send(Err(anyhow!("工作流并发闸门已关闭")));
                    return;
                }
            };
            if let Err(error) = inner.run_store.mark_running(&task_run_id).await {
                warn!("工作流标记 running 失败 run={} err={}", task_run_id, error);
            }
            let result = execute_graph(
                inner.clone(),
                &task_run_id,
                graph,
                request,
                meta,
                sink,
            )
            .await;
            drop(permit);
            let _ = tx.send(result);
        });
        match rx.await {
            Ok(result) => result,
            Err(_) => Err(anyhow!("工作流执行器已退出")),
        }
    }

    /// 当前工作流图（默认图或界面保存的图）。
    pub fn graph(&self) -> Result<WorkflowGraph> {
        self.inner.graph_store.load()
    }

    pub fn graph_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(&self.graph()?)?)
    }

    pub fn is_custom_graph(&self) -> bool {
        self.inner.graph_store.is_custom()
    }

    pub fn node_descriptors(&self) -> Vec<NodeDescriptor> {
        self.inner.registry.descriptors()
    }

    pub fn save_graph_json(&self, raw: &str) -> Result<WorkflowGraph> {
        let graph = serde_json::from_str::<WorkflowGraph>(raw)?;
        graph.validate(&self.inner.registry)?;
        self.inner.graph_store.save(&graph)?;
        Ok(graph)
    }

    pub fn reset_graph(&self) -> Result<WorkflowGraph> {
        self.inner.graph_store.reset()?;
        let graph = self.inner.graph_store.load()?;
        graph.validate(&self.inner.registry)?;
        Ok(graph)
    }

    pub fn validate_graph_json(&self, raw: &str) -> Result<()> {
        let graph = serde_json::from_str::<WorkflowGraph>(raw)?;
        graph.validate(&self.inner.registry)
    }

    pub async fn list_runs(&self, limit: u64) -> Result<Vec<WorkflowRunInfo>> {
        self.inner.run_store.list_runs(limit).await
    }

    pub async fn list_run_nodes(&self, run_id: &str) -> Result<Vec<WorkflowNodeRunInfo>> {
        self.inner.run_store.list_nodes(run_id).await
    }
}

async fn execute_graph(
    inner: Arc<RuntimeInner>,
    run_id: &str,
    graph: WorkflowGraph,
    request: InferRequest,
    meta: RequestMeta,
    sink: Option<ChunkSink>,
) -> Result<ModelResponse> {
    info!(
        "工作流开始 run={} start={} nodes={}",
        run_id,
        graph.start,
        graph.nodes.len()
    );
    let mut ctx = WorkflowContext {
        router: inner.router.clone(),
        title: inner.title.clone(),
        request,
        meta,
        run_id: run_id.to_string(),
        title_request: false,
        session_plan: None,
        pending_record: None,
        route: None,
        result: None,
        client_sink: sink,
        request_id: None,
        channel: None,
        local_session_id: None,
    };

    let mut current = graph.start.clone();
    let mut seq = 0i64;
    let mut node_error = None;
    let mut steps = 0;
    let mut saved_request_id: Option<String> = None;

    loop {
        steps += 1;
        if steps > 128 {
            node_error = Some(anyhow!("工作流步骤超过上限"));
            break;
        }
        let Some(instance) = graph.node(&current).cloned() else {
            node_error = Some(anyhow!("工作流节点不存在：{current}"));
            break;
        };
        let Some(node) = inner.registry.get(&instance.kind) else {
            node_error = Some(anyhow!("工作流节点类型未注册：{}", instance.kind));
            break;
        };
        let started_at = chrono::Utc::now().timestamp_millis();
        let outcome = node.run(&mut ctx, &instance.config).await;
        let finished_at = chrono::Utc::now().timestamp_millis();
        let duration = (finished_at - started_at).max(0);
        seq += 1;
        let (status, output_port, node_error_text) = match &outcome {
            Ok(NodeOutcome::Next) => ("success", Some("next".to_string()), None),
            Ok(NodeOutcome::Port(port)) => ("success", Some(port.clone()), None),
            Ok(NodeOutcome::End) => ("success", None, None),
            Err(error) => ("error", None, Some(format!("{error:#}"))),
        };
        if let Err(error) = inner
            .run_store
            .insert_node(
                run_id,
                &instance.id,
                &instance.kind,
                seq,
                status,
                started_at,
                finished_at,
                duration,
                output_port.clone(),
                node_error_text.clone(),
            )
            .await
        {
            warn!("写入工作流节点记录失败 run={} err={}", run_id, error);
        }
        if let Some(request_id) = ctx.request_id.clone() {
            if saved_request_id.as_deref() != Some(request_id.as_str()) {
                if let Err(error) = inner.run_store.set_request_id(run_id, &request_id).await {
                    warn!("写入工作流 request_id 失败 run={} err={}", run_id, error);
                }
                saved_request_id = Some(request_id);
            }
        }
        match outcome {
            Ok(NodeOutcome::End) => break,
            Ok(NodeOutcome::Next) => {
                let Some(next) = graph.outgoing(&current, "next") else {
                    node_error = Some(anyhow!("节点 {} 没有 next 出边", current));
                    break;
                };
                current = next.to_string();
            }
            Ok(NodeOutcome::Port(port)) => {
                let Some(next) = graph.outgoing(&current, &port) else {
                    node_error = Some(anyhow!("节点 {} 的端口 {} 没有出边", current, port));
                    break;
                };
                current = next.to_string();
            }
            Err(error) => {
                node_error = Some(error);
                break;
            }
        }
    }

    let mut result = ctx
        .result
        .take()
        .unwrap_or_else(|| Err(anyhow!("工作流结束但没有产生结果")));
    if let Some(error) = node_error {
        result = Err(error);
    }
    let (prompt_tokens, completion_tokens) = usage_tokens(&result);
    let status = if result.is_ok() { "success" } else { "error" };
    let error = result
        .as_ref()
        .err()
        .map(|error| format!("{error:#}"));
    if let Err(store_error) = inner
        .run_store
        .finish_run(
            run_id,
            status,
            ctx.request_id.clone(),
            ctx.channel.clone(),
            ctx.local_session_id.clone(),
            prompt_tokens,
            completion_tokens,
            error,
        )
        .await
    {
        warn!("写入工作流运行结果失败 run={} err={}", run_id, store_error);
    }
    info!(
        "工作流结束 run={} status={} seq={}",
        run_id, status, seq
    );
    result
}

fn usage_tokens(result: &Result<ModelResponse>) -> (Option<i64>, Option<i64>) {
    let Ok(response) = result else {
        return (None, None);
    };
    let Some(usage) = &response.usage else {
        return (None, None);
    };
    (
        usage.get("prompt_tokens").and_then(|value| value.as_i64()),
        usage
            .get("completion_tokens")
            .and_then(|value| value.as_i64()),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use anyhow::Result;
    use futures_util::future::BoxFuture;
    use serde_json::{json, Value};

    use super::WorkflowRuntime;
    use crate::chat_router::ChatRouter;
    use crate::models::defs::{
        ChunkSink, Concurrency, InferRequest, ModelCapability, ModelResponse,
    };
    use crate::models::model::Model;
    use crate::models::plugin::PluginModel;
    use crate::providers::provider_control::ProviderControl;
    use crate::request_record::RequestMeta;
    use crate::workflow::{WorkflowGraphStore, WorkflowRunStore};

    struct MockModel;

    impl Model for MockModel {
        fn id(&self) -> &str {
            "deepseek-web"
        }

        fn capability(&self) -> ModelCapability {
            ModelCapability::basic()
        }

        fn concurrency(&self) -> Concurrency {
            Concurrency::single()
        }

        fn infer<'a>(&'a self, _request: InferRequest) -> BoxFuture<'a, Result<ModelResponse>> {
            Box::pin(async {
                Ok(ModelResponse {
                    text: "ok".to_string(),
                    ..ModelResponse::default()
                })
            })
        }

        fn infer_stream<'a>(
            &'a self,
            _request: InferRequest,
            mut on_chunk: ChunkSink,
        ) -> BoxFuture<'a, Result<ModelResponse>> {
            Box::pin(async move {
                on_chunk(json!({ "RESPONSE": "ok" }).to_string());
                Ok(ModelResponse {
                    text: "ok".to_string(),
                    ..ModelResponse::default()
                })
            })
        }
    }

    impl PluginModel for MockModel {
        fn channel(&self) -> &str {
            "deepseek-web"
        }

        fn create_channel_session(&self) -> BoxFuture<'_, Result<Option<String>>> {
            Box::pin(async { Ok(None) })
        }

        fn list_channel_sessions(&self) -> BoxFuture<'_, Result<Value>> {
            Box::pin(async { Ok(json!([])) })
        }

        fn delete_channel_session<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<bool>> {
            Box::pin(async { Ok(true) })
        }

        fn rename_channel_session<'a>(
            &'a self,
            _id: &'a str,
            _title: &'a str,
        ) -> BoxFuture<'a, Result<bool>> {
            Box::pin(async { Ok(true) })
        }

        fn channel_history<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<Value>> {
            Box::pin(async { Ok(json!({})) })
        }
    }

    #[tokio::test]
    async fn default_workflow_runs_and_persists_nodes() {
        let path = std::env::temp_dir().join(format!(
            "poolman-workflow-run-{}.db",
            uuid::Uuid::new_v4()
        ));
        let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
        let db = sea_orm::Database::connect(&url)
            .await
            .expect("初始化测试数据库失败");
        let run_store = Arc::new(
            WorkflowRunStore::new(db)
                .await
                .expect("初始化工作流运行表失败"),
        );
        let graph_store = Arc::new(WorkflowGraphStore::new(
            path.with_extension("workflow.json"),
        ));
        let router = Arc::new(ChatRouter::new(
            vec![Arc::new(MockModel)],
            Arc::new(ProviderControl::new(HashMap::new())),
        ));
        let runtime = WorkflowRuntime::new(router, graph_store, run_store.clone(), 1);
        let mut request = InferRequest::new(
            "deepseek-web",
            vec![json!({ "role": "user", "content": "你好" })],
        );
        request.api_key = Some("sk-test".to_string());
        let response = runtime
            .chat_completions(
                request,
                RequestMeta {
                    protocol: "openai".to_string(),
                    method: "POST".to_string(),
                    endpoint: "/v1/chat/completions".to_string(),
                    headers: json!({}),
                },
            )
            .await
            .expect("工作流应成功");
        assert_eq!(response.text, "ok");

        let runs = run_store.list_runs(10).await.expect("读取运行记录失败");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "success");
        let nodes = run_store
            .list_nodes(&runs[0].id)
            .await
            .expect("读取节点记录失败");
        assert!(nodes.iter().any(|node| node.node_id == "channel_send"));
        assert!(nodes.iter().any(|node| node.node_kind == "finalize"));

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn title_branch_skips_session_plan_and_records_run() {
        let path = std::env::temp_dir().join(format!(
            "poolman-workflow-title-{}.db",
            uuid::Uuid::new_v4()
        ));
        let url = format!("sqlite://{}?mode=rwc", path.to_string_lossy());
        let db = sea_orm::Database::connect(&url)
            .await
            .expect("初始化测试数据库失败");
        let run_store = Arc::new(
            WorkflowRunStore::new(db)
                .await
                .expect("初始化工作流运行表失败"),
        );
        let graph_store = Arc::new(WorkflowGraphStore::new(
            path.with_extension("workflow.json"),
        ));
        let router = Arc::new(ChatRouter::new(
            vec![Arc::new(MockModel)],
            Arc::new(ProviderControl::new(HashMap::new())),
        ));
        let runtime = WorkflowRuntime::new(router, graph_store, run_store.clone(), 1);
        let mut request = InferRequest::new(
            "deepseek-web",
            vec![
                json!({
                    "role": "system",
                    "content": "Create a concise title for an AI coding-assistant session."
                }),
                json!({
                    "role": "user",
                    "content": "Generate the session title from this JSON array of human messages: []"
                }),
            ],
        );
        request.api_key = Some("sk-test".to_string());
        runtime
            .chat_completions(
                request,
                RequestMeta {
                    protocol: "openai".to_string(),
                    method: "POST".to_string(),
                    endpoint: "/v1/chat/completions".to_string(),
                    headers: json!({}),
                },
            )
            .await
            .expect("标题工作流应成功");
        let runs = run_store.list_runs(10).await.expect("读取运行记录失败");
        let nodes = run_store
            .list_nodes(&runs[0].id)
            .await
            .expect("读取节点记录失败");
        assert!(nodes.iter().any(|node| node.node_id == "title_branch"));
        assert!(!nodes.iter().any(|node| node.node_id == "session_plan"));
        assert!(nodes.iter().any(|node| node.node_kind == "finalize"));

        let _ = std::fs::remove_file(path);
    }
}

