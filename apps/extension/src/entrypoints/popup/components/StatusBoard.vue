<script setup lang="ts">
// 将后台同步的供应商状态映射为卡片和操作菜单，菜单动作通过内部消息交给 background。
import { computed, ref } from 'vue';
import { NDropdown, NSpin, useMessage } from 'naive-ui';
import type { ProviderStatusMap } from '@ai-gateway/shared';
import { PROVIDER_LABELS, type ExtensionState } from '~/shared/constants';
import { sendMessage } from '~/lib/messaging';

const props = defineProps<{ connected: boolean; state: ExtensionState | null }>();
const message = useMessage();
const menuVisible = ref(false);
const menuX = ref(0);
const menuY = ref(0);
const selectedProvider = ref<keyof typeof PROVIDER_LABELS | null>(null);

const providers = computed(() => {
  const map: ProviderStatusMap = props.state?.providers ?? {};
  return (Object.entries(PROVIDER_LABELS) as [keyof typeof PROVIDER_LABELS, string][]).map(
    ([id, label]) => {
      const status = map[id];
      const enabled = props.state?.settings.enabledProviders[id] ?? true;
      return {
        id,
        label,
        enabled,
        authed: status?.authed ?? false,
        active: enabled && (status?.authed ?? false),
        tokenCaptured: status?.tokenCaptured ?? false,
        templateCaptured: status?.templateCaptured ?? false,
      };
    },
  );
});

const menuOptions = computed(() => {
  const provider = providers.value.find((item) => item.id === selectedProvider.value);
  return [
    { label: provider?.enabled ? '断开供应商' : '连接供应商', key: 'toggle' },
    { label: '打开供应商页面', key: 'open' },
  ];
});

function showMenu(event: MouseEvent, id: keyof typeof PROVIDER_LABELS): void {
  event.preventDefault();
  selectedProvider.value = id;
  menuX.value = event.clientX;
  menuY.value = event.clientY;
  menuVisible.value = true;
}

async function selectAction(key: string): Promise<void> {
  const id = selectedProvider.value;
  if (!id) return;
  try {
    if (key === 'open') {
      const response = await sendMessage<{ ok: true }>({ type: 'OPEN_PROVIDER', payload: { provider: id } });
      if (!response.ok) throw new Error('打开供应商页面失败');
      return;
    }

    const enabledProviders = { ...(props.state?.settings.enabledProviders ?? {}) };
    const currentlyEnabled = enabledProviders[id] ?? true;
    enabledProviders[id] = !currentlyEnabled;
    await sendMessage({ type: 'UPDATE_SETTINGS', payload: { enabledProviders } });
    message.success(enabledProviders[id] ? '供应商已连接' : '供应商已断开');
  } catch (err) {
    message.error(err instanceof Error ? err.message : String(err));
  }
}
</script>

<template>
  <n-spin :show="state === null">
    <div v-if="providers.length" class="provider-grid">
      <button
        v-for="provider in providers"
        :key="provider.id"
        type="button"
        class="provider-tile"
        :class="{ active: provider.active, disabled: !provider.enabled }"
        :aria-label="`${provider.label}，${provider.active ? '已连接' : provider.enabled ? '未登录' : '已断开'}，右键打开操作菜单`"
        @contextmenu="showMenu($event, provider.id)"
      >
        <span class="provider-logo" :class="`logo-${provider.id}`">{{ provider.label.slice(0, 1) }}</span>
        <span class="provider-label">{{ provider.label }}</span>
        <span class="provider-status">{{ provider.active ? '已连接' : provider.enabled ? '未登录' : '已断开' }}</span>
        <span
          v-if="provider.tokenCaptured || provider.templateCaptured"
          class="credential-mark"
          :title="provider.tokenCaptured ? '已捕获访问凭证' : '已捕获页面聊天接口'"
        >✓</span>
      </button>
    </div>
    <div v-else class="empty-state">正在读取供应商状态…</div>
    <div class="provider-help">亮色表示已启用且检测到登录状态；右键图标可连接、断开或打开网页。</div>
  </n-spin>
  <n-dropdown
    v-model:show="menuVisible"
    :options="menuOptions"
    :x="menuX"
    :y="menuY"
    placement="bottom-start"
    trigger="manual"
    @select="selectAction"
  />
</template>
