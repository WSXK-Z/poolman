import type { ChatMessage, ChatOptions, ChatResult, ModelProvider } from '@ai-gateway/shared';
import { browser } from 'wxt/browser';
import { createLogger } from '@ai-gateway/utils';
import { diag } from '~/background/diagnostics';
import { backgroundState } from '~/background/state';
import { buildPrompt, ensureTab, type AIProvider, ProviderError } from './base';
import { solvePowAndBuildHeader } from './deepseek-pow';

const log = createLogger('ext:deepseek');

const PROVIDER_ID: ModelProvider = 'deepseek-web';

const deepseekSessionMap: Record<string, {
  current_message_id: number;
}> = {};

/* ------------------------------------------------------------------ */
/* DeepSeek 网页版「后端接口」的默认配置（网页前端自己调用的内部接口，    */
/* 非官方开放 API）。                                                    */
/*                                                                     */
/* ⚠️ 这些是常见默认值。网页前端更新后可能变化。若请求失败，             */
/*    请打开 chat.deepseek.com → F12 → Network 面板，查看网页自己发起的   */
/*    聊天请求的真实地址、model_name 与鉴权方式，再调整下面的常量。      */
/* ------------------------------------------------------------------ */
const DEEPSEEK_API_URL = 'https://chat.deepseek.com/api/v0/chat/completion';
/** 创建会话接口：body 传 {} 即可获得新会话 ID */
const DEEPSEEK_SESSION_URL = 'https://chat.deepseek.com/api/v0/chat_session/create';
/** 网页版 model_type（2026-08 网站实测为 'default'） */
const DEEPSEEK_MODEL_TYPE = 'default';
/** 思考模式（V4 网页默认开启，与 localStorage.thinkingEnabled 一致） */
const DEEPSEEK_THINK = true;

/** 主动请求页面捕获凭证（token + settingsToken，带重试） */
async function requestTokenCapture(): Promise<void> {
  const tabs = await browser.tabs.query({ url: 'https://chat.deepseek.com/*' });
  const tab = tabs.find((t) => t.id != null);
  if (!tab?.id) return;
  for (let i = 0; i < 4; i++) {
    try {
      const resp = (await browser.tabs.sendMessage(tab.id, {
        type: 'CAPTURE_TOKEN',
        provider: PROVIDER_ID,
      })) as { token?: string; settingsToken?: string } | undefined;
      if (resp?.token) backgroundState.setToken(PROVIDER_ID, resp.token);
      if (resp?.settingsToken) backgroundState.setSettingsToken(PROVIDER_ID, resp.settingsToken);
      if (resp?.token) return;
    } catch {
      /* content script 可能尚未注入 */
    }
    await new Promise((r) => setTimeout(r, 1500));
  }
}

/**
 * 获取 DeepSeek 访问凭证：优先缓存，其次主动捕获（requestTokenCapture 会直接写入状态）。
 * 返回 null 时并不代表未登录 —— 鉴权也可能走其它方式。
 */
async function getDeepSeekToken(): Promise<string | null> {
  const cached = backgroundState.getToken(PROVIDER_ID);
  if (cached) return cached;
  await requestTokenCapture();
  return backgroundState.getToken(PROVIDER_ID) ?? null;
}

/** 从 422 报错里解析缺失的字段名，如 "missing field `ref_file_ids`" */
function parseMissingField(text: string): string | null {
  const m = /missing field `([^`]+)`/.exec(text);
  return m?.[1] ?? null;
}

/** 为缺失字段猜测默认值（serde 会忽略未知字段，只报缺失的必填字段） */
function guessDefault(field: string): unknown {
  const known: Record<string, unknown> = {
    ref_file_ids: [],
    search_enabled: false,
    force_search: false,
    n: 1,
    session_version: 0,
    prompt: '',
    message_meta: {},
  };
  if (field in known) return known[field];
  // 启发式：id / ids / files / list 类字段大概率是数组
  if (/(_?ids?|_?files?|_?list)$/i.test(field)) return [];
  // 布尔习惯前缀
  if (/^(is|has|enable|disable|force|search)/i.test(field)) return false;
  return '';
}

/**
 * 通过 /api/v0/chat_session/create 新建一个会话，返回会话 ID；失败返回 null。
 * body 传 {} 即可（2026-08 网站实测）。
 */
async function createDeepSeekSession(token: string, settingsToken?: string): Promise<string | null> {
  try {
    const headers: Record<string, string> = {
      'x-client-bundle-id': 'com.deepseek.chat',
      'x-client-platform': 'web',
      'x-client-version': '2.4.0',
      'x-client-locale': 'zh_CN',
      'x-client-timezone-offset': '28800',
      'Content-Type': 'application/json',
      Authorization: `Bearer ${token}`,
    };
    if (settingsToken) headers['x-settings-token'] = settingsToken;
    const res = await fetch(DEEPSEEK_SESSION_URL, { method: 'POST', headers, body: '{}' });
    const text = await res.text();
    if (!res.ok) {
      diag('error', 'deepseek', '创建会话失败', { status: res.status, body: text.slice(0, 300) });
      return null;
    }
    let json: any;
    try {
      json = JSON.parse(text);
    } catch {
      diag('error', 'deepseek', '创建会话返回非 JSON', { body: text.slice(0, 300) });
      return null;
    }
    // 兼容多种响应结构，取第一个存在的会话 ID 字段
    // 实测：create 返回 data.biz_data.chat_session.id
    const data = json?.data;
    const candidates = [
      data?.biz_data?.chat_session?.id,
      data?.biz_data?.chat_session_id,
      data?.biz_data?.id,
      data?.biz_data?.session_id,
      data?.chat_session_id,
      data?.id,
      data?.session_id,
      json?.chat_session_id,
    ];
    const sessionId = candidates.find((v) => typeof v === 'string' && v.length > 10) ?? null;
    diag('info', 'deepseek', '创建会话', { sessionId, raw: text.slice(0, 300) });
    return sessionId;
  } catch (err) {
    diag('error', 'deepseek', '创建会话异常', { error: String(err) });
    return null;
  }
}

/**
 * 通过 /api/v0/chat_session/fetch_page 获取历史会话列表，返回最近的会话 ID（无则 null）。
 * 页面左侧「历史记录」就是它加载的。用于没有捕获到会话 ID 时复用最近一次会话。
 */
async function fetchDeepSeekSessions(token: string): Promise<string | null> {
  try {
    const headers: Record<string, string> = {
      'x-client-bundle-id': 'com.deepseek.chat',
      'x-client-platform': 'web',
      'x-client-version': '2.4.0',
      'x-client-locale': 'zh_CN',
      'x-client-timezone-offset': '28800',
      'Content-Type': 'application/json',
      Authorization: `Bearer ${token}`,
    };
    const res = await fetch(
      'https://chat.deepseek.com/api/v0/chat_session/fetch_page?lte_cursor.pinned=false',
      { method: 'GET', headers },
    );
    const text = await res.text();
    if (!res.ok) {
      diag('error', 'deepseek', '获取历史会话失败', { status: res.status, body: text.slice(0, 300) });
      return null;
    }
    let json: any;
    try {
      json = JSON.parse(text);
    } catch {
      diag('error', 'deepseek', '获取历史会话返回非 JSON', { body: text.slice(0, 300) });
      return null;
    }
    // 兼容多种响应结构：data.biz_data.chat_sessions / items / list ...
    const data = json?.data;
    const items: any[] =
      data?.biz_data?.chat_sessions ??
      data?.biz_data?.items ??
      data?.biz_data?.list ??
      data?.items ??
      data?.list ??
      [];
    const sessions = (Array.isArray(items) ? items : [])
      .map((it) => ({
        id: String(it?.chat_session_id ?? it?.id ?? ''),
        title: it?.title,
        updatedAt: Number(it?.updated_at ?? it?.updatedAt ?? 0),
      }))
      .filter((s) => s.id.length > 10)
      .sort((a, b) => b.updatedAt - a.updatedAt);
    diag('info', 'deepseek', '获取历史会话', {
      count: sessions.length,
      latestId: sessions[0]?.id ?? null,
      latestTitle: sessions[0]?.title ?? null,
      raw: text.slice(0, 300),
    });
    return sessions[0]?.id ?? null;
  } catch (err) {
    diag('error', 'deepseek', '获取历史会话异常', { error: String(err) });
    return null;
  }
}

/**
 * 通过 /api/v0/chat/history_messages 获取指定会话的历史消息和最新状态。
 * 同时更新 deepseekSessionMap 缓存，供后续请求获取 parent_message_id。
 * 
 * @param token - Bearer 鉴权令牌
 * @param sessionId - 会话 UUID
 * @param cacheVersion - 缓存版本，默认 8
 * @returns 会话完整数据（含 chat_session 和 chat_messages），失败返回空数组
 * 
 * 响应结构示例：
 * {
 *   data: {
 *     biz_data: {
 *       chat_session: { id, current_message_id, version, updated_at, ... },
 *       chat_messages: [{ message_id, role, fragments: [{ type: "THINK"|"RESPONSE", content }], ... }],
 *       cache_control: "MERGE"
 *     }
 *   }
 * }
 */
async function fetchDeepSeekHistoryMessages(
  token: string,
  sessionId: string,
  cacheVersion: number = 8,
): Promise<any> {
  try {
    const headers: Record<string, string> = {
      'x-client-bundle-id': 'com.deepseek.chat',
      'x-client-platform': 'web',
      'x-client-version': '2.4.0',
      'x-client-locale': 'zh_CN',
      'x-client-timezone-offset': '28800',
      'Content-Type': 'application/json',
      Authorization: `Bearer ${token}`,
    };
    // 可选加入 x-settings-token（但示例中未使用，根据实际需要添加）
    // const settingsToken = backgroundState.getSettingsToken(PROVIDER_ID);
    // if (settingsToken) headers['x-settings-token'] = settingsToken;

    const url =
      `https://chat.deepseek.com/api/v0/chat/history_messages?chat_session_id=${encodeURIComponent(sessionId)}&cache_version=${cacheVersion}`;
    // 注意：示例中还有 cache_reset_at 参数，但该值通常从会话信息中获取，
    // 不提供时服务端可能使用默认值，暂不添加以保持简单。

    const res = await fetch(url, { method: 'GET', headers });
    const text = await res.text();

    if (!res.ok) {
      diag('error', 'deepseek', '获取会话历史消息失败', {
        status: res.status,
        body: text.slice(0, 300),
        sessionId,
      });
      return [];
    }

    let json: any;
    try {
      json = JSON.parse(text);
    } catch {
      diag('error', 'deepseek', '获取历史消息返回非 JSON', { body: text.slice(0, 300), sessionId });
      return [];
    }

    const messages = json?.data?.biz_data?.chat_messages ?? [];
    if (!Array.isArray(messages)) {
      diag('warn', 'deepseek', '历史消息数据格式异常', { sessionId, data: json?.data });
      return [];
    }

    diag('info', 'deepseek', '获取历史消息成功', {
      sessionId,
      count: messages.length,
    });
    deepseekSessionMap[sessionId]={
      current_message_id: json?.data?.biz_data?.chat_session?.current_message_id ?? 0
    }
    return json?.data?.biz_data;
  } catch (err) {
    diag('error', 'deepseek', '获取历史消息异常', { error: String(err), sessionId });
    return [];
  }
}

type DeepSeekFragment = { type: string; content: string };

/**
 * 创建 DeepSeek SSE 片段解析器（2026-08 网站实测协议）：
 * - event: ready / update_session / finish / done 等（忽略 event 行，只取 data 行）
 * - data: {"v":{"response":{"fragments":[{type:"THINK"|"TEXT",content,...},...]}}}  → 片段快照
 * - data: {"p":"response/fragments/<idx>/content","o":"APPEND","v":"<文本>"}         → 片段内容追加
 * - data: {"v":"<文本>"}                                                              → 追加到当前片段
 * 最终答案 = 所有非 THINK（思考）片段的拼接；无片段类型信息时退回全部内容。
 * 传入 onChunk 后，每处理一个 data 块都会把「新增的答案增量」实时回调（用于流式）。
 */
function createDeepSeekParser(
  onChunk?: (delta: string) => void,
): {
  processLine: (line: string) => void;
  answer: () => string;
} {
  // 按片段索引记录类型与内容（THINK=思考，TEXT/RESPONSE=答案）
  const fragments: DeepSeekFragment[] = [];
  // 每个片段已发送的内容长度
  const sentLength: number[] = [];

  const handleData = (jsonStr: string): void => {
    let obj: any;
    try {
      obj = JSON.parse(jsonStr);
    } catch {
      return;
    }
    if (!obj || typeof obj !== 'object') return;

    // 1) 快照：{"v":{"response":{"fragments":[...]}}} —— 记录/更新片段类型与内容
    const resp = obj?.v?.response;
    if (resp && Array.isArray(resp.fragments)) {
      resp.fragments.forEach((f: any, i: number) => {
        if (!fragments[i]) fragments[i] = { type: String(f?.type ?? ''), content: String(f?.content ?? '') };
        else {
          if (typeof f?.type === 'string') fragments[i].type = f.type;
          if (typeof f?.content === 'string') fragments[i].content = f.content;
        }
      });
      return;
    }

    const p = obj?.p;

    // 2) 新片段追加：{"p":"response/fragments","o":"APPEND","v":[fragment...]}
    //    网站用它在 THINK 之后追加 RESPONSE（答案）片段，必须注册进数组，否则答案会混入 THINK
    if (p === 'response/fragments' && obj?.o === 'APPEND' && Array.isArray(obj.v)) {
      for (const f of obj.v) {
        if (f && typeof f === 'object') {
          fragments.push({ type: String(f.type ?? ''), content: String(f.content ?? '') });
        }
      }
      return;
    }

    // 3) 片段内容追加：{"p":"response/fragments/<idx>/content","o":"APPEND"|"SET","v":"..."}
    //    仅当 p 精确指向 fragments/.../content 才处理，否则忽略（如 response/status、elapsed_secs 等）
    if (typeof p === 'string' && typeof obj.v === 'string') {
      const m = /^response\/fragments\/(-?\d+)\/content$/i.exec(p);
      if (m) {
        let idx = parseInt(m[1], 10);
        if (idx === -1) idx = fragments.length - 1;
        const op = String(obj.o ?? 'APPEND').toUpperCase();
        if (idx >= 0) {
          if (!fragments[idx]) fragments[idx] = { type: '', content: '' };
          fragments[idx].content = op === 'APPEND' ? fragments[idx].content + obj.v : obj.v;
        }
      }
      return;
    }

    // 4) 纯文本 v（无路径）：追加到最后一个片段（网站用它流式输出当前片段内容）
    if (typeof obj.v === 'string') {
      const idx = fragments.length - 1;
      if (idx >= 0) fragments[idx].content += obj.v;
      else fragments.push({ type: '', content: obj.v });
    }
  };

  const answer = (): string => {
    // 只有「整条流都没有类型信息」（老协议）才回退到全部内容；
    // 只要有类型信息（THINK/RESPONSE 等），就严格按「非 THINK」过滤。
    // 否则流式过程中会误把思考内容当答案输出（此时 a 为空，a||all 会错回退到 THINK）。
    const hasTyped = fragments.some((f) => f.type !== '');
    if (!hasTyped) return fragments.map((f) => f.content).join('');
    //return fragments.filter((f) => f.type !== 'THINK').map((f) => f.content).join('');
    
    const chunks:any={}
    fragments.forEach((f, i) => {
      if (!sentLength[i]) sentLength[i] = 0;
      const cur = f.content;
      if (cur.length > sentLength[i]) {
        const delta = cur.slice(sentLength[i]);
        chunks[f.type] = delta;
        sentLength[i] = cur.length;
      }
    })
   
    return chunks;
  };

  const processLine = (line: string): void => {
    const trimmed = line.trim();
    if (!trimmed) return;
    if (trimmed.startsWith('data:')) {
      const jsonStr = trimmed.slice(5).trim();
      if (jsonStr && jsonStr !== '[DONE]') {
        handleData(jsonStr);
        // 流式：每处理一个 data 块就推送新增的答案增量
        if (onChunk) {
          const cur = answer();
          if (Object.keys(cur).length > 0) {
            onChunk(JSON.stringify(cur));
          }
        }
      }
    }
    if (trimmed.startsWith('event:')) {
      const msg = trimmed.slice(6).trim();
      //todo: 可选处理 event 行，如 ready / update_session / finish / done 等
    }
  };

  return { processLine, answer };
}

/** 读取 DeepSeek SSE 响应流，逐行交给 processLine */
async function pumpDeepSeekBody(
  body: ReadableStream<Uint8Array> | null,
  processLine: (line: string) => void,
  onRaw?: (text: string) => void,
): Promise<void> {
  if (!body) throw new ProviderError('DeepSeek 返回了空响应流', 'deepseek_empty_stream');
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = '';
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    const text = decoder.decode(value, { stream: true });
    buffer += text;
    onRaw?.(text);
    let nl: number;
    while ((nl = buffer.indexOf('\n')) >= 0) {
      const line = buffer.slice(0, nl);
      buffer = buffer.slice(nl + 1);
      processLine(line);
    }
  }
  // 剩余内容（可能没有换行结尾）
  if (buffer.trim()) processLine(buffer);
}

/** 非流式读取：完整收集答案与原始文本 */
async function readDeepSeekStream(
  body: ReadableStream<Uint8Array> | null,
): Promise<{ content: string; raw: string }> {
  const parser = createDeepSeekParser();
  let raw = '';
  await pumpDeepSeekBody(body, parser.processLine, (t) => (raw += t));
  return { content: parser.answer(), raw };
}

/** 流式读取：解析过程中实时把新增的答案增量交给 onChunk，最后返回汇总 */
async function readDeepSeekStreamChunked(
  body: ReadableStream<Uint8Array> | null,
  onChunk: (delta: string) => void,
): Promise<{ content: string }> {
  const parser = createDeepSeekParser(onChunk);
  await pumpDeepSeekBody(body, parser.processLine);
  return { content: parser.answer() };
}

/** 只保留「站点自定义头」（x-*），丢弃浏览器禁止手动设置的系统头 */
function pickSafeHeaders(all: Record<string, string> | undefined): Record<string, string> {
  const safe: Record<string, string> = {};
  if (!all) return safe;
  for (const [k, v] of Object.entries(all)) {
    const key = k.toLowerCase();
    if (key.startsWith('x-')) safe[key] = v;
  }
  return safe;
}

/**
 * 解析要请求的 DeepSeek 地址：
 * - 只接受 https://chat.deepseek.com 域内的地址（host_permissions 已覆盖，避免跨域被拦）
 * - 去掉可能过期的 query/hash
 * - 捕获地址在其它域名 / 非法时，回退到默认已知端点
 */
function resolveDeepSeekUrl(templateUrl: string | undefined): string {
  if (templateUrl) {
    try {
      const u = new URL(templateUrl);
      if (u.protocol === 'https:' && u.hostname === 'chat.deepseek.com') {
        u.search = '';
        u.hash = '';
        return u.toString();
      }
      log.warn(`捕获到的接口地址不在 chat.deepseek.com（${templateUrl}），改用默认地址`);
    } catch {
      /* ignore */
    }
  }
  return DEEPSEEK_API_URL;
}

interface DeepSeekChatRequest {
  url: string;
  headers: Record<string, string>;
  body: Record<string, unknown>;
}

/**
 * 构造 DeepSeek 聊天请求。
 * 优先复用内容脚本从网页真实请求中捕获的「接口模板」（URL/请求头/body 结构），
 * 未捕获到模板时才回退到顶部默认常量 —— 实现自适配，无需人工配置。
 */
async function buildDeepSeekRequest(
  messages: ChatMessage[],
  options?: ChatOptions,
): Promise<DeepSeekChatRequest> {
  const template = backgroundState.getApiTemplate(PROVIDER_ID);
  const token = await getDeepSeekToken();

  const url = resolveDeepSeekUrl(template?.url);

  // 默认客户端头（静态值，实测来自网页真实请求）；捕获模板里已有的 x-* 会覆盖。
  // ⚠️ 不能直接复制网页全部请求头：host / origin / referer / cookie / content-length /
  // user-agent 等属于 forbidden headers，在 fetch 里手动设置会立刻抛 "Failed to fetch"。
  const headers: Record<string, string> = {
    'x-client-bundle-id': 'com.deepseek.chat',
    'x-client-platform': 'web',
    'x-client-version': '2.4.0',
    'x-client-locale': 'zh_CN',
    'x-client-timezone-offset': '28800',
    ...pickSafeHeaders(template?.headers),
  };
  headers['Content-Type'] = 'application/json';
  // 鉴权凭证在 localStorage.userToken，作为 Bearer 请求头发送（实测 cookie 无鉴权）
  if (token) headers['Authorization'] = `Bearer ${token}`;
  // x-settings-token（来自 localStorage.settingsJwt；缺失可能报 MISSING_HEADER）
  const settingsToken = backgroundState.getSettingsToken(PROVIDER_ID);
  if (settingsToken) headers['x-settings-token'] = settingsToken;
  // x-hif-leim / x-hif-dliq（DeepSeek 完成接口需要的会话级头，由内容脚本从真实请求捕获并持久化）
  const hif = backgroundState.getHifHeaders(PROVIDER_ID);
  if (hif?.leim) headers['x-hif-leim'] = hif.leim;
  if (hif?.dliq) headers['x-hif-dliq'] = hif.dliq;

  // 实测 /api/v0/chat/completion 的 schema 用单个 prompt 字符串（不是 messages 数组），
  // 多轮对话拼接成一段 prompt 发送；会话历史由服务端按 chat_session_id 维护。
  const tpl = template?.template as
    | {
        chat_session_id?: string;
        parent_message_id?: string | null;
        model_type?: string;
        thinking_enabled?: boolean;
        search_enabled?: boolean;
      }
    | undefined;

  // 会话 ID：固定会话(请求指定) > 模板捕获的网页当前会话 > 最近历史会话 > 自动新建（2026-08 网站实测）
  const chatSessionId =
    options?.sessionId ||
    tpl?.chat_session_id ||
    ((await fetchDeepSeekSessions(token ?? '')) ??
      ((await createDeepSeekSession(token ?? '', settingsToken ?? undefined)) ?? crypto.randomUUID()));
  if(chatSessionId)await fetchDeepSeekHistoryMessages(token ?? '',chatSessionId)
  // 2026-08 网站真实 body schema（实测）：无 agent_name/model_name/think_enabled/generation_settings
  const body: Record<string, unknown> = {
    chat_session_id: chatSessionId,
    parent_message_id: deepseekSessionMap[chatSessionId]?.current_message_id ?? null,
    model_type: tpl?.model_type ?? DEEPSEEK_MODEL_TYPE,
    prompt: buildPrompt(messages),
    ref_file_ids: [],
    thinking_enabled: tpl?.thinking_enabled ?? DEEPSEEK_THINK,
    search_enabled: tpl?.search_enabled ?? false,
    action: null,
    preempt: false,
  };

  return { url, headers, body };
}

/**
 * 执行一次 DeepSeek 聊天请求（含自愈循环 / 错误处理 / 流式读取的公共逻辑）。
 * onChunk 存在时按流式读取并逐块回调，否则收集完整答案。
 */
async function executeDeepSeekChat(
  url: string,
  headers: Record<string, string>,
  body: Record<string, unknown>,
  onChunk?: (delta: string) => void,
): Promise<string> {
  // 自愈循环：422 提示「缺 XX 字段」时自动补全并重试，插件自己完成 schema 学习
  const MAX_ATTEMPTS = 6;
  for (let attempt = 1; attempt <= MAX_ATTEMPTS; attempt++) {
    let res: Response;
    try {
      res = await fetch(url, {
        method: 'POST',
        // 鉴权走 Authorization 头（userToken），不走 cookie：避免跨域凭证请求触发 CORS 预检失败
        headers,
        body: JSON.stringify(body),
      });
    } catch (err) {
      const msg = err instanceof TypeError ? 'Failed to fetch（网络层被拦截，多为请求头或权限问题）' : String(err);
      log.error(`DeepSeek 网络请求失败 url=${url}`, err);
      diag('error', 'deepseek', '网络请求失败', { url, error: msg });
      throw new ProviderError(`DeepSeek 网络请求失败 (${url})：${msg}`, 'deepseek_network_error');
    }

    if (res.status === 401 || res.status === 403) {
      diag('warn', 'deepseek', `鉴权失败 HTTP ${res.status}`, { url });
      throw new ProviderError(
        'DeepSeek 登录状态失效或未登录：请先在浏览器打开 chat.deepseek.com 并登录',
        'deepseek_unauthorized',
      );
    }

    // 422：尝试解析「missing field `X`」自动补全
    if (res.status === 422) {
      const text = await res.text().catch(() => '');
      const missing = parseMissingField(text);
      if (missing && attempt < MAX_ATTEMPTS) {
        body[missing] = guessDefault(missing);
        diag('warn', 'deepseek', `自动补全字段 ${missing}=${JSON.stringify(body[missing])} 后重试(第${attempt + 1}次)`, {
          url,
          responseBody: text.slice(0, 300),
        });
        continue;
      }
      diag('error', 'deepseek', `HTTP 422 未能自动修复`, { url, responseBody: text.slice(0, 500) });
      throw new ProviderError(
        `DeepSeek API 请求失败 (422): ${text.slice(0, 300)}`,
        'deepseek_api_error',
      );
    }

    if (!res.ok) {
      const text = await res.text().catch(() => '');
      diag('error', 'deepseek', `HTTP ${res.status}`, { url, responseBody: text.slice(0, 500) });
      throw new ProviderError(
        `DeepSeek API 请求失败 (${res.status}): ${text.slice(0, 200)}`,
        'deepseek_api_error',
      );
    }

    // 成功：读取响应流（流式按块回调，非流式收集完整内容）
    const started = Date.now();
    let content: string;
    let raw = '';
    if (onChunk) {
      content = (await readDeepSeekStreamChunked(res.body, onChunk)).content;
    } else {
      ({ content, raw } = await readDeepSeekStream(res.body));
    }
    log.info(`DeepSeek API done in ${Date.now() - started}ms`);
    diag('info', 'deepseek', '请求完成', { url, elapsedMs: Date.now() - started, contentLength: content.length });
    if (!content || content.length < 20) {
      // 内容为空或过短：把原始响应 dump 出来，便于判断是报错还是被截断
      diag('warn', 'deepseek', content.length === 0 ? '响应为空，原始响应片段如下' : `响应过短(${content.length}字符)，原始响应片段如下`, {
        url,
        content,
        raw: raw.slice(0, 3000),
      });
    }
    if (!content) {
      throw new ProviderError(`DeepSeek 未返回内容（响应片段: ${raw.slice(0, 150)}）`, 'deepseek_empty_response');
    }
    try {
      content = JSON.stringify(content);
    } catch (error) {
      //
    }
    return content;
  }

  throw new ProviderError('DeepSeek 请求重试次数用尽', 'deepseek_retry_exhausted');
}

/** 构造请求 + 附加 PoW 头 + 上报诊断，返回可直接 fetch 的三元组 */
async function prepareDeepSeekRequest(
  messages: ChatMessage[],
  options: ChatOptions | undefined,
  streaming: boolean,
): Promise<{ url: string; headers: Record<string, string>; body: Record<string, unknown> }> {
  const { url, headers, body } = await buildDeepSeekRequest(messages, options);

  // DeepSeek 反爬 PoW：每次请求需携带 x-ds-pow-response（实时求解，无法复用）
  const token = backgroundState.getToken(PROVIDER_ID);
  if (token) {
    const settingsToken = backgroundState.getSettingsToken(PROVIDER_ID) ?? undefined;
    const pow = await solvePowAndBuildHeader(token, settingsToken);
    if (pow) headers['x-ds-pow-response'] = pow;
  }

  // 把实际请求完整上报（URL / 请求头 / body 结构），服务端终端可见
  diag('info', 'deepseek', streaming ? '发起流式请求' : '发起请求', {
    url,
    headers,
    body: { ...body, prompt: `[${String(body.prompt).length} 字符]` },
  });

  return { url, headers, body };
}

/**
 * DeepSeek 网页版适配器（API 转发模式）。
 * 复用浏览器已登录会话：从页面捕获访问凭证后，直接调用网页前端使用的后端接口，
 * 不依赖易变的 DOM 类名，天然支持流式。
 */
export const deepseekProvider: AIProvider = {
  id: PROVIDER_ID,
  name: 'DeepSeek',
  url: 'https://chat.deepseek.com',
  matches: ['https://chat.deepseek.com/*'],
  getModels: () => ['deepseek-web'],

  async open(): Promise<number> {
    // 用户手动“打开”平台时，激活并聚焦标签页
    return ensureTab('https://chat.deepseek.com', true);
  },

  async checkAuth(): Promise<boolean> {
    if (backgroundState.getToken(PROVIDER_ID)) return true;
    await requestTokenCapture();
    return !!backgroundState.getToken(PROVIDER_ID);
  },

  async chat(model: ModelProvider, messages: ChatMessage[], options?: ChatOptions): Promise<ChatResult> {
    const { url, headers, body } = await prepareDeepSeekRequest(messages, options, false);
    const content = await executeDeepSeekChat(url, headers, body);
    return { content, model, finishedReason: 'stop' };
  },

  async chatStream(
    model: ModelProvider,
    messages: ChatMessage[],
    onChunk: (delta: string) => void,
    options?: ChatOptions,
  ): Promise<ChatResult> {
    const { url, headers, body } = await prepareDeepSeekRequest(messages, options, true);
    const content = await executeDeepSeekChat(url, headers, body, onChunk);
    return { content, model, finishedReason: 'stop' };
  },
};
