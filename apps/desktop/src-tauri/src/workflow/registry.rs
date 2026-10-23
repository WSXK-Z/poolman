//! 工作流节点注册表：节点实现在代码里注册，界面只读取描述。

use std::collections::HashMap;
use std::sync::Arc;

use crate::workflow::node::{NodeDescriptor, WorkflowNode};

#[derive(Default)]
pub struct NodeRegistry {
    nodes: HashMap<&'static str, Arc<dyn WorkflowNode>>,
}

impl NodeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<N: WorkflowNode + 'static>(&mut self, node: N) {
        let kind = node.descriptor().kind;
        self.nodes.insert(kind, Arc::new(node));
    }

    pub fn contains(&self, kind: &str) -> bool {
        self.nodes.contains_key(kind)
    }

    pub fn get(&self, kind: &str) -> Option<Arc<dyn WorkflowNode>> {
        self.nodes.get(kind).cloned()
    }

    pub fn descriptor(&self, kind: &str) -> Option<NodeDescriptor> {
        self.nodes.get(kind).map(|node| node.descriptor())
    }

    pub fn descriptors(&self) -> Vec<NodeDescriptor> {
        let mut descriptors = self
            .nodes
            .values()
            .map(|node| node.descriptor())
            .collect::<Vec<_>>();
        descriptors.sort_by(|left, right| left.kind.cmp(right.kind));
        descriptors
    }
}
