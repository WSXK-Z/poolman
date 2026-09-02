<script setup lang="ts">
import { onMounted, onUnmounted, ref } from "vue";
import {
  NAlert,
  NCard,
  NDescriptions,
  NDescriptionsItem,
  NSpin,
  NTag,
} from "naive-ui";
import { getHealthUrl } from "../config";

interface ProviderCred {
  token_captured?: boolean;
  settings_token_captured?: boolean;
  template_captured?: boolean;
  updated_at?: number;
}

const status = ref<{
  status: string;
  uptime: number;
  extension_connected: boolean;
  credentials: Record<string, ProviderCred>;
  timestamp: string;
} | null>(null);
const error = ref("");
let timer: number | undefined;

async function load() {
  try {
    const res = await fetch(getHealthUrl());
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    status.value = await res.json();
    error.value = "";
  } catch (e: unknown) {
    error.value = e instanceof Error ? e.message : String(e);
    status.value = null;
  }
}

onMounted(() => {
  load();
  timer = window.setInterval(load, 3000);
});
onUnmounted(() => window.clearInterval(timer));
</script>

<template>
  <div>
    <n-alert v-if="error" type="error" :show-icon="true" style="margin-bottom: 16px">
      无法连接网关服务:{{ error }}
    </n-alert>

    <n-card title="连接状态" style="max-width: 720px">
      <n-spin v-if="!status && !error" />
      <n-descriptions v-if="status" :column="1" bordered>
        <n-descriptions-item label="网关服务">
          <n-tag :type="status.status === 'ok' ? 'success' : 'error'">
            {{ status.status === 'ok' ? '运行中' : status.status }}
          </n-tag>
        </n-descriptions-item>
        <n-descriptions-item label="扩展连接">
          <n-tag :type="status.extension_connected ? 'success' : 'default'">
            {{ status.extension_connected ? '已连接' : '未连接' }}
          </n-tag>
        </n-descriptions-item>
        <n-descriptions-item label="运行时间">
          {{ Math.floor((status.uptime || 0) / 60) }} 分钟
        </n-descriptions-item>
        <n-descriptions-item label="最近更新">
        </n-descriptions-item>
      </n-descriptions>
    </n-card>

    <n-card v-if="status" title="凭证状态" style="max-width: 720px; margin-top: 16px">
      <n-descriptions v-if="Object.keys(status.credentials || {}).length" :column="1" bordered>
        <n-descriptions-item
          v-for="(v, p) in status.credentials"
          :key="String(p)"
          :label="String(p)"
        >
          <n-tag :type="v.token_captured ? 'success' : 'default'">
            {{ v.token_captured ? '凭证已捕获' : '未捕获凭证' }}
          </n-tag>
          <span v-if="v.updated_at" style="margin-left: 8px; color: #888">
            更新于 {{ new Date(v.updated_at).toLocaleString() }}
          </span>
        </n-descriptions-item>
      </n-descriptions>
      <span v-else>暂无凭证</span>
    </n-card>
</div>

</template>
