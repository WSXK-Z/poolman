<script setup lang="ts">
// 编辑本地网关监听地址、端口和运行选项，并通过 Tauri command 保存配置。
import { onMounted, ref } from "vue";
import { invoke } from "@tauri-apps/api/core";
import {
  NButton,
  NForm,
  NFormItem,
  NInput,
  NInputNumber,
  NSelect,
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

/** 会话标题设置：来源策略与本地模型目录。 */
interface TitleSettings {
  source: string;
  model_dir: string;
  default_model_dir: string;
  has_local_model: boolean;
  model_dir_resolved: string;
}

const titleSource = ref("heuristic");
const titleModelDir = ref("");
const titleDefaults = ref({ dir: "models/qwen3-0.6b", hasModel: false, resolved: "" });
const savingTitle = ref(false);

const titleSourceOptions = [
  { label: "启发式（默认，零依赖）", value: "heuristic" },
  { label: "本地模型（优先，Qwen3-0.6B）", value: "local" },
  { label: "网页渠道（旧行为）", value: "web" },
];

async function loadTitleSettings() {
  try {
    const data = await invoke<TitleSettings>("get_title_settings");
    titleSource.value = data.source;
    titleModelDir.value = data.model_dir;
    titleDefaults.value = {
      dir: data.default_model_dir,
      hasModel: data.has_local_model,
      resolved: data.model_dir_resolved,
    };
  } catch {
    // 非 Tauri 环境：保留默认值
  }
}

async function saveTitleSettings() {
  savingTitle.value = true;
  try {
    const data = await invoke<TitleSettings>("set_title_settings", {
      source: titleSource.value,
      modelDir: titleModelDir.value,
    });
    titleModelDir.value = data.model_dir;
    titleDefaults.value = {
      dir: data.default_model_dir,
      hasModel: data.has_local_model,
      resolved: data.model_dir_resolved,
    };
    message.success("标题设置已保存");
  } catch (error) {
    const reason = error instanceof Error ? error.message : String(error);
    message.error(`保存失败：${reason}`);
  } finally {
    savingTitle.value = false;
  }
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

onMounted(() => {
  void load();
  void loadTitleSettings();
});
</script>

<template>
  <div class="settings panel-stack">
    <header class="page-header">
      <div>
        <p class="page-kicker">07 / Config</p>
        <h1 class="page-title">配置</h1>
      </div>
    </header>

    <section class="panel">
      <div class="panel-head">
        <h2 class="panel-title">连接</h2>
      </div>
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
    </section>

    <section class="panel">
      <div class="panel-head">
        <h2 class="panel-title">对话请求频率</h2>
      </div>
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
    </section>
    <section class="panel">
      <div class="panel-head">
        <h2 class="panel-title">会话标题</h2>
      </div>
      <n-form label-placement="left" label-width="130">
        <n-form-item label="生成方式">
          <n-select v-model:value="titleSource" :options="titleSourceOptions" style="width: 280px" />
        </n-form-item>
        <n-form-item label="本地模型目录">
          <div class="model-dir">
            <n-input
              v-model:value="titleModelDir"
              :placeholder="titleDefaults.dir"
              style="flex: 1"
            />
            <n-button quaternary @click="titleModelDir = titleDefaults.dir">用默认目录</n-button>
          </div>
        </n-form-item>
        <n-form-item label=" ">
          <div class="model-hint">
            <span v-if="titleDefaults.hasModel" class="ok">
              已配置：{{ titleDefaults.resolved }}
            </span>
            <span v-else class="muted">
              未配置本地模型，启用「本地模型」时会回落到网页渠道。模型不随安装包分发，需自行下载到数据目录。
            </span>
          </div>
        </n-form-item>
        <n-form-item label=" ">
          <n-button type="primary" :loading="savingTitle" @click="saveTitleSettings">
            保存
          </n-button>
        </n-form-item>
      </n-form>
    </section>
  </div>
</template>

<style scoped>
.settings {
  max-width: 640px;
}

.interval {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}

.unit {
  color: var(--muted);
  font-size: var(--text-xs);
}

.model-dir {
  display: flex;
  gap: var(--space-2);
  align-items: center;
  width: 100%;
}

.model-hint {
  font-size: var(--text-xs);
  line-height: 1.5;
}

.model-hint .ok {
  color: var(--fg-2);
  font-family: var(--font-mono);
  overflow-wrap: anywhere;
}

.model-hint .muted {
  color: var(--muted);
}
</style>
