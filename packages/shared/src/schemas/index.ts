import { z } from 'zod';
import { MessageType } from '../types/protocol.js';
import { PROVIDERS } from '../types/models.js';
import type { ChatMessage } from '../types/requests.js';

const functionDefinitionSchema = z.object({
  name: z.string(),
  description: z.string().optional(),
  parameters: z.record(z.string(), z.unknown()).optional(),
});
const toolSchema = z.object({
  type: z.literal('function'),
  function: functionDefinitionSchema,
});

const toolCallSchema = z.object({
  id: z.string(),
  type: z.literal('function'),
  function: z.object({
    name: z.string(),
    arguments: z.string(),
  }),
});
/* ------------------------------ 基础类型 ------------------------------ */

export const chatRoleSchema = z.enum(['system', 'user', 'assistant']);

const chatMessageSchema: z.ZodType<ChatMessage> = z.object({
  role: z.enum(['system', 'user', 'assistant', 'tool']),
  content: z.string().nullable(),
  name: z.string().optional(),
  tool_calls: z.array(toolCallSchema).optional(),
  tool_call_id: z.string().optional(),
});

export const chatOptionsSchema = z.object({
  temperature: z.number().min(0).max(2).optional(),
  topP: z.number().min(0).max(1).optional(),
  maxTokens: z.number().int().positive().optional(),
  stream: z.boolean().optional(),
  timeoutMs: z.number().int().positive().optional(),
  /** 固定会话 ID（如 DeepSeek 网页会话），传递给插件复用 */
  sessionId: z.string().optional(),
});

export const tokenUsageSchema = z.object({
  prompt_tokens: z.number().int().nonnegative(),
  completion_tokens: z.number().int().nonnegative(),
  total_tokens: z.number().int().nonnegative(),
});

export const providerIdSchema = z.enum(PROVIDERS);

export const providerStatusSchema = z.object({
  provider: providerIdSchema,
  connected: z.boolean(),
  authed: z.boolean(),
  models: z.array(z.string()),
  lastChecked: z.number(),
  error: z.string().optional(),
  tokenCaptured: z.boolean().optional(),
  templateCaptured: z.boolean().optional(),
});

// 注意：使用 string 作为 key 而非枚举，否则 Zod v4 会要求枚举所有 key 都存在
export const providerStatusMapSchema = z.record(z.string(), providerStatusSchema);

/* --------------------------- 协议消息 schema --------------------------- */

const baseMessageSchema = z.object({
  id: z.string().min(1),
  timestamp: z.number(),
});

export const chatRequestSchema = baseMessageSchema.extend({
  type: z.literal(MessageType.CHAT_REQUEST),
  payload: z.object({
    model: providerIdSchema,
    messages: z.array(chatMessageSchema).min(1),
    options: chatOptionsSchema.optional(),
  }),
});

export const chatChunkSchema = baseMessageSchema.extend({
  type: z.literal(MessageType.CHAT_CHUNK),
  payload: z.object({
    delta: z.string(),
  }),
});

export const chatResponseSchema = baseMessageSchema.extend({
  type: z.literal(MessageType.CHAT_RESPONSE),
  payload: z.object({
    content: z.string(),
    model: z.string(),
    usage: tokenUsageSchema.optional(),
    finishedReason: z.string().optional(),
  }),
});

export const chatErrorSchema = baseMessageSchema.extend({
  type: z.literal(MessageType.CHAT_ERROR),
  payload: z.object({
    error: z.string(),
    code: z.string().optional(),
  }),
});

export const statusSyncSchema = baseMessageSchema.extend({
  type: z.literal(MessageType.STATUS_SYNC),
  payload: providerStatusMapSchema,
});

export const diagnosticMessageSchema = baseMessageSchema.extend({
  type: z.literal(MessageType.DIAGNOSTIC),
  payload: z.object({
    level: z.enum(['debug', 'info', 'warn', 'error']),
    source: z.string(),
    message: z.string(),
    data: z.unknown().optional(),
  }),
});

/* ---------------------- 连接器化新增消息 schema ---------------------- */
/* 插件只作"连接器"：凭证同步 + 网页执行器。业务逻辑（存储/组装/解析）全在服务端。 */

/** 服务 -> 插件：请求主动捕获凭证 */
export const captureRequestSchema = baseMessageSchema.extend({
  type: z.literal(MessageType.CAPTURE_REQUEST),
  payload: z.object({
    provider: providerIdSchema,
  }),
});

/** 服务 -> 插件：在指定平台网页执行一次 fetch */
export const executeRequestSchema = baseMessageSchema.extend({
  type: z.literal(MessageType.EXECUTE_REQUEST),
  payload: z.object({
    provider: providerIdSchema,
    method: z.string().optional(),
    url: z.string(),
    headers: z.record(z.string(), z.string()),
    body: z.string().nullable().optional(),
    streaming: z.boolean().optional(),
  }),
});

/** 插件 -> 服务：凭证/接口模板同步（apiTemplate 用 z.unknown 宽松校验，避免丢弃网页捕获的未知结构） */
export const credentialSyncSchema = baseMessageSchema.extend({
  type: z.literal(MessageType.CREDENTIAL_SYNC),
  payload: z.object({
    provider: providerIdSchema,
    token: z.string().nullable().optional(),
    settingsToken: z.string().nullable().optional(),
    hifHeaders: z
      .object({
        leim: z.string().optional(),
        dliq: z.string().optional(),
      })
      .optional(),
    apiTemplate: z.unknown().nullable().optional(),
  }),
});

/** 插件 -> 服务：执行过程中的原始响应流文本块 */
export const executeChunkSchema = baseMessageSchema.extend({
  type: z.literal(MessageType.EXECUTE_CHUNK),
  payload: z.object({
    data: z.string(),
  }),
});

/** 插件 -> 服务：执行完成（非流式或响应非 2xx 时带完整响应体，供服务端做 422 自愈等） */
export const executeResultSchema = baseMessageSchema.extend({
  type: z.literal(MessageType.EXECUTE_RESULT),
  payload: z.object({
    status: z.number(),
    ok: z.boolean(),
    statusText: z.string().optional(),
    body: z.string().nullable().optional(),
  }),
});

/** 插件 -> 服务：执行失败（网络错误、页面不可达等） */
export const executeErrorSchema = baseMessageSchema.extend({
  type: z.literal(MessageType.EXECUTE_ERROR),
  payload: z.object({
    error: z.string(),
    code: z.string().optional(),
  }),
});

/** 协议消息校验器（WebSocket 入口统一使用） */
export const protocolMessageSchema = z.discriminatedUnion('type', [
  chatRequestSchema,
  chatChunkSchema,
  chatResponseSchema,
  chatErrorSchema,
  statusSyncSchema,
  diagnosticMessageSchema,
  captureRequestSchema,
  executeRequestSchema,
  credentialSyncSchema,
  executeChunkSchema,
  executeResultSchema,
  executeErrorSchema,
]);

/* ------------------------- OpenAI 兼容接口 schema ------------------------- */

export const chatCompletionRequestSchema = z.object({
  model: z.string(),
  messages: z.array(chatMessageSchema),
  tools: z.array(toolSchema).optional(),
  tool_choice: z.union([
    z.literal('auto'),
    z.literal('none'),
    z.object({ type: z.literal('function'), function: z.object({ name: z.string() }) }),
  ]).optional(),
  temperature: z.number().min(0).max(2).optional(),
  top_p: z.number().min(0).max(1).optional(),
  max_tokens: z.number().int().positive().optional(),
  stream: z.boolean().optional(),
  session_id: z.string().optional(),
  stream_options: z.object({ include_usage: z.boolean().optional() }).optional(),
});

export type ChatCompletionRequest = z.infer<typeof chatCompletionRequestSchema>;

/* ------------------------------ 类型推导 ------------------------------ */

export type ChatMessageInput = z.infer<typeof chatMessageSchema>;
export type ChatOptionsInput = z.infer<typeof chatOptionsSchema>;
export type ProviderStatusMapInput = z.infer<typeof providerStatusMapSchema>;


