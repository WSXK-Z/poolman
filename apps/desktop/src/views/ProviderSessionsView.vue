<script setup lang="ts">
// 展示指定网页供应商的远端会话，并将删除操作委托给对应的桌面适配器。
import { onMounted, ref, watch } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { NAlert, NButton, NCard, NEmpty, NPopconfirm, NScrollbar, useMessage } from "naive-ui";

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

watch(() => props.providerId, () => {
  selected.value = null;
  void loadSessions();
});
onMounted(() => void loadSessions());
</script>

<template>
  <div>
    <div class="header">
      <n-button quaternary @click="emit('back')">返回供应商</n-button>
      <h2>{{ providerName }}会话管理</h2>
      <n-button :loading="loading" @click="loadSessions">刷新</n-button>
    </div>
    <n-alert v-if="error" type="error" style="margin-bottom: 12px">{{ error }}</n-alert>
    <div class="layout">
      <n-card class="sidebar" size="small" title="会话列表">
        <n-empty v-if="sessions.length === 0 && !loading" description="暂无会话记录" />
        <n-scrollbar v-else style="max-height: calc(100vh - 190px)">
          <div class="session-list">
            <div v-for="session in sessions" :key="session.id"
              class="session-item" :class="{ selected: selected?.id === session.id }">
              <button class="session-select" @click="selectSession(session)">
                <strong>{{ session.title || "未命名会话" }}</strong>
                <small>{{ session.updated_at ? new Date(session.updated_at).toLocaleString() : "时间未知" }}</small>
              </button>
              <n-popconfirm @positive-click="deleteSession(session.id)">
                <template #trigger>
                  <n-button size="tiny" type="error" quaternary>删除</n-button>
                </template>
                确定删除该会话及其对话记录吗？
              </n-popconfirm>
            </div>
          </div>
        </n-scrollbar>
      </n-card>
      <n-card class="conversation" size="small" :title="selected?.title || '会话详情'">
        <n-empty v-if="!selected" description="从左侧选择一个会话查看网页端会话信息" />
        <template v-else>
          <n-alert type="info">显示并管理 {{ providerName }} 网页端会话。选择左侧会话可查看会话元数据。</n-alert>
          <pre class="session-data">{{ JSON.stringify(selected, null, 2) }}</pre>
        </template>
      </n-card>
    </div>
  </div>
</template>

<style scoped>
.header { display: flex; align-items: center; gap: 12px; margin-bottom: 16px; }
.header h2 { flex: 1; margin: 0; }
.layout { display: grid; grid-template-columns: minmax(260px, 340px) minmax(0, 1fr); gap: 12px; }
.sidebar, .conversation { min-height: calc(100vh - 140px); }
.session-list { display: grid; gap: 6px; }
.session-item { display: flex; align-items: center; gap: 4px; padding: 7px; border-radius: 5px; }
.session-item.selected { background: rgba(24, 160, 88, .12); }
.session-select { min-width: 0; flex: 1; border: 0; background: transparent; color: inherit; text-align: left; cursor: pointer; }
.session-select strong, .session-select small { display: block; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.session-select small, .session-data { white-space: pre-wrap; overflow-wrap: anywhere; font-size: 12px; }
@media (max-width: 800px) { .layout { grid-template-columns: 1fr; } .sidebar, .conversation { min-height: 240px; } }
</style>
