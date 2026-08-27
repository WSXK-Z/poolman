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
  for (const key of TOKEN_STORAGE_KEYS) {
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
    // 1) 转发主世界捕获的 Token / 接口模板
    window.addEventListener('message', (event) => {
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
          }
        | undefined;
      if (!data || data.source !== 'ai-gateway') return;

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

    // 2) 响应 background 的主动捕获请求
    browser.runtime.onMessage.addListener((message: unknown) => {
      const msg = message as { type?: string; provider?: string } | undefined;
      if (!msg || msg.type !== 'CAPTURE_TOKEN' || !msg.provider) return undefined;
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
