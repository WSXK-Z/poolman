<script setup lang="ts">
import { NCard, NTag, NButton, NSpace, NSpin } from 'naive-ui';
import { browser } from 'wxt/browser';
import { computed } from 'vue';
import type { ProviderStatusMap } from '@ai-gateway/shared';
import { PROVIDER_LABELS, type ExtensionState } from '~/shared/constants';
import { sendMessage } from '~/lib/messaging';

const props = defineProps<{ connected: boolean; state: ExtensionState | null }>();

const loading = computed(() => props.state === null);

const providers = computed(() => {
  const map: ProviderStatusMap = props.state?.providers ?? {};
  return (Object.entries(PROVIDER_LABELS) as [keyof typeof PROVIDER_LABELS, string][]).map(
    ([id, label]) => {
      const s = map[id];
      return {
        id,
        label,
        authed: s?.authed ?? false,
        connected: s?.connected ?? false,
        tokenCaptured: s?.tokenCaptured ?? false,
        templateCaptured: s?.templateCaptured ?? false,
      };
    },
  );
});

async function openOptions(): Promise<void> {
  await browser.runtime.openOptionsPage();
}

async function openProvider(id: keyof typeof PROVIDER_LABELS): Promise<void> {
  await sendMessage({ type: 'OPEN_PROVIDER', payload: { provider: id } });
}
</script>

<template>
  <n-card size="small" title="平台状态">
    <n-spin :show="loading">
      <n-space vertical size="small">
        <div v-for="p in providers" :key="p.id" class="provider-row">
          <span class="provider-name">{{ p.label }}</span>
          <n-tag :type="p.authed ? 'success' : 'warning'" size="small" round>
            {{ p.authed ? '已登录' : '未登录' }}
          </n-tag>
          <n-tag v-if="p.tokenCaptured" type="info" size="small" round>凭证✓</n-tag>
          <n-tag v-if="p.templateCaptured" type="info" size="small" round>接口✓</n-tag>
          <n-button size="tiny" quaternary @click="openProvider(p.id)">打开</n-button>
        </div>
        <n-button size="small" block secondary @click="openOptions">打开设置</n-button>
      </n-space>
    </n-spin>
  </n-card>
</template>

<style scoped>
.provider-row {
  display: flex;
  align-items: center;
  gap: 8px;
}
.provider-name {
  flex: 1;
  font-size: 13px;
}
</style>
