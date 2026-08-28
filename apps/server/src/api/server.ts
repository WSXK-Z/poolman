import { Hono } from 'hono';
import { cors } from 'hono/cors';
import { prettyJSON } from 'hono/pretty-json';
import type { ServerConfig } from '../config/index.js';
import type { WebSocketBridge } from '../bridge/websocket.js';
import type { ChatHandler } from '../handlers/chatHandler.js';
import type { CredentialStore } from '../providers/credentials.js';
import { httpLogger } from './middleware/logger.js';
import { errorHandler } from './middleware/error.js';
import { createV1Routes } from './routes/v1/index.js';

export interface AppDeps {
  bridge: WebSocketBridge;
  chatHandler: ChatHandler;
  config: ServerConfig;
  /** 服务端凭证存储（供 /health 展示各平台凭证同步状态） */
  credentials: CredentialStore;
}

/** 创建 Hono 应用（含中间件与路由） */
export function createApp(deps: AppDeps): Hono {
  const app = new Hono();

  app.use('*', httpLogger());
  app.use('*', cors({
    origin: deps.config.corsOrigin,
    allowMethods: ['GET', 'POST', 'OPTIONS'],
    allowHeaders: ['Content-Type', 'x-deepseek-session'],
  }));
  app.use('*', prettyJSON());

  // 健康检查
  app.get('/health', (c) =>
    c.json({
      status: 'ok',
      uptime: process.uptime(),
      extensionConnected: deps.bridge.connected,
      providers: deps.bridge.latestStatus,
      // 连接器化后凭证在服务端，展示各平台凭证同步状态
      credentials: deps.credentials.summary(),
      timestamp: new Date().toISOString(),
    }),
  );

  // v1 路由
  app.route('/v1', createV1Routes(deps.chatHandler));

  // 404
  app.notFound((c) =>
    c.json(
      {
        error: { message: 'Not Found', type: 'invalid_request_error', param: null, code: 'not_found' },
      },
      404,
    ),
  );

  // 统一错误处理（最后注册）
  app.onError(errorHandler);

  return app;
}
