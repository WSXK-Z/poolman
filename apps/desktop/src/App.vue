<script setup lang="ts">
// 桌面端主框架：252px 侧栏 + 最大宽度内容区，按设计系统切换亮/暗主题。
import { computed, h, ref, watchEffect, type Component } from "vue";
import {
  NButton,
  NConfigProvider,
  NIcon,
  NMenu,
  NMessageProvider,
  darkTheme,
  dateZhCN,
  zhCN,
} from "naive-ui";
import {
  ChatbubblesOutline,
  CubeOutline,
  DocumentTextOutline,
  ExtensionPuzzleOutline,
  HelpCircleOutline,
  KeyOutline,
  LayersOutline,
  MoonOutline,
  PulseOutline,
  SettingsOutline,
  SunnyOutline,
} from "@vicons/ionicons5";
import ConfigView from "./views/ConfigView.vue";
import HelpView from "./views/HelpView.vue";
import KeysView from "./views/KeysView.vue";
import SessionView from "./views/SessionView.vue";
import StatusView from "./views/StatusView.vue";
import PluginManagementView from "./views/PluginManagementView.vue";
import ProviderManagementView from "./views/ProviderManagementView.vue";
import RequestRecordsView from "./views/RequestRecordsView.vue";
import LocalSessionsView from "./views/LocalSessionsView.vue";
import { darkThemeOverrides, lightThemeOverrides } from "./theme";

const THEME_KEY = "poolman-theme";
const isDark = ref(localStorage.getItem(THEME_KEY) === "dark");

watchEffect(() => {
  document.documentElement.dataset.theme = isDark.value ? "dark" : "light";
  localStorage.setItem(THEME_KEY, isDark.value ? "dark" : "light");
});

const themeOverrides = computed(() =>
  isDark.value ? darkThemeOverrides : lightThemeOverrides,
);

function toggleTheme() {
  isDark.value = !isDark.value;
}

function renderIcon(icon: Component) {
  return () => h(NIcon, null, { default: () => h(icon) });
}

const activeMenu = ref("keys");
const menuOptions = [
  { label: "密钥", key: "keys", icon: renderIcon(KeyOutline) },
  { label: "会话", key: "sessions", icon: renderIcon(ChatbubblesOutline) },
  { label: "会话管理", key: "local-sessions", icon: renderIcon(LayersOutline) },
  { label: "本地请求管理", key: "requests", icon: renderIcon(DocumentTextOutline) },
  { label: "状态", key: "status", icon: renderIcon(PulseOutline) },
  { label: "插件管理", key: "plugins", icon: renderIcon(ExtensionPuzzleOutline) },
  { label: "供应商管理", key: "providers", icon: renderIcon(CubeOutline) },
  { label: "配置", key: "config", icon: renderIcon(SettingsOutline) },
  { label: "帮助", key: "help", icon: renderIcon(HelpCircleOutline) },
];
</script>

<template>
  <n-config-provider
    :locale="zhCN"
    :date-locale="dateZhCN"
    :theme="isDark ? darkTheme : undefined"
    :theme-overrides="themeOverrides"
  >
    <n-message-provider>
      <div class="app-shell">
        <aside class="sidebar">
          <div class="brand">
            <div class="brand-mark">P</div>
            <div class="brand-copy">
              <span class="brand-name">poolman</span>
              <span class="brand-meta">AI Gateway</span>
            </div>
          </div>

          <div class="nav-block">
            <div class="nav-label">Navigation</div>
            <n-menu
              v-model:value="activeMenu"
              class="side-menu"
              :options="menuOptions"
              :indent="18"
              :root-indent="8"
            />
          </div>

          <div class="sidebar-footer">
            <span class="sidebar-footer-label">{{ isDark ? "Dark" : "Light" }}</span>
            <n-button
              quaternary
              circle
              size="small"
              :aria-label="isDark ? '切换到亮色主题' : '切换到暗色主题'"
              @click="toggleTheme"
            >
              <template #icon>
                <n-icon>
                  <SunnyOutline v-if="isDark" />
                  <MoonOutline v-else />
                </n-icon>
              </template>
            </n-button>
          </div>
        </aside>

        <main class="content">
          <div class="content-inner">
            <ConfigView v-if="activeMenu === 'config'" />
            <HelpView v-else-if="activeMenu === 'help'" />
            <KeysView v-else-if="activeMenu === 'keys'" />
            <SessionView v-else-if="activeMenu === 'sessions'" />
            <LocalSessionsView v-else-if="activeMenu === 'local-sessions'" />
            <RequestRecordsView v-else-if="activeMenu === 'requests'" />
            <PluginManagementView v-else-if="activeMenu === 'plugins'" />
            <ProviderManagementView v-else-if="activeMenu === 'providers'" />
            <StatusView v-else />
          </div>
        </main>
      </div>
    </n-message-provider>
  </n-config-provider>
</template>
