<script setup lang="ts">
// 编辑本地网关监听地址、端口和运行选项，并通过 Tauri command 保存配置。
import { onMounted, ref } from "vue";
import { invoke } from "@tauri-apps/api/core";
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

/** 对话请求频率：界面按秒输入，服务端按毫秒保存。 */
const chatIntervalSeconds = ref<number | null>(1);
const maxChatIntervalSeconds = ref(600);
const savingChatInterval = ref(false);

/** 桌面端命令返回的频率设置（含允许范围）。 */
interface ChatSettings {
  chat_interval_ms: number;
  default_chat_interval_ms: number;
  max_chat_interval_ms: number;
}

async function load() {
  try {
    const res = await fetch(getHealthUrl());
    const data = await res.json();
    if (data?.config) {
      config.value = {
        host: data.config.host ?? config.value.host,
        port: data.config.port ?? config.value.port,
        ws_port: data.config.ws_port ?? config.value.ws_port,
        request_timeout_ms: data.config.request_timeout_ms ?? config.value.request_timeout_ms,
      };
      loaded.value = true;
      if (typeof data.config.chat_interval_ms === "number") {
        chatIntervalSeconds.value = data.config.chat_interval_ms / 1000;
      }
    }
  } catch {
    // 服务未启动,回退到本地保存的配置
    const local = localStorage.getItem("gateway-config");
    if (local) {
      try {
        Object.assign(config.value, JSON.parse(local));
      } catch {
        /* ignore */
      }
    }
  }
  // 频率的当前值与上下限以桌面端命令为准（服务没起也读得到）。
  try {
    const data = await invoke<ChatSettings>("get_chat_settings");
    chatIntervalSeconds.value = data.chat_interval_ms / 1000;
    maxChatIntervalSeconds.value = data.max_chat_interval_ms / 1000;
  } catch {
    // 浏览器预览等非 Tauri 环境：沿用 /health 里的值
  }
}

function save() {
  setBaseUrl(baseUrl.value);
  localStorage.setItem("gateway-config", JSON.stringify(config.value));
  message.success("配置已保存");
}

async function saveChatInterval() {
  const seconds = chatIntervalSeconds.value ?? 0;
  savingChatInterval.value = true;
  try {
    const intervalMs = await invoke<number>("set_chat_interval", { intervalSeconds: seconds });
    chatIntervalSeconds.value = intervalMs / 1000;
    message.success(
      intervalMs === 0
        ? "已关闭对话请求频率限制"
        : `已设置：每 ${intervalMs / 1000} 秒最多一次对话请求`,
    );
  } catch (error) {
    const reason = error instanceof Error ? error.message : String(error);
    message.error(`保存失败：${reason}`);
  } finally {
    savingChatInterval.value = false;
  }
}

onMounted(load);
</script>

<template>
  <div class="settings">
    <n-card title="连接配置">
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

    <n-card title="对话请求频率" style="margin-top: 16px">
      <n-form label-placement="left" label-width="130">
        <n-form-item label="请求间隔">
          <div class="interval">
            <n-input-number
              v-model:value="chatIntervalSeconds"
              :min="0"
              :max="maxChatIntervalSeconds"
              :step="1"
              :precision="0"
              style="width: 160px"
            />
            <span class="unit">秒</span>
          </div>
        </n-form-item>
        <n-form-item label=" ">
          <n-button type="primary" :loading="savingChatInterval" @click="saveChatInterval">
            保存
          </n-button>
        </n-form-item>
      </n-form>
      <n-alert type="info" :show-icon="true" style="margin-top: 8px">
        每个网页渠道每 x 秒最多发起一次对话请求，避免网页端因请求过快而报错；0 表示不限制。
        修改后立即生效并保存到数据目录，无需重启。
      </n-alert>
    </n-card>
  </div>
</template>

<style scoped>
.settings {
  max-width: 560px;
}

.interval {
  display: flex;
  align-items: center;
  gap: 8px;
}

.unit {
  color: #888;
  font-size: 12px;
}
</style>
