/**
 * 主世界（MAIN）内容脚本：拦截网页自身的 fetch / XHR，捕获：
 *   1) 访问 Token（Authorization 等）
 *   2) 聊天接口的「完整请求模板」：URL、请求头、请求体结构（model_name / agent_name /
 *      generation_settings / 会话字段等）
 * 通过 window.postMessage 上报给 isolated world，再由其转发给 background。
 * 这样插件能「自己学会」各平台当前的接口格式，网站改版也能自适应，无需人工配置。
 */
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
  world: 'MAIN',
  runAt: 'document_start',
  main() {
    function detectProvider(hostname: string): string | null {
      if (hostname.includes('deepseek')) return 'deepseek-web';
      if (hostname.includes('chatgpt') || hostname.includes('openai')) return 'chatgpt-web';
      if (hostname.includes('claude')) return 'claude-web';
      if (hostname.includes('gemini')) return 'gemini-web';
      if (hostname.includes('kimi') || hostname.includes('moonshot')) return 'kimi-web';
      if (hostname.includes('tongyi') || hostname.includes('aliyun')) return 'qwen-web';
      return null;
    }

    function getHeader(headers: Headers | Record<string, string>, name: string): string | undefined {
      if (headers instanceof Headers) {
        return headers.get(name) ?? undefined;
      }
      return headers[name.toLowerCase()] ?? headers[name];
    }

    function headersToRecord(headers: Headers | Record<string, string>): Record<string, string> {
      const out: Record<string, string> = {};
      if (headers instanceof Headers) {
        headers.forEach((v, k) => {
          out[k.toLowerCase()] = v;
        });
      } else {
        for (const [k, v] of Object.entries(headers)) {
          out[k.toLowerCase()] = String(v);
        }
      }
      return out;
    }

    function extractToken(headers: Headers | Record<string, string>, url: string): string | null {
      const auth = getHeader(headers, 'authorization');
      if (auth) return auth.replace(/^Bearer\s+/i, '');
      const apiKey = getHeader(headers, 'x-api-key');
      if (apiKey) return apiKey;
      try {
        const token = new URL(url).searchParams.get('token');
        if (token) return token;
      } catch {
        /* ignore */
      }
      return null;
    }

    /** 是否是聊天完成类接口（各平台常见路径关键词）。
     *  注意排除 create_pow_challenge / chat_session 等辅助接口，避免污染聊天接口模板。 */
    function isChatApi(url: string): boolean {
      if (/(create_pow_challenge|chat_session|session\/create)/i.test(url)) return false;
      return /(chat\/completion|completion|conversation|chat\/send|api\/v\d+\/chat\/completion)/i.test(url);
    }

    /** 从请求体 JSON 中提取「模板字段」（不含具体消息内容） */
    function extractApiTemplate(body?: string | null): Record<string, unknown> | null {
      if (typeof body !== 'string' || !body.trim().startsWith('{')) return null;
      try {
        const parsed = JSON.parse(body) as Record<string, unknown>;
        if (!parsed || typeof parsed !== 'object') return null;
        return {
          agent_name: parsed.agent_name,
          model_name: parsed.model_name,
          think_enabled: parsed.think_enabled,
          generation_settings: parsed.generation_settings,
          has_session_id: 'chat_session_id' in parsed,
          has_parent_message_id: 'parent_message_id' in parsed,
          message_count: Array.isArray(parsed.messages) ? parsed.messages.length : 0,
          // 复用网页当前会话 ID（随机 UUID 会被服务端判 invalid chat session id）
          chat_session_id: typeof parsed.chat_session_id === 'string' ? parsed.chat_session_id : undefined,
          parent_message_id: typeof parsed.parent_message_id === 'string' ? parsed.parent_message_id : null,
          // 真实 schema 字段（2026-08 网站）
          model_type: typeof parsed.model_type === 'string' ? parsed.model_type : undefined,
          thinking_enabled: typeof parsed.thinking_enabled === 'boolean' ? parsed.thinking_enabled : undefined,
          search_enabled: typeof parsed.search_enabled === 'boolean' ? parsed.search_enabled : undefined,
          // 记录请求体顶层所有字段名，便于完整还原 schema
          body_keys: Object.keys(parsed),
        };
      } catch {
        return null;
      }
    }

    function report(
      url: string,
      headers: Headers | Record<string, string>,
      body?: string | null,
    ): void {
      const provider = detectProvider(location.hostname);
      if (!provider) return;
      const token = extractToken(headers, url);
      const isChat = isChatApi(url);
      const template = isChat ? extractApiTemplate(body) : null;
      const headerRecord = headersToRecord(headers);

      if (isChat) {
        // DeepSeek 完成接口需要 x-hif-leim / x-hif-dliq（会话级静态头），单独提取上报
        const hifHeaders: { leim?: string; dliq?: string } = {};
        if (headerRecord['x-hif-leim']) hifHeaders.leim = headerRecord['x-hif-leim'];
        if (headerRecord['x-hif-dliq']) hifHeaders.dliq = headerRecord['x-hif-dliq'];
        window.postMessage(
          {
            source: 'ai-gateway',
            type: 'CAPTURED_API',
            provider,
            token,
            url,
            headers: headerRecord,
            template,
            hifHeaders,
          },
          '*',
        );
      } else if (token) {
        window.postMessage(
          { source: 'ai-gateway', type: 'CAPTURED_TOKEN', provider, token, url },
          '*',
        );
      }
    }

    // 拦截 fetch
    const origFetch = window.fetch.bind(window);
    window.fetch = async (...args: Parameters<typeof fetch>): Promise<Response> => {
      const [input, init] = args;
      const url = typeof input === 'string' ? input : input instanceof Request ? input.url : String(input);
      const headers = (init?.headers as Headers | Record<string, string> | undefined) ?? (input instanceof Request ? input.headers : undefined);
      const body = typeof init?.body === 'string' ? init.body : undefined;
      if (headers) report(url, headers as Headers | Record<string, string>, body);
      return origFetch(...(args as [RequestInfo | URL, RequestInit | undefined]));
    };

    // 拦截 XMLHttpRequest
    const origOpen = XMLHttpRequest.prototype.open;
    const origSetRequestHeader = XMLHttpRequest.prototype.setRequestHeader;
    const origSend = XMLHttpRequest.prototype.send;
    let pendingUrl = '';
    const captured: Record<string, string> = {};
    XMLHttpRequest.prototype.open = function (
      this: XMLHttpRequest,
      method: string,
      url: string | URL,
      async?: boolean,
      username?: string | null,
      password?: string | null,
    ): void {
      pendingUrl = String(url);
      origOpen.call(this, method, String(url), async ?? true, username, password);
    };
    XMLHttpRequest.prototype.setRequestHeader = function (this: XMLHttpRequest, name: string, value: string): void {
      captured[name.toLowerCase()] = value;
      origSetRequestHeader.call(this, name, value);
    };
    XMLHttpRequest.prototype.send = function (
      this: XMLHttpRequest,
      body?: Document | XMLHttpRequestBodyInit | null,
    ): void {
      const bodyStr = typeof body === 'string' ? body : undefined;
      if (pendingUrl) report(pendingUrl, captured, bodyStr);
      origSend.call(this, body);
    };
  },
});
