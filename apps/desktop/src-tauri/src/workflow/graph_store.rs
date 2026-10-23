//! 工作流图的文件存储：默认图来自代码，界面修改写入 `workflow.json`。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::workflow::graph::{default_graph, WorkflowGraph};

pub struct WorkflowGraphStore {
    path: PathBuf,
}

impl WorkflowGraphStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<WorkflowGraph> {
        if !self.path.exists() {
            return Ok(default_graph());
        }
        let raw = std::fs::read_to_string(&self.path)
            .with_context(|| format!("读取工作流文件失败：{}", self.path.display()))?;
        let graph = serde_json::from_str::<WorkflowGraph>(&raw)
            .with_context(|| format!("解析工作流文件失败：{}", self.path.display()))?;
        Ok(graph)
    }

    pub fn save(&self, graph: &WorkflowGraph) -> Result<()> {
        let raw = serde_json::to_string_pretty(graph)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = self.path.with_extension("json.tmp");
        std::fs::write(&temp, raw)?;
        // Windows 的 rename 在目标已存在时会失败，先移除再替换。
        if self.path.exists() {
            std::fs::remove_file(&self.path)?;
        }
        std::fs::rename(&temp, &self.path)?;
        Ok(())
    }

    pub fn reset(&self) -> Result<()> {
        if self.path.exists() {
            std::fs::remove_file(&self.path)?;
        }
        Ok(())
    }

    pub fn is_custom(&self) -> bool {
        self.path.exists()
    }
}
