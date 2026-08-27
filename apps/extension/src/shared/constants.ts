import type { ModelProvider, ProviderStatusMap } from '@ai-gateway/shared';

/** 默认本地服务 WebSocket 地址 */
export const DEFAULT_SERVER_URL = 'ws://127.0.0.1:3001';

/** 插件配置 */
export interface AppSettings {
  serverUrl: string;
  serverToken: string;
  autoReconnect: boolean;
  requestTimeoutMs: number;
  enabledProviders: Partial<Record<ModelProvider, boolean>>;
}

export const DEFAULT_SETTINGS: AppSettings = {
  serverUrl: DEFAULT_SERVER_URL,
  serverToken: '',
  autoReconnect: true,
  requestTimeoutMs: 120_000,
  enabledProviders: {},
};

export const PROVIDER_LABELS: Record<ModelProvider, string> = {
  'deepseek-web': 'DeepSeek',
  'chatgpt-web': 'ChatGPT',
  'claude-web': 'Claude',
  'gemini-web': 'Gemini',
  'kimi-web': 'Kimi',
  'qwen-web': 'Qwen',
};

export interface LogEntry {
  id: string;
  time: number;
  level: 'info' | 'warn' | 'error';
  message: string;
}

/** 拦截到的平台聊天接口模板（网页前端真实请求的 body 结构，用于自适配转发） */
export interface ChatApiTemplate {
  agent_name?: string;
  model_name?: string;
  think_enabled?: boolean;
  generation_settings?: Record<string, unknown>;
  has_session_id?: boolean;
  has_parent_message_id?: boolean;
  /** 网页真实请求里使用的会话 ID（复用即可，随机 UUID 会被判 invalid） */
  chat_session_id?: string;
  /** 网页真实请求里的父消息 ID */
  parent_message_id?: string | null;
  /** 2026-08 网站真实字段：模型类型（如 'default'） */
  model_type?: string;
  thinking_enabled?: boolean;
  search_enabled?: boolean;
}

/** 拦截到的平台聊天接口完整信息 */
export interface CapturedChatApi {
  provider: ModelProvider;
  url: string;
  headers: Record<string, string>;
  template: ChatApiTemplate;
  token: string;
  capturedAt: number;
}

/** 插件内部（UI <-> background）消息协议 */
export type InternalMessage =
  | { type: 'GET_STATE' }
  | { type: 'GET_SETTINGS' }
  | { type: 'UPDATE_SETTINGS'; payload: Partial<AppSettings> }
  | { type: 'QUICK_TEST'; payload: { model: ModelProvider; prompt: string } }
  | { type: 'OPEN_PROVIDER'; payload: { provider: ModelProvider } }
  | { type: 'GET_LOGS' }
  | { type: 'CLEAR_LOGS' };

export interface ExtensionState {
  settings: AppSettings;
  connected: boolean;
  providers: ProviderStatusMap;
  logs: LogEntry[];
}

export type InternalResponse =
  | { ok: true; state: ExtensionState }
  | { ok: true; logs: LogEntry[] }
  | { ok: true; result: { content: string; elapsedMs: number } }
  | { ok: true; data?: unknown }
  | { ok: false; error: string };
