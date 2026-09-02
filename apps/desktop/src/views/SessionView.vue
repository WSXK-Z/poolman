<script setup lang="ts">
import { computed, h, onMounted, ref } from "vue";
import {
  NButton,
  NCard,
  NDataTable,
  NDescriptions,
  NDescriptionsItem,
  NModal,
  NPopconfirm,
  NSelect,
  NSpace,
  NTag,
  useMessage,
  type DataTableColumns,
} from "naive-ui";
import { invoke } from "@tauri-apps/api/core";

interface ApiKey {
  key: string;
  platform: string;
  created_at: number;
  note: string;
}
interface SessionInfo {
  id: string;
  web_session_id: string;
  title: string;
  platform: string;
  api_key: string;
  created_at: number;
  updated_at: number;
  raw_count: number;
  converted_count: number;
}
interface MessageRecord {
  role: string;
  content: string;
  created_at: number;
}
interface SessionDetail extends SessionInfo {
  raw_messages: MessageRecord[];
  converted_messages: MessageRecord[];
}

const message = useMessage();

const apiKeys = ref<ApiKey[]>([]);
const selectedKey = ref<string | null>(null);
const keyOptions = computed(() =>
  apiKeys.value.map((k) => ({ label: k.key, value: k.key })),
);
const sessions = ref<SessionInfo[]>([]);
const loading = ref(false);

const detail = ref<SessionDetail | null>(null);
const showDetail = ref(false);

function formatTime(ms: number): string {
  return new Date(ms).toLocaleString();
}

function roleTagType(role: string): "info" | "warning" | "error" | "success" {
  if (role === "user") return "info";
  if (role === "system") return "warning";
  if (role === "tool") return "error";
  return "success";
}

const columns: DataTableColumns<SessionInfo> = [
  { title: "标题", key: "title", ellipsis: { tooltip: true } },
  { title: "会话 id", key: "id", ellipsis: { tooltip: true } },
  { title: "平台 id", key: "web_session_id", ellipsis: { tooltip: true } },
  {
    title: "网页平台",
    key: "platform",
    width: 130,
    render: (row) => h(NTag, { size: "small", type: "info" }, { default: () => row.platform }),
  },
  {
    title: "原始/转换",
    key: "counts",
    width: 100,
    render: (row) => `${row.raw_count}/${row.converted_count}`,
  },
  {
    title: "操作",
    key: "actions",
    width: 160,
    render: (row) =>
      h(NSpace, {}, {
        default: () => [
          h(NButton, { size: "small", onClick: () => openDetail(row.id) }, { default: () => "详情" }),
          h(
            NPopconfirm,
            { onPositiveClick: () => del(row.id) },
            {
              trigger: () => h(NButton, { size: "small", type: "error" }, { default: () => "删除" }),
              default: () => "确定删除该会话及其对话记录吗？",
            },
          ),
        ],
      }),
  },
];

const msgColumns: DataTableColumns<MessageRecord> = [
  {
    title: "角色",
    key: "role",
    width: 90,
    render: (row) =>
      h(NTag, { size: "small", type: roleTagType(row.role) }, { default: () => row.role }),
  },
  { title: "内容", key: "content" },
  {
    title: "时间",
    key: "created_at",
    width: 170,
    render: (row) => formatTime(row.created_at),
  },
];

async function loadKeys() {
  try {
    apiKeys.value = await invoke<ApiKey[]>("list_api_keys");
  } catch {
    // 非 tauri 环境忽略
  }
}

async function loadSessions() {
  loading.value = true;
  try {
    sessions.value = await invoke<SessionInfo[]>("list_sessions", {
      apiKey: selectedKey.value ?? null,
    });
  } catch {
    // 非 tauri 环境忽略
  } finally {
    loading.value = false;
  }
}

async function openDetail(id: string) {
  try {
    detail.value = await invoke<SessionDetail>("get_session", { id });
    showDetail.value = true;
  } catch (e: unknown) {
    message.error(e instanceof Error ? e.message : String(e));
  }
}

async function del(id: string) {
  try {
    await invoke("delete_session", { id });
    if (detail.value?.id === id) {
      detail.value = null;
      showDetail.value = false;
    }
    message.success("已删除会话");
    await loadSessions();
  } catch (e: unknown) {
    message.error(e instanceof Error ? e.message : String(e));
  }
}

onMounted(() => {
  loadKeys();
  loadSessions();
});
</script>

<template>
  <n-card title="会话管理">
    <!-- 上部操作 bar -->
    <n-space align="center" style="margin-bottom: 12px">
      <n-select
        v-model:value="selectedKey"
        :options="keyOptions"
        placeholder="选择 API Key（留空 = 全部）"
        clearable
        style="width: 340px"
        @update:value="loadSessions"
      />
      <n-button :loading="loading" @click="loadSessions">刷新</n-button>
    </n-space>

    <!-- 下部表格 -->
    <n-data-table
      :columns="columns"
      :data="sessions"
      :loading="loading"
      :pagination="{ pageSize: 10 }"
      :bordered="false"
    />
  </n-card>

  <!-- 详情弹窗：表格显示对话记录 -->
  <n-modal
    v-model:show="showDetail"
    preset="card"
    :title="detail ? `会话详情 - ${detail.title}` : '会话详情'"
    style="width: 760px"
  >
    <n-space v-if="detail" vertical>
      <n-descriptions :columns="2" size="small" label-placement="left">
        <n-descriptions-item label="自身 id">{{ detail.id }}</n-descriptions-item>
        <n-descriptions-item label="平台 id">{{ detail.web_session_id }}</n-descriptions-item>
        <n-descriptions-item label="网页平台">{{ detail.platform }}</n-descriptions-item>
        <n-descriptions-item label="API Key">{{ detail.api_key }}</n-descriptions-item>
      </n-descriptions>

      <n-card size="small" title="原始对话记录">
        <n-data-table
          :columns="msgColumns"
          :data="detail.raw_messages"
          size="small"
          :max-height="260"
          :scroll-x="900"
        />
      </n-card>

      <n-card size="small" title="转换后对话记录">
        <n-data-table
          :columns="msgColumns"
          :data="detail.converted_messages"
          size="small"
          :max-height="260"
          :scroll-x="900"
        />
      </n-card>
    </n-space>
  </n-modal>
</template>
