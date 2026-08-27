import { Hono } from 'hono';
import { HTTPException } from 'hono/http-exception';
import { streamText } from 'hono/streaming';
import { chatCompletionRequestSchema } from '@ai-gateway/shared';
import type { ChatHandler } from '../../../handlers/chatHandler.js';

/** OpenAI 兼容的核心聊天接口：POST /v1/chat/completions */
export function createChatCompletionsRoute(handler: ChatHandler): Hono {
  const app = new Hono();

  app.post('/', async (c) => {
    const raw = await c.req.json().catch(() => null);
    if (raw === null) {
      throw new HTTPException(400, { message: '请求体必须是合法的 JSON' });
    }
    console.log('received raw chat request', JSON.stringify(raw, null, 2));
    const parsed = chatCompletionRequestSchema.safeParse(raw);
    if (!parsed.success) {
      throw new HTTPException(400, {
        message: parsed.error.issues.map((i) => `${i.path.join('.')}: ${i.message}`).join('; '),
      });
    }
    console.log('received chat request', JSON.stringify(parsed.data, null, 2));
    const last = parsed.data.messages.pop();
    parsed.data.messages=last?[last]:[];
    const req = parsed.data;

    // 固定会话：优先取请求头 x-deepseek-session，其次请求体 session_id
    const headerSession = c.req.header('x-deepseek-session');
    if (headerSession) req.session_id = headerSession;

    // 流式：通过插件实时接收增量块并转发为 SSE 事件（真正的流式输出）
    if (req.stream) {
      return streamText(c, async (stream) => {
        const base = {
          id: `chatcmpl-${Date.now().toString(36)}`,
          object: 'chat.completion.chunk',
          created: Math.floor(Date.now() / 1000),
          model: req.model,
        };
        try {
          const result = await handler.handleStream(req, (delta) => {
            const chunks = JSON.parse(delta) as Record<string, unknown>;
            stream.write(
              `data: ${JSON.stringify({
                ...base,
                choices: [{ index: 0, delta: { 
                  content: chunks["RESPONSE"] ?? "",
                  reasoning_content: chunks["THINK"] ?? ""
                },
                  finish_reason: null }],
              })}\n\n`,
            );
          });
          // 结束标记（含 finish_reason）
          stream.write(
            `data: ${JSON.stringify({
              ...base,
              choices: [{ index: 0, delta: {}, finish_reason: result.finishedReason ?? 'stop' }],
            })}\n\n`,
          );

          // 可选 usage
          if (req.stream_options?.include_usage && result.usage) {
            stream.write(`data: ${JSON.stringify({ ...base, choices: [], usage: result.usage })}\n\n`);
          }
        } catch (err) {
          const e = err as Error & { code?: string };
          const msg = err instanceof Error ? err.message : 'provider error';
          const code = e.code || 'provider_error';
          stream.write(`data: ${JSON.stringify({ error: { message: msg, type: 'api_error', code } })}\n\n`);
        }

        stream.write('data: [DONE]\n\n');
      });
    }

    // 非流式：直接返回完整 JSON
    const result = await handler.handle(req);
    return c.json({
      id: result.id,
      object: 'chat.completion',
      created: result.created,
      model: result.model,
      choices: [
        {
          index: 0,
          message: { role: 'assistant', content: result.content },
          finish_reason: result.finishedReason ?? 'stop',
        },
      ],
      usage: result.usage,
    });
  });

  return app;
}
