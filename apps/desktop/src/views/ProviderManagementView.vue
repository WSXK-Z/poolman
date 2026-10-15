<script setup lang="ts">
// 供应商管理：展示各供应商及其提供的模型。
import { onMounted, ref } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { NAlert, NCard, NEmpty, NTag } from "naive-ui";

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
    <h2>供应商管理</h2>
    <n-alert v-if="error" type="error" style="margin-bottom: 16px">{{ error }}</n-alert>
    <n-empty v-if="providers.length === 0" description="暂无供应商" />
    <div class="provider-list">
      <n-card v-for="provider in providers" :key="provider.id" size="small">
        <template #header>
          <div class="header">
            <strong>{{ provider.name }}</strong>
            <n-tag size="small">{{ provider.kind }}</n-tag>
            <n-tag size="small" :type="provider.enabled ? 'success' : 'warning'">
              {{ provider.enabled ? "已启用" : "已停用" }}
            </n-tag>
          </div>
        </template>
        <div class="muted">标识：{{ provider.id }}</div>
        <div class="section-title">提供的模型（{{ provider.models.length }}）</div>
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
      </n-card>
    </div>
    <n-alert type="info" style="margin-top: 16px">
      插件供应商经由浏览器插件在已登录网页上执行请求；具体使用哪台插件实例由插件管理器统一调度，
      请在「插件管理」中查看与启停插件。
    </n-alert>
  </div>
</template>

<style scoped>
.provider-list { display: grid; gap: 12px; max-width: 900px; }
.header { display: flex; align-items: center; gap: 8px; }
.muted { color: #888; font-size: 12px; }
.section-title { margin: 12px 0 6px; font-weight: 600; font-size: 13px; }
.tags { display: flex; flex-wrap: wrap; gap: 6px; }
</style>
