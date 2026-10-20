/**
 * Background Service Worker 到本地桌面网关的 WebSocket 客户端。
 *
 * 管理连接、重连与状态心跳；具体协议消息交由 background message router 处理。
 */
import { MessageType, type ProtocolMessage, type ProviderStatusMap, type StatusSync } from '@ai-gateway/shared';
import { createLogger } from '@ai-gateway/utils';
import { browser } from 'wxt/browser';
import { backgroundState } from './state';

const log = createLogger('ext:connection');

const RECONNECT_DELAY = 3_000;
/** 周期上报插件状态（同时起到心跳保活作用） */
const STATUS_INTERVAL = 15_000;

export interface ConnectionOptions {
  url: string;
  token?: string;
  autoReconnect: boolean;
  getStatus: () => ProviderStatusMap;
  onMessage: (msg: ProtocolMessage) => void;
  /** 连接（或重连）建立后触发，用于向服务端重发本地已捕获凭证等 */
  onOpen?: () => void;
}

/**
 * 插件端 WebSocket 客户端。
 * 负责连接本地服务、自动重连、周期上报状态（心跳）。
 */
export class GatewayConnection {
  private ws: WebSocket | null = null;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private statusTimer: ReturnType<typeof setInterval> | null = null;
  private manualClosed = false;
  private readonly opts: ConnectionOptions;

  constructor(opts: ConnectionOptions) {
    this.opts = opts;
  }

  get connected(): boolean {
    return this.ws !== null && this.ws.readyState === WebSocket.OPEN;
  }

  start(): void {
    // 状态周期发送器与 WebSocket 生命周期一致，stop() 同时关闭两者。
    this.manualClosed = false;
    this.connect();
    this.statusTimer = setInterval(() => this.sendStatus(), STATUS_INTERVAL);
  }

  stop(): void {
    this.manualClosed = true;
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    if (this.statusTimer) clearInterval(this.statusTimer);
    this.ws?.close();
    this.ws = null;
    backgroundState.setConnected(false);
  }

  send(msg: ProtocolMessage): void {
    if (!this.connected) {
      log.warn('cannot send, not connected');
      return;
    }
    this.ws!.send(JSON.stringify(msg));
  }

  private connect(): void {
    // 防止并发重连创建多条 socket；CONNECTING 状态也视为已有连接尝试。
    if (this.ws && (this.ws.readyState === WebSocket.OPEN || this.ws.readyState === WebSocket.CONNECTING)) {
      return;
    }

    let url = this.opts.url;
    try {
      const params = new URLSearchParams();
      if (this.opts.token) params.set('token', this.opts.token);
      // 自报稳定身份：桌面端据此登记插件实例并复用其启用选择
      const manifest = browser.runtime.getManifest();
      params.set('clientId', browser.runtime.id);
      params.set('name', manifest.name);
      params.set('version', manifest.version);
      params.set('platform', manifest.manifest_version === 3 ? 'mv3' : 'mv2');
      url += (url.includes('?') ? '&' : '?') + params.toString();
      const ws = new WebSocket(url);
      this.ws = ws;

      ws.onopen = () => {
        log.info('connected to server', url);
        backgroundState.setConnected(true);
        this.sendStatus();
        // 连接（或重连）建立后，让 background 重发本地已捕获凭证给服务端
        this.opts.onOpen?.();
      };
      ws.onmessage = (event) => {
        try {
          const msg = JSON.parse(String(event.data)) as ProtocolMessage;
          this.opts.onMessage(msg);
        } catch (err) {
          log.warn('failed to parse server message', err);
        }
      };
      ws.onerror = () => {
        log.warn('websocket error');
      };
      ws.onclose = (event) => {
        log.warn(`websocket closed (code=${event.code})`);
        backgroundState.setConnected(false);
        this.ws = null;
        if (!this.manualClosed && this.opts.autoReconnect) {
          log.info(`reconnecting in ${RECONNECT_DELAY}ms...`);
          this.reconnectTimer = setTimeout(() => this.connect(), RECONNECT_DELAY);
        }
      };
    } catch (err) {
      log.error('failed to create websocket', err);
      if (!this.manualClosed && this.opts.autoReconnect) {
        this.reconnectTimer = setTimeout(() => this.connect(), RECONNECT_DELAY);
      }
    }
  }

  /** 立即上报一次供应商状态：状态刚变化（捕获到凭证 / 模板）时调用，避免等下一个周期。 */
  sendStatus(): void {
    if (!this.connected) return;
    const status: StatusSync = {
      id: `status_${Date.now().toString(36)}`,
      type: MessageType.STATUS_SYNC,
      timestamp: Date.now(),
      payload: this.opts.getStatus(),
    };
    this.send(status);
  }
}
