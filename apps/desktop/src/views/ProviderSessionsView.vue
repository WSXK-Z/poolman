<script setup lang="ts">
// 展示指定网页供应商的远端会话，并将删除操作委托给对应的桌面适配器。
import { onMounted, ref, watch } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { NAlert, NButton, NEmpty, NInput, NPopconfirm, NScrollbar, useMessage } from "naive-ui";

const props = defineProps<{ providerId: string; providerName: string }>();
const emit = defineEmits<{ back: [] }>();

interface SessionInfo {
  id: string;
  title: string;
  updated_at?: number;
  created_at?: number;
  [key: string]: unknown;
}

const message = useMessage();
const sessions = ref<SessionInfo[]>([]);
const selected = ref<SessionInfo | null>(null);
const loading = ref(false);
const error = ref("");
/// 正在改名的会话 id（同一时刻只允许一个，避免多点）。
const renamingId = ref("");
const renameDraft = ref("");
const renaming = ref(false);

async function loadSessions() {
  loading.value = true;
  try {
    const remoteSessions = await invoke<Record<string, unknown>[]>("list_provider_sessions", {
      provider: props.providerId,
    });
    sessions.value = remoteSessions.map((session) => ({
      ...session,
      id: String(session.id ?? session.chat_session_id ?? ""),
      title: String(session.title ?? session.name ?? session.id ?? session.chat_session_id ?? "未命名会话"),
      updated_at: Number(session.updated_at ?? session.update_time ?? session.created_at ?? session.create_time ?? 0),
    })).filter((session) => !!session.id);
    if (selected.value && !sessions.value.some((session) => session.id === selected.value?.id)) {
      selected.value = null;
    }
    error.value = "";
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    loading.value = false;
  }
}

async function selectSession(session: SessionInfo) {
  selected.value = session;
}

async function deleteSession(id: string) {
  try {
    await invoke("delete_provider_session", { provider: props.providerId, sessionId: id });
    if (selected.value?.id === id) selected.value = null;
    message.success("已删除会话");
    await loadSessions();
  } catch (e) {
    message.error(e instanceof Error ? e.message : String(e));
  }
}

/// 进入改名状态：预填当前标题。
function startRename(session: SessionInfo) {
  renamingId.value = session.id;
  renameDraft.value = session.title === session.id ? "" : session.title;
}

function cancelRename() {
  renamingId.value = "";
  renameDraft.value = "";
}

/// 提交改名：成功后以本地返回值更新列表，避免网页端列表有延迟时看起来没生效。
async function submitRename(session: SessionInfo) {
  const title = renameDraft.value.trim();
  if (!title) {
    message.warning("标题不能为空");
    return;
  }
  if (title === session.title) {
    cancelRename();
    return;
  }
  renaming.value = true;
  try {
    await invoke("rename_provider_session", {
      provider: props.providerId,
      sessionId: session.id,
      title,
    });
    session.title = title;
    if (selected.value?.id === session.id) selected.value = { ...session };
    message.success("已修改标题");
    cancelRename();
  } catch (e) {
    message.error(e instanceof Error ? e.message : String(e));
  } finally {
    renaming.value = false;
  }
}

watch(() => props.providerId, () => {
  selected.value = null;
  void loadSessions();
});
onMounted(() => void loadSessions());
</script>

<template>
  <div>
    <header class="page-header">
      <div>
        <p class="page-kicker">05.1 / Sessions</p>
        <h1 class="page-title">{{ providerName }}</h1>
      </div>
      <div class="toolbar">
        <n-button quaternary @click="emit('back')">返回插件</n-button>
        <n-button :loading="loading" @click="loadSessions">刷新</n-button>
      </div>
    </header>

    <n-alert v-if="error" type="error" style="margin-bottom: 12px">{{ error }}</n-alert>

    <div class="session-layout">
      <section class="panel session-sidebar">
        <div class="panel-head">
          <h2 class="panel-title">会话列表</h2>
          <span class="mono muted">{{ sessions.length }}</span>
        </div>
        <n-empty v-if="sessions.length === 0 && !loading" description="暂无会话记录" />
        <n-scrollbar v-else style="max-height: calc(100vh - 260px)">
          <div class="session-list">
            <div
              v-for="session in sessions"
              :key="session.id"
              class="session-item"
              :class="{ selected: selected?.id === session.id }"
            >
              <div v-if="renamingId === session.id" class="session-rename">
                <n-input
                  v-model:value="renameDraft"
                  size="small"
                  placeholder="输入新的会话标题"
                  :maxlength="60"
                  @keyup.enter="submitRename(session)"
                  @keyup.esc="cancelRename"
                />
                <div class="session-rename-actions">
                  <n-button size="tiny" type="primary" :loading="renaming" @click="submitRename(session)">
                    保存
                  </n-button>
                  <n-button size="tiny" quaternary :disabled="renaming" @click="cancelRename">
                    取消
                  </n-button>
                </div>
              </div>
              <template v-else>
                <button class="session-select" @click="selectSession(session)">
                  <strong>{{ session.title || "未命名会话" }}</strong>
                  <small class="mono">
                    {{ session.updated_at ? new Date(session.updated_at).toLocaleString() : "时间未知" }}
                  </small>
                </button>
                <n-button size="tiny" quaternary @click="startRename(session)">改名</n-button>
                <n-popconfirm @positive-click="deleteSession(session.id)">
                  <template #trigger>
                    <n-button size="tiny" type="error" quaternary>删除</n-button>
                  </template>
                  确定删除该会话及其对话记录吗？
                </n-popconfirm>
              </template>
            </div>
          </div>
        </n-scrollbar>
      </section>

      <section class="panel session-detail">
        <div class="panel-head">
          <h2 class="panel-title">{{ selected?.title || "会话详情" }}</h2>
        </div>
        <n-empty v-if="!selected" description="从左侧选择一个会话" />
        <pre v-else class="session-data">{{ JSON.stringify(selected, null, 2) }}</pre>
      </section>
    </div>
  </div>
</template>

<style scoped>
.session-layout {
  display: grid;
  grid-template-columns: minmax(260px, 340px) minmax(0, 1fr);
  gap: var(--space-4);
}

.session-sidebar,
.session-detail {
  min-height: calc(100vh - 220px);
}

.session-list {
  display: grid;
  gap: var(--space-1);
}

.session-item {
  display: flex;
  align-items: center;
  gap: var(--space-1);
  padding: var(--space-2);
  border-radius: var(--radius-md);
}

.session-item:hover {
  background: var(--surface-2);
}

.session-item.selected {
  background: var(--accent-soft);
}

.session-rename {
  display: grid;
  flex: 1;
  gap: var(--space-1);
  min-width: 0;
}

.session-rename-actions {
  display: flex;
  gap: var(--space-1);
}

.session-select {
  flex: 1;
  min-width: 0;
  padding: 0;
  border: 0;
  background: transparent;
  color: inherit;
  text-align: left;
  cursor: pointer;
}

.session-select strong,
.session-select small {
  display: block;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.session-select strong {
  color: var(--fg);
  font-size: var(--text-sm);
  font-weight: 500;
}

.session-select small {
  color: var(--muted);
  font-size: var(--text-2xs);
}

.session-data {
  margin: 0;
  color: var(--fg-2);
  font-family: var(--font-mono);
  font-size: var(--text-xs);
  line-height: 1.6;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}

@media (max-width: 800px) {
  .session-layout {
    grid-template-columns: 1fr;
  }

  .session-sidebar,
  .session-detail {
    min-height: 240px;
  }
}
</style>
