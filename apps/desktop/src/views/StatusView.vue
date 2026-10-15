<script setup lang="ts">
// 展示本地 HTTP/WebSocket 服务、插件连接与供应商登录状态的汇总视图。
import { onMounted, onUnmounted, ref } from "vue";
import {
  NAlert,
  NCard,
  NDescriptions,
  NDescriptionsItem,
  NSpin,
  NTag,
  NButton,
} from "naive-ui";
import { getHealthUrl } from "../config";
import { invoke } from "@tauri-apps/api/core";

interface ManagedProvider {
  id: string;
  name: string;
  connected: boolean;
  authed: boolean;
  token_captured: boolean;
  enabled: boolean;
}

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
const providers = ref<ManagedProvider[]>([]);
const updatingProvider = ref<string | null>(null);

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
  try {
    providers.value = await invoke<ManagedProvider[]>("list_managed_providers");
  } catch (e: unknown) {
    error.value = e instanceof Error ? e.message : String(e);
  }
}

async function toggleProvider(provider: ManagedProvider) {
  updatingProvider.value = provider.id;
  try {
    await invoke("set_provider_enabled", {
      provider: provider.id,
      enabled: !provider.enabled,
    });
    provider.enabled = !provider.enabled;
  } catch (e: unknown) {
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    updatingProvider.value = null;
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
      <div class="provider-list">
        <div v-for="provider in providers" :key="provider.id" class="provider-row">
          <div class="provider-info">
            <strong>{{ provider.name }}</strong>
            <n-tag :type="provider.connected ? 'success' : 'default'" size="small">
              {{ provider.connected ? '插件已连接' : '插件未连接' }}
            </n-tag>
            <n-tag :type="provider.authed ? 'success' : 'warning'" size="small">
              {{ provider.authed ? '已登录' : '未检测到登录凭证' }}
            </n-tag>
          </div>
          <n-button
            size="small"
            :type="provider.enabled ? 'warning' : 'success'"
            :loading="updatingProvider === provider.id"
            :disabled="!provider.authed"
            @click="toggleProvider(provider)"
          >
            {{ provider.enabled ? '桌面端停用' : '桌面端启用' }}
          </n-button>
        </div>
      </div>
      <n-alert type="info" :show-icon="true" style="margin: 12px 0">
        桌面端停用只会阻止网关 API 使用该供应商，不会更改插件端的连接开关。
      </n-alert>
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

<style scoped>
.provider-list {
  display: flex;
  flex-direction: column;
  gap: 8px;
  margin-bottom: 12px;
}

.provider-row,
.provider-info {
  display: flex;
  align-items: center;
  gap: 8px;
}

.provider-row {
  justify-content: space-between;
  padding: 10px 12px;
  border: 1px solid rgba(128, 128, 128, 0.2);
  border-radius: 6px;
}
</style>
