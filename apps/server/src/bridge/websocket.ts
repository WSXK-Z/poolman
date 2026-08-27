import { WebSocket, WebSocketServer } from 'ws';
import { createLogger, requestId, safeEqual } from '@ai-gateway/utils';
import {
  MessageType,
  type ChatChunkPayload,
  type ChatRequest,
  type ChatRequestPayload,
  type ChatResponsePayload,
  type ProviderStatusMap,
} from '@ai-gateway/shared';

const log = createLogger('server:bridge');

interface PendingRequest {
  resolve: (payload: ChatResponsePayload) => void;
  reject: (err: Error) => void;
  timer: NodeJS.Timeout;
  /** 流式：每收到一个 CHAT_CHUNK 就调用一次 */
  onChunk?: (payload: ChatChunkPayload) => void;
}

export interface WebSocketBridgeOptions {
  host: string;
  port: number;
  authToken?: string;
  requestTimeoutMs?: number;
}

/**
 * WebSocket 桥接：维护与插件（单个扩展）的连接，并管理请求 ID -> Promise 的映射。
 * 服务端通过 sendRequest 向插件发起聊天请求，等待 CHAT_RESPONSE / CHAT_ERROR 回包。
 */
export class WebSocketBridge {
  private wss: WebSocketServer | null = null;
  private client: WebSocket | null = null;
  private readonly pending = new Map<string, PendingRequest>();
  private readonly statusListeners = new Set<(status: ProviderStatusMap) => void>();
  private readonly connectListeners = new Set<(connected: boolean) => void>();
  private lastStatus: ProviderStatusMap = {};
  private readonly authToken?: string;
  private readonly defaultTimeoutMs: number;
  private readonly host: string;
  private readonly port: number;

  constructor(options: WebSocketBridgeOptions) {
    this.authToken = options.authToken;
    this.defaultTimeoutMs = options.requestTimeoutMs ?? 120_000;
    this.host = options.host;
    this.port = options.port;
  }

  get connected(): boolean {
    return this.client !== null && this.client.readyState === WebSocket.OPEN;
  }

  get latestStatus(): ProviderStatusMap {
    return this.lastStatus;
  }

  start(): void {
    this.wss = new WebSocketServer({ port: this.port, host: this.host });
    this.wss.on('connection', (ws, req) => this.handleConnection(ws, req.url));
    this.wss.on('error', (err) => log.error('ws server error', err));
    log.info(`websocket bridge listening on ws://${this.host}:${this.port}`);
  }

  async stop(): Promise<void> {
    for (const { timer, reject } of this.pending.values()) {
      clearTimeout(timer);
      reject(new Error('Bridge shutting down'));
    }
    this.pending.clear();
    await new Promise<void>((resolve) => {
      if (this.wss) {
        this.wss.close(() => resolve());
        this.wss.clients.forEach((c) => c.close());
      } else {
        resolve();
      }
    });
    this.client = null;
  }

  /** 监听插件连接状态变化 */
  onConnectionChange(listener: (connected: boolean) => void): () => void {
    this.connectListeners.add(listener);
    return () => this.connectListeners.delete(listener);
  }

  /** 监听插件状态同步 */
  onStatus(listener: (status: ProviderStatusMap) => void): () => void {
    this.statusListeners.add(listener);
    return () => this.statusListeners.delete(listener);
  }

  /** 向插件发起聊天请求，返回 Promise<完整响应>。传入 onChunk 后可实时收到流式增量块 */
  sendRequest(
    payload: ChatRequestPayload,
    timeoutMs: number = this.defaultTimeoutMs,
    onChunk?: (payload: ChatChunkPayload) => void,
  ): Promise<ChatResponsePayload> {
    return new Promise((resolve, reject) => {
      if (!this.connected) {
        reject(
          Object.assign(new Error('插件未连接，请先在浏览器中加载扩展并启动本地服务'), {
            status: 503,
            code: 'extension_not_connected',
          }),
        );
        return;
      }
      const id = requestId('req');
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(
          Object.assign(new Error(`请求超时（${timeoutMs}ms），插件未在限定时间内返回`), {
            status: 504,
            code: 'request_timeout',
          }),
        );
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, timer, onChunk });
      const message: ChatRequest = {
        id,
        type: MessageType.CHAT_REQUEST,
        timestamp: Date.now(),
        payload,
      };
      log.info(`send CHAT_REQUEST ${id} -> ${payload.model}`);
      this.client!.send(JSON.stringify(message));
    });
  }

  /** 插件流式回包：增量文本块（不删除 pending，直到收到 CHAT_RESPONSE / CHAT_ERROR） */
  emitChunk(id: string, payload: ChatChunkPayload): void {
    const req = this.pending.get(id);
    if (!req) {
      log.warn(`received chunk for unknown request ${id}`);
      return;
    }
    req.onChunk?.(payload);
  }

  /** 插件回包：成功 */
  resolveResponse(id: string, payload: ChatResponsePayload): void {
    const req = this.pending.get(id);
    if (!req) {
      log.warn(`received response for unknown request ${id}`);
      return;
    }
    clearTimeout(req.timer);
    this.pending.delete(id);
    log.info(`resolved request ${id}`);
    req.resolve(payload);
  }

  /** 插件回包：失败 */
  rejectResponse(id: string, error: string, code?: string): void {
    const req = this.pending.get(id);
    if (!req) {
      log.warn(`received error for unknown request ${id}`);
      return;
    }
    clearTimeout(req.timer);
    this.pending.delete(id);
    const err = new Error(error) as Error & { code?: string };
    err.code = code;
    req.reject(err);
  }

  /** 插件上报状态 */
  emitStatus(status: ProviderStatusMap): void {
    this.lastStatus = status;
    this.statusListeners.forEach((fn) => fn(status));
  }

  private handleConnection(ws: WebSocket, url: string | undefined): void {
    // 简单鉴权：客户端通过 query string 携带 token
    if (this.authToken) {
      const token = new URL(url ?? '', 'http://localhost').searchParams.get('token');
      if (!token || !safeEqual(token, this.authToken)) {
        log.warn('unauthorized ws connection, closing');
        ws.close(1008, 'unauthorized');
        return;
      }
    }

    if (this.client && this.client.readyState === WebSocket.OPEN) {
      log.warn('duplicate extension connection, closing old one');
      this.client.close();
    }

    this.client = ws;
    log.info('extension connected');
    this.notifyConnection(true);

    ws.on('message', (data) => this.handleRawMessage(data.toString()));
    ws.on('close', () => this.handleDisconnect(ws));
    ws.on('error', (err) => log.error('ws client error', err));
  }

  private handleRawMessage(raw: string): void {
    if (this.onMessage) {
      this.onMessage(raw);
    }
  }

  private onMessage: ((raw: string) => void) | null = null;

  /** 注册原始消息处理器（由 handler.ts 负责解析与分发） */
  setMessageHandler(fn: (raw: string) => void): void {
    this.onMessage = fn;
  }

  private handleDisconnect(ws: WebSocket): void {
    // 只有当前连接断开才清理状态；被替换掉的旧连接（duplicate）忽略，避免清掉新连接
    if (this.client !== ws) return;
    log.warn('extension disconnected');
    this.client = null;
    for (const { timer, reject } of this.pending.values()) {
      clearTimeout(timer);
      reject(new Error('插件连接断开'));
    }
    this.pending.clear();
    this.notifyConnection(false);
  }

  private notifyConnection(connected: boolean): void {
    this.connectListeners.forEach((fn) => fn(connected));
  }
}
