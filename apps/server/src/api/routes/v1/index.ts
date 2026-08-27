import { Hono } from 'hono';
import type { ChatHandler } from '../../../handlers/chatHandler.js';
import { createChatCompletionsRoute } from './chat-completions.js';
import { createModelsRoute } from './models.js';

/** 挂载 v1 版本路由 */
export function createV1Routes(handler: ChatHandler): Hono {
  const v1 = new Hono();
  v1.route('/models', createModelsRoute());
  v1.route('/chat/completions', createChatCompletionsRoute(handler));
  return v1;
}
