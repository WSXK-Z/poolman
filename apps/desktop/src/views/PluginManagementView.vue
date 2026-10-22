<script setup lang="ts">
// 汇总插件连接及其渠道登录状态，并提供插件级和渠道级启停控制。
import { computed, onMounted, onUnmounted, ref } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { NAlert, NButton, NEmpty, NTag } from "naive-ui";
import ProviderSessionsView from "./ProviderSessionsView.vue";

interface PluginChannel {
  connected?: boolean;
  authed?: boolean;
  tokenCaptured?: boolean;
  desktopEnabled?: boolean;
}
interface ManagedPlugin {
  id: string;
  enabled: boolean;
  channels: Record<string, PluginChannel>;
  // 以下字段来自桌面端持久化的插件连接信息
  name?: string;
  version?: string;
  platform?: string;
  connected?: boolean;
  firstSeenAt?: number;
  lastSeenAt?: number;
  connectCount?: number;
}

const plugins = ref<ManagedPlugin[]>([]);
const selectedId = ref<string | null>(null);
const sessionProviderId = ref<string | null>(null);
const error = ref("");
const busyKey = ref("");
let timer: number | undefined;

const selected = computed(() => plugins.value.find((plugin) => plugin.id === selectedId.value));

function formatTime(value?: number): string {
  if (!value) return "—";
  return new Date(value).toLocaleString();
}
const channelLabels: Record<string, string> = {
  "chatgpt-web": "ChatGPT",
  "claude-web": "Claude",
  "deepseek-web": "DeepSeek",
  "gemini-web": "Gemini",
  "kimi-web": "Kimi",
  "qwen-web": "通义千问",
};

async function load() {
  try {
    plugins.value = await invoke<ManagedPlugin[]>("list_plugins");
    if (selectedId.value && !plugins.value.some((plugin) => plugin.id === selectedId.value)) selectedId.value = null;
    error.value = "";
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
  }
}

async function togglePlugin(plugin: ManagedPlugin) {
  busyKey.value = plugin.id;
  try {
    await invoke("set_plugin_enabled", { pluginId: plugin.id, enabled: !plugin.enabled });
    plugin.enabled = !plugin.enabled;
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    busyKey.value = "";
  }
}

async function controlProvider(providerId: string, action: "connect" | "disconnect") {
  if (!selected.value) return;
  busyKey.value = `${selected.value.id}:${providerId}:${action}`;
  try {
    await invoke("control_plugin_provider", {
      pluginId: selected.value.id,
      provider: providerId,
      action,
    });
    await load();
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    busyKey.value = "";
  }
}

async function openProviderSession(providerId: string) {
  if (!selected.value) return;
  busyKey.value = `${selected.value.id}:${providerId}:open`;
  try {
    await invoke("control_plugin_provider", {
      pluginId: selected.value.id,
      provider: providerId,
      action: "open",
    });
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    busyKey.value = "";
  }
}

async function toggleChannel(channelId: string, current: boolean) {
  if (!selected.value) return;
  busyKey.value = `${selected.value.id}:${channelId}:enabled`;
  try {
    await invoke("set_plugin_channel_enabled", {
      pluginId: selected.value.id,
      channel: channelId,
      enabled: !current,
    });
    await load();
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    busyKey.value = "";
  }
}

onMounted(() => {
  void load();
  timer = window.setInterval(() => void load(), 3000);
});
onUnmounted(() => window.clearInterval(timer));
</script>

<template>
  <div>
    <ProviderSessionsView
      v-if="sessionProviderId"
      :provider-id="sessionProviderId"
      :provider-name="channelLabels[sessionProviderId] || sessionProviderId"
      @back="sessionProviderId = null"
    />
    <template v-else>
      <header class="page-header">
        <div>
          <p class="page-kicker">05 / Extensions</p>
          <h1 class="page-title">插件</h1>
        </div>
      </header>

      <n-alert v-if="error" type="error" style="margin-bottom: 16px">{{ error }}</n-alert>

      <template v-if="!selected">
        <n-empty v-if="plugins.length === 0" description="暂无连接的插件" />
        <div v-else class="plugin-grid">
          <section v-for="plugin in plugins" :key="plugin.id" class="panel plugin-card">
            <div class="plugin-head">
              <div class="plugin-title">
                <strong>{{ plugin.name || "浏览器插件" }}</strong>
                <span class="mono muted">{{ plugin.id }}</span>
              </div>
              <div class="row">
                <n-tag :type="plugin.connected ? 'success' : 'default'" size="small">
                  {{ plugin.connected ? "已连接" : "已断开" }}
                </n-tag>
                <n-tag :type="plugin.enabled ? 'success' : 'default'" size="small">
                  {{ plugin.enabled ? "参与分配" : "已停用" }}
                </n-tag>
              </div>
            </div>

            <div class="plugin-meta">
              <span v-if="plugin.version">版本 {{ plugin.version }}</span>
              <span v-if="plugin.platform">{{ plugin.platform }}</span>
              <span v-if="!plugin.connected">最近连接 {{ formatTime(plugin.lastSeenAt) }}</span>
            </div>

            <div class="plugin-actions">
              <n-button
                size="small"
                :type="plugin.enabled ? 'warning' : 'success'"
                :loading="busyKey === plugin.id"
                @click="togglePlugin(plugin)"
              >
                {{ plugin.enabled ? "停用" : "启用" }}
              </n-button>
              <n-button size="small" @click="selectedId = plugin.id">详情</n-button>
            </div>
          </section>
        </div>
      </template>

      <template v-else>
        <div class="detail-head">
          <n-button quaternary @click="selectedId = null">返回插件列表</n-button>
          <div class="detail-title">
            <h2>{{ selected.name || selected.id }}</h2>
            <span class="mono muted">{{ selected.id }}</span>
          </div>
          <n-tag :type="selected.connected ? 'success' : 'default'" size="small">
            {{ selected.connected ? "已连接" : "已断开" }}
          </n-tag>
        </div>

        <n-empty
          v-if="Object.keys(selected.channels).length === 0"
          description="插件尚未上报渠道状态"
        />
        <div v-else class="channel-grid">
          <section v-for="(channel, id) in selected.channels" :key="id" class="panel channel-card">
            <div class="channel-head">
              <strong>{{ channelLabels[id] || id }}</strong>
              <div class="row">
                <n-tag :type="channel.connected ? 'success' : 'default'" size="small">
                  {{ channel.connected ? "已连接" : "未连接" }}
                </n-tag>
                <n-tag :type="channel.authed ? 'success' : 'warning'" size="small">
                  {{ channel.authed ? "已登录" : "未登录" }}
                </n-tag>
              </div>
            </div>

            <div class="channel-actions">
              <n-button
                size="small"
                :disabled="!selected.connected || !!channel.connected"
                :loading="busyKey === `${selected.id}:${id}:connect`"
                @click="controlProvider(String(id), 'connect')"
              >
                连接
              </n-button>
              <n-button
                size="small"
                :disabled="!selected.connected || !channel.connected"
                :loading="busyKey === `${selected.id}:${id}:disconnect`"
                @click="controlProvider(String(id), 'disconnect')"
              >
                断开
              </n-button>
              <n-button
                size="small"
                :disabled="!selected.connected || !channel.authed"
                :loading="busyKey === `${selected.id}:${id}:open`"
                @click="openProviderSession(String(id))"
              >
                打开会话
              </n-button>
              <n-button
                size="small"
                :disabled="!selected.connected || !channel.authed || id !== 'deepseek-web'"
                :title="id !== 'deepseek-web' ? '该渠道的网页会话管理尚未实现' : '管理渠道网页会话'"
                @click="sessionProviderId = String(id)"
              >
                会话管理
              </n-button>
              <n-button
                size="small"
                :type="channel.desktopEnabled !== false ? 'warning' : 'success'"
                :disabled="channel.desktopEnabled === false ? !channel.authed : false"
                :loading="busyKey === `${selected.id}:${id}:enabled`"
                @click="toggleChannel(String(id), channel.desktopEnabled !== false)"
              >
                {{ channel.desktopEnabled !== false ? "停用" : "启用" }}
              </n-button>
            </div>
          </section>
        </div>
      </template>
    </template>
  </div>
</template>

<style scoped>
.plugin-grid,
.channel-grid {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: var(--space-4);
}

.plugin-card,
.channel-card {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
}

.plugin-head,
.channel-head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: var(--space-3);
}

.plugin-title {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}

.plugin-title strong,
.channel-head strong {
  font-size: var(--text-md);
  font-weight: 600;
  letter-spacing: -0.01em;
}

.plugin-title .mono,
.detail-title .mono {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: var(--text-xs);
}

.plugin-meta {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-2);
  color: var(--muted);
  font-size: var(--text-xs);
}

.plugin-actions,
.channel-actions {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-2);
  padding-top: var(--space-3);
  border-top: 1px solid var(--border);
}

.detail-head {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  margin-bottom: var(--space-4);
}

.detail-title {
  display: flex;
  flex-direction: column;
  gap: 2px;
  flex: 1;
  min-width: 0;
}

.detail-title h2 {
  margin: 0;
  font-family: var(--font-display);
  font-size: var(--text-lg);
  font-weight: 600;
  letter-spacing: -0.015em;
}

@media (max-width: 900px) {
  .plugin-grid,
  .channel-grid {
    grid-template-columns: 1fr;
  }
}
</style>
