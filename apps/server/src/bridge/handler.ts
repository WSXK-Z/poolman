import {
  MessageType,
  protocolMessageSchema,
  type CapturedChatApi,
} from '@ai-gateway/shared';
import { createLogger } from '@ai-gateway/utils';
import type { WebSocketBridge } from './websocket.js';
import type { CredentialStore } from '../providers/credentials.js';

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
 * @param credentials 服务端凭证存储（CREDENTIAL_SYNC 更新入口）
 */
export function handleBridgeMessage(
  bridge: WebSocketBridge,
  raw: string,
  credentials: CredentialStore,
): void {
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
    case MessageType.CHAT_REQUEST:
    case MessageType.CAPTURE_REQUEST:
    case MessageType.EXECUTE_REQUEST:
      // 这三个是「服务端 -> 插件」的下行消息，插件不应回发；收到即忽略（可能是旧版本插件）
      log.warn('unexpected server->extension message type from extension', msg.type);
      break;
    case MessageType.CREDENTIAL_SYNC: {
      // 插件上报凭证：写入服务端 CredentialStore，并完成可能等待中的 CAPTURE_REQUEST。
      // 只覆盖消息里携带的字段；未携带的保持原值（避免覆盖更完整的历史捕获）。
      const provider = msg.payload.provider;
      const prev = credentials.get(provider);
      // 只在访问凭证（token/settingsToken）实际变化时才打印，避免插件每 2s 轮询上报刷屏。
      const newToken = msg.payload.token ?? undefined;
      const newSettings = msg.payload.settingsToken ?? undefined;
      const changed =
        (newToken !== undefined && newToken !== (prev?.token ?? null)) ||
        (newSettings !== undefined && newSettings !== (prev?.settingsToken ?? null));
      credentials.upsert(provider, {
        token: newToken,
        settingsToken: newSettings,
        hifHeaders: msg.payload.hifHeaders,
        apiTemplate: msg.payload.apiTemplate as CapturedChatApi | null | undefined,
      });
      bridge.resolveCapture(provider);
      if (changed) {
        console.log(`[插件:capture] ${provider} 访问凭证已捕获`, {
          provider,
          tokenLength: (newToken ?? '').length,
          hasSettingsToken: !!newSettings,
        });
      }
      break;
    }
    case MessageType.EXECUTE_CHUNK:
      bridge.emitExecuteChunk(msg.id, msg.payload);
      break;
    case MessageType.EXECUTE_RESULT:
      bridge.resolveExecute(msg.id, msg.payload);
      break;
    case MessageType.EXECUTE_ERROR:
      bridge.rejectExecute(msg.id, msg.payload.error, msg.payload.code);
      break;
    case MessageType.STATUS_SYNC:
      bridge.emitStatus(msg.payload);
      break;
    case MessageType.DIAGNOSTIC:
      logDiagnostic(msg.payload);
      break;
    default:
      // 所有协议消息类型都已在上面覆盖；此处仅防御性兜底（msg 已被窄化为 never）
      log.warn('unhandled message type', msg);
  }
}
