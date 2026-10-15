/**
 * 页面隔离世界与扩展后台之间的消息桥。
 *
 * 隔离世界能调用扩展 API，但访问不到网页主世界的 fetch 覆盖与 JS 变量；
 * 它只转发经来源标记识别的消息，不解释供应商请求或响应内容。
 */
import { browser } from 'wxt/browser';

/**
 * 隔离世界（ISOLATED）内容脚本：桥接 MAIN world 与 background。
 * 1) 监听主世界 postMessage 上报的 Token，转发给 background（仅内存）。
 * 2) 响应 background 的 CAPTURE_TOKEN 请求，从页面 localStorage/cookie 提取凭证。
 */

/** 常见凭证存储位置（各平台可能不同，可在此扩展） */
const TOKEN_STORAGE_KEYS = [
  'token',
  'access_token',
  'accessToken',
  'auth_token',
  'authorization',
  'userToken',
];

/** 从页面存储 / cookie 中提取访问凭证 */
function extractTokenFromPage(): string | null {
  // DeepSeek 正确的访问凭证在 localStorage.userToken（Bearer 头来源）。
  // 页面上其它通用 key（token/accessToken 等）可能是无关值，若按通用顺序先命中会导致
  // 上报错误的 token（与主世界拦截的真实请求头不一致，造成 92/64 交替刷屏）。
  const isDeepSeek = location.hostname.includes('deepseek');
  const keys = isDeepSeek
    ? ['userToken', ...TOKEN_STORAGE_KEYS.filter((k) => k !== 'userToken')]
    : TOKEN_STORAGE_KEYS;
  for (const key of keys) {
    try {
      const v = localStorage.getItem(key);
      if (v) return v.replace(/^Bearer\s+/i, '');
    } catch {
      /* 某些页面可能禁用 localStorage */
    }
  }
  try {
    const match = document.cookie.match(/(?:^|;\s*)(?:access_token|token)=([^;]+)/i);
    if (match?.[1]) return decodeURIComponent(match[1]).replace(/^Bearer\s+/i, '');
  } catch {
    /* ignore */
  }
  return null;
}

/** 提取 settingsToken（用于 x-settings-token 请求头，DeepSeek 存在 localStorage.settingsJwt） */
function extractSettingsToken(): string | null {
  try {
    const v = localStorage.getItem('settingsJwt') ?? localStorage.getItem('settings_token');
    if (!v) return null;
    // settingsJwt 可能是 JSON 包装 {"value":{"ownerHash":..,"jwt":"..."},"__version":"0"}
    // 请求头只需要裸 JWT，需先解析提取
    try {
      const parsed = JSON.parse(v) as { value?: { jwt?: string }; jwt?: string };
      if (parsed?.value?.jwt) return parsed.value.jwt;
      if (parsed?.jwt) return parsed.jwt;
    } catch {
      /* 不是 JSON，直接用原值 */
    }
    return v.replace(/^Bearer\s+/i, '');
  } catch {
    return null;
  }
}

export default defineContentScript({
  matches: [
    'https://chat.deepseek.com/*',
    'https://chatgpt.com/*',
    'https://chat.openai.com/*',
    'https://claude.ai/*',
    'https://gemini.google.com/*',
    'https://kimi.moonshot.cn/*',
    'https://tongyi.aliyun.com/*',
  ],
  world: 'ISOLATED',
  runAt: 'document_idle',
  main() {
    // 防重复注入：服务端触发的兜底注入可能与 manifest 正常注入重叠
    const w = window as unknown as { __aiGatewayIsolatedWorld?: boolean };
    if (w.__aiGatewayIsolatedWorld) return;
    w.__aiGatewayIsolatedWorld = true;

    // 1) 转发主世界捕获的 Token / 接口模板 + 执行回包（EXECUTE_CHUNK / RESULT / ERROR）
    window.addEventListener('message', (event) => {
      // 单一宽对象类型：主世界会发送多种 payload，字段全部可选，避免联合类型窄化失败
      const data = event.data as
        | {
            source?: string;
            type?: string;
            provider?: string;
            token?: string;
            url?: string;
            headers?: Record<string, string>;
            template?: Record<string, unknown> | null;
            hifHeaders?: { leim?: string; dliq?: string };
            requestId?: string;
            data?: string;
            status?: number;
            ok?: boolean;
            statusText?: string;
            body?: string | null;
            error?: string;
            code?: string;
          }
        | undefined;
      if (!data || data.source !== 'ai-gateway') return;

      // 执行回包：MAIN world 的网页执行器结果 → background（再转 EXECUTE_* 协议消息给服务端）
      if (
        data.type === 'EXECUTE_CHUNK' ||
        data.type === 'EXECUTE_RESULT' ||
        data.type === 'EXECUTE_ERROR'
      ) {
        if (data.requestId) {
          browser.runtime
            .sendMessage({
              type: data.type,
              requestId: data.requestId,
              data: data.data,
              status: data.status,
              ok: data.ok,
              statusText: data.statusText,
              body: data.body,
              error: data.error,
              code: data.code,
            })
            .catch(() => {});
        }
        return;
      }

      if (data.type === 'CAPTURED_API' && data.provider) {
        browser.runtime
          .sendMessage({
            type: 'API_CAPTURED',
            payload: {
              provider: data.provider,
              url: data.url,
              headers: data.headers ?? {},
              template: data.template ?? null,
              token: data.token ?? null,
              hifHeaders: data.hifHeaders,
            },
          })
          .catch(() => {});
      } else if (data.type === 'CAPTURED_TOKEN' && data.provider && data.token) {
        browser.runtime
          .sendMessage({
            type: 'TOKEN_CAPTURED',
            payload: { provider: data.provider, token: data.token, url: data.url },
          })
          .catch(() => {});
      }
    });

    // 2) 响应 background 的主动捕获请求 + 执行指令转发
    browser.runtime.onMessage.addListener((message: unknown) => {
      // 单一宽对象类型：CAPTURE_TOKEN / EXECUTE_IN_PAGE 两种指令的字段并集，全部可选
      const msg = message as
        | {
            type?: string;
            provider?: string;
            requestId?: string;
            method?: string;
            url?: string;
            headers?: Record<string, string>;
            body?: string | null;
            streaming?: boolean;
          }
        | undefined;
      if (!msg) return undefined;

      // 执行指令：转发给 MAIN world 的网页执行器（它才是真正 fetch 的地方）
      if (msg.type === 'EXECUTE_IN_PAGE' && msg.requestId) {
        window.postMessage(
          {
            source: 'ai-gateway',
            type: 'EXECUTE_IN_PAGE',
            requestId: msg.requestId,
            method: msg.method,
            url: msg.url,
            headers: msg.headers,
            body: msg.body,
            streaming: msg.streaming,
          },
          '*',
        );
        return undefined;
      }

      if (msg.type !== 'CAPTURE_TOKEN' || !msg.provider) return undefined;
      const token = extractTokenFromPage();
      const settingsToken = extractSettingsToken();
      if (token || settingsToken) {
        browser.runtime
          .sendMessage({
            type: 'TOKEN_CAPTURED',
            payload: {
              provider: msg.provider,
              token: token ?? undefined,
              settingsToken: settingsToken ?? undefined,
            },
          })
          .catch(() => {});
      }
      return { token, settingsToken };
    });

    // 主动上报（若已登录）。⚠️ DeepSeek 的 userToken 可能是页面初始化后才写入，
    // 因此：立即读一次 + 每 2 秒轮询 + 监听 storage 事件，token 一出现就自动捕获，
    // 不再需要反复刷新页面 / 反复发请求。
    const report = (): void => {
      const provider = detectProvider(location.hostname);
      const token = provider ? extractTokenFromPage() : null;
      const settingsToken = provider ? extractSettingsToken() : null;
      if (provider && (token || settingsToken)) {
        browser.runtime
          .sendMessage({
            type: 'TOKEN_CAPTURED',
            payload: {
              provider,
              token: token ?? undefined,
              settingsToken: settingsToken ?? undefined,
            },
          })
          .catch(() => {});
      }
    };

    report();
    let polls = 0;
    const timer = setInterval(() => {
      if (extractTokenFromPage()) {
        report();
        clearInterval(timer);
      } else if (++polls > 20) {
        clearInterval(timer);
      }
    }, 2000);

    window.addEventListener('storage', (e) => {
      if (e.key && TOKEN_STORAGE_KEYS.includes(e.key)) report();
    });
  },
});

function detectProvider(hostname: string): string | null {
  if (hostname.includes('deepseek')) return 'deepseek-web';
  if (hostname.includes('chatgpt') || hostname.includes('openai')) return 'chatgpt-web';
  if (hostname.includes('claude')) return 'claude-web';
  if (hostname.includes('gemini')) return 'gemini-web';
  if (hostname.includes('kimi') || hostname.includes('moonshot')) return 'kimi-web';
  if (hostname.includes('tongyi') || hostname.includes('aliyun')) return 'qwen-web';
  return null;
}
