<script setup lang="ts">
import { NButton, NInput, NSelect, NCard, NSpace, NAlert } from 'naive-ui';
import { ref } from 'vue';
import type { ModelProvider } from '@ai-gateway/shared';
import { PROVIDER_LABELS } from '~/shared/constants';
import { sendMessage } from '~/lib/messaging';

const providerOptions = (Object.entries(PROVIDER_LABELS) as [ModelProvider, string][]).map(
  ([value, label]) => ({ value, label }),
);

const selected = ref<ModelProvider>('deepseek-web');
const prompt = ref('');
const result = ref('');
const loading = ref(false);
const error = ref('');

async function run(): Promise<void> {
  if (!prompt.value.trim()) return;
  loading.value = true;
  result.value = '';
  error.value = '';
  try {
    const resp = await sendMessage<{ ok: true; result: { content: string; elapsedMs: number } }>({
      type: 'QUICK_TEST',
      payload: { model: selected.value, prompt: prompt.value },
    });
    result.value = `[${resp.result.elapsedMs}ms]\n${resp.result.content}`;
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
  } finally {
    loading.value = false;
  }
}
</script>

<template>
  <n-card size="small" title="快速测试">
    <n-space vertical size="small">
      <n-select v-model:value="selected" :options="providerOptions" size="small" />
      <n-input
        v-model:value="prompt"
        type="textarea"
        :rows="3"
        placeholder="输入测试问题"
        @keydown.ctrl.enter="run"
      />
      <n-button type="primary" size="small" :loading="loading" block @click="run">发送</n-button>
      <n-alert v-if="error" type="error" size="small">
        {{ error }}
      </n-alert>
      <n-alert v-if="result" type="info" size="small">
        <pre class="result">{{ result }}</pre>
      </n-alert>
    </n-space>
  </n-card>
</template>

<style scoped>
.result {
  white-space: pre-wrap;
  word-break: break-word;
  margin: 0;
  font-size: 12px;
  max-height: 220px;
  overflow: auto;
}
</style>
