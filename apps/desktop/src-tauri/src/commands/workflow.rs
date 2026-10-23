//! 工作流图与运行状态命令。

use std::sync::Arc;

use serde::Serialize;

use crate::handlers::{command_set, CommandSet};
use crate::workflow::node::NodeDescriptor;
use crate::workflow::store::{WorkflowNodeRunInfo, WorkflowRunInfo};
use crate::workflow::{WorkflowGraph, WorkflowRuntime};

pub(super) fn handler() -> CommandSet<tauri::Wry> {
    command_set![
        get_workflow,
        save_workflow_graph,
        validate_workflow_graph,
        reset_workflow_graph,
        list_workflow_runs,
        list_workflow_run_nodes
    ]
}

#[derive(Debug, Serialize)]
pub struct WorkflowSnapshot {
    pub graph: WorkflowGraph,
    pub graph_json: String,
    pub nodes: Vec<NodeDescriptor>,
    pub is_custom: bool,
}

fn snapshot(runtime: &WorkflowRuntime) -> Result<WorkflowSnapshot, String> {
    let graph = runtime.graph().map_err(|e| e.to_string())?;
    let graph_json = runtime.graph_json().map_err(|e| e.to_string())?;
    Ok(WorkflowSnapshot {
        graph,
        graph_json,
        nodes: runtime.node_descriptors(),
        is_custom: runtime.is_custom_graph(),
    })
}

/// 当前工作流图、代码注册的节点描述和是否使用了界面保存的图。
#[tauri::command]
pub fn get_workflow(
    runtime: tauri::State<'_, Arc<WorkflowRuntime>>,
) -> Result<WorkflowSnapshot, String> {
    snapshot(&runtime)
}

/// 保存界面编辑的工作流图；保存前校验节点、端口和 DAG。
#[tauri::command]
pub fn save_workflow_graph(
    runtime: tauri::State<'_, Arc<WorkflowRuntime>>,
    graph_json: String,
) -> Result<WorkflowSnapshot, String> {
    runtime
        .save_graph_json(&graph_json)
        .map_err(|e| e.to_string())?;
    snapshot(&runtime)
}

/// 只校验，不保存。
#[tauri::command]
pub fn validate_workflow_graph(
    runtime: tauri::State<'_, Arc<WorkflowRuntime>>,
    graph_json: String,
) -> Result<(), String> {
    runtime
        .validate_graph_json(&graph_json)
        .map_err(|e| e.to_string())
}

/// 删除界面保存的图，恢复代码默认工作流。
#[tauri::command]
pub fn reset_workflow_graph(
    runtime: tauri::State<'_, Arc<WorkflowRuntime>>,
) -> Result<WorkflowSnapshot, String> {
    runtime.reset_graph().map_err(|e| e.to_string())?;
    snapshot(&runtime)
}

/// 最近的工作流运行记录。
#[tauri::command]
pub async fn list_workflow_runs(
    runtime: tauri::State<'_, Arc<WorkflowRuntime>>,
    limit: Option<u64>,
) -> Result<Vec<WorkflowRunInfo>, String> {
    runtime
        .list_runs(limit.unwrap_or(200))
        .await
        .map_err(|e| e.to_string())
}

/// 某次运行的节点时间线。
#[tauri::command]
pub async fn list_workflow_run_nodes(
    runtime: tauri::State<'_, Arc<WorkflowRuntime>>,
    run_id: String,
) -> Result<Vec<WorkflowNodeRunInfo>, String> {
    runtime
        .list_run_nodes(&run_id)
        .await
        .map_err(|e| e.to_string())
}
