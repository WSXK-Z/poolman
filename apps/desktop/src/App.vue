<script setup lang="ts">
import { ref } from "vue";
import {
  NConfigProvider,
  NLayout,
  NLayoutContent,
  NLayoutSider,
  NMenu,
  NMessageProvider,
  zhCN,
  dateZhCN,
} from "naive-ui";
import ConfigView from "./views/ConfigView.vue";
import HelpView from "./views/HelpView.vue";
import KeysView from "./views/KeysView.vue";
import SessionView from "./views/SessionView.vue";
import StatusView from "./views/StatusView.vue";

const activeMenu = ref("keys");
const menuOptions = [
  { label: "密钥", key: "keys" },
  { label: "会话", key: "sessions" },
  { label: "状态", key: "status" },
  { label: "配置", key: "config" },
  { label: "帮助", key: "help" },
];
</script>

<template>
  <n-config-provider :locale="zhCN" :date-locale="dateZhCN">
    <n-message-provider>
      <n-layout has-sider style="height: 100vh">
        <n-layout-sider bordered width="200" :native-scrollbar="false">
          <div class="brand">
            <span class="brand-dot" /> 穷门
          </div>
          <n-menu v-model:value="activeMenu" :options="menuOptions" />
        </n-layout-sider>
        <n-layout-content class="page">
          <ConfigView v-if="activeMenu === 'config'" />
          <HelpView v-else-if="activeMenu === 'help'" />
          <KeysView v-else-if="activeMenu === 'keys'" />
          <SessionView v-else-if="activeMenu === 'sessions'" />
          <StatusView v-else />
        </n-layout-content>
      </n-layout>
    </n-message-provider>
  </n-config-provider>
</template>

<style scoped>
.brand {
  height: 48px;
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 0 18px;
  font-size: 15px;
  font-weight: 600;
  border-bottom: 1px solid rgba(128, 128, 128, 0.18);
}

.brand-dot {
  width: 10px;
  height: 10px;
  border-radius: 50%;
  background: #18a058;
}

.page {
  padding: 24px;
  overflow: auto;
}
</style>
