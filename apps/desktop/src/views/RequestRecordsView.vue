<script setup lang="ts">
// 本地请求管理：每次对话请求一条记录，表格单行展示摘要，点击字段在右侧抽屉看原文 / 格式化内容。
import { computed, h, onMounted, ref } from "vue";
import { invoke } from "@tauri-apps/api/core";
import {
  NAlert,
  NButton,
  NDataTable,
  NDrawer,
  NDrawerContent,
  NEmpty,
  NPopconfirm,
  NSpin,
  NTabPane,
  NTabs,
  NTag,
  useMessage,
  type DataTableColumns,
} from "naive-ui";

/** 可点击查看的字段。 */
type FieldKey =
  | "id"
  | "protocol"
  | "api_key"
  | "headers"
  | "duration"
  | "date"
  | "system_prompt"
  | "user_prompt"
  | "tools"
  | "history"
  | "answer";

interface RequestRecordSummary {
  id: string;
  model: string;
  stream: boolean;
  status: string;
  protocol: string;
  method: string;
  endpoint: string;
  api_key: string;
  headers: string;
  duration_ms: number;
  created_at: number;
  system_prompt: string;
  user_prompt: string;
  tools: string;
  history: string;
  answer_type: string;
  answer_preview: string;
}

interface RequestRecordDetail {
  id: string;
  model: string;
  stream: boolean;
  status: string;
  protocol: string;
  method: string;
  endpoint: string;
  headers: string;
  api_key: string;
  duration_ms: number;
  created_at: number;
  system_prompt: string;
  user_prompt: string;
  tools: string;
  history: string;
  answer_type: string;
  answer_content: string;
  answer_reasoning: string;
  answer_tool_calls: string;
  error: string;
}

const message = useMessage();
const records = ref<RequestRecordSummary[]>([]);
const loading = ref(false);
const loadError = ref("");

const drawerVisible = ref(false);
const drawerLoading = ref(false);
const activeField = ref<FieldKey | null>(null);
const activeTab = ref("raw");
const detail = ref<RequestRecordDetail | null>(null);

/** 抽屉标题：字段名 + 请求 id。 */
const drawerTitle = computed(() => {
  const label = activeField.value ? fieldTitles[activeField.value] : "记录详情";
  const id = detail.value?.id ?? "";
  return id ? `${label} · ${id}` : label;
});

/** 回答分类标签（工具调用 / 正文 / 错误 / 无回答）。 */
const answerTag = computed(() => {
  const record = detail.value;
  if (!record) return null;
  if (record.status === "error") {
    return { label: "错误", type: "error" as const };
  }
  if (record.answer_type === "tool_call") {
    return { label: "工具调用", type: "warning" as const };
  }
  if (record.answer_type === "text") {
    return { label: "正文", type: "success" as const };
  }
  return { label: "无回答", type: "default" as const };
});

const fieldTitles: Record<FieldKey, string> = {
  id: "请求 ID",
  protocol: "协议类型",
  api_key: "API Key",
  headers: "Header",
  duration: "耗时",
  date: "日期",
  system_prompt: "系统提示词",
  user_prompt: "用户提问",
  tools: "可用工具",
  history: "历史对话",
  answer: "AI 回答",
};

function formatTime(value: number): string {
  return new Date(value).toLocaleString();
}

/** 毫秒耗时 → 可读文本。 */
function formatDuration(value: number): string {
  if (!Number.isFinite(value) || value < 0) return "—";
  if (value < 1000) return `${Math.round(value)} ms`;
  const seconds = value / 1000;
  if (seconds < 60) return `${seconds.toFixed(2)} s`;
  const minutes = Math.floor(seconds / 60);
  const rest = Math.round(seconds % 60);
  return `${minutes} 分 ${rest} 秒`;
}

/** Header JSON → `name: value` 逐行文本。 */
function formatHeaders(raw: string): string {
  const parsed = parseJson(raw);
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    return raw || "（无）";
  }
  const entries = Object.entries(parsed as Record<string, unknown>);
  if (entries.length === 0) return "（无）";
  return entries
    .map(([name, value]) => {
      const text = typeof value === "string" ? value : JSON.stringify(value);
      return `${name}: ${text}`;
    })
    .join("\n");
}

/** 表格行 key：请求 id 全局唯一。 */
function rowKey(row: RequestRecordSummary): string {
  return row.id;
}

/** 行状态：交给全局设计系统样式着色。 */
function rowProps(row: RequestRecordSummary) {
  return {
    class: row.status === "error" ? "row-error" : row.status === "success" ? "row-success" : "",
  };
}

/** 请求状态标签。 */
function statusCell(record: RequestRecordSummary) {
  const failed = record.status === "error";
  const success = record.status === "success";
  const tagType: "error" | "success" | "default" = failed
    ? "error"
    : success
      ? "success"
      : "default";
  return h(
    NTag,
    { size: "small", type: tagType },
    { default: () => (failed ? "失败" : success ? "成功" : "未知") },
  );
}

/** 角色中文名。 */
function roleLabel(role: string): string {
  if (role === "system") return "系统";
  if (role === "user") return "用户";
  if (role === "assistant") return "AI";
  if (role === "tool") return "工具";
  return role || "未知";
}

function asRecord(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

function parseJson(raw: string): unknown {
  try {
    return JSON.parse(raw);
  } catch {
    return null;
  }
}

/** 消息 content → 纯文本（兼容多模态片段）。 */
function contentText(content: unknown): string {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content
      .map((part) => {
        const item = asRecord(part);
        if (typeof item.text === "string") return item.text;
        return `[${typeof item.type === "string" ? item.type : "未知片段"}]`;
      })
      .filter((text) => text.length > 0)
      .join("\n");
  }
  if (content == null) return "";
  return JSON.stringify(content, null, 2);
}

/** 工具定义格式化：逐个工具列出名称、描述与参数 JSON。 */
function formatTools(raw: string): string {
  const parsed = parseJson(raw);
  if (!Array.isArray(parsed) || parsed.length === 0) {
    return "（无可用工具）";
  }
  return parsed
    .map((item, index) => {
      const record = asRecord(item);
      const fn = asRecord(record.function ?? record);
      const name = typeof fn.name === "string" ? fn.name : "未命名工具";
      const description = typeof fn.description === "string" ? fn.description : "";
      const lines = [`工具 ${index + 1}：${name}`];
      if (description) lines.push(`描述：${description}`);
      lines.push(`参数：\n${JSON.stringify(fn.parameters ?? {}, null, 2)}`);
      return lines.join("\n");
    })
    .join("\n\n----------------\n\n");
}

/** 历史对话格式化：按顺序列出角色、正文、工具调用与工具调用 id。 */
function formatHistory(raw: string): string {
  const parsed = parseJson(raw);
  if (!Array.isArray(parsed) || parsed.length === 0) {
    return "（无历史对话）";
  }
  return parsed
    .map((item, index) => {
      const record = asRecord(item);
      const role = roleLabel(typeof record.role === "string" ? record.role : "");
      const lines = [`[${index + 1}] ${role}`];
      const content = contentText(record.content).trim();
      if (content) lines.push(content);
      if (typeof record.name === "string" && record.name) {
        lines.push(`工具名：${record.name}`);
      }
      if (Array.isArray(record.tool_calls) && record.tool_calls.length > 0) {
        lines.push(`工具调用：\n${JSON.stringify(record.tool_calls, null, 2)}`);
      }
      if (typeof record.tool_call_id === "string" && record.tool_call_id) {
        lines.push(`工具调用 id：${record.tool_call_id}`);
      }
      return lines.join("\n");
    })
    .join("\n\n----------------\n\n");
}

/** AI 回答原文：assistant 消息 JSON，思考内容与工具调用原样保留。 */
function rawAnswer(record: RequestRecordDetail): string {
  if (record.status === "error") {
    return JSON.stringify(
      { status: "error", error: record.error || "未知错误" },
      null,
      2,
    );
  }
  const payload: Record<string, unknown> = { role: "assistant" };
  payload.content = record.answer_type === "tool_call" ? null : record.answer_content;
  if (record.answer_reasoning.trim()) {
    payload.reasoning_content = record.answer_reasoning;
  }
  if (record.answer_type === "tool_call") {
    payload.tool_calls = parseJson(record.answer_tool_calls) ?? record.answer_tool_calls;
  }
  return JSON.stringify(payload, null, 2);
}

/** AI 回答格式化：先思考内容，再按分类展示正文 / 工具调用 / 失败原因。 */
function formattedAnswer(record: RequestRecordDetail): string {
  const sections: string[] = [];
  if (record.answer_reasoning.trim()) {
    sections.push(`【思考内容】\n${record.answer_reasoning}`);
  }
  if (record.status === "error") {
    sections.push(`【请求失败】\n${record.error || "未知错误"}`);
  } else if (record.answer_type === "tool_call") {
    const calls = parseJson(record.answer_tool_calls);
    sections.push(
      `【工具调用】\n${calls == null ? record.answer_tool_calls : JSON.stringify(calls, null, 2)}`,
    );
    if (record.answer_content.trim()) {
      sections.push(`【附带正文】\n${record.answer_content}`);
    }
  } else {
    sections.push(`【回答正文】\n${record.answer_content.trim() || "（空）"}`);
  }
  return sections.join("\n\n");
}

/** 原文页签内容。 */
const rawText = computed(() => {
  const record = detail.value;
  const field = activeField.value;
  if (!record || !field) return "";
  switch (field) {
    case "id":
      return record.id;
    case "protocol":
      return record.protocol || "（未知协议）";
    case "api_key":
      return record.api_key || "（无）";
    case "headers":
      return record.headers || "{}";
    case "duration":
      return String(record.duration_ms);
    case "date":
      return String(record.created_at);
    case "system_prompt":
      return record.system_prompt || "（无系统提示词）";
    case "user_prompt":
      return record.user_prompt || "（无用户提问）";
    case "tools":
      return record.tools || "[]";
    case "history":
      return record.history || "[]";
    case "answer":
      return rawAnswer(record);
  }
});

/** 格式化页签内容。 */
const formattedText = computed(() => {
  const record = detail.value;
  const field = activeField.value;
  if (!record || !field) return "";
  switch (field) {
    case "id":
      return [
        `请求 ID：${record.id}`,
        `协议：${record.protocol || "未知"} ${record.method || "POST"} ${record.endpoint || ""}`,
        `模型：${record.model}`,
        `流式：${record.stream ? "是" : "否"}`,
        `状态：${record.status === "error" ? "失败" : "成功"}`,
        `API Key：${record.api_key || "（无）"}`,
        `耗时：${formatDuration(record.duration_ms)}`,
        `日期：${formatTime(record.created_at)}`,
        record.status === "error" && record.error
          ? `错误：${record.error}`
          : "",
      ]
        .filter(Boolean)
        .join("\n");
    case "protocol":
      return [
        `协议类型：${record.protocol || "未知"}`,
        `方法：${record.method || "POST"}`,
        `端点：${record.endpoint || "-"}`,
      ].join("\n");
    case "api_key":
      return record.api_key ? `API Key：${record.api_key}` : "（无）";
    case "headers":
      return formatHeaders(record.headers);
    case "duration":
      return formatDuration(record.duration_ms);
    case "date":
      return formatTime(record.created_at);
    case "system_prompt":
      return record.system_prompt.trim() || "（无系统提示词）";
    case "user_prompt":
      return record.user_prompt.trim() || "（无用户提问）";
    case "tools":
      return formatTools(record.tools);
    case "history":
      return formatHistory(record.history);
    case "answer":
      return formattedAnswer(record);
  }
});

/** 表格里的单行可点击字段。 */
function clickableCell(record: RequestRecordSummary, field: FieldKey, text: string) {
  const title = text.length > 300 ? `${text.slice(0, 300)}…` : text;
  return h(
    "div",
    {
      title: title || "（空）",
      role: "button",
      tabindex: 0,
      style:
        "width:100%;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;cursor:pointer;color:var(--accent);",
      onClick: () => void openField(record, field),
      onKeydown: (event: KeyboardEvent) => {
        if (event.key === "Enter" || event.key === " ") {
          event.preventDefault();
          void openField(record, field);
        }
      },
    },
    text || "—",
  );
}

/** 表格里的普通单行文本。 */
function plainCell(text: string) {
  return h(
    "span",
    {
      title: text,
      style:
        "display:block;width:100%;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;",
    },
    text || "—",
  );
}

/** AI 回答列：分类标签 + 单行摘要，点击打开抽屉。 */
function answerCell(record: RequestRecordSummary) {
  const failed = record.status === "error";
  const toolCall = record.answer_type === "tool_call";
  const hasText = record.answer_type === "text";
  const tagType: "error" | "warning" | "success" | "default" = failed
    ? "error"
    : toolCall
      ? "warning"
      : hasText
        ? "success"
        : "default";
  const label = failed ? "失败" : toolCall ? "工具调用" : hasText ? "正文" : "无回答";
  const title =
    record.answer_preview.length > 300
      ? `${record.answer_preview.slice(0, 300)}…`
      : record.answer_preview;
  return h(
    "div",
    {
      title: title || "（空）",
      role: "button",
      tabindex: 0,
      style: "display:flex;align-items:center;gap:6px;width:100%;min-width:0;cursor:pointer;",
      onClick: () => void openField(record, "answer"),
      onKeydown: (event: KeyboardEvent) => {
        if (event.key === "Enter" || event.key === " ") {
          event.preventDefault();
          void openField(record, "answer");
        }
      },
    },
    [
      h(
        NTag,
        { size: "small", type: tagType, style: "flex:none;" },
        { default: () => label },
      ),
      h(
        "span",
        {
          style:
            "overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--fg-2);",
        },
        record.answer_preview || "—",
      ),
    ],
  );
}

const columns: DataTableColumns<RequestRecordSummary> = [
  {
    title: "请求 ID",
    key: "id",
    width: 165,
    render: (row) => clickableCell(row, "id", row.id),
  },
  {
    title: "状态",
    key: "status",
    width: 70,
    render: (row) => statusCell(row),
  },
  {
    title: "协议",
    key: "protocol",
    width: 125,
    render: (row) =>
      clickableCell(
        row,
        "protocol",
        `${row.protocol || "未知"} · ${row.endpoint || "-"}`,
      ),
  },
  {
    title: "模型",
    key: "model",
    width: 90,
    render: (row) => plainCell(row.model),
  },
  {
    title: "API Key",
    key: "api_key",
    width: 125,
    render: (row) => clickableCell(row, "api_key", row.api_key),
  },
  {
    title: "系统提示词",
    key: "system_prompt",
    width: 150,
    render: (row) => clickableCell(row, "system_prompt", row.system_prompt),
  },
  {
    title: "用户提问",
    key: "user_prompt",
    width: 160,
    render: (row) => clickableCell(row, "user_prompt", row.user_prompt),
  },
  {
    title: "可用工具",
    key: "tools",
    width: 120,
    render: (row) => clickableCell(row, "tools", row.tools),
  },
  {
    title: "历史对话",
    key: "history",
    width: 150,
    render: (row) => clickableCell(row, "history", row.history),
  },
  {
    title: "AI 回答",
    key: "answer",
    width: 165,
    render: (row) => answerCell(row),
  },
  {
    title: "Header",
    key: "headers",
    width: 130,
    render: (row) => clickableCell(row, "headers", row.headers),
  },
  {
    title: "耗时",
    key: "duration",
    width: 70,
    align: "right",
    render: (row) => clickableCell(row, "duration", formatDuration(row.duration_ms)),
  },
  {
    title: "日期",
    key: "created_at",
    width: 145,
    align: "right",
    render: (row) => clickableCell(row, "date", formatTime(row.created_at)),
  },
];

async function load() {
  loading.value = true;
  try {
    records.value = await invoke<RequestRecordSummary[]>("list_request_records", {
      limit: 1000,
    });
    loadError.value = "";
  } catch (error) {
    loadError.value = error instanceof Error ? error.message : String(error);
  } finally {
    loading.value = false;
  }
}

async function clearAll() {
  try {
    const removed = await invoke<number>("clear_request_records");
    message.success(`已清空 ${removed} 条请求记录`);
    await load();
  } catch (error) {
    message.error(error instanceof Error ? error.message : String(error));
  }
}

async function openField(record: RequestRecordSummary, field: FieldKey) {
  activeField.value = field;
  activeTab.value = "raw";
  detail.value = null;
  drawerVisible.value = true;
  drawerLoading.value = true;
  try {
    const loaded = await invoke<RequestRecordDetail | null>("get_request_record", {
      id: record.id,
    });
    if (!loaded) {
      drawerVisible.value = false;
      message.warning("该请求记录不存在或已被清空");
      return;
    }
    detail.value = loaded;
  } catch (error) {
    drawerVisible.value = false;
    message.error(error instanceof Error ? error.message : String(error));
  } finally {
    drawerLoading.value = false;
  }
}

onMounted(() => void load());
</script>

<template>
  <div>
    <header class="page-header">
      <div>
        <p class="page-kicker">03 / Requests</p>
        <h1 class="page-title">本地请求管理</h1>
      </div>
      <div class="toolbar">
        <n-button :loading="loading" @click="load">刷新</n-button>
        <n-popconfirm @positive-click="clearAll">
          <template #trigger>
            <n-button type="error" quaternary>清空记录</n-button>
          </template>
          确定清空全部本地请求记录吗？该操作不可恢复。
        </n-popconfirm>
      </div>
    </header>

    <n-alert v-if="loadError" type="error" :show-icon="true" style="margin-bottom: 16px">
      {{ loadError }}
    </n-alert>

    <section class="panel">
      <n-empty v-if="!loading && records.length === 0" description="暂无请求记录" />
      <n-data-table
        v-else
        :columns="columns"
        :data="records"
        :loading="loading"
        :bordered="false"
        :scroll-x="1665"
        :pagination="{ pageSize: 20 }"
        :row-key="rowKey"
        :row-props="rowProps"
        table-layout="fixed"
        size="small"
      />
    </section>

    <n-drawer v-model:show="drawerVisible" :width="760" placement="right">
      <n-drawer-content :title="drawerTitle" closable>
        <n-spin :show="drawerLoading">
          <div v-if="activeField === 'answer' && answerTag" class="answer-kind">
            <n-tag size="small" :type="answerTag.type">{{ answerTag.label }}</n-tag>
          </div>
          <n-tabs v-model:value="activeTab" type="segment" animated>
            <n-tab-pane name="raw" tab="原文">
              <pre class="drawer-text">{{ rawText }}</pre>
            </n-tab-pane>
            <n-tab-pane name="formatted" tab="格式化">
              <pre class="drawer-text">{{ formattedText }}</pre>
            </n-tab-pane>
          </n-tabs>
        </n-spin>
      </n-drawer-content>
    </n-drawer>
  </div>
</template>

<style scoped>
.answer-kind {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  margin-bottom: var(--space-3);
}

.drawer-text {
  margin: 0;
  padding: var(--space-4);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  background: var(--surface-2);
  color: var(--fg-2);
  max-height: calc(100vh - 190px);
  overflow: auto;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  font-family: var(--font-mono);
  font-size: var(--text-xs);
  line-height: 1.6;
}
</style>
