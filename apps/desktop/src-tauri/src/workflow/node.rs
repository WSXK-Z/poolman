//! 工作流节点契约。

use anyhow::Result;
use futures_util::future::BoxFuture;
use serde::Serialize;
use serde_json::Value;

use crate::workflow::context::WorkflowContext;

#[derive(Debug, Clone, Serialize)]
pub struct PortDef {
    pub name: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeDescriptor {
    pub kind: &'static str,
    pub label: &'static str,
    pub category: &'static str,
    pub outputs: Vec<PortDef>,
    pub config_schema: Value,
}

#[derive(Debug, Clone)]
pub enum NodeOutcome {
    Next,
    Port(String),
    End,
}

pub trait WorkflowNode: Send + Sync {
    fn descriptor(&self) -> NodeDescriptor;

    fn run<'a>(
        &'a self,
        ctx: &'a mut WorkflowContext,
        config: &'a Value,
    ) -> BoxFuture<'a, Result<NodeOutcome>>;
}

pub fn next_port() -> PortDef {
    PortDef {
        name: "next".to_string(),
        label: "下一步".to_string(),
    }
}
