import type { MiddlewareHandler } from 'hono';
import { createLogger } from '@ai-gateway/utils';

const log = createLogger('server:http');

/** 轻量请求日志中间件 */
export function httpLogger(): MiddlewareHandler {
  return async (c, next) => {
    const start = performance.now();
    await next();
    const elapsed = Math.round(performance.now() - start);
    log.info(`${c.req.method} ${c.req.url} -> ${c.res.status} (${elapsed}ms)`);
  };
}
