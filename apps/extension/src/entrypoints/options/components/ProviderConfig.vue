<script setup lang="ts">
import { NCard, NSpace, NTag, NSwitch, NButton, NSpin } from 'naive-ui';
import { computed } from 'vue';
import type { ModelProvider, ProviderStatusMap } from '@ai-gateway/shared';
import { PROVIDER_LABELS } from '~/shared/constants';
import { sendMessage } from '~/lib/messaging';
import { useExtensionState } from '~/lib/useExtensionState';

const { state, refresh } = useExtensionState();

const providers = computed(() => {
  const map: ProviderStatusMap = state.value?.providers ?? {};
  const enabled = state.value?.settings.enabledProviders ?? {};
  return (Object.entries(PROVIDER_LABELS) as [ModelProvider, string][]).map(([id, label]) => ({
    id,
    label,
    enabled: enabled[id] ?? true,
    status: map[id],
  }));
});

const captureText = (p: { status?: ProviderStatusMap[ModelProvider] }): string => {
  const s = p.status;
  if (s?.templateCaptured) return '凭证+接口已捕获';
  if (s?.tokenCaptured) return '凭证已捕获';
  return '未捕获';
};


async function toggle(id: ModelProvider, value: boolean): Promise<void> {
  const enabledProviders = { ...(state.value?.settings.enabledProviders ?? {}) };
  enabledProviders[id] = value;
  await sendMessage({ type: 'UPDATE_SETTINGS', payload: { enabledProviders } });
  await refresh();
}

async function open(id: ModelProvider): Promise<void> {
  await sendMessage({ type: 'OPEN_PROVIDER', payload: { provider: id } });
}
</script>

<template>
  <n-spin :show="state === null">
    <n-space vertical size="medium">
      <n-card v-for="p in providers" :key="p.id" size="small" :title="p.label">
        <div class="provider-config-row">
          <n-switch :value="p.enabled" @update:value="(v: boolean) => toggle(p.id, v)" />
          <n-tag :type="p.status?.authed ? 'success' : 'warning'" size="small" round>
            {{ p.status?.authed ? '已登录' : '未登录' }}
          </n-tag>
          <n-tag :type="p.status?.templateCaptured ? 'success' : 'default'" size="small" round>
            {{ captureText(p) }}
          </n-tag>
          <span class="models">
            {{ (p.status?.models ?? []).join(' / ') || '—' }}
          </span>
          <n-button size="small" secondary @click="open(p.id)">打开页面</n-button>
        </div>
      </n-card>
    </n-space>
  </n-spin>
</template>

<style scoped>
.provider-config-row {
  display: flex;
  align-items: center;
  gap: 12px;
}
.models {
  flex: 1;
  font-size: 12px;
  color: #999;
}
</style>
