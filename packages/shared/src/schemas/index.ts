import { z } from 'zod';
import { MessageType } from '../types/protocol.js';
import { PROVIDERS } from '../types/models.js';

/* ------------------------------ 基础类型 ------------------------------ */

export const chatRoleSchema = z.enum(['system', 'user', 'assistant']);

export const chatMessageSchema = z.object({
  role: chatRoleSchema,
  content: z.string().min(1),
  name: z.string().optional(),
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

/** 协议消息校验器（WebSocket 入口统一使用） */
export const protocolMessageSchema = z.discriminatedUnion('type', [
  chatRequestSchema,
  chatChunkSchema,
  chatResponseSchema,
  chatErrorSchema,
  statusSyncSchema,
  diagnosticMessageSchema,
]);

/* ------------------------- OpenAI 兼容接口 schema ------------------------- */

export const chatCompletionRequestSchema = z.object({
  model: z.string().min(1),
  messages: z.array(chatMessageSchema).min(1),
  temperature: z.number().min(0).max(2).optional(),
  top_p: z.number().min(0).max(1).optional(),
  max_tokens: z.number().int().positive().optional(),
  stream: z.boolean().optional(),
  stream_options: z
    .object({
      include_usage: z.boolean().optional(),
    })
    .optional(),
  /** 固定会话 ID（与请求头 x-deepseek-session 等价，两者皆可） */
  session_id: z.string().optional(),
});

export type ChatCompletionRequest = z.infer<typeof chatCompletionRequestSchema>;

/* ------------------------------ 类型推导 ------------------------------ */

export type ChatMessageInput = z.infer<typeof chatMessageSchema>;
export type ChatOptionsInput = z.infer<typeof chatOptionsSchema>;
export type ProviderStatusMapInput = z.infer<typeof providerStatusMapSchema>;
