import { MessageType, type ChatResult, type ModelProvider, type ProtocolMessage } from '@ai-gateway/shared';
import { createLogger, errorMessage } from '@ai-gateway/utils';
import { browser } from 'wxt/browser';
import { diag } from '~/background/diagnostics';
import { getProvider } from '~/providers';
import { ProviderError } from '~/providers/base';
import { sendMessageToTab } from '~/lib/contentScript';
import { backgroundState } from './state';

const log = createLogger('ext:router');

/** 查找 provider 对应的已打开标签页（只查询，不创建标签页） */
async function findProviderTab(provider: ModelProvider): Promise<{ id?: number } | undefined> {
  const p = getProvider(provider);
  for (const pattern of p.matches) {
    try {
      const tabs = await browser.tabs.query({ url: pattern });
      const tab = tabs.find((t) => t.id != null);
      if (tab) return tab;
    } catch {
      /* ignore */
    }
  }
  return undefined;
}

/**
 * 处理来自本地服务的协议消息，并把响应通过 send 回传。
 * 连接器化后：
 *   - CHAT_REQUEST：仅 DOM 平台（chatgpt/claude/gemini/kimi/qwen）；DeepSeek 已被服务端直管
 *   - EXECUTE_REQUEST：下发到 content 网页执行器（插件只作"执行器"，业务逻辑全在服务端）
 *   - CAPTURE_REQUEST：触发内容脚本主动捕获凭证（结果经 CREDENTIAL_SYNC 上报服务端）
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
      log.info(`收到聊天请求 ${id}，模型=${payload.model}`);
      backgroundState.addLog('info', `收到请求 ${id} (${payload.model})`);
      diag('info', 'router', `收到请求 ${id}`, {
        id,
        model: payload.model,
        messages: payload.messages.length,
        options: payload.options,
      });
      try {
        // DeepSeek 已由服务端直管（走 EXECUTE 通道），插件不再处理其 CHAT_REQUEST
        if (payload.model === 'deepseek-web') {
          throw new ProviderError('DeepSeek 已由服务端直管', 'deepseek_server_managed');
        }
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
          contentLength: result.content?.length ?? 0,
          streaming,
        });
      } catch (err) {
        const message = errorMessage(err);
        const code = (err as { code?: string })?.code;
        log.error(`聊天请求 ${id} 失败`, message);
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
    case MessageType.EXECUTE_REQUEST: {
      // 服务端下发的「网页执行」指令：插件只在对应平台网页机械执行 fetch 并回传原始流
      const { id, payload } = msg;
      log.info(`收到网页执行请求 ${id}：${payload.method ?? 'POST'} ${payload.url}`);
      backgroundState.addLog('info', `执行网页请求 ${id}`);
      try {
        const tab = await findProviderTab(payload.provider);
        if (!tab?.id) {
          throw new ProviderError(`未找到 ${payload.provider} 的标签页`, 'provider_tab_not_found');
        }
        // 下发到 isolated world → 转发 MAIN world 网页执行器执行
        // （内容脚本缺失时自动补注入并重试，避免「Receiving end does not exist」）
        await sendMessageToTab(tab.id, {
          type: 'EXECUTE_IN_PAGE',
          requestId: id,
          method: payload.method,
          url: payload.url,
          headers: payload.headers,
          body: payload.body,
          streaming: payload.streaming,
        });
        // 注意：这里不回传结果。content 执行过程中会以 EXECUTE_CHUNK/RESULT/ERROR 回包，
        // 由 background 的 runtime.onMessage 转发成协议消息给服务端（id 与本次请求一致）。
      } catch (err) {
        const message = errorMessage(err);
        const code = (err as { code?: string })?.code;
        log.error(`网页执行请求 ${id} 失败`, message);
        diag('error', 'router', `执行 ${id} 失败`, { id, error: message, code });
        send({
          id,
          type: MessageType.EXECUTE_ERROR,
          timestamp: Date.now(),
          payload: { error: message, code },
        });
      }
      break;
    }
    case MessageType.CAPTURE_REQUEST: {
      // 服务端缺失凭证时，触发内容脚本从页面主动捕获（读 localStorage）。
      // 该消息无需回包：内容脚本捕获后会自动上报 CREDENTIAL_SYNC 给服务端。
      const { payload } = msg;
      log.info(`收到凭证捕获请求，平台=${payload.provider}`);
      try {
        const tab = await findProviderTab(payload.provider);
        if (tab?.id) {
          // isolated world 响应 CAPTURE_TOKEN：读 localStorage 后主动发 TOKEN_CAPTURED
          // → background 转 CREDENTIAL_SYNC → 服务端 resolveCapture
          await sendMessageToTab(tab.id, { type: 'CAPTURE_TOKEN', provider: payload.provider }).catch(
            () => {},
          );
        }
      } catch (err) {
        log.warn('凭证捕获请求失败', errorMessage(err));
      }
      break;
    }
    default:
      // 所有协议消息类型都已在上面覆盖；此处仅防御性兜底（msg 已被窄化为 never）
      log.warn('收到未处理的服务端消息类型', msg);
  }
}
