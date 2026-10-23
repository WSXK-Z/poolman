<script setup lang="ts">
// 工作流：代码注册节点 + 可编辑图 + 运行状态。
import { computed, onMounted, ref } from "vue";
import { invoke } from "@tauri-apps/api/core";
import {
  NButton,
  NEmpty,
  NInput,
  NPopconfirm,
  NScrollbar,
  NSpin,
  NTag,
  useMessage,
} from "naive-ui";

interface PortDef {
  name: string;
  label: string;
}

interface NodeDescriptor {
  kind: string;
  label: string;
  category: string;
  outputs: PortDef[];
  config_schema: unknown;
}

interface WorkflowSnapshot {
  graph_json: string;
  nodes: NodeDescriptor[];
  is_custom: boolean;
}

interface WorkflowRunInfo {
  id: string;
  request_id: string | null;
  workflow_version: string;
  status: string;
  protocol: string;
  model: string;
  channel: string | null;
  stream: boolean;
  local_session_id: string | null;
  started_at: number;
  finished_at: number | null;
  duration_ms: number | null;
  prompt_tokens: number | null;
  completion_tokens: number | null;
  error: string | null;
}

interface WorkflowNodeRunInfo {
  id: number;
  run_id: string;
  node_id: string;
  node_kind: string;
  seq: number;
  status: string;
  started_at: number;
  finished_at: number;
  duration_ms: number;
  output_port: string | null;
  error: string | null;
}

interface PreviewEdge {
  port: string;
  to: string;
}

interface PreviewNode {
  id: string;
  kind: string;
  edges: PreviewEdge[];
}

const message = useMessage();
const graphJson = ref("");
const nodes = ref<NodeDescriptor[]>([]);
const isCustom = ref(false);
const runs = ref<WorkflowRunInfo[]>([]);
const selectedRun = ref<WorkflowRunInfo | null>(null);
const runNodes = ref<WorkflowNodeRunInfo[]>([]);
const loading = ref(false);
const saving = ref(false);
const validating = ref(false);
const runsLoading = ref(false);
const nodesLoading = ref(false);

const runCount = computed(() => runs.value.length);
const graphPreview = computed<PreviewNode[]>(() => {
  try {
    const graph = JSON.parse(graphJson.value) as {
      nodes?: Array<{ id?: unknown; kind?: unknown }>;
      edges?: Array<{ from?: unknown; port?: unknown; to?: unknown }>;
    };
    const edges = Array.isArray(graph.edges) ? graph.edges : [];
    return (Array.isArray(graph.nodes) ? graph.nodes : []).map((node) => ({
      id: String(node.id ?? ""),
      kind: String(node.kind ?? ""),
      edges: edges
        .filter((edge) => String(edge.from ?? "") === String(node.id ?? ""))
        .map((edge) => ({
          port: String(edge.port ?? "next"),
          to: String(edge.to ?? ""),
        })),
    }));
  } catch {
    return [];
  }
});

function formatTime(value: number | null): string {
  if (!value) return "—";
  return new Date(value).toLocaleString();
}

function formatDuration(value: number | null): string {
  if (value === null || !Number.isFinite(value) || value < 0) return "—";
  if (value < 1000) return `${Math.round(value)} ms`;
  return `${(value / 1000).toFixed(2)} s`;
}

function statusType(status: string): "success" | "error" | "warning" | "info" | "default" {
  switch (status) {
    case "success":
      return "success";
    case "error":
      return "error";
    case "running":
      return "warning";
    case "queued":
      return "info";
    default:
      return "default";
  }
}

function applySnapshot(snapshot: WorkflowSnapshot) {
  graphJson.value = snapshot.graph_json;
  nodes.value = snapshot.nodes;
  isCustom.value = snapshot.is_custom;
}

async function loadWorkflow() {
  loading.value = true;
  try {
    applySnapshot(await invoke<WorkflowSnapshot>("get_workflow"));
  } catch (error) {
    message.error(error instanceof Error ? error.message : String(error));
  } finally {
    loading.value = false;
  }
}

async function loadRuns() {
  runsLoading.value = true;
  try {
    runs.value = await invoke<WorkflowRunInfo[]>("list_workflow_runs", { limit: 200 });
    if (selectedRun.value) {
      const current = runs.value.find((run) => run.id === selectedRun.value?.id);
      if (current) selectedRun.value = current;
    }
  } catch (error) {
    message.error(error instanceof Error ? error.message : String(error));
  } finally {
    runsLoading.value = false;
  }
}

async function selectRun(run: WorkflowRunInfo) {
  selectedRun.value = run;
  nodesLoading.value = true;
  try {
    runNodes.value = await invoke<WorkflowNodeRunInfo[]>("list_workflow_run_nodes", {
      runId: run.id,
    });
  } catch (error) {
    message.error(error instanceof Error ? error.message : String(error));
  } finally {
    nodesLoading.value = false;
  }
}

async function validateGraph() {
  validating.value = true;
  try {
    await invoke("validate_workflow_graph", { graphJson: graphJson.value });
    message.success("工作流校验通过");
  } catch (error) {
    message.error(error instanceof Error ? error.message : String(error));
  } finally {
    validating.value = false;
  }
}

async function saveGraph() {
  saving.value = true;
  try {
    applySnapshot(
      await invoke<WorkflowSnapshot>("save_workflow_graph", { graphJson: graphJson.value }),
    );
    message.success("工作流已保存，下一次请求生效");
  } catch (error) {
    message.error(error instanceof Error ? error.message : String(error));
  } finally {
    saving.value = false;
  }
}

async function resetGraph() {
  saving.value = true;
  try {
    applySnapshot(await invoke<WorkflowSnapshot>("reset_workflow_graph"));
    message.success("已恢复默认工作流");
  } catch (error) {
    message.error(error instanceof Error ? error.message : String(error));
  } finally {
    saving.value = false;
  }
}

onMounted(() => {
  void loadWorkflow();
  void loadRuns();
});
</script>

<template>
  <div>
    <header class="page-header">
      <div>
        <p class="page-kicker">05 / Workflow</p>
        <h1 class="page-title">工作流</h1>
      </div>
      <div class="toolbar">
        <n-button quaternary :loading="validating" @click="validateGraph">校验</n-button>
        <n-button type="primary" :loading="saving" @click="saveGraph">保存</n-button>
        <n-popconfirm @positive-click="resetGraph">
          <template #trigger>
            <n-button quaternary :loading="saving">恢复默认</n-button>
          </template>
          确定删除界面保存的工作流图，恢复代码默认工作流吗？
        </n-popconfirm>
      </div>
    </header>

    <div class="workflow-layout">
      <section class="panel graph-panel">
        <div class="panel-head">
          <h2 class="panel-title">工作流图</h2>
          <n-tag size="small" :type="isCustom ? 'warning' : 'info'">
            {{ isCustom ? "界面自定义" : "代码默认" }}
          </n-tag>
        </div>
        <n-spin :show="loading">
          <n-input
            v-model:value="graphJson"
            type="textarea"
            class="graph-editor"
            :autosize="{ minRows: 25, maxRows: 45 }"
            spellcheck="false"
          />
        </n-spin>
        <p class="hint">
          节点只能使用右侧已注册的类型；分支通过 <code>edges[].port</code> 连接。保存前会校验节点、端口与 DAG。
        </p>
        <div v-if="graphPreview.length > 0" class="graph-preview">
          <div v-for="node in graphPreview" :key="node.id" class="preview-node">
            <div class="node-head">
              <span class="mono node-kind">{{ node.id }}</span>
              <n-tag size="small">{{ node.kind }}</n-tag>
            </div>
            <div class="preview-edges">
              <span v-for="edge in node.edges" :key="`${edge.port}-${edge.to}`" class="mono port">
                {{ edge.port }} → {{ edge.to }}
              </span>
              <span v-if="node.edges.length === 0" class="mono muted">终止</span>
            </div>
          </div>
        </div>
      </section>

      <section class="panel node-panel">
        <div class="panel-head">
          <h2 class="panel-title">注册节点</h2>
          <span class="mono muted">{{ nodes.length }}</span>
        </div>
        <n-scrollbar class="node-scroll">
          <div class="node-list">
            <div v-for="node in nodes" :key="node.kind" class="node-row">
              <div class="node-head">
                <span class="mono node-kind">{{ node.kind }}</span>
                <n-tag size="small">{{ node.category }}</n-tag>
              </div>
              <div class="node-label">{{ node.label }}</div>
              <div class="node-ports">
                <span v-for="port in node.outputs" :key="port.name" class="mono port">
                  {{ port.name }}
                </span>
                <span v-if="node.outputs.length === 0" class="mono muted">终止节点</span>
              </div>
            </div>
          </div>
        </n-scrollbar>
      </section>

      <section class="panel runs-panel">
        <div class="panel-head">
          <h2 class="panel-title">运行记录</h2>
          <div class="row">
            <span class="mono muted">{{ runCount }}</span>
            <n-button size="tiny" quaternary :loading="runsLoading" @click="loadRuns">刷新</n-button>
          </div>
        </div>
        <n-spin :show="runsLoading">
          <n-empty v-if="!runsLoading && runs.length === 0" description="暂无运行记录" />
          <n-scrollbar v-else class="run-scroll">
            <div class="run-list">
              <button
                v-for="run in runs"
                :key="run.id"
                class="run-row"
                :class="{ active: selectedRun?.id === run.id }"
                @click="selectRun(run)"
              >
                <div class="run-line">
                  <n-tag size="small" :type="statusType(run.status)">{{ run.status }}</n-tag>
                  <span class="mono run-model">{{ run.model }}</span>
                  <span class="mono run-time">{{ formatDuration(run.duration_ms) }}</span>
                </div>
                <div class="run-meta">
                  <span class="mono">{{ formatTime(run.started_at) }}</span>
                  <span v-if="run.error" class="run-error">{{ run.error }}</span>
                </div>
              </button>
            </div>
          </n-scrollbar>
        </n-spin>
      </section>

      <section class="panel run-detail-panel">
        <div class="panel-head">
          <h2 class="panel-title">节点时间线</h2>
          <span class="mono muted">{{ selectedRun?.id ?? "—" }}</span>
        </div>
        <n-spin :show="nodesLoading">
          <n-empty v-if="!selectedRun" description="选择左侧运行记录" />
          <n-empty v-else-if="runNodes.length === 0" description="暂无节点记录" />
          <n-scrollbar v-else class="timeline-scroll">
            <div class="timeline">
              <div v-for="node in runNodes" :key="node.id" class="timeline-row">
                <span class="mono timeline-seq">{{ node.seq }}</span>
                <div class="timeline-body">
                  <div class="run-line">
                    <n-tag size="small" :type="statusType(node.status)">{{ node.status }}</n-tag>
                    <span class="mono">{{ node.node_id }}</span>
                    <span class="mono muted">{{ node.node_kind }}</span>
                    <span class="mono run-time">{{ formatDuration(node.duration_ms) }}</span>
                    <n-tag v-if="node.output_port" size="small" type="info">
                      {{ node.output_port }}
                    </n-tag>
                  </div>
                  <div v-if="node.error" class="run-error">{{ node.error }}</div>
                </div>
              </div>
            </div>
          </n-scrollbar>
        </n-spin>
      </section>
    </div>
  </div>
</template>

<style scoped>
.workflow-layout {
  display: grid;
  grid-template-columns: minmax(0, 1.6fr) minmax(280px, 0.7fr);
  grid-template-areas:
    "graph nodes"
    "runs detail";
  gap: var(--space-4);
  align-items: start;
}

.graph-panel {
  grid-area: graph;
}

.node-panel {
  grid-area: nodes;
}

.runs-panel {
  grid-area: runs;
}

.run-detail-panel {
  grid-area: detail;
}

.graph-editor :deep(textarea) {
  font-family: var(--font-mono);
  font-size: var(--text-xs);
  line-height: 1.55;
}

.hint {
  margin: var(--space-2) 0 0;
  color: var(--muted);
  font-size: var(--text-xs);
  line-height: 1.55;
}

.hint code {
  color: var(--brass);
}

.graph-preview {
  display: flex;
  gap: var(--space-2);
  margin-top: var(--space-3);
  padding-top: var(--space-3);
  border-top: 1px solid var(--border);
  overflow-x: auto;
}

.preview-node {
  flex: 0 0 auto;
  min-width: 150px;
  padding: var(--space-2);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  background: var(--surface-2);
}

.preview-edges {
  display: flex;
  flex-direction: column;
  gap: 2px;
  margin-top: var(--space-1);
}

.node-scroll,
.run-scroll,
.timeline-scroll {
  max-height: 420px;
}

.node-list,
.run-list,
.timeline {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
}

.node-row {
  padding: var(--space-2);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  background: var(--surface-2);
}

.node-head,
.run-line,
.run-meta {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  flex-wrap: wrap;
}

.node-kind {
  color: var(--accent);
  font-size: var(--text-xs);
}

.node-label {
  margin-top: 2px;
  color: var(--fg-2);
  font-size: var(--text-sm);
}

.node-ports {
  display: flex;
  gap: var(--space-1);
  flex-wrap: wrap;
  margin-top: var(--space-1);
}

.port {
  padding: 1px 5px;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  color: var(--muted);
  font-size: var(--text-2xs);
}

.run-row {
  display: block;
  width: 100%;
  padding: var(--space-2);
  border: 0;
  border-radius: var(--radius-md);
  background: transparent;
  color: var(--fg-2);
  text-align: left;
  cursor: pointer;
}

.run-row:hover {
  background: var(--surface-2);
}

.run-row.active {
  background: var(--accent-soft);
}

.run-model {
  color: var(--fg);
  font-size: var(--text-xs);
}

.run-time {
  color: var(--muted);
  font-size: var(--text-2xs);
}

.run-meta {
  margin-top: 2px;
  color: var(--muted);
  font-size: var(--text-2xs);
}

.run-error {
  color: var(--danger);
  font-size: var(--text-2xs);
  overflow-wrap: anywhere;
}

.timeline-row {
  display: flex;
  gap: var(--space-2);
  padding: var(--space-2) 0;
  border-bottom: 1px solid var(--border);
}

.timeline-row:last-child {
  border-bottom: 0;
}

.timeline-seq {
  flex: 0 0 24px;
  color: var(--brass);
  font-size: var(--text-2xs);
  text-align: right;
}

.timeline-body {
  flex: 1;
  min-width: 0;
}

@media (max-width: 1100px) {
  .workflow-layout {
    grid-template-columns: 1fr;
    grid-template-areas:
      "graph"
      "nodes"
      "runs"
      "detail";
  }
}
</style>
