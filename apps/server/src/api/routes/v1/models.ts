import { Hono } from 'hono';
import { ModelMapping } from '@ai-gateway/shared';

/** GET /v1/models —— 返回支持的模型列表 */
export function createModelsRoute(): Hono {
  const app = new Hono();

  app.get('/', (c) => {
    const data = Object.keys(ModelMapping).map((id) => ({
      id,
      object: 'model',
      created: 0,
      owned_by: 'ai-gateway',
    }));
    return c.json({ object: 'list', data });
  });

  return app;
}
