<script setup lang="ts">
import {
  NAlert,
  NButton,
  NForm,
  NFormItem,
  NInput,
  NInputNumber,
  NSpace,
  NSwitch,
  useMessage,
} from 'naive-ui';
import { onMounted, ref } from 'vue';
import { sendMessage } from '~/lib/messaging';
import { DEFAULT_SERVER_URL, type AppSettings } from '~/shared/constants';

const message = useMessage();

const form = ref<AppSettings>({
  serverUrl: DEFAULT_SERVER_URL,
  serverToken: '',
  autoReconnect: true,
  requestTimeoutMs: 120_000,
  enabledProviders: {},
});
const saving = ref(false);

onMounted(async () => {
  const resp = await sendMessage<{ ok: true; data: AppSettings }>({ type: 'GET_SETTINGS' });
  form.value = { ...form.value, ...resp.data };
});

async function save(): Promise<void> {
  saving.value = true;
  try {
    await sendMessage({ type: 'UPDATE_SETTINGS', payload: { ...form.value } });
    message.success('设置已保存');
  } catch (err) {
    message.error(err instanceof Error ? err.message : String(err));
  } finally {
    saving.value = false;
  }
}
</script>

<template>
  <n-form label-placement="left" label-width="120" :model="form">
    <n-form-item label="服务地址">
      <n-input v-model:value="form.serverUrl" placeholder="ws://127.0.0.1:3001" />
    </n-form-item>
    <n-form-item label="鉴权 Token">
      <n-input v-model:value="form.serverToken" type="password" show-password-on="click" placeholder="可选" />
    </n-form-item>
    <n-form-item label="自动重连">
      <n-switch v-model:value="form.autoReconnect" />
    </n-form-item>
    <n-form-item label="请求超时(ms)">
      <n-input-number v-model:value="form.requestTimeoutMs" :min="5000" :max="600000" :step="5000" />
    </n-form-item>
    <n-space>
      <n-button type="primary" :loading="saving" @click="save">保存设置</n-button>
    </n-space>
    <n-alert type="info" style="margin-top: 16px">
      插件会连接本地服务（默认 ws://127.0.0.1:3001）。请确保已先启动 <code>apps/server</code>。
    </n-alert>
  </n-form>
</template>
