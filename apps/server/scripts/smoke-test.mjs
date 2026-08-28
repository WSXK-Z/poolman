/**
 * 端到端冒烟测试：以「模拟插件/连接器」身份连接 WebSocket 桥接，
 * 验证连接器化后的协议闭环：
 *   - CREDENTIAL_SYNC：插件上报凭证 → 服务端存储
 *   - EXECUTE_REQUEST / EXECUTE_CHUNK / EXECUTE_RESULT：DeepSeek 服务端直管链路
 *   - CHAT_REQUEST / CHAT_RESPONSE：DOM 平台链路（仍走插件）
 *   - /v1/chat/completions（非流式 + 流式）与 /health
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
  // 连接器化：插件上报凭证 → 服务端存储（否则服务端无法组装 DeepSeek 请求）
  ws.send(
    JSON.stringify({
      id: 'credsync_1',
      type: 'CREDENTIAL_SYNC',
      timestamp: Date.now(),
      payload: {
        provider: 'deepseek-web',
        token: 'mock-token',
        settingsToken: null,
        apiTemplate: {
          provider: 'deepseek-web',
          url: 'https://chat.deepseek.com/api/v0/chat/completion',
          headers: {},
          template: { model_type: 'default', thinking_enabled: true, search_enabled: false },
          token: 'mock-token',
          capturedAt: Date.now(),
        },
      },
    }),
  );
  console.log('[mock-ext] 已上报凭证 CREDENTIAL_SYNC');
});

/** 构造一段模拟的 DeepSeek SSE 响应（含 THINK / RESPONSE 片段），供 EXECUTE 回包使用 */
function makeMockSse() {
  return [
    'event: ready',
    'data: {"v":{"response":{"fragments":[{"type":"THINK","content":"思考中"},{"type":"RESPONSE","content":"你好"}]}}}',
    '',
    'data: {"p":"response/fragments/1/content","o":"APPEND","v":"，我是模拟回复"}',
    '',
    'data: {"v":{"response":{"status":"finish"}}}',
  ].join('\n');
}

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
  } else if (msg.type === 'EXECUTE_REQUEST') {
    // 模拟「网页执行器」：把一段 DeepSeek SSE 原始文本回传（服务端自行解析）
    const sse = makeMockSse();
    if (msg.payload.streaming) {
      // 流式：分块回传原始文本 + 完成标记
      const chunks = sse.match(/[\s\S]{1,64}/g) ?? [sse];
      for (const c of chunks) {
        ws.send(
          JSON.stringify({
            id: msg.id,
            type: 'EXECUTE_CHUNK',
            timestamp: Date.now(),
            payload: { data: c },
          }),
        );
        await new Promise((r) => setTimeout(r, 20));
      }
      ws.send(
        JSON.stringify({
          id: msg.id,
          type: 'EXECUTE_RESULT',
          timestamp: Date.now(),
          payload: { status: 200, ok: true, body: null },
        }),
      );
    } else {
      // 非流式：完整响应体一次回传（供服务端解析）
      ws.send(
        JSON.stringify({
          id: msg.id,
          type: 'EXECUTE_RESULT',
          timestamp: Date.now(),
          payload: { status: 200, ok: true, body: sse },
        }),
      );
    }
    replies.push(msg.id);
    console.log('[mock-ext] 回包 EXECUTE_RESULT', msg.id);
  } else if (msg.type === 'CAPTURE_REQUEST') {
    // 模拟内容脚本响应：主动上报凭证
    ws.send(
      JSON.stringify({
        id: 'credsync_2',
        type: 'CREDENTIAL_SYNC',
        timestamp: Date.now(),
        payload: { provider: msg.payload.provider, token: 'mock-token', settingsToken: null },
      }),
    );
    console.log('[mock-ext] 响应 CAPTURE_REQUEST 并上报凭证');
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

  // 1. DeepSeek 非流式（服务端直管：EXECUTE 通道）
  const res1 = await post({ model: 'deepseek-web', messages: [{ role: 'user', content: '你好' }] });
  const body1 = await res1.json();
  console.log('\n[api] deepseek 非流式 status:', res1.status);
  console.log('[api] deepseek 非流式:', JSON.stringify(body1, null, 2));

  // 2. DeepSeek 流式（服务端直管：EXECUTE streaming 通道）
  const res2 = await post({
    model: 'deepseek-web',
    stream: true,
    messages: [{ role: 'user', content: '讲个笑话' }],
  });
  const text2 = await res2.text();
  console.log('\n[api] deepseek 流式 status:', res2.status);
  console.log('[api] deepseek 流式片段:', text2.slice(0, 500).replace(/\n/g, ' '));

  // 3. DOM 平台（仍走 CHAT_REQUEST 通道）
  const res3 = await post({ model: 'gpt-4o', messages: [{ role: 'user', content: 'hi' }] });
  const body3 = await res3.json();
  console.log('\n[api] dom(gpt-4o) status:', res3.status);
  console.log('[api] dom:', JSON.stringify(body3, null, 2));

  // 4. 健康检查（应反映 mock 上报的凭证/状态）
  const health = await (await fetch(`${API_URL}/health`)).json();
  console.log('\n[api] health:', JSON.stringify(health, null, 2));

  ws.close();
  process.exit(0);
}

main().catch((e) => {
  console.error('[smoke] 失败:', e);
  process.exit(1);
});
