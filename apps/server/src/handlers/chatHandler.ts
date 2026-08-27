import {
  ModelMapping,
  type ChatRequestPayload,
  type ChatResponsePayload,
  type ModelProvider,
  PROVIDERS,
} from '@ai-gateway/shared';
import { createLogger } from '@ai-gateway/utils';
import type { WebSocketBridge } from '../bridge/websocket.js';
import type { ChatCompletionRequest } from '@ai-gateway/shared';

const log = createLogger('server:chat');

export interface ChatCompletionResult {
  id: string;
  created: number;
  model: string;
  content: string;
  usage?: ChatResponsePayload['usage'];
  finishedReason?: string;
}

/** 将外部请求模型名映射为插件 Provider 标识 */
export function mapModel(model: string): ModelProvider {
  if (PROVIDERS.includes(model as ModelProvider)) {
    return model as ModelProvider;
  }
  if (model in ModelMapping) {
    return ModelMapping[model as keyof typeof ModelMapping];
  }
  throw Object.assign(new Error(`模型 ${model} 不受支持`), {
    status: 404,
    code: 'model_not_found',
  });
}

export class ChatHandler {
  constructor(
    private readonly bridge: WebSocketBridge,
    private readonly timeoutMs = 120_000,
  ) {}

  /**
   * 处理一次 OpenAI 兼容的聊天请求（非流式）。
   * 将请求转发给插件，等待完整响应后返回。
   */
  async handle(req: ChatCompletionRequest): Promise<ChatCompletionResult> {
    const provider = mapModel(req.model);
    const payload: ChatRequestPayload = {
      model: provider,
      messages: req.messages,
      options: {
        temperature: req.temperature,
        topP: req.top_p,
        maxTokens: req.max_tokens,
        stream: req.stream,
        sessionId: req.session_id,
      },
    };

    log.info(`handling chat request model=${req.model} -> provider=${provider} messages=${req.messages.length}`);

    const response = await this.bridge.sendRequest(payload, this.timeoutMs);

    return {
      id: `chatcmpl-${Date.now().toString(36)}`,
      created: Math.floor(Date.now() / 1000),
      model: req.model,
      content: response.content,
      usage: response.usage,
      finishedReason: response.finishedReason,
    };
  }

  /**
   * 处理一次 OpenAI 兼容的聊天请求（流式）。
   * 通过 WebSocket 向插件发起流式请求，插件每产出增量文本就调用 onChunk，
   * 全部完成（或出错）后 resolve/reject。
   */
  async handleStream(
    req: ChatCompletionRequest,
    onChunk: (delta: string) => void,
  ): Promise<ChatCompletionResult> {
    const provider = mapModel(req.model);
    const payload: ChatRequestPayload = {
      model: provider,
      messages: req.messages,
      options: {
        temperature: req.temperature,
        topP: req.top_p,
        maxTokens: req.max_tokens,
        stream: true,
        sessionId: req.session_id,
      },
    };

    log.info(`handling stream request model=${req.model} -> provider=${provider} messages=${req.messages.length}`);

    const response = await this.bridge.sendRequest(payload, this.timeoutMs, (chunk) => {
      if (chunk.delta) onChunk(chunk.delta);
    });

    return {
      id: `chatcmpl-${Date.now().toString(36)}`,
      created: Math.floor(Date.now() / 1000),
      model: req.model,
      content: response.content,
      usage: response.usage,
      finishedReason: response.finishedReason,
    };
  }
}
