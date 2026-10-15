/**
 * Background 诊断消息发送端。
 *
 * WebSocket 建立后由连接入口注入发送函数；尚未连接时诊断信息不排队，
 * 以免 Service Worker 在离线期间无限累积日志消息。
 */
import { MessageType, type DiagnosticMessage } from '@ai-gateway/shared';

type Sender = (msg: DiagnosticMessage) => void;
let sender: Sender | null = null;

/** 由 background 在建立连接后绑定发送器 */
export function bindDiagnosticSender(fn: Sender): void {
  sender = fn;
}

/**
 * 发送诊断消息到本地服务（服务端会打印到终端，便于排查）。
 * 插件内的捕获信息、请求/响应、所有报错都应通过这里上报。
 */
export function diag(
  level: 'debug' | 'info' | 'warn' | 'error',
  source: string,
  message: string,
  data?: unknown,
): void {
  sender?.({
    id: `diag_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 6)}`,
    type: MessageType.DIAGNOSTIC,
    timestamp: Date.now(),
    payload: { level, source, message, data },
  });
}
