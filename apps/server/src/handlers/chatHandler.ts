import {
  ModelMapping,
  type ChatRequestPayload,
  type ChatResponsePayload,
  type ModelProvider,
  PROVIDERS,
} from '@ai-gateway/shared';
import { createLogger } from '@ai-gateway/utils';
import type { WebSocketBridge } from '../bridge/websocket.js';
import type { ChatCompletionRequest, ToolCall } from '@ai-gateway/shared';
import type { DeepSeekService } from '../providers/deepseek.js';

const log = createLogger('server:chat');

export interface ChatCompletionResult {
  id: string;
  created: number;
  model: string;
  content?: string;
  tool_calls?: ToolCall[];
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
    /** 服务端直管的 DeepSeek 服务（连接器化后 deepseek-web 不再走插件 CHAT_REQUEST） */
    private readonly deepseek: DeepSeekService | null = null,
    private readonly timeoutMs = 120_000,
  ) { }

  /**
   * 处理一次 OpenAI 兼容的聊天请求（非流式）。
   * 将请求转发给插件，等待完整响应后返回。
   */
  async handle(req: ChatCompletionRequest): Promise<ChatCompletionResult> {
    const provider = mapModel(req.model);
    // DOM 平台只需最后一条消息（网页自身维护历史）；DeepSeek 服务端直管需要完整 messages（判断对话/prompt）
    const messages = provider === 'deepseek-web' ? req.messages : req.messages.slice(-1);
    const payload: ChatRequestPayload = {
      model: provider,
      messages,
      options: {
        temperature: req.temperature,
        topP: req.top_p,
        maxTokens: req.max_tokens,
        stream: req.stream,
        sessionId: req.session_id,
        tools: req.tools,
        tool_choice: req.tool_choice,
      },
    };

    log.info(`handling chat request model=${req.model} -> provider=${provider} messages=${req.messages.length}`);

    // DeepSeek 由服务端直管（组装请求 → EXECUTE 通道 → 服务端解析），不再经插件 CHAT_REQUEST
    if (provider === 'deepseek-web' && this.deepseek) {
      const result = await this.deepseek.chat(provider, req.messages, payload.options);
      return {
        id: `chatcmpl-${Date.now().toString(36)}`,
        created: Math.floor(Date.now() / 1000),
        model: req.model,
        content: result.content,
        tool_calls: result.tool_calls,
        usage: result.usage,
        finishedReason: result.finishedReason,
      };
    }

    const response = await this.bridge.sendRequest(payload, this.timeoutMs);

    return {
      id: `chatcmpl-${Date.now().toString(36)}`,
      created: Math.floor(Date.now() / 1000),
      model: req.model,
      content: response.content,
      tool_calls: response.tool_calls,
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
    // DOM 平台只需最后一条消息（网页自身维护历史）；DeepSeek 服务端直管需要完整 messages（判断对话/prompt）
    const messages = provider === 'deepseek-web' ? req.messages : req.messages.slice(-1);
    const payload: ChatRequestPayload = {
      model: provider,
      messages,
      options: {
        temperature: req.temperature,
        topP: req.top_p,
        maxTokens: req.max_tokens,
        stream: true,
        sessionId: req.session_id,
        tools: req.tools,
        tool_choice: req.tool_choice,
      },
    };

    log.info(`handling stream request model=${req.model} -> provider=${provider} messages=${req.messages.length}`);

    // DeepSeek 由服务端直管：chatStream 每次产出增量 JSON 字符串 {THINK, RESPONSE} 交给 onChunk
    if (provider === 'deepseek-web' && this.deepseek) {
      const result = await this.deepseek.chatStream(
        provider,
        req.messages,
        (delta) => onChunk(delta),
        payload.options,
      );
      return {
        id: `chatcmpl-${Date.now().toString(36)}`,
        created: Math.floor(Date.now() / 1000),
        model: req.model,
        content: result.content,
        usage: result.usage,
        tool_calls: result.tool_calls,
        finishedReason: result.finishedReason,
      };
    }

    const response = await this.bridge.sendRequest(payload, this.timeoutMs, (chunk) => {
      if (chunk.delta) onChunk(chunk.delta);
    });

    return {
      id: `chatcmpl-${Date.now().toString(36)}`,
      created: Math.floor(Date.now() / 1000),
      model: req.model,
      content: response.content,
      usage: response.usage,
      tool_calls: response.tool_calls,
      finishedReason: response.finishedReason,
    };
  }
}
