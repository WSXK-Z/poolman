<script setup lang="ts">
import { NButton, NEmpty, NScrollbar, NTag, NSpace } from 'naive-ui';
import { onUnmounted, ref } from 'vue';
import { sendMessage } from '~/lib/messaging';
import type { LogEntry } from '~/shared/constants';

const logs = ref<LogEntry[]>([]);
let timer: ReturnType<typeof setInterval> | undefined;

async function refresh(): Promise<void> {
  const resp = await sendMessage<{ ok: true; logs: LogEntry[] }>({ type: 'GET_LOGS' });
  logs.value = resp.logs;
}

async function clear(): Promise<void> {
  const resp = await sendMessage<{ ok: true; logs: LogEntry[] }>({ type: 'CLEAR_LOGS' });
  logs.value = resp.logs;
}

timer = setInterval(() => void refresh(), 2_000);
void refresh();
onUnmounted(() => {
  if (timer) clearInterval(timer);
});

function formatTime(ts: number): string {
  return new Date(ts).toLocaleTimeString();
}
</script>

<template>
  <div class="log-viewer">
    <n-space style="margin-bottom: 12px" justify="space-between">
      <span class="log-count">共 {{ logs.length }} 条</span>
      <n-button size="small" @click="clear">清空日志</n-button>
    </n-space>
    <n-scrollbar style="max-height: 480px">
      <n-empty v-if="logs.length === 0" description="暂无日志" style="padding: 24px" />
      <div v-else class="log-list">
        <div v-for="log in logs" :key="log.id" class="log-item">
          <span class="log-time">{{ formatTime(log.time) }}</span>
          <n-tag :type="log.level === 'error' ? 'error' : log.level === 'warn' ? 'warning' : 'info'" size="tiny">
            {{ log.level }}
          </n-tag>
          <span class="log-message">{{ log.message }}</span>
        </div>
      </div>
    </n-scrollbar>
  </div>
</template>

<style scoped>
.log-list {
  display: flex;
  flex-direction: column;
}
.log-item {
  display: flex;
  align-items: baseline;
  gap: 8px;
  padding: 6px 4px;
  border-bottom: 1px solid #f0f0f0;
  font-size: 12px;
}
.log-time {
  color: #999;
  flex-shrink: 0;
}
.log-message {
  word-break: break-all;
}
</style>
