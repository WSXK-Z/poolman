//! 工作流运行状态持久化：每次请求一条 run，每个执行节点一条 node run。

use anyhow::Result;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, Set,
};
use serde::Serialize;

use crate::entitys::workflow_node_run;
use crate::entitys::workflow_run;

pub struct WorkflowRunStore {
    db: DatabaseConnection,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowRunInfo {
    pub id: String,
    pub request_id: Option<String>,
    pub workflow_version: String,
    pub status: String,
    pub protocol: String,
    pub model: String,
    pub channel: Option<String>,
    pub stream: bool,
    pub local_session_id: Option<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub duration_ms: Option<i64>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowNodeRunInfo {
    pub id: i64,
    pub run_id: String,
    pub node_id: String,
    pub node_kind: String,
    pub seq: i64,
    pub status: String,
    pub started_at: i64,
    pub finished_at: i64,
    pub duration_ms: i64,
    pub output_port: Option<String>,
    pub error: Option<String>,
}

impl WorkflowRunStore {
    pub async fn new(db: DatabaseConnection) -> Result<Self> {
        db.get_schema_builder()
            .register(workflow_run::Entity)
            .register(workflow_node_run::Entity)
            .sync(&db)
            .await?;
        Ok(Self { db })
    }

    pub async fn create_run(
        &self,
        id: &str,
        protocol: &str,
        model: &str,
        stream: bool,
        graph_json: &str,
        workflow_version: &str,
    ) -> Result<()> {
        workflow_run::ActiveModel {
            id: Set(id.to_string()),
            request_id: Set(None),
            workflow_version: Set(workflow_version.to_string()),
            graph_json: Set(graph_json.to_string()),
            status: Set("queued".to_string()),
            protocol: Set(protocol.to_string()),
            model: Set(model.to_string()),
            channel: Set(None),
            stream: Set(stream),
            local_session_id: Set(None),
            started_at: Set(chrono::Utc::now().timestamp_millis()),
            finished_at: Set(None),
            duration_ms: Set(None),
            prompt_tokens: Set(None),
            completion_tokens: Set(None),
            error: Set(None),
        }
        .insert(&self.db)
        .await?;
        Ok(())
    }

    pub async fn mark_running(&self, id: &str) -> Result<()> {
        let Some(row) = workflow_run::Entity::find_by_id(id.to_string())
            .one(&self.db)
            .await?
        else {
            return Ok(());
        };
        let mut active: workflow_run::ActiveModel = row.into();
        active.status = Set("running".to_string());
        active.started_at = Set(chrono::Utc::now().timestamp_millis());
        active.update(&self.db).await?;
        Ok(())
    }

    pub async fn set_request_id(&self, id: &str, request_id: &str) -> Result<()> {
        let Some(row) = workflow_run::Entity::find_by_id(id.to_string())
            .one(&self.db)
            .await?
        else {
            return Ok(());
        };
        let mut active: workflow_run::ActiveModel = row.into();
        active.request_id = Set(Some(request_id.to_string()));
        active.update(&self.db).await?;
        Ok(())
    }

    pub async fn finish_run(
        &self,
        id: &str,
        status: &str,
        request_id: Option<String>,
        channel: Option<String>,
        local_session_id: Option<String>,
        prompt_tokens: Option<i64>,
        completion_tokens: Option<i64>,
        error: Option<String>,
    ) -> Result<()> {
        let Some(row) = workflow_run::Entity::find_by_id(id.to_string())
            .one(&self.db)
            .await?
        else {
            return Ok(());
        };
        let now = chrono::Utc::now().timestamp_millis();
        let duration = (now - row.started_at).max(0);
        let mut active: workflow_run::ActiveModel = row.into();
        active.status = Set(status.to_string());
        active.finished_at = Set(Some(now));
        active.duration_ms = Set(Some(duration));
        if request_id.is_some() {
            active.request_id = Set(request_id);
        }
        if channel.is_some() {
            active.channel = Set(channel);
        }
        if local_session_id.is_some() {
            active.local_session_id = Set(local_session_id);
        }
        active.prompt_tokens = Set(prompt_tokens);
        active.completion_tokens = Set(completion_tokens);
        active.error = Set(error);
        active.update(&self.db).await?;
        Ok(())
    }

    pub async fn insert_node(
        &self,
        run_id: &str,
        node_id: &str,
        node_kind: &str,
        seq: i64,
        status: &str,
        started_at: i64,
        finished_at: i64,
        duration_ms: i64,
        output_port: Option<String>,
        error: Option<String>,
    ) -> Result<()> {
        workflow_node_run::ActiveModel {
            id: sea_orm::ActiveValue::NotSet,
            run_id: Set(run_id.to_string()),
            node_id: Set(node_id.to_string()),
            node_kind: Set(node_kind.to_string()),
            seq: Set(seq),
            status: Set(status.to_string()),
            started_at: Set(started_at),
            finished_at: Set(finished_at),
            duration_ms: Set(duration_ms),
            output_port: Set(output_port),
            error: Set(error),
        }
        .insert(&self.db)
        .await?;
        Ok(())
    }

    pub async fn list_runs(&self, limit: u64) -> Result<Vec<WorkflowRunInfo>> {
        let rows = workflow_run::Entity::find()
            .order_by_desc(workflow_run::Column::StartedAt)
            .order_by_desc(workflow_run::Column::Id)
            .all(&self.db)
            .await?;
        Ok(rows
            .into_iter()
            .take(limit as usize)
            .map(|row| WorkflowRunInfo {
                id: row.id,
                request_id: row.request_id,
                workflow_version: row.workflow_version,
                status: row.status,
                protocol: row.protocol,
                model: row.model,
                channel: row.channel,
                stream: row.stream,
                local_session_id: row.local_session_id,
                started_at: row.started_at,
                finished_at: row.finished_at,
                duration_ms: row.duration_ms,
                prompt_tokens: row.prompt_tokens,
                completion_tokens: row.completion_tokens,
                error: row.error,
            })
            .collect())
    }

    pub async fn list_nodes(&self, run_id: &str) -> Result<Vec<WorkflowNodeRunInfo>> {
        let rows = workflow_node_run::Entity::find()
            .filter(workflow_node_run::Column::RunId.eq(run_id))
            .order_by_asc(workflow_node_run::Column::Seq)
            .all(&self.db)
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| WorkflowNodeRunInfo {
                id: row.id,
                run_id: row.run_id,
                node_id: row.node_id,
                node_kind: row.node_kind,
                seq: row.seq,
                status: row.status,
                started_at: row.started_at,
                finished_at: row.finished_at,
                duration_ms: row.duration_ms,
                output_port: row.output_port,
                error: row.error,
            })
            .collect())
    }
}
