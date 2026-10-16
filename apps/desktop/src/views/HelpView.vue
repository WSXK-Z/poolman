<script setup lang="ts">
// 展示连接说明和诊断信息，并提供打开相关本地目录或网页的操作。
import { onMounted, ref } from "vue";
import { NAlert, NButton, NCard, NCode } from "naive-ui";
import { invoke } from "@tauri-apps/api/core";
import { openPath } from "@tauri-apps/plugin-opener";

const extDir = ref("");
const extError = ref("");
const extHint = ref("");

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
    await openPath(extDir.value);
  } catch (e: unknown) {
    extError.value = e instanceof Error ? e.message : String(e);
  }
}

// 打开扩展管理页：内部页面只能由浏览器本体打开（WebView 与系统协议处理器都不支持），
// 桌面端找不到浏览器时退化成「复制地址自行粘贴」。
async function openExtensionsPage() {
  try {
    const page = await invoke<{ browser: string; url: string }>("open_extensions_page");
    extError.value = "";
    extHint.value = `已调用 ${page.browser} 打开扩展管理页（${page.url}）；若浏览器没有跳转，请把该地址粘贴到地址栏。`;
  } catch (e: unknown) {
    extError.value = e instanceof Error ? e.message : String(e);
    extHint.value = `请把 ${EXTENSIONS_URL} 粘贴到浏览器地址栏打开`;
    await copyExtensionsUrl();
  }
}

async function copyExtensionsUrl() {
  try {
    await navigator.clipboard.writeText(EXTENSIONS_URL);
    extHint.value += "（地址已复制）";
  } catch {
    // 剪贴板不可用时保留提示文字即可，不再叠加错误
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
      <n-alert v-if="extHint" type="info" :show-icon="true" style="margin-bottom: 12px">
        {{ extHint }}
      </n-alert>
      <div style="color: #888; margin-bottom: 4px">插件构建目录：</div>
      <n-code v-if="extDir" :code="extDir" style="word-break: break-all" />
      <div style="display: flex; gap: 8px; margin-top: 12px; flex-wrap: wrap">
        <n-button type="primary" :disabled="!extDir" @click="openExtDir">打开插件目录</n-button>
        <n-button :disabled="!extDir" @click="openExtensionsPage">打开扩展管理页</n-button>
      </div>
      <n-alert type="info" :show-icon="true" style="margin-top: 12px">
        安装步骤：① 打开「扩展管理页」（Chrome 为 chrome://extensions，Edge 为 edge://extensions）；② 开启右上角「开发者模式」；③ 点击「加载已解压的扩展程序」；④ 选择上面的插件目录。
      </n-alert>
    </n-card>
  </div>
</template>
