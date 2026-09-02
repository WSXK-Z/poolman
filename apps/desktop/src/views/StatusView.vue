<script setup lang="ts">
import { onMounted, onUnmounted, ref } from "vue";
import {
  NAlert,
  NButton,
  NCard,
  NCode,
  NDescriptions,
  NDescriptionsItem,
  NSpin,
  NTag,
} from "naive-ui";
import { invoke } from "@tauri-apps/api/core";
import { openPath, openUrl } from "@tauri-apps/plugin-opener";
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

// 浏览器插件安装引导
const extDir = ref("");
const extError = ref("");

async function loadExtDir() {
  try {
    extDir.value = await invoke<string>("plugin_extension_dir");
    extError.value = "";
  } catch (e: unknown) {
    extError.value = e instanceof Error ? e.message : String(e);
  }
}

async function openExtDir() {
  try {
    await openPath(extDir.value);
  } catch (e: unknown) {
    extError.value = e instanceof Error ? e.message : String(e);
  }
}

async function openExtensionsPage() {
  try {
    await openUrl("chrome://extensions");
  } catch (e: unknown) {
    extError.value = e instanceof Error ? e.message : String(e);
  }
}

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
  loadExtDir();
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
          {{ status.timestamp }}
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

    <n-card title="浏览器插件安装" style="max-width: 720px; margin-top: 16px">
      <n-alert v-if="extError" type="warning" :show-icon="true" style="margin-bottom: 12px">
        {{ extError }}
      </n-alert>
      <div style="color: #888; margin-bottom: 4px">插件构建目录：</div>
      <n-code v-if="extDir" :code="extDir" style="word-break: break-all" />
      <div style="display: flex; gap: 8px; margin-top: 12px; flex-wrap: wrap">
        <n-button type="primary" :disabled="!extDir" @click="openExtDir">打开插件目录</n-button>
        <n-button :disabled="!extDir" @click="openExtensionsPage">打开扩展管理页</n-button>
      </div>
      <n-alert type="info" :show-icon="true" style="margin-top: 12px">
        安装步骤：① 打开「扩展管理页」；② 开启右上角「开发者模式」；③ 点击「加载已解压的扩展程序」；④ 选择上面的插件目录。
      </n-alert>
    </n-card>
  </div>
</template>
