//! 默认工作流使用的节点实现。

use anyhow::{bail, Result};
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use tracing::info;

use crate::chat_router::ChatRouter;
use crate::models::defs::ModelResponse;
use crate::request_record::detect_title_request;
use crate::title_engine::TitleSource;
use crate::workflow::context::WorkflowContext;
use crate::workflow::node::{next_port, NodeDescriptor, NodeOutcome, PortDef, WorkflowNode};
use crate::workflow::registry::NodeRegistry;

fn descriptor(
    kind: &'static str,
    label: &'static str,
    category: &'static str,
    outputs: Vec<PortDef>,
) -> NodeDescriptor {
    NodeDescriptor {
        kind,
        label,
        category,
        outputs,
        config_schema: json!({}),
    }
}

pub struct IngressNode;
pub struct AuthNode;
pub struct NormalizeNode;
pub struct TitleBranchNode;
pub struct TitleGenerateNode;
pub struct SessionPlanNode;
pub struct RecordBeginNode;
pub struct RouteNode;
pub struct ChannelPrepareNode;
pub struct ChannelSendNode;
pub struct MarkerNode;
pub struct FinalizeNode;

impl WorkflowNode for IngressNode {
    fn descriptor(&self) -> NodeDescriptor {
        descriptor("ingress", "接收请求", "入口", vec![next_port()])
    }

    fn run<'a>(
        &'a self,
        _ctx: &'a mut WorkflowContext,
        _config: &'a Value,
    ) -> BoxFuture<'a, Result<NodeOutcome>> {
        Box::pin(async { Ok(NodeOutcome::Next) })
    }
}

impl WorkflowNode for AuthNode {
    fn descriptor(&self) -> NodeDescriptor {
        descriptor("auth", "校验调用方", "入口", vec![next_port()])
    }

    fn run<'a>(
        &'a self,
        ctx: &'a mut WorkflowContext,
        _config: &'a Value,
    ) -> BoxFuture<'a, Result<NodeOutcome>> {
        Box::pin(async move {
            if ctx
                .request
                .api_key
                .as_deref()
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .is_none()
            {
                bail!("缺少调用方 token，无法执行工作流");
            }
            Ok(NodeOutcome::Next)
        })
    }
}

impl WorkflowNode for NormalizeNode {
    fn descriptor(&self) -> NodeDescriptor {
        descriptor("normalize", "协议规范化", "入口", vec![next_port()])
    }

    fn run<'a>(
        &'a self,
        _ctx: &'a mut WorkflowContext,
        _config: &'a Value,
    ) -> BoxFuture<'a, Result<NodeOutcome>> {
        Box::pin(async { Ok(NodeOutcome::Next) })
    }
}

impl WorkflowNode for TitleBranchNode {
    fn descriptor(&self) -> NodeDescriptor {
        descriptor(
            "title_branch",
            "标题请求分支",
            "分支",
            vec![
                PortDef {
                    name: "title".to_string(),
                    label: "标题生成".to_string(),
                },
                PortDef {
                    name: "normal".to_string(),
                    label: "正常对话".to_string(),
                },
            ],
        )
    }

    fn run<'a>(
        &'a self,
        ctx: &'a mut WorkflowContext,
        _config: &'a Value,
    ) -> BoxFuture<'a, Result<NodeOutcome>> {
        Box::pin(async move {
            let title = detect_title_request(&ctx.request.messages, ctx.request.tools.as_ref());
            ctx.title_request = title;
            Ok(NodeOutcome::Port(
                if title { "title" } else { "normal" }.to_string(),
            ))
        })
    }
}

impl WorkflowNode for TitleGenerateNode {
    fn descriptor(&self) -> NodeDescriptor {
        descriptor(
            "title_generate",
            "生成会话标题",
            "标题",
            vec![
                PortDef {
                    name: "local".to_string(),
                    label: "本地生成".to_string(),
                },
                PortDef {
                    name: "web".to_string(),
                    label: "交给网页渠道".to_string(),
                },
            ],
        )
    }

    fn run<'a>(
        &'a self,
        ctx: &'a mut WorkflowContext,
        _config: &'a Value,
    ) -> BoxFuture<'a, Result<NodeOutcome>> {
        Box::pin(async move {
            let source = ctx.title.source();
            if source == TitleSource::Web {
                return Ok(NodeOutcome::Port("web".to_string()));
            }

            // 选了「本地模型」就先跑模型：这样界面上的切换是真正的 A/B，
            // 而不是"启发式成功就永远轮不到模型"。
            if source == TitleSource::Local {
                if let Some(dir) = ctx.title.model_path() {
                    let conversation =
                        crate::title_engine::conversation_for_model(&ctx.request.messages);
                    let generated = tokio::task::spawn_blocking(move || {
                        let model = crate::title_model::load_model(&dir)?;
                        model.title(&conversation)
                    })
                    .await;
                    match generated {
                        Ok(Ok(raw)) => {
                            if let Some(title) =
                                crate::title_engine::sanitize_generated_title(&raw)
                            {
                                info!(
                                    "标题由本地模型生成 request={:?} title={}",
                                    ctx.request_id, title
                                );
                                ctx.result = Some(Ok(ModelResponse {
                                    text: title,
                                    finish_reason: Some("stop".to_string()),
                                    is_complete: true,
                                    ..ModelResponse::default()
                                }));
                                return Ok(NodeOutcome::Port("local".to_string()));
                            }
                            info!("本地模型输出无法用作标题，尝试启发式");
                        }
                        Ok(Err(error)) => {
                            info!("本地模型推理失败，尝试启发式：{}", error)
                        }
                        Err(error) => info!("本地模型任务异常，尝试启发式：{}", error),
                    }
                } else {
                    info!("未配置本地模型目录，尝试启发式");
                }
            }

            // 启发式：零依赖、确定性，能拿出内容就不必再走网页渠道。
            if let Some(title) = crate::title_engine::heuristic_title(&ctx.request.messages) {
                info!(
                    "标题由启发式生成 source={:?} request={:?} title={}",
                    source, ctx.request_id, title
                );
                ctx.result = Some(Ok(ModelResponse {
                    text: title,
                    finish_reason: Some("stop".to_string()),
                    is_complete: true,
                    ..ModelResponse::default()
                }));
                return Ok(NodeOutcome::Port("local".to_string()));
            }

            info!(
                "标题回落网页渠道 source={:?} has_model={}",
                source,
                ctx.title.has_local_model()
            );
            Ok(NodeOutcome::Port("web".to_string()))
        })
    }
}

impl WorkflowNode for SessionPlanNode {
    fn descriptor(&self) -> NodeDescriptor {
        descriptor("session_plan", "本地会话计划", "会话", vec![next_port()])
    }

    fn run<'a>(
        &'a self,
        ctx: &'a mut WorkflowContext,
        _config: &'a Value,
    ) -> BoxFuture<'a, Result<NodeOutcome>> {
        Box::pin(async move {
            let plan = ctx.router.plan_session_for(&ctx.request).await;
            if let Some(plan) = &plan {
                ctx.request.web_context_key = Some(plan.session_id.clone());
                ctx.local_session_id = Some(plan.session_id.clone());
            }
            ctx.session_plan = plan;
            Ok(NodeOutcome::Next)
        })
    }
}

impl WorkflowNode for RecordBeginNode {
    fn descriptor(&self) -> NodeDescriptor {
        descriptor("record_begin", "请求记录快照", "持久化", vec![next_port()])
    }

    fn run<'a>(
        &'a self,
        ctx: &'a mut WorkflowContext,
        _config: &'a Value,
    ) -> BoxFuture<'a, Result<NodeOutcome>> {
        Box::pin(async move {
            ctx.pending_record = ctx.router.begin_record(&ctx.request, &ctx.meta);
            if let Some((_, draft)) = &ctx.pending_record {
                ctx.request_id = Some(draft.id.clone());
            }
            Ok(NodeOutcome::Next)
        })
    }
}

impl WorkflowNode for RouteNode {
    fn descriptor(&self) -> NodeDescriptor {
        descriptor("route", "模型路由", "路由", vec![next_port()])
    }

    fn run<'a>(
        &'a self,
        ctx: &'a mut WorkflowContext,
        _config: &'a Value,
    ) -> BoxFuture<'a, Result<NodeOutcome>> {
        Box::pin(async move {
            let decision = ctx.router.resolve_route_for(&ctx.request);
            if let Ok(decision) = &decision {
                ctx.channel = Some(decision.channel.clone());
            }
            ctx.route = Some(decision);
            Ok(NodeOutcome::Next)
        })
    }
}

impl WorkflowNode for ChannelPrepareNode {
    fn descriptor(&self) -> NodeDescriptor {
        descriptor(
            "channel_prepare",
            "准备渠道请求",
            "渠道",
            vec![
                PortDef {
                    name: "next".to_string(),
                    label: "渠道可用".to_string(),
                },
                PortDef {
                    name: "error".to_string(),
                    label: "路由失败".to_string(),
                },
            ],
        )
    }

    fn run<'a>(
        &'a self,
        ctx: &'a mut WorkflowContext,
        _config: &'a Value,
    ) -> BoxFuture<'a, Result<NodeOutcome>> {
        Box::pin(async move {
            let unavailable = matches!(ctx.route.as_ref(), None | Some(Err(_)));
            if unavailable {
                let decision = ctx
                    .route
                    .take()
                    .unwrap_or_else(|| Err(anyhow::anyhow!("路由节点没有产出")));
                let error = match decision {
                    Ok(_) => anyhow::anyhow!("路由节点没有产出"),
                    Err(error) => error,
                };
                ctx.result = Some(Err(error));
                return Ok(NodeOutcome::Port("error".to_string()));
            }
            // 新会话标记必须在渠道调用前送出，保证流式客户端先看到标记。
            if let (Some(plan), Some(sink)) = (ctx.session_plan.as_ref(), ctx.client_sink.as_mut()) {
                if let Some(marker) = plan.marker.as_deref() {
                    sink(ChatRouter::marker_frame(marker));
                }
            }
            Ok(NodeOutcome::Next)
        })
    }
}

impl WorkflowNode for ChannelSendNode {
    fn descriptor(&self) -> NodeDescriptor {
        descriptor(
            "channel_send",
            "发送供应商渠道",
            "渠道",
            vec![next_port()],
        )
    }

    fn run<'a>(
        &'a self,
        ctx: &'a mut WorkflowContext,
        _config: &'a Value,
    ) -> BoxFuture<'a, Result<NodeOutcome>> {
        Box::pin(async move {
            let decision = ctx
                .route
                .take()
                .unwrap_or_else(|| Err(anyhow::anyhow!("路由节点没有产出")));
            let sink = ctx.client_sink.take();
            let result = ctx
                .router
                .dispatch_channel(ctx.request.clone(), decision, sink)
                .await;
            ctx.result = Some(result);
            Ok(NodeOutcome::Next)
        })
    }
}

impl WorkflowNode for MarkerNode {
    fn descriptor(&self) -> NodeDescriptor {
        descriptor("marker", "会话标记注入", "渠道", vec![next_port()])
    }

    fn run<'a>(
        &'a self,
        ctx: &'a mut WorkflowContext,
        _config: &'a Value,
    ) -> BoxFuture<'a, Result<NodeOutcome>> {
        Box::pin(async move {
            if let (Some(result), Some(plan)) =
                (&mut ctx.result, ctx.session_plan.as_ref())
            {
                if let (Ok(response), Some(marker)) = (result, plan.marker.as_deref()) {
                    response.text = ChatRouter::prepend_marker(&response.text, marker);
                }
            }
            Ok(NodeOutcome::Next)
        })
    }
}

impl WorkflowNode for FinalizeNode {
    fn descriptor(&self) -> NodeDescriptor {
        descriptor("finalize", "会话与记录收尾", "持久化", Vec::new())
    }

    fn run<'a>(
        &'a self,
        ctx: &'a mut WorkflowContext,
        _config: &'a Value,
    ) -> BoxFuture<'a, Result<NodeOutcome>> {
        Box::pin(async move {
            let result = ctx
                .result
                .take()
                .unwrap_or_else(|| Err(anyhow::anyhow!("渠道节点没有产出")));
            let pending = ctx.pending_record.take();
            let session_plan = ctx.session_plan.take();
            ctx.router
                .finish_prepared(pending, session_plan, &result)
                .await;
            ctx.result = Some(result);
            Ok(NodeOutcome::End)
        })
    }
}

/// 注册默认工作流用到的全部节点。
pub fn build_registry() -> NodeRegistry {
    let mut registry = NodeRegistry::new();
    registry.register(IngressNode);
    registry.register(AuthNode);
    registry.register(NormalizeNode);
    registry.register(TitleBranchNode);
    registry.register(TitleGenerateNode);
    registry.register(SessionPlanNode);
    registry.register(RecordBeginNode);
    registry.register(RouteNode);
    registry.register(ChannelPrepareNode);
    registry.register(ChannelSendNode);
    registry.register(MarkerNode);
    registry.register(FinalizeNode);
    registry
}
