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

// 捕获凭证/接口模板类型已上移到共享包（服务端与插件共用），此处 re-export 保持兼容。
export type { ChatApiTemplate, CapturedChatApi } from '@ai-gateway/shared';

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
