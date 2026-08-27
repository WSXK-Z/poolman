import { onUnmounted, ref } from 'vue';
import { sendMessage } from './messaging';
import type { ExtensionState } from '~/shared/constants';

/**
 * 轮询 background 状态（MV3 service worker 无法主动推送，
 * 采用轻量轮询保证 UI 实时性）。
 */
export function useExtensionState(pollMs = 2_000) {
  const state = ref<ExtensionState | null>(null);
  const error = ref<string | null>(null);

  async function refresh(): Promise<void> {
    try {
      const resp = await sendMessage<{ ok: true; state: ExtensionState }>({ type: 'GET_STATE' });
      state.value = resp.state;
      error.value = null;
    } catch (err) {
      error.value = err instanceof Error ? err.message : String(err);
    }
  }

  const timer = setInterval(() => void refresh(), pollMs);
  void refresh();

  onUnmounted(() => clearInterval(timer));

  return { state, error, refresh };
}
