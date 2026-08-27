import { MessageType, protocolMessageSchema } from '@ai-gateway/shared';
import { createLogger } from '@ai-gateway/utils';
import type { WebSocketBridge } from './websocket.js';

const log = createLogger('server:bridge:handler');

/** 把插件上报的诊断信息打印到终端（便于本机排查） */
function logDiagnostic(payload: {
  level: string;
  source: string;
  message: string;
  data?: unknown;
}): void {
  const tag = `[插件:${payload.source}]`;
  if (payload.level === 'error') {
    console.error('\n========== 插件报错 ==========');
    console.error(`${tag} ${payload.message}`);
    if (payload.data !== undefined) {
      console.error(JSON.stringify(payload.data, null, 2));
    }
    console.error('================================\n');
  } else if (payload.level === 'warn') {
    console.warn(`${tag} [warn] ${payload.message}`, payload.data !== undefined ? payload.data : '');
  } else {
    console.log(`${tag} ${payload.message}`, payload.data !== undefined ? payload.data : '');
  }
}

/**
 * 解析并分发来自插件的 WebSocket 消息。
 * @param bridge 桥接实例
 * @param raw 原始 JSON 字符串
 */
export function handleBridgeMessage(bridge: WebSocketBridge, raw: string): void {
  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch {
    log.warn('received non-JSON message');
    return;
  }

  const result = protocolMessageSchema.safeParse(parsed);
  if (!result.success) {
    log.warn('received invalid protocol message', result.error.issues[0]?.message ?? 'unknown');
    return;
  }

  const msg = result.data;
  switch (msg.type) {
    case MessageType.CHAT_CHUNK:
      bridge.emitChunk(msg.id, msg.payload);
      break;
    case MessageType.CHAT_RESPONSE:
      bridge.resolveResponse(msg.id, msg.payload);
      break;
    case MessageType.CHAT_ERROR:
      bridge.rejectResponse(msg.id, msg.payload.error, msg.payload.code);
      break;
    case MessageType.STATUS_SYNC:
      bridge.emitStatus(msg.payload);
      break;
    case MessageType.DIAGNOSTIC:
      logDiagnostic(msg.payload);
      break;
    default:
      log.warn('unhandled message type', msg.type);
  }
}
