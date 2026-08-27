import { MessageType, type ChatResult, type ProtocolMessage } from '@ai-gateway/shared';
import { createLogger, errorMessage } from '@ai-gateway/utils';
import { diag } from '~/background/diagnostics';
import { getProvider } from '~/providers';
import { backgroundState } from './state';

const log = createLogger('ext:router');

/**
 * 处理来自本地服务的协议消息，并把响应通过 send 回传。
 * @param msg  服务端下发的消息
 * @param send 回传函数
 */
export async function handleServerMessage(
  msg: ProtocolMessage,
  send: (msg: ProtocolMessage) => void,
): Promise<void> {
  switch (msg.type) {
    case MessageType.CHAT_REQUEST: {
      const { id, payload } = msg;
      log.info(`received CHAT_REQUEST ${id} model=${payload.model}`);
      backgroundState.addLog('info', `收到请求 ${id} (${payload.model})`);
      diag('info', 'router', `收到请求 ${id}`, {
        id,
        model: payload.model,
        messages: payload.messages.length,
        options: payload.options,
      });
      try {
        const provider = getProvider(payload.model);
        const started = Date.now();
        // 流式：优先使用 provider.chatStream，把增量文本逐块回传为 CHAT_CHUNK
        const streaming = !!payload.options?.stream;
        let result: ChatResult;
        if (streaming && provider.chatStream) {
          result = await provider.chatStream(
            payload.model,
            payload.messages,
            (delta) => {
              send({
                id,
                type: MessageType.CHAT_CHUNK,
                timestamp: Date.now(),
                payload: { delta },
              });
            },
            payload.options,
          );
        } else {
          result = await provider.chat(payload.model, payload.messages, payload.options);
        }
        send({
          id,
          type: MessageType.CHAT_RESPONSE,
          timestamp: Date.now(),
          payload: {
            content: result.content,
            model: result.model,
            usage: result.usage,
            finishedReason: result.finishedReason,
          },
        });
        backgroundState.addLog('info', `请求 ${id} 完成${streaming ? '（流式）' : ''}`);
        diag('info', 'router', `请求 ${id} 完成`, {
          id,
          elapsedMs: Date.now() - started,
          contentLength: result.content.length,
          streaming,
        });
      } catch (err) {
        const message = errorMessage(err);
        const code = (err as { code?: string })?.code;
        log.error(`request ${id} failed`, message);
        backgroundState.addLog('error', `请求 ${id} 失败: ${message}`);
        diag('error', 'router', `请求 ${id} 失败`, { id, error: message, code });
        send({
          id,
          type: MessageType.CHAT_ERROR,
          timestamp: Date.now(),
          payload: { error: message, code },
        });
      }
      break;
    }
    default:
      log.warn('unhandled message type from server', msg.type);
  }
}
