<script setup lang="ts">
// 编辑本地网关监听地址、端口和运行选项，并通过 Tauri command 保存配置。
import { onMounted, ref } from "vue";
import {
  NAlert,
  NButton,
  NCard,
  NForm,
  NFormItem,
  NInput,
  NInputNumber,
  useMessage,
} from "naive-ui";
import { getBaseUrl, getHealthUrl, setBaseUrl } from "../config";

const message = useMessage();

const baseUrl = ref(getBaseUrl());

const config = ref({
  host: "127.0.0.1",
  port: 13213,
  ws_port: 3001,
  request_timeout_ms: 120000,
});
const loaded = ref(false);

async function load() {
  try {
    const res = await fetch(getHealthUrl());
    const data = await res.json();
    if (data?.config) {
      Object.assign(config.value, data.config);
      loaded.value = true;
      return;
    }
  } catch {
    // 服务未启动,回退到本地保存的配置
  }
  const local = localStorage.getItem("gateway-config");
  if (local) {
    try {
      Object.assign(config.value, JSON.parse(local));
    } catch {
      /* ignore */
    }
  }
}

function save() {
  setBaseUrl(baseUrl.value);
  localStorage.setItem("gateway-config", JSON.stringify(config.value));
  message.success("配置已保存");
}

onMounted(load);
</script>

<template>
  <n-card title="连接配置" style="max-width: 560px">
    <n-form label-placement="left" label-width="130">
      <n-form-item label="网关服务地址">
        <n-input v-model:value="baseUrl" placeholder="http://127.0.0.1:13213" />
      </n-form-item>
      <n-form-item label="Host">
        <n-input v-model:value="config.host" placeholder="127.0.0.1" />
      </n-form-item>
      <n-form-item label="HTTP 端口">
        <n-input-number v-model:value="config.port" :min="1" :max="65535" />
      </n-form-item>
      <n-form-item label="WebSocket 端口">
        <n-input-number v-model:value="config.ws_port" :min="1" :max="65535" />
      </n-form-item>
      <n-form-item label="请求超时 (ms)">
        <n-input-number v-model:value="config.request_timeout_ms" :min="1000" :step="1000" />
      </n-form-item>
      <n-form-item label=" ">
        <n-button type="primary" @click="save">保存</n-button>
      </n-form-item>
    </n-form>
    <n-alert v-if="loaded" type="info" :show-icon="true" style="margin-top: 8px">
      当前显示的是服务端实际运行的连接配置。修改后保存到本地,需重启应用生效。
    </n-alert>
  </n-card>
</template>
