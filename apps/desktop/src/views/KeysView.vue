<script setup lang="ts">
import { h, onMounted, ref } from "vue";
import {
  NButton,
  NCard,
  NDataTable,
  NInput,
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

const message = useMessage();

const platformOptions = [
  { label: "VS Code Copilot", value: "vscode-copilot" },
  { label: "Trae Work (SOLO)", value: "trae_work" },
];
const platform = ref("vscode-copilot");
const apiKeys = ref<ApiKey[]>([]);
const loading = ref(false);
const showCreated = ref(false);
const createdKey = ref<ApiKey | null>(null);

function formatTime(ms: number): string {
  return new Date(ms).toLocaleString();
}

const columns: DataTableColumns<ApiKey> = [
  { title: "Key", key: "key", ellipsis: { tooltip: true } },
  {
    title: "平台",
    key: "platform",
    width: 140,
    render: (row) => h(NTag, { size: "small", type: "info" }, { default: () => row.platform }),
  },
  {
    title: "创建时间",
    key: "created_at",
    width: 180,
    render: (row) => formatTime(row.created_at),
  },
  {
    title: "操作",
    key: "actions",
    width: 170,
    render: (row) =>
      h(NSpace, {}, {
        default: () => [
          h(NButton, { size: "small", onClick: () => copyKey(row.key) }, { default: () => "复制" }),
          h(
            NPopconfirm,
            { onPositiveClick: () => revokeKey(row.key) },
            {
              trigger: () => h(NButton, { size: "small", type: "error" }, { default: () => "吊销" }),
              default: () => "确定吊销该 API Key 吗？",
            },
          ),
        ],
      }),
  },
];

async function loadKeys() {
  loading.value = true;
  try {
    apiKeys.value = await invoke<ApiKey[]>("list_api_keys");
  } catch {
    // 非 tauri 环境忽略
  } finally {
    loading.value = false;
  }
}

async function createKey() {
  try {
    createdKey.value = await invoke<ApiKey>("create_api_key", {
      platform: platform.value,
      note: null,
    });
    showCreated.value = true;
    await loadKeys();
  } catch (e: unknown) {
    message.error(e instanceof Error ? e.message : String(e));
  }
}

async function revokeKey(key: string) {
  try {
    await invoke("revoke_api_key", { key });
    message.success("已吊销");
    if (createdKey.value?.key === key) createdKey.value = null;
    await loadKeys();
  } catch (e: unknown) {
    message.error(e instanceof Error ? e.message : String(e));
  }
}

async function copyKey(key: string) {
  try {
    await navigator.clipboard.writeText(key);
    message.success("已复制");
  } catch {
    message.error("复制失败");
  }
}

onMounted(loadKeys);
</script>

<template>
  <n-card title="API Key 管理">
    <!-- 上部操作 bar -->
    <n-space align="center" style="margin-bottom: 12px">
      <n-select v-model:value="platform" :options="platformOptions" style="width: 220px" />
      <n-button type="primary" :loading="loading" @click="createKey">创建 API Key</n-button>
    </n-space>

    <!-- 下部表格 -->
    <n-data-table
      :columns="columns"
      :data="apiKeys"
      :loading="loading"
      :pagination="{ pageSize: 10 }"
      :bordered="false"
    />
  </n-card>

  <!-- 创建成功弹窗 -->
  <n-modal v-model:show="showCreated" preset="card" title="新 Key 已创建（请立即复制保存）" style="width: 560px">
    <n-space vertical>
      <n-input :value="createdKey?.key ?? ''" readonly />
      <n-button type="primary" @click="createdKey && copyKey(createdKey.key)">复制</n-button>
    </n-space>
  </n-modal>
</template>
