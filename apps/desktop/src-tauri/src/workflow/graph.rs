//! 工作流图定义、默认图和校验。

use std::collections::{HashMap, HashSet};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::workflow::registry::NodeRegistry;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowGraph {
    #[serde(default = "default_version")]
    pub version: u32,
    pub start: String,
    pub nodes: Vec<NodeInstance>,
    pub edges: Vec<Edge>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInstance {
    pub id: String,
    pub kind: String,
    #[serde(default = "empty_config")]
    pub config: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    #[serde(default = "default_port")]
    pub port: String,
    pub to: String,
}

fn default_version() -> u32 {
    1
}

fn default_port() -> String {
    "next".to_string()
}

fn empty_config() -> Value {
    json!({})
}

impl WorkflowGraph {
    pub fn node(&self, id: &str) -> Option<&NodeInstance> {
        self.nodes.iter().find(|node| node.id == id)
    }

    pub fn outgoing(&self, from: &str, port: &str) -> Option<&str> {
        self.edges
            .iter()
            .find(|edge| edge.from == from && edge.port == port)
            .map(|edge| edge.to.as_str())
    }

    pub fn validate(&self, registry: &NodeRegistry) -> Result<()> {
        if self.start.trim().is_empty() {
            bail!("工作流缺少 start 节点");
        }
        if self.node(&self.start).is_none() {
            bail!("start 节点 {} 不存在", self.start);
        }
        let mut ids = HashSet::new();
        for node in &self.nodes {
            if node.id.trim().is_empty() {
                bail!("存在空节点 id");
            }
            if !ids.insert(node.id.as_str()) {
                bail!("节点 id 重复：{}", node.id);
            }
            if !registry.contains(&node.kind) {
                bail!("节点 {} 的类型 {} 未注册", node.id, node.kind);
            }
        }
        let mut used_ports = HashSet::new();
        for edge in &self.edges {
            if self.node(&edge.from).is_none() {
                bail!("边起点 {} 不存在", edge.from);
            }
            if self.node(&edge.to).is_none() {
                bail!("边终点 {} 不存在", edge.to);
            }
            let from = self.node(&edge.from).expect("已校验");
            let descriptor = registry
                .descriptor(&from.kind)
                .ok_or_else(|| anyhow::anyhow!("节点类型 {} 未注册", from.kind))?;
            if !descriptor
                .outputs
                .iter()
                .any(|port| port.name == edge.port)
            {
                bail!(
                    "节点 {}（{}）没有输出端口 {}",
                    edge.from,
                    from.kind,
                    edge.port
                );
            }
            if !used_ports.insert((edge.from.as_str(), edge.port.as_str())) {
                bail!("节点 {} 的端口 {} 有多条出边", edge.from, edge.port);
            }
        }
        // 每个输出端口都必须有出边，避免运行时走到死路。
        for node in &self.nodes {
            let Some(descriptor) = registry.descriptor(&node.kind) else {
                continue;
            };
            for port in &descriptor.outputs {
                if !used_ports.contains(&(node.id.as_str(), port.name.as_str())) {
                    bail!("节点 {} 的端口 {} 没有出边", node.id, port.name);
                }
            }
        }
        // 只允许 DAG：分支和循环节点不能形成环，bootstrap 循环留在节点内部。
        self.ensure_acyclic()?;
        Ok(())
    }

    fn ensure_acyclic(&self) -> Result<()> {
        let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
        for edge in &self.edges {
            adjacency
                .entry(edge.from.as_str())
                .or_default()
                .push(edge.to.as_str());
        }
        let mut visiting = HashSet::new();
        let mut visited = HashSet::new();
        for node in &self.nodes {
            self.visit(&node.id, &adjacency, &mut visiting, &mut visited)?;
        }
        Ok(())
    }

    fn visit<'a>(
        &self,
        node: &'a str,
        adjacency: &HashMap<&'a str, Vec<&'a str>>,
        visiting: &mut HashSet<&'a str>,
        visited: &mut HashSet<&'a str>,
    ) -> Result<()> {
        if visited.contains(node) {
            return Ok(());
        }
        if !visiting.insert(node) {
            bail!("工作流存在环：{}", node);
        }
        if let Some(next) = adjacency.get(node) {
            for target in next {
                self.visit(target, adjacency, visiting, visited)?;
            }
        }
        visiting.remove(node);
        visited.insert(node);
        Ok(())
    }
}

/// 代码内置的默认工作流。
pub fn default_graph() -> WorkflowGraph {
    WorkflowGraph {
        version: 1,
        start: "ingress".to_string(),
        nodes: vec![
            node("ingress", "ingress"),
            node("auth", "auth"),
            node("normalize", "normalize"),
            node("title_branch", "title_branch"),
            node("title_record", "record_begin"),
            node("title_generate", "title_generate"),
            node("session_plan", "session_plan"),
            node("record_begin", "record_begin"),
            node("route", "route"),
            node("channel_prepare", "channel_prepare"),
            node("channel_send", "channel_send"),
            node("marker", "marker"),
            node("finalize", "finalize"),
        ],
        edges: vec![
            edge("ingress", "next", "auth"),
            edge("auth", "next", "normalize"),
            edge("normalize", "next", "title_branch"),
            // 标题请求：先拍记录，再尝试本地生成；生成不出来交给 route 走网页渠道。
            edge("title_branch", "title", "title_record"),
            edge("title_record", "next", "title_generate"),
            edge("title_generate", "local", "finalize"),
            edge("title_generate", "web", "route"),
            edge("title_branch", "normal", "session_plan"),
            edge("session_plan", "next", "record_begin"),
            edge("record_begin", "next", "route"),
            edge("route", "next", "channel_prepare"),
            edge("channel_prepare", "next", "channel_send"),
            edge("channel_prepare", "error", "finalize"),
            edge("channel_send", "next", "marker"),
            edge("marker", "next", "finalize"),
        ],
    }
}

fn node(id: &str, kind: &str) -> NodeInstance {
    NodeInstance {
        id: id.to_string(),
        kind: kind.to_string(),
        config: json!({}),
    }
}

fn edge(from: &str, port: &str, to: &str) -> Edge {
    Edge {
        from: from.to_string(),
        port: port.to_string(),
        to: to.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::default_graph;
    use crate::workflow::nodes::build_registry;

    #[test]
    fn default_graph_is_valid() {
        let registry = build_registry();
        default_graph().validate(&registry).expect("默认图应有效");
    }

    #[test]
    fn rejects_unknown_port() {
        let registry = build_registry();
        let mut graph = default_graph();
        graph.edges[0].port = "missing".to_string();
        assert!(graph.validate(&registry).is_err());
    }

    #[test]
    fn rejects_cycle() {
        let registry = build_registry();
        let mut graph = default_graph();
        let edge = graph
            .edges
            .iter_mut()
            .find(|edge| edge.from == "title_branch" && edge.port == "title")
            .expect("默认图应有 title 分支");
        edge.to = "title_branch".to_string();
        assert!(graph.validate(&registry).is_err());
    }

    #[test]
    fn default_graph_has_title_branch() {
        let graph = default_graph();
        assert_eq!(
            graph.outgoing("title_branch", "title"),
            Some("title_record")
        );
        assert_eq!(
            graph.outgoing("title_branch", "normal"),
            Some("session_plan")
        );
        // 标题本地生成成功直接收尾，失败回落到网页渠道的 route。
        assert_eq!(graph.outgoing("title_generate", "local"), Some("finalize"));
        assert_eq!(graph.outgoing("title_generate", "web"), Some("route"));
    }
}

