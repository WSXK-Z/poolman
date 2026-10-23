<script setup lang="ts">
// 本地会话管理：同步请求记录，按系统提示词 + 第一条用户提问归并会话。
import { computed, onMounted, ref } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { NButton, NCheckbox, NEmpty, NPopconfirm, NScrollbar, NSpin, NTabPane, NTabs, NTag, useMessage } from "naive-ui";

interface LocalSessionInfo {
  id: string;
  base_id: string;
  first_user_prompt: string;
  request_count: number;
  collision_marker: string | null;
  created_at: number;
  updated_at: number;
}

interface LocalSessionRequestInfo {
  request_id: string;
  model: string;
  status: string;
  user_prompt: string;
  answer_type: string;
  duration_ms: number;
  created_at: number;
}

interface LocalSessionContentInfo {
  id: number;
  session_id: string;
  kind: string;
  content: string;
  request_id: string | null;
  tool_name: string | null;
  tool_call_id: string | null;
  token_count: number;
  created_at: number;
}

interface SyncResult {
  added_sessions: number;
  linked_requests: number;
  collision_sessions: number;
  collision_markers: string[];
}

const message = useMessage();
const sessions = ref<LocalSessionInfo[]>([]);
const selectedId = ref<string | null>(null);
const selectedIds = ref<string[]>([]);
const requests = ref<LocalSessionRequestInfo[]>([]);
const contents = ref<LocalSessionContentInfo[]>([]);
const detailTab = ref<"content" | "requests">("content");
const loading = ref(false);
const syncing = ref(false);
const deleting = ref(false);
const requestsLoading = ref(false);
const contentsLoading = ref(false);

const selected = computed(
  () => sessions.value.find((session) => session.id === selectedId.value) ?? null,
);
const allSelected = computed(
  () => sessions.value.length > 0 && selectedIds.value.length === sessions.value.length,
);
const someSelected = computed(
  () => selectedIds.value.length > 0 && !allSelected.value,
);

function formatTime(value: number): string {
  return new Date(value).toLocaleString();
}

function formatDuration(value: number): string {
  if (!Number.isFinite(value) || value < 0) return "—";
  if (value < 1000) return `${Math.round(value)} ms`;
  return `${(value / 1000).toFixed(2)} s`;
}

type ContentTagType = "default" | "info" | "success" | "warning" | "error" | "primary";

const CONTENT_LABELS: Record<string, string> = {
  system_prompt: "系统提示词",
  tools: "工具表",
  user: "用户提问",
  reasoning: "思考",
  assistant_text: "正文",
  assistant_tool_calls: "工具调用",
  tool_result: "工具结果",
};

function contentLabel(kind: string): string {
  return CONTENT_LABELS[kind] ?? kind;
}

function contentTagType(kind: string): ContentTagType {
  switch (kind) {
    case "system_prompt":
      return "info";
    case "tools":
      return "warning";
    case "user":
      return "primary";
    case "reasoning":
      return "default";
    case "assistant_text":
      return "success";
    case "assistant_tool_calls":
      return "warning";
    case "tool_result":
      return "info";
    default:
      return "default";
  }
}

async function loadSessions() {
  loading.value = true;
  try {
    sessions.value = await invoke<LocalSessionInfo[]>("list_local_sessions");
    selectedIds.value = selectedIds.value.filter((id) =>
      sessions.value.some((item) => item.id === id),
    );
    if (selectedId.value && !sessions.value.some((item) => item.id === selectedId.value)) {
      selectedId.value = null;
    }
    if (!selectedId.value && sessions.value.length > 0) {
      selectedId.value = sessions.value[0].id;
    }
    if (selectedId.value) {
      await loadDetail(selectedId.value);
    } else {
      requests.value = [];
      contents.value = [];
    }
  } catch (error) {
    message.error(error instanceof Error ? error.message : String(error));
  } finally {
    loading.value = false;
  }
}

async function loadRequests(sessionId: string) {
  requestsLoading.value = true;
  try {
    requests.value = await invoke<LocalSessionRequestInfo[]>(
      "list_local_session_requests",
      { sessionId },
    );
  } catch (error) {
    message.error(error instanceof Error ? error.message : String(error));
  } finally {
    requestsLoading.value = false;
  }
}

async function loadContents(sessionId: string) {
  contentsLoading.value = true;
  try {
    contents.value = await invoke<LocalSessionContentInfo[]>(
      "list_local_session_contents",
      { sessionId },
    );
  } catch (error) {
    message.error(error instanceof Error ? error.message : String(error));
  } finally {
    contentsLoading.value = false;
  }
}

async function loadDetail(sessionId: string) {
  await Promise.all([loadRequests(sessionId), loadContents(sessionId)]);
}

async function selectSession(sessionId: string) {
  selectedId.value = sessionId;
  await loadDetail(sessionId);
}

function toggleSelection(sessionId: string, checked: boolean) {
  if (checked) {
    if (!selectedIds.value.includes(sessionId)) {
      selectedIds.value = [...selectedIds.value, sessionId];
    }
  } else {
    selectedIds.value = selectedIds.value.filter((id) => id !== sessionId);
  }
}

function toggleAll(checked: boolean) {
  selectedIds.value = checked ? sessions.value.map((session) => session.id) : [];
}

async function deleteSessions(ids: string[]) {
  if (ids.length === 0) return;
  deleting.value = true;
  try {
    const removed = await invoke<number>("delete_local_sessions", { sessionIds: ids });
    message.success(`已删除 ${removed} 个会话`);
    selectedIds.value = selectedIds.value.filter((id) => !ids.includes(id));
    if (selectedId.value && ids.includes(selectedId.value)) {
      selectedId.value = null;
    }
    await loadSessions();
  } catch (error) {
    message.error(error instanceof Error ? error.message : String(error));
  } finally {
    deleting.value = false;
  }
}

async function syncRequests() {
  syncing.value = true;
  try {
    const result = await invoke<SyncResult>("sync_local_sessions");
    message.success(
      `同步完成：生成 ${result.added_sessions} 个会话，关联 ${result.linked_requests} 条请求`,
    );
    await loadSessions();
  } catch (error) {
    message.error(error instanceof Error ? error.message : String(error));
  } finally {
    syncing.value = false;
  }
}

onMounted(() => void loadSessions());
</script>

<template>
  <div>
    <header class="page-header">
      <div>
        <p class="page-kicker">02.1 / Local Sessions</p>
        <h1 class="page-title">会话管理</h1>
      </div>
      <div class="toolbar">
        <n-button type="primary" :loading="syncing" @click="syncRequests">同步请求</n-button>
      </div>
    </header>

    <div class="local-session-layout">
      <section class="panel session-list-panel">
        <div class="panel-head session-list-head">
          <div class="row">
            <h2 class="panel-title">会话列表</h2>
            <span class="mono muted">{{ sessions.length }}</span>
          </div>
          <div class="session-list-actions">
            <n-checkbox
              :checked="allSelected"
              :indeterminate="someSelected"
              :disabled="sessions.length === 0"
              @update:checked="toggleAll"
            >
              全选
            </n-checkbox>
            <n-popconfirm @positive-click="deleteSessions(selectedIds)">
              <template #trigger>
                <n-button
                  size="small"
                  type="error"
                  quaternary
                  :disabled="selectedIds.length === 0"
                  :loading="deleting"
                >
                  删除选中
                </n-button>
              </template>
              确定删除选中的 {{ selectedIds.length }} 个会话吗？对应远端网页会话会一并删除；请求记录会保留，不再参与会话归并。
            </n-popconfirm>
          </div>
        </div>
        <n-empty v-if="!loading && sessions.length === 0" description="暂无会话" />
        <n-scrollbar v-else class="session-scroll">
          <div class="session-list">
            <div
              v-for="session in sessions"
              :key="session.id"
              class="session-row"
              :class="{ active: session.id === selectedId }"
            >
              <n-checkbox
                :checked="selectedIds.includes(session.id)"
                @update:checked="(checked) => toggleSelection(session.id, checked)"
              />
              <button class="session-select" @click="selectSession(session.id)">
                <span class="mono session-id">{{ session.id }}</span>
              </button>
              <n-popconfirm @positive-click="deleteSessions([session.id])">
                <template #trigger>
                  <n-button size="tiny" type="error" quaternary :loading="deleting">删除</n-button>
                </template>
                确定删除该会话吗？对应远端网页会话会一并删除；请求记录会保留，不再参与会话归并。
              </n-popconfirm>
            </div>
          </div>
        </n-scrollbar>
      </section>

      <section class="panel session-detail-panel">
        <n-empty v-if="!selected" description="选择左侧会话查看详情" />
        <template v-else>
          <div class="session-head">
            <span class="mono session-id-full">{{ selected.id }}</span>
            <n-tag size="small" type="info">{{ selected.request_count }} 条请求</n-tag>
            <n-popconfirm @positive-click="deleteSessions([selected.id])">
              <template #trigger>
                <n-button size="small" type="error" quaternary :loading="deleting">
                  删除会话
                </n-button>
              </template>
              确定删除该会话吗？对应远端网页会话会一并删除；请求记录会保留，不再参与会话归并。
            </n-popconfirm>
          </div>

          <div class="meta-grid">
            <div class="meta-item">
              <span class="meta-label">基础 ID</span>
              <span class="mono meta-value">{{ selected.base_id }}</span>
            </div>
            <div class="meta-item">
              <span class="meta-label">生成时间</span>
              <span class="mono meta-value">{{ formatTime(selected.created_at) }}</span>
            </div>
            <div class="meta-item">
              <span class="meta-label">第一条提问</span>
              <span class="meta-value">{{ selected.first_user_prompt || "—" }}</span>
            </div>
            <div v-if="selected.collision_marker" class="meta-item">
              <span class="meta-label">会话标记</span>
              <span class="meta-value">{{ selected.collision_marker }}</span>
            </div>
          </div>

          <n-tabs v-model:value="detailTab" type="segment" size="small" class="detail-tabs">
            <n-tab-pane name="content" tab="会话内容">
              <n-spin :show="contentsLoading">
                <n-empty
                  v-if="!contentsLoading && contents.length === 0"
                  description="暂无会话内容"
                />
                <n-scrollbar v-else class="content-scroll">
                  <div class="content-list">
                    <div v-for="item in contents" :key="item.id" class="content-row">
                      <div class="content-line">
                        <n-tag size="small" :type="contentTagType(item.kind)">
                          {{ contentLabel(item.kind) }}
                        </n-tag>
                        <span class="mono request-time">{{ formatTime(item.created_at) }}</span>
                        <span class="mono request-duration">{{ item.token_count }} tok</span>
                        <span v-if="item.tool_name" class="mono muted">{{ item.tool_name }}</span>
                        <span v-if="item.tool_call_id" class="mono muted">
                          {{ item.tool_call_id }}
                        </span>
                      </div>
                      <div class="content-text">{{ item.content }}</div>
                    </div>
                  </div>
                </n-scrollbar>
              </n-spin>
            </n-tab-pane>
            <n-tab-pane name="requests" tab="关联请求">
              <n-spin :show="requestsLoading">
                <n-empty
                  v-if="!requestsLoading && requests.length === 0"
                  description="暂无关联请求"
                />
                <div v-else class="request-list">
                  <div v-for="request in requests" :key="request.request_id" class="request-row">
                    <div class="request-line">
                      <span class="mono request-time">{{ formatTime(request.created_at) }}</span>
                      <n-tag size="small" :type="request.status === 'error' ? 'error' : 'success'">
                        {{ request.status === "error" ? "失败" : "成功" }}
                      </n-tag>
                      <n-tag size="small">{{ request.model }}</n-tag>
                      <span class="mono request-duration">
                        {{ formatDuration(request.duration_ms) }}
                      </span>
                    </div>
                    <div class="request-prompt">{{ request.user_prompt || "—" }}</div>
                  </div>
                </div>
              </n-spin>
            </n-tab-pane>
          </n-tabs>
        </template>
      </section>
    </div>
  </div>
</template>

<style scoped>
.local-session-layout {
  display: grid;
  grid-template-columns: minmax(300px, 380px) minmax(0, 1fr);
  gap: var(--space-4);
}

.session-list-panel,
.session-detail-panel {
  min-height: calc(100vh - 190px);
}

.session-scroll {
  max-height: calc(100vh - 260px);
}

.session-list {
  display: grid;
  gap: var(--space-1);
}

.session-list-head {
  align-items: center;
  gap: var(--space-2);
}

.session-list-actions {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  flex-wrap: wrap;
}

.session-row {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  width: 100%;
  padding: var(--space-1) var(--space-2);
  border-radius: var(--radius-md);
  background: transparent;
  color: var(--fg-2);
}

.session-row:hover {
  background: var(--surface-2);
  color: var(--fg);
}

.session-row.active {
  background: var(--accent-soft);
  color: var(--accent);
}

.session-select {
  flex: 1;
  min-width: 0;
  padding: var(--space-1) 0;
  border: 0;
  background: transparent;
  color: inherit;
  text-align: left;
  cursor: pointer;
}

.session-id {
  display: block;
  overflow: hidden;
  font-size: var(--text-xs);
  text-overflow: ellipsis;
  white-space: nowrap;
}

.session-head {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  margin-bottom: var(--space-4);
}

.session-id-full {
  flex: 1;
  min-width: 0;
  color: var(--fg);
  font-size: var(--text-sm);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.meta-grid {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: var(--space-3) var(--space-4);
  padding: var(--space-4);
  margin-bottom: var(--space-4);
  background: var(--surface-2);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
}

.meta-item {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}

.meta-label {
  color: var(--brass);
  font-family: var(--font-mono);
  font-size: var(--text-2xs);
  letter-spacing: 0.12em;
  text-transform: uppercase;
}

.meta-value {
  color: var(--fg-2);
  font-size: var(--text-xs);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.request-head {
  margin-bottom: var(--space-2);
}

.request-list {
  display: flex;
  flex-direction: column;
}

.request-row {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  padding: var(--space-3) 0;
  border-bottom: 1px solid var(--border);
}

.request-row:last-child {
  border-bottom: 0;
}

.request-line {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  flex-wrap: wrap;
}

.request-time,
.request-duration {
  color: var(--muted);
  font-size: var(--text-2xs);
}

.request-prompt {
  color: var(--fg-2);
  font-size: var(--text-sm);
  line-height: 1.55;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.detail-tabs {
  margin-top: var(--space-2);
}

.content-scroll {
  max-height: 460px;
}

.content-list {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
  padding-right: var(--space-2);
}

.content-row {
  padding: var(--space-2);
  background: var(--surface-2);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
}

.content-line {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  flex-wrap: wrap;
  margin-bottom: var(--space-1);
}

.content-text {
  max-height: 180px;
  overflow: auto;
  color: var(--fg-2);
  font-family: var(--font-mono);
  font-size: var(--text-xs);
  line-height: 1.55;
  overflow-wrap: anywhere;
  white-space: pre-wrap;
}

@media (max-width: 900px) {
  .local-session-layout,
  .meta-grid {
    grid-template-columns: 1fr;
  }

  .session-list-panel,
  .session-detail-panel {
    min-height: 260px;
  }
}
</style>
