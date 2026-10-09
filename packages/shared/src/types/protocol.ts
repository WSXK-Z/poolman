import type { ChatMessage, ChatOptions, TokenUsage, ToolCall } from './requests.js';
import type { ModelProvider, ProviderStatusMap } from './models.js';
import type { CapturedChatApi } from './capture.js';

/** WebSocket 消息类型（服务端 <-> 插件） */
export const MessageType = {
  /** 服务 -> 插件：发起一次聊天请求（DOM 平台使用；DeepSeek 服务端自管，不再走此消息） */
  CHAT_REQUEST: 'CHAT_REQUEST',
  /** 插件 -> 服务：流式增量文本块（DOM 平台使用，id 与对应 CHAT_REQUEST 相同） */
  CHAT_CHUNK: 'CHAT_CHUNK',
  /** 插件 -> 服务：聊天成功响应（DOM 平台使用） */
  CHAT_RESPONSE: 'CHAT_RESPONSE',
  /** 插件 -> 服务：聊天失败（DOM 平台使用） */
  CHAT_ERROR: 'CHAT_ERROR',
  /** 插件 -> 服务：定期同步各平台状态 */
  STATUS_SYNC: 'STATUS_SYNC',
  /** 插件 -> 服务端：诊断日志（捕获信息 / 请求响应 / 报错），服务端打印到终端 */
  DIAGNOSTIC: 'DIAGNOSTIC',
  /** 服务 -> 插件：请求插件从页面主动捕获凭证（服务端缺失凭证时下发） */
  CAPTURE_REQUEST: 'CAPTURE_REQUEST',
  /** 服务 -> 插件：让插件在指定平台网页执行一次 fetch（插件仅作"执行器"，业务逻辑全在服务端） */
  EXECUTE_REQUEST: 'EXECUTE_REQUEST',
  /** 插件 -> 服务：凭证/接口模板同步（内容脚本捕获后上报，由服务端存储与决策） */
  CREDENTIAL_SYNC: 'CREDENTIAL_SYNC',
  /** 插件 -> 服务：执行过程中的原始响应流文本块（服务端自行分行解析） */
  EXECUTE_CHUNK: 'EXECUTE_CHUNK',
  /** 插件 -> 服务：执行完成（非流式或响应非 2xx 时带完整响应体，供服务端做 422 自愈） */
  EXECUTE_RESULT: 'EXECUTE_RESULT',
  /** 插件 -> 服务：执行失败（网络错误、页面不可达等） */
  EXECUTE_ERROR: 'EXECUTE_ERROR',
  /** 服务 -> 插件：控制指定供应商的连接状态 */
  PROVIDER_CONTROL: 'PROVIDER_CONTROL',
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
  id?: string;
  content?: string;
  tool_calls?: ToolCall[];
  model: string;
  usage?: TokenUsage;
  finishedReason?: string;
  // 流式增量（旧版兼容）
  delta?: string;
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

/* ------------------------ 连接器化新增消息 ------------------------ */
/* 插件退化为"纯连接器"后新增的消息：凭证同步 + 网页执行器。            */
/* 业务逻辑（凭证存储/请求组装/PoW/SSE 解析/422 自愈）全在服务端。      */

/** 服务 -> 插件：请求插件从页面主动捕获凭证（服务端缺失凭证时下发） */
export interface CaptureRequest extends BaseMessage {
  type: typeof MessageType.CAPTURE_REQUEST;
  payload: {
    provider: ModelProvider;
  };
}

/** 服务 -> 插件：让插件在指定平台网页执行一次 fetch（插件仅作"执行器"） */
export interface ExecuteRequestPayload {
  provider: ModelProvider;
  /** 请求方法，默认 POST */
  method?: string;
  url: string;
  /** 请求头（服务端已过滤 forbidden headers，仅 x-* 自定义头 + Content-Type/Authorization） */
  headers: Record<string, string>;
  /** 请求体 JSON 字符串 */
  body?: string | null;
  /** 是否流式读取响应体（流式时逐块回 EXECUTE_CHUNK，最后回 EXECUTE_RESULT） */
  streaming?: boolean;
}

export interface ExecuteRequest extends BaseMessage {
  type: typeof MessageType.EXECUTE_REQUEST;
  payload: ExecuteRequestPayload;
}

/** 插件 -> 服务：凭证/接口模板同步（内容脚本捕获后上报，由服务端存储与决策） */
export interface CredentialSync extends BaseMessage {
  type: typeof MessageType.CREDENTIAL_SYNC;
  payload: {
    provider: ModelProvider;
    token?: string | null;
    settingsToken?: string | null;
    hifHeaders?: { leim?: string; dliq?: string };
    apiTemplate?: CapturedChatApi | null;
  };
}

/** 插件 -> 服务：执行过程中的原始响应流文本块（服务端自行分行解析） */
export interface ExecuteChunkPayload {
  /** 原始响应流文本块 */
  data: string;
}

export interface ExecuteChunk extends BaseMessage {
  type: typeof MessageType.EXECUTE_CHUNK;
  payload: ExecuteChunkPayload;
}

/** 插件 -> 服务：执行完成（非流式或响应非 2xx 时带完整响应体，供服务端做 422 自愈等） */
export interface ExecuteResultPayload {
  /** HTTP 状态码 */
  status: number;
  /** 是否 2xx 成功 */
  ok: boolean;
  statusText?: string;
  /** 响应体文本（非流式或 !ok 时携带） */
  body?: string | null;
}

export interface ExecuteResult extends BaseMessage {
  type: typeof MessageType.EXECUTE_RESULT;
  payload: ExecuteResultPayload;
}

/** 插件 -> 服务：执行失败（网络错误、页面不可达等） */
export interface ExecuteError extends BaseMessage {
  type: typeof MessageType.EXECUTE_ERROR;
  payload: { error: string; code?: string };
}

export interface ProviderControlMessage extends BaseMessage {
  type: typeof MessageType.PROVIDER_CONTROL;
  payload: { provider: ModelProvider; action: 'connect' | 'disconnect' };
}

export type ProtocolMessage =
  | ChatRequest
  | ChatChunk
  | ChatResponse
  | ChatError
  | StatusSync
  | DiagnosticMessage
  | CaptureRequest
  | ExecuteRequest
  | CredentialSync
  | ExecuteChunk
  | ExecuteResult
  | ExecuteError
  | ProviderControlMessage;
