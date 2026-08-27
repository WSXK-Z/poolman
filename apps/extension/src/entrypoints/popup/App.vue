<script setup lang="ts">
import { darkTheme, dateZhCN, zhCN, NConfigProvider, NMessageProvider, NDialogProvider } from 'naive-ui';
import { computed, ref } from 'vue';
import { useExtensionState } from '~/lib/useExtensionState';
import StatusBoard from './components/StatusBoard.vue';
import QuickTest from './components/QuickTest.vue';

const { state } = useExtensionState();
const dark = ref(false);

const connected = computed(() => state.value?.connected ?? false);
const theme = computed(() => (dark.value ? darkTheme : null));
</script>

<template>
  <n-config-provider :theme="theme" :locale="zhCN" :date-locale="dateZhCN">
    <n-message-provider>
      <n-dialog-provider>
        <div class="popup-root" :class="{ dark }">
          <div class="popup-header">
            <span class="popup-title">AI Gateway</span>
            <span class="status-badge">
              <span class="status-dot" :class="connected ? 'on' : 'off'" />
              {{ connected ? '服务已连接' : '未连接' }}
            </span>
          </div>
          <StatusBoard :connected="connected" :state="state" />
          <QuickTest />
        </div>
      </n-dialog-provider>
    </n-message-provider>
  </n-config-provider>
</template>
