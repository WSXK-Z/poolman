<script setup lang="ts">
// 展示连接说明和诊断信息，并提供打开相关本地目录或网页的操作。
import { onMounted, ref } from "vue";
import { NAlert, NButton, NCode } from "naive-ui";
import { invoke } from "@tauri-apps/api/core";

const extDir = ref("");
const extError = ref("");

// 扩展管理页地址：Chrome 是 `chrome://extensions`，Edge 对应 `edge://extensions`（由桌面端命令选择）。
const EXTENSIONS_URL = "chrome://extensions";

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
    await invoke("open_extension_dir");
    extError.value = "";
  } catch (e: unknown) {
    extError.value = e instanceof Error ? e.message : String(e);
  }
}

// 打开扩展管理页：内部页面只能由浏览器本体打开（WebView 与系统协议处理器都不支持），
// 桌面端找不到浏览器时退化成「复制地址自行粘贴」。
async function openExtensionsPage() {
  try {
    await invoke<{ browser: string; url: string }>("open_extensions_page");
    extError.value = "";
  } catch (e: unknown) {
    extError.value = e instanceof Error ? e.message : String(e);
    await copyExtensionsUrl();
  }
}

async function copyExtensionsUrl() {
  try {
    await navigator.clipboard.writeText(EXTENSIONS_URL);
  } catch {
    // 剪贴板不可用时不额外报错
  }
}

onMounted(() => {
  loadExtDir();
});
</script>

<template>
  <div class="help">
    <header class="page-header">
      <div>
        <p class="page-kicker">08 / Help</p>
        <h1 class="page-title">帮助</h1>
      </div>
    </header>

    <section class="panel">
      <div class="panel-head">
        <h2 class="panel-title">浏览器插件</h2>
      </div>
      <n-alert v-if="extError" type="warning" :show-icon="true" style="margin-bottom: 12px">
        {{ extError }}
      </n-alert>
      <div class="field-label">插件构建目录</div>
      <n-code v-if="extDir" :code="extDir" style="word-break: break-all" />
      <div class="toolbar" style="margin-top: 16px">
        <n-button type="primary" :disabled="!extDir" @click="openExtDir">打开插件目录</n-button>
        <n-button :disabled="!extDir" @click="openExtensionsPage">打开扩展管理页</n-button>
      </div>
    </section>
  </div>
</template>

<style scoped>
.help {
  max-width: 720px;
}

.field-label {
  margin-bottom: var(--space-2);
  color: var(--fg-2);
  font-family: var(--font-mono);
  font-size: var(--text-2xs);
  letter-spacing: 0.08em;
  text-transform: uppercase;
}
</style>
