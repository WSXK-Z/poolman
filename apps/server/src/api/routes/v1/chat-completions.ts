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
    const parsed = chatCompletionRequestSchema.safeParse(raw);
    if (!parsed.success) {
      throw new HTTPException(400, {
        message: parsed.error.issues.map((i) => `${i.path.join('.')}: ${i.message}`).join('; '),
      });
    }
    // 保留完整 messages（含 system 提示词），供服务端判断对话归属 / 构造 DeepSeek prompt；
    // DOM 平台只需最后一条，由 chatHandler 按 provider 裁剪。
    const req = parsed.data;

    // 分析辅助：设置 DUMP_CHAT_REQUESTS=1 时，把每次 chat/completions 请求的完整 messages 转储到
    // apps/server/data/requests/chat-<ts>.json，用于分析对话结构（分析完毕后删除该逻辑）。
    if (process.env.DUMP_CHAT_REQUESTS === '1') {
      try {
        const { mkdirSync, writeFileSync } = await import('node:fs');
        const { join } = await import('node:path');
        const dir = join(process.cwd(), 'data', 'requests');
        mkdirSync(dir, { recursive: true });
        writeFileSync(
          join(dir, `chat-${Date.now()}.json`),
          JSON.stringify({ model: req.model, stream: req.stream, messages: req.messages }, null, 2),
        );
      } catch {
        /* 转储失败不影响主流程 */
      }
    }

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
            // 工具调用：以 OpenAI 流式 tool_calls delta 输出
            if (Array.isArray(chunks.tool_calls)) {
              stream.write(
                `data: ${JSON.stringify({
                  ...base,
                  choices: [{
                    index: 0,
                    delta: { tool_calls: chunks.tool_calls },
                    finish_reason: null,
                  }],
                })}\n\n`,
              );
              return;
            }
            stream.write(
              `data: ${JSON.stringify({
                ...base,
                choices: [{
                  index: 0, delta: {
                    content: chunks["RESPONSE"] ?? "",
                    reasoning_content: chunks["THINK"] ?? ""
                  },
                  finish_reason: null
                }],
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
    // 判断是否返回 tool_calls
    const hasToolCalls = result.tool_calls && result.tool_calls.length > 0;
    return c.json({
      id: result.id,
      object: 'chat.completion',
      created: result.created,
      model: result.model,
      choices: [{
        index: 0,
        message: {
          role: 'assistant',
          content: hasToolCalls ? null : result.content,
          ...(hasToolCalls && { tool_calls: result.tool_calls }),
        },
        finish_reason: hasToolCalls ? 'tool_calls' : (result.finishedReason ?? 'stop'),
      }],
      usage: result.usage,
    });
  });

  return app;
}
