<script setup lang="ts">
// Popup 主界面：编辑桌面服务连接设置，并展示供应商登录和连接状态。
import {
  NButton,
  NCard,
  NForm,
  NFormItem,
  NInput,
  NInputNumber,
  NSwitch,
  useMessage,
} from 'naive-ui';
import { computed, onMounted, ref } from 'vue';
import type { AppSettings } from '~/shared/constants';
import { DEFAULT_SERVER_URL } from '~/shared/constants';
import { useExtensionState } from '~/lib/useExtensionState';
import { sendMessage } from '~/lib/messaging';
import StatusBoard from './components/StatusBoard.vue';

const { state, refresh } = useExtensionState();
const message = useMessage();
const saving = ref(false);
const form = ref<AppSettings>({
  serverUrl: DEFAULT_SERVER_URL,
  serverToken: '',
  autoReconnect: true,
  requestTimeoutMs: 120_000,
  enabledProviders: {},
});
const connected = computed(() => state.value?.connected ?? false);

onMounted(async () => {
  try {
    const response = await sendMessage<{ ok: true; data: AppSettings }>({ type: 'GET_SETTINGS' });
    form.value = { ...form.value, ...response.data };
  } catch (err) {
    message.error(err instanceof Error ? err.message : String(err));
  }
});

async function saveSettings(): Promise<void> {
  saving.value = true;
  try {
    await sendMessage({ type: 'UPDATE_SETTINGS', payload: { ...form.value } });
    message.success('服务端配置已保存');
    await refresh();
  } catch (err) {
    message.error(err instanceof Error ? err.message : String(err));
  } finally {
    saving.value = false;
  }
}
</script>

<template>
  <main class="popup-root">
    <header class="popup-header">
      <div>
        <div class="popup-title">AI Gateway</div>
        <div class="popup-subtitle">本地 AI 网关连接器</div>
      </div>
      <span class="status-badge">
        <span class="status-dot" :class="connected ? 'on' : 'off'" />
        {{ connected ? '服务端已连接' : '服务端未连接' }}
      </span>
    </header>

    <n-card size="small" class="server-card">
      <template #header>
        <div class="section-title"><span class="section-icon">01</span>服务端配置</div>
      </template>
      <n-form label-placement="top" size="small">
        <n-form-item label="WebSocket 服务地址">
          <n-input v-model:value="form.serverUrl" placeholder="ws://127.0.0.1:3001" />
        </n-form-item>
        <n-form-item label="鉴权 Token（可选）">
          <n-input
            v-model:value="form.serverToken"
            type="password"
            show-password-on="click"
            placeholder="服务端未启用鉴权时可留空"
          />
        </n-form-item>
        <div class="settings-row">
          <span>自动重连</span>
          <n-switch v-model:value="form.autoReconnect" />
        </div>
        <div class="settings-row timeout-row">
          <span>请求超时</span>
          <n-input-number
            v-model:value="form.requestTimeoutMs"
            :min="5000"
            :max="600000"
            :step="5000"
            size="small"
          />
          <span class="unit">毫秒</span>
        </div>
        <n-button type="primary" block :loading="saving" @click="saveSettings">
          保存并连接
        </n-button>
      </n-form>
    </n-card>

    <n-card size="small" class="providers-card">
      <template #header>
        <div class="section-title">
          <span class="section-icon">02</span>供应商连接
          <span class="section-hint">右键图标管理</span>
        </div>
      </template>
      <StatusBoard :connected="connected" :state="state" />
    </n-card>
  </main>
</template>
