import type { LogLevel } from '@ai-gateway/utils';

export interface ServerConfig {
  /** HTTP 监听地址 */
  host: string;
  /** OpenAI 兼容 API 端口 */
  port: number;
  /** WebSocket 桥接端口 */
  wsPort: number;
  logLevel: LogLevel;
  /** 单个聊天请求超时（毫秒） */
  requestTimeoutMs: number;
  /** 请求体大小限制 */
  maxBodySize: string;
  /** CORS 允许来源 */
  corsOrigin: string;
  /** WebSocket 鉴权 Token（可选，空则不鉴权） */
  wsAuthToken?: string;
}

/** 从环境变量加载配置 */
export function loadConfig(): ServerConfig {
  return {
    host: process.env.HOST || '127.0.0.1',
    port: Number(process.env.PORT ?? 3000),
    wsPort: Number(process.env.WS_PORT ?? 3001),
    logLevel: (process.env.LOG_LEVEL as LogLevel) || 'info',
    requestTimeoutMs: Number(process.env.REQUEST_TIMEOUT_MS ?? 120_000),
    maxBodySize: process.env.MAX_BODY_SIZE || '10mb',
    corsOrigin: process.env.CORS_ORIGIN || '*',
    wsAuthToken: process.env.WS_AUTH_TOKEN || undefined,
  };
}
