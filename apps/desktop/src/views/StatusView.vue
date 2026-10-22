<script setup lang="ts">
// 展示本地 HTTP/WebSocket 服务、插件连接与供应商登录状态的汇总视图。
import { onMounted, onUnmounted, ref } from "vue";
import {
  NAlert,
  NButton,
  NSpin,
  NTag,
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

/** 秒数 → 运行时长文本。 */
function formatUptime(seconds: number): string {
  const total = Math.max(0, Math.floor(seconds || 0));
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const rest = total % 60;
  if (hours > 0) return `${hours}h ${minutes}m`;
  if (minutes > 0) return `${minutes}m ${rest}s`;
  return `${rest}s`;
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
    <header class="page-header">
      <div>
        <p class="page-kicker">04 / Runtime</p>
        <h1 class="page-title">状态</h1>
      </div>
      <div class="toolbar">
        <n-button @click="load">刷新</n-button>
      </div>
    </header>

    <n-alert v-if="error" type="error" :show-icon="true" style="margin-bottom: 16px">
      {{ error }}
    </n-alert>

    <n-spin v-if="!status && !error" style="display: block; padding: 48px 0" />

    <template v-if="status">
      <div class="stat-grid">
        <section class="panel stat-card">
          <span class="stat-label">Gateway</span>
          <span
            class="stat-value"
            :class="status.status === 'ok' ? 'is-ok' : 'is-danger'"
          >
            {{ status.status === "ok" ? "运行中" : status.status }}
          </span>
          <span class="stat-note">HTTP · WebSocket</span>
        </section>
        <section class="panel stat-card">
          <span class="stat-label">Extension</span>
          <span
            class="stat-value"
            :class="status.extension_connected ? 'is-ok' : 'is-warn'"
          >
            {{ status.extension_connected ? "已连接" : "未连接" }}
          </span>
          <span class="stat-note">浏览器插件</span>
        </section>
        <section class="panel stat-card">
          <span class="stat-label">Uptime</span>
          <span class="stat-value mono">{{ formatUptime(status.uptime || 0) }}</span>
          <span class="stat-note">进程运行时间</span>
        </section>
      </div>

      <div class="panel-stack">
        <section class="panel">
          <div class="panel-head">
            <h2 class="panel-title">供应商</h2>
          </div>
          <div class="provider-list">
            <div v-for="provider in providers" :key="provider.id" class="provider-row">
              <div class="provider-info">
                <strong>{{ provider.name }}</strong>
                <n-tag :type="provider.connected ? 'success' : 'default'" size="small">
                  {{ provider.connected ? "插件已连接" : "插件未连接" }}
                </n-tag>
                <n-tag :type="provider.authed ? 'success' : 'warning'" size="small">
                  {{ provider.authed ? "已登录" : "未登录" }}
                </n-tag>
              </div>
              <n-button
                size="small"
                :type="provider.enabled ? 'warning' : 'success'"
                :loading="updatingProvider === provider.id"
                :disabled="!provider.authed"
                @click="toggleProvider(provider)"
              >
                {{ provider.enabled ? "桌面端停用" : "桌面端启用" }}
              </n-button>
            </div>
            <span v-if="providers.length === 0" class="muted">暂无供应商</span>
          </div>
        </section>

        <section class="panel">
          <div class="panel-head">
            <h2 class="panel-title">凭证</h2>
          </div>
          <div v-if="Object.keys(status.credentials || {}).length" class="cred-list">
            <div v-for="(v, p) in status.credentials" :key="String(p)" class="cred-row">
              <span class="cred-name mono">{{ p }}</span>
              <n-tag :type="v.token_captured ? 'success' : 'default'" size="small">
                {{ v.token_captured ? "凭证已捕获" : "未捕获凭证" }}
              </n-tag>
              <span class="cred-time mono">
                {{ v.updated_at ? new Date(v.updated_at).toLocaleString() : "—" }}
              </span>
            </div>
          </div>
          <span v-else class="muted">暂无凭证</span>
        </section>
      </div>
    </template>
  </div>
</template>

<style scoped>
.stat-grid {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: var(--space-4);
  margin-bottom: var(--space-4);
}

.stat-card {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}

.stat-label {
  color: var(--brass);
  font-family: var(--font-mono);
  font-size: var(--text-2xs);
  letter-spacing: 0.16em;
  text-transform: uppercase;
}

.stat-value {
  color: var(--fg);
  font-family: var(--font-display);
  font-size: var(--text-xl);
  font-weight: 600;
  letter-spacing: -0.02em;
  line-height: 1.2;
}

.stat-value.is-ok {
  color: var(--ok);
}

.stat-value.is-warn {
  color: var(--warn);
}

.stat-value.is-danger {
  color: var(--danger);
}

.stat-note {
  color: var(--muted);
  font-size: var(--text-xs);
}

.provider-list {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}

.provider-row,
.provider-info {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}

.provider-row {
  justify-content: space-between;
  padding: var(--space-3);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
}

.cred-list {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}

.cred-row {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-3);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
}

.cred-name {
  flex: 1;
  min-width: 0;
  color: var(--fg);
  font-size: var(--text-sm);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.cred-time {
  color: var(--muted);
  font-size: var(--text-xs);
}

@media (max-width: 768px) {
  .stat-grid {
    grid-template-columns: 1fr;
  }
}
</style>
