import type { ChatMessage, ChatOptions, TokenUsage } from './requests.js';
import type { ModelProvider, ProviderStatusMap } from './models.js';

/** WebSocket 消息类型（服务端 <-> 插件） */
export const MessageType = {
  CHAT_REQUEST: 'CHAT_REQUEST',  /** 插件 -> 服务：流式增量文本块（id 与对应 CHAT_REQUEST 相同，CHAT_RESPONSE 前可发任意多个） */
  CHAT_CHUNK: 'CHAT_CHUNK',  CHAT_RESPONSE: 'CHAT_RESPONSE',
  CHAT_ERROR: 'CHAT_ERROR',
  STATUS_SYNC: 'STATUS_SYNC',
  /** 插件 -> 服务端：诊断日志（捕获信息 / 请求响应 / 报错），服务端打印到终端 */
  DIAGNOSTIC: 'DIAGNOSTIC',
} as const;

export type MessageType = (typeof MessageType)[keyof typeof MessageType];

export interface BaseMessage {
  id: string;
  type: MessageType;
  timestamp: number;
}

/** 服务 -> 插件：发起一次聊天请求 */
export interface ChatRequest extends BaseMessage {
  type: typeof MessageType.CHAT_REQUEST;
  payload: ChatRequestPayload;
}

export interface ChatRequestPayload {
  model: ModelProvider;
  messages: ChatMessage[];
  options?: ChatOptions;
}

/** 插件 -> 服务：流式增量文本块（id 与对应 CHAT_REQUEST 相同） */
export interface ChatChunk extends BaseMessage {
  type: typeof MessageType.CHAT_CHUNK;
  payload: ChatChunkPayload;
}

export interface ChatChunkPayload {
  /** 本次新增的答案文本 */
  delta: string;
}

/** 插件 -> 服务：聊天成功响应 */
export interface ChatResponse extends BaseMessage {
  type: typeof MessageType.CHAT_RESPONSE;
  payload: ChatResponsePayload;
}

export interface ChatResponsePayload {
  content: string;
  model: string;
  usage?: TokenUsage;
  finishedReason?: string;
}

/** 插件 -> 服务：聊天失败 */
export interface ChatError extends BaseMessage {
  type: typeof MessageType.CHAT_ERROR;
  payload: {
    error: string;
    code?: string;
  };
}

/** 插件 -> 服务：定期同步各平台状态 */
export interface StatusSync extends BaseMessage {
  type: typeof MessageType.STATUS_SYNC;
  payload: ProviderStatusMap;
}

/** 插件 -> 服务：诊断日志（用于排查，服务端打印到终端） */
export interface DiagnosticMessage extends BaseMessage {
  type: typeof MessageType.DIAGNOSTIC;
  payload: {
    level: 'debug' | 'info' | 'warn' | 'error';
    source: string;
    message: string;
    data?: unknown;
  };
}

export type ProtocolMessage = ChatRequest | ChatChunk | ChatResponse | ChatError | StatusSync | DiagnosticMessage;
