<script setup lang="ts">
// 展示连接说明和诊断信息，并提供打开相关本地目录或网页的操作。
import { onMounted, ref } from "vue";
import { NAlert, NButton, NCard, NCode } from "naive-ui";
import { invoke } from "@tauri-apps/api/core";
import { openPath, openUrl } from "@tauri-apps/plugin-opener";

const extDir = ref("");
const extError = ref("");

async function loadExtDir() {
  try {
    extDir.value = await invoke<string>("plugin_extension_dir");
    extError.value = "";
  } catch (e: unknown) {
    extError.value = e instanceof Error ? e.message : String(e);
  }
}

async function openExtDir() {
  try {
    await openPath(extDir.value);
  } catch (e: unknown) {
    extError.value = e instanceof Error ? e.message : String(e);
  }
}

async function openExtensionsPage() {
  try {
    await openUrl("chrome://extensions");
  } catch (e: unknown) {
    extError.value = e instanceof Error ? e.message : String(e);
  }
}

onMounted(() => {
  loadExtDir();
});
</script>

<template>
  <div>
    <n-card title="浏览器插件安装" style="max-width: 720px">
      <n-alert v-if="extError" type="warning" :show-icon="true" style="margin-bottom: 12px">
        {{ extError }}
      </n-alert>
      <div style="color: #888; margin-bottom: 4px">插件构建目录：</div>
      <n-code v-if="extDir" :code="extDir" style="word-break: break-all" />
      <div style="display: flex; gap: 8px; margin-top: 12px; flex-wrap: wrap">
        <n-button type="primary" :disabled="!extDir" @click="openExtDir">打开插件目录</n-button>
        <n-button :disabled="!extDir" @click="openExtensionsPage">打开扩展管理页</n-button>
      </div>
      <n-alert type="info" :show-icon="true" style="margin-top: 12px">
        安装步骤：① 打开「扩展管理页」；② 开启右上角「开发者模式」；③ 点击「加载已解压的扩展程序」；④ 选择上面的插件目录。
      </n-alert>
    </n-card>
  </div>
</template>
