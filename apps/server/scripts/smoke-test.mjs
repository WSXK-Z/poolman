/**
 * 端到端冒烟测试：以「模拟插件」身份连接 WebSocket 桥接，
 * 验证 STATUS_SYNC / CHAT_REQUEST / CHAT_RESPONSE 协议闭环与 chat API。
 * 用法：确保 server 已启动后，在 apps/server 目录执行：
 *   node scripts/smoke-test.mjs
 */
import { WebSocket } from 'ws';

const WS_URL = process.env.WS_URL || 'ws://127.0.0.1:3001';
const API_URL = process.env.API_URL || 'http://127.0.0.1:3000';

const ws = new WebSocket(WS_URL);
const replies = [];

ws.on('open', () => {
  console.log('[mock-ext] 已连接', WS_URL);
  // 测试诊断通道
  ws.send(
    JSON.stringify({
      id: 'diag_1',
      type: 'DIAGNOSTIC',
      timestamp: Date.now(),
      payload: {
        level: 'info',
        source: 'smoke-test',
        message: '诊断通道测试',
        data: { hello: 'world' },
      },
    }),
  );
  ws.send(
    JSON.stringify({
      id: 'status_1',
      type: 'STATUS_SYNC',
      timestamp: Date.now(),
      payload: {
        'deepseek-web': {
          provider: 'deepseek-web',
          connected: true,
          authed: true,
          models: ['deepseek-web'],
          lastChecked: Date.now(),
        },
      },
    }),
  );
});

ws.on('message', async (raw) => {
  const msg = JSON.parse(raw.toString());
  console.log('[mock-ext] 收到', msg.type, msg.id);
  if (msg.type === 'CHAT_REQUEST') {
    const lastUser = [...msg.payload.messages].reverse().find((m) => m.role === 'user');
    const reply = {
      id: msg.id,
      type: 'CHAT_RESPONSE',
      timestamp: Date.now(),
      payload: {
        content: `【模拟回复】模型=${msg.payload.model}，你的问题是：${lastUser?.content ?? ''}`,
        model: msg.payload.model,
        finishedReason: 'stop',
      },
    };
    replies.push(msg.id);
    await new Promise((r) => setTimeout(r, 200));
    ws.send(JSON.stringify(reply));
    console.log('[mock-ext] 回包 CHAT_RESPONSE', msg.id);
  }
});

const post = (body) =>
  fetch(`${API_URL}/v1/chat/completions`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  });

async function main() {
  await new Promise((r) => setTimeout(r, 500));

  // 1. 非流式
  const res1 = await post({ model: 'deepseek-web', messages: [{ role: 'user', content: '你好' }] });
  const body1 = await res1.json();
  console.log('\n[api] 非流式 status:', res1.status);
  console.log('[api] 非流式:', JSON.stringify(body1, null, 2));

  // 2. 流式
  const res2 = await post({ model: 'gpt-4o', stream: true, messages: [{ role: 'user', content: '讲个笑话' }] });
  const text2 = await res2.text();
  console.log('\n[api] 流式 status:', res2.status);
  console.log('[api] 流式片段:', text2.slice(0, 300).replace(/\n/g, ' '));

  // 3. 健康检查（应反映 mock 上报的状态）
  const health = await (await fetch(`${API_URL}/health`)).json();
  console.log('\n[api] health:', JSON.stringify(health, null, 2));

  ws.close();
  process.exit(0);
}

main().catch((e) => {
  console.error('[smoke] 失败:', e);
  process.exit(1);
});
