import { serve } from '@hono/node-server';
import { setLogLevel, createLogger } from '@ai-gateway/utils';
import { loadConfig } from './config/index.js';
import { WebSocketBridge } from './bridge/websocket.js';
import { handleBridgeMessage } from './bridge/handler.js';
import { ChatHandler } from './handlers/chatHandler.js';
import { createApp } from './api/server.js';

const log = createLogger('server');

async function main(): Promise<void> {
  const config = loadConfig();
  setLogLevel(config.logLevel);

  // 1. WebSocket 桥接
  const bridge = new WebSocketBridge({
    host: config.host,
    port: config.wsPort,
    authToken: config.wsAuthToken,
    requestTimeoutMs: config.requestTimeoutMs,
  });
  bridge.setMessageHandler((raw) => handleBridgeMessage(bridge, raw));
  bridge.onConnectionChange((connected) => {
    log.info(connected ? '插件已连接' : '插件已断开');
  });
  bridge.start();

  // 2. 聊天处理器 + HTTP 服务
  const chatHandler = new ChatHandler(bridge, config.requestTimeoutMs);
  const app = createApp({ bridge, chatHandler, config });

  const server = serve({ fetch: app.fetch, port: config.port, hostname: config.host }, (info) => {
    log.info(`OpenAI-compatible API listening on http://${info.address}:${info.port}`);
  });

  // 3. 优雅退出
  const shutdown = async (signal: string) => {
    log.info(`received ${signal}, shutting down...`);
    await bridge.stop();
    server.close();
    process.exit(0);
  };
  process.on('SIGINT', () => void shutdown('SIGINT'));
  process.on('SIGTERM', () => void shutdown('SIGTERM'));
}

main().catch((err) => {
  log.error('failed to start server', err);
  process.exit(1);
});
