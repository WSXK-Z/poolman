import type { Context } from 'hono';
import type { ContentfulStatusCode } from 'hono/utils/http-status';
import { createLogger } from '@ai-gateway/utils';
import { HTTPException } from 'hono/http-exception';
import { ZodError } from 'zod';

const log = createLogger('server:error');

export interface ApiErrorBody {
  error: {
    message: string;
    type: string;
    param: string | null;
    code: string;
  };
}

function errorBody(message: string, type: string, code: string, param: string | null = null): ApiErrorBody {
  return {
    error: { message, type, param, code },
  };
}

/** Hono onError 处理器：输出 OpenAI 风格错误结构 */
export async function errorHandler(err: Error, c: Context): Promise<Response> {
  log.error('request failed', err);

  // Zod 校验错误
  if (err instanceof ZodError) {
    return c.json(
      errorBody('Invalid request: ' + err.issues.map((i) => i.message).join('; '), 'invalid_request_error', 'invalid_request'),
      400,
    );
  }

  // Hono HTTPException
  if (err instanceof HTTPException) {
    const status = err.status;
    return c.json(
      errorBody(err.message || 'Request failed', status >= 500 ? 'api_error' : 'invalid_request_error', String(status)),
      status,
    );
  }

  // 业务错误（带 status/code 属性的 Error）
  const e = err as Error & { status?: number; code?: string };
  const status = (e.status && e.status >= 400 && e.status < 600 ? e.status : 500) as ContentfulStatusCode;
  const code = e.code || (status >= 500 ? 'server_error' : 'request_error');
  const type = status >= 500 ? 'api_error' : 'invalid_request_error';
  return c.json(errorBody(e.message || 'Internal server error', type, code), status);
}
