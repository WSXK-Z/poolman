<script setup lang="ts">
// 汇总插件连接及其渠道登录状态，并提供插件级和渠道级启停控制。
import { computed, onMounted, onUnmounted, ref } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { NAlert, NButton, NCard, NEmpty, NTag } from "naive-ui";
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
    <h2>插件管理</h2>
    <n-alert v-if="error" type="error" style="margin-bottom: 16px">{{ error }}</n-alert>
    <template v-if="!selected">
      <n-empty v-if="plugins.length === 0" description="暂无连接的插件" />
      <div class="plugin-list">
        <n-card v-for="plugin in plugins" :key="plugin.id" size="small">
          <div class="plugin-row">
            <div>
              <strong>{{ plugin.name || "浏览器插件" }}</strong>
              <div class="muted">连接标识：{{ plugin.id }}</div>
              <div class="muted" v-if="plugin.version">
                版本：{{ plugin.version }}<span v-if="plugin.platform"> · {{ plugin.platform }}</span>
              </div>
              <div class="tags">
                <n-tag :type="plugin.connected ? 'success' : 'default'" size="small">
                  {{ plugin.connected ? "已连接" : "已断开" }}
                </n-tag>
                <n-tag :type="plugin.enabled ? 'success' : 'default'" size="small">
                  {{ plugin.enabled ? "参与请求分配" : "已停用" }}
                </n-tag>
              </div>
              <div class="muted" v-if="!plugin.connected">
                最近连接：{{ formatTime(plugin.lastSeenAt) }}
              </div>
            </div>
            <div class="actions">
              <n-button size="small" :type="plugin.enabled ? 'warning' : 'success'"
                :loading="busyKey === plugin.id" @click="togglePlugin(plugin)">
                {{ plugin.enabled ? "停用" : "启用" }}
              </n-button>
              <n-button size="small" @click="selectedId = plugin.id">详情</n-button>
            </div>
          </div>
        </n-card>
      </div>
    </template>
    <template v-else>
      <div class="heading">
        <n-button quaternary @click="selectedId = null">返回插件列表</n-button>
        <h3>插件渠道 · {{ selected.id }}</h3>
        <n-tag :type="selected.connected ? 'success' : 'default'" size="small">
          {{ selected.connected ? "已连接" : "已断开" }}
        </n-tag>
      </div>
      <n-alert v-if="!selected.connected" type="warning" style="margin-bottom: 12px">
        插件当前未连接，无法下发连接/断开/打开会话指令；启用选择仍会保存并在插件重新连接后生效。
      </n-alert>
      <n-empty v-if="Object.keys(selected.channels).length === 0" description="插件尚未上报渠道状态" />
      <div class="provider-list">
        <n-card v-for="(channel, id) in selected.channels" :key="id" size="small">
          <div class="plugin-row">
            <div>
              <strong>{{ channelLabels[id] || id }}</strong>
              <div class="tags">
                <n-tag :type="channel.connected ? 'success' : 'default'" size="small">
                  {{ channel.connected ? "已连接" : "未连接" }}
                </n-tag>
                <n-tag :type="channel.authed ? 'success' : 'warning'" size="small">
                  {{ channel.authed ? "已登录" : "未登录" }}
                </n-tag>
              </div>
            </div>
            <div class="actions">
              <n-button size="small" :disabled="!selected.connected || !!channel.connected"
                :loading="busyKey === `${selected.id}:${id}:connect`"
                @click="controlProvider(String(id), 'connect')">连接</n-button>
              <n-button size="small" :disabled="!selected.connected || !channel.connected"
                :loading="busyKey === `${selected.id}:${id}:disconnect`"
                @click="controlProvider(String(id), 'disconnect')">断开</n-button>
              <n-button size="small" :disabled="!selected.connected || !channel.authed"
                :loading="busyKey === `${selected.id}:${id}:open`"
                @click="openProviderSession(String(id))">打开会话</n-button>
              <n-button size="small" :disabled="!selected.connected || !channel.authed || id !== 'deepseek-web'"
                :title="id !== 'deepseek-web' ? '该渠道的网页会话管理尚未实现' : '管理渠道网页会话'"
                @click="sessionProviderId = String(id)">会话管理</n-button>
              <n-button size="small" :type="channel.desktopEnabled !== false ? 'warning' : 'success'"
                :disabled="channel.desktopEnabled === false ? !channel.authed : false"
                :loading="busyKey === `${selected.id}:${id}:enabled`"
                @click="toggleChannel(String(id), channel.desktopEnabled !== false)">
                {{ channel.desktopEnabled !== false ? "停用" : "启用" }}
              </n-button>
            </div>
          </div>
        </n-card>
      </div>
      <n-alert type="info" style="margin-top: 12px">
        连接会在浏览器中打开该渠道页面；插件启用状态控制网页连接，桌面启用状态控制此插件是否参与对应渠道的请求分配。
      </n-alert>
    </template>
    </template>
  </div>
</template>

<style scoped>
.plugin-list, .provider-list { display: grid; gap: 10px; max-width: 900px; }
.plugin-row, .actions, .tags, .heading { display: flex; align-items: center; gap: 8px; }
.plugin-row { justify-content: space-between; }
.heading { margin: 12px 0; }
.heading h3 { margin: 0; }
.muted { color: #888; font-size: 12px; margin: 5px 0; }
.tags { margin-top: 6px; }
</style>
