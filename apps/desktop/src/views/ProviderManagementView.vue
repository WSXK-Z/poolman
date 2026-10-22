<script setup lang="ts">
// 供应商管理：展示各供应商及其提供的模型。
import { onMounted, ref } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { NAlert, NEmpty, NTag } from "naive-ui";

interface ProviderModel {
  id: string;
  channel: string | null;
  displayName: string | null;
  enabled: boolean;
  status: string | null;
}
interface ManagedProvider {
  id: string;
  name: string;
  kind: string;
  enabled: boolean;
  models: ProviderModel[];
}

function modelLabel(model: ProviderModel) {
  return model.displayName ?? model.id;
}

const providers = ref<ManagedProvider[]>([]);
const error = ref("");

async function load() {
  try {
    providers.value = await invoke<ManagedProvider[]>("list_providers");
    error.value = "";
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
  }
}

onMounted(() => void load());
</script>

<template>
  <div>
    <header class="page-header">
      <div>
        <p class="page-kicker">06 / Providers</p>
        <h1 class="page-title">供应商</h1>
      </div>
    </header>

    <n-alert v-if="error" type="error" style="margin-bottom: 16px">{{ error }}</n-alert>
    <n-empty v-if="providers.length === 0" description="暂无供应商" />
    <div class="provider-grid">
      <section v-for="provider in providers" :key="provider.id" class="panel provider-card">
        <div class="provider-head">
          <strong>{{ provider.name }}</strong>
          <div class="row">
            <n-tag size="small">{{ provider.kind }}</n-tag>
            <n-tag size="small" :type="provider.enabled ? 'success' : 'warning'">
              {{ provider.enabled ? "已启用" : "已停用" }}
            </n-tag>
          </div>
        </div>
        <div class="provider-id mono">{{ provider.id }}</div>
        <div class="model-block">
          <span class="model-label">提供的模型 · {{ provider.models.length }}</span>
          <div class="tags">
            <n-tag
              v-for="model in provider.models"
              :key="model.id"
              size="small"
              :type="model.enabled ? 'info' : 'warning'"
            >
              {{ modelLabel(model) }}
            </n-tag>
          </div>
        </div>
      </section>
    </div>
  </div>
</template>

<style scoped>
.provider-grid {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: var(--space-4);
}

.provider-card {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
}

.provider-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-3);
}

.provider-head strong {
  font-size: var(--text-md);
  font-weight: 600;
  letter-spacing: -0.01em;
}

.provider-id {
  color: var(--muted);
  font-size: var(--text-xs);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.model-block {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
  padding-top: var(--space-3);
  border-top: 1px solid var(--border);
}

.model-label {
  color: var(--brass);
  font-family: var(--font-mono);
  font-size: var(--text-2xs);
  letter-spacing: 0.16em;
  text-transform: uppercase;
}

.tags {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}

@media (max-width: 900px) {
  .provider-grid {
    grid-template-columns: 1fr;
  }
}
</style>
