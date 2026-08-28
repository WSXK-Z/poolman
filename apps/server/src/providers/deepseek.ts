/**
 * DeepSeek 网页版服务端实现（DeepSeekService）。
 *
 * 连接器化重构后，插件的 deepseek.ts 业务逻辑整体迁移到服务端：
 *   - 凭证从 CredentialStore 读取（内容脚本捕获 → CREDENTIAL_SYNC 上报）
 *   - 请求组装 / PoW 求解 / SSE 解析 / 422 自愈 / 会话管理 全部在此完成
 *   - 真正的 fetch 执行通过 bridge 的 EXECUTE_REQUEST 通道下发，
 *     由插件在网页 context 发起（规避 Node 直连反爬），原始流经 EXECUTE_CHUNK 回传
 *
 * 对外暴露 chat / chatStream，供 ChatHandler 在 model=deepseek-web 时调用。
 */
import type {
  CapturedChatApi,
  ChatMessage,
  ChatOptions,
  ChatResult,
  ExecuteResultPayload,
  ModelProvider,
  ToolCall,
} from '@ai-gateway/shared';
import { createLogger } from '@ai-gateway/utils';
import type { WebSocketBridge } from '../bridge/websocket.js';
import type { CredentialStore } from './credentials.js';
import { buildPowHeader, solvePow } from './deepseek-pow.js';
import {
  TOOL_PHASE1_GUIDE,
  TOOL_PHASE2_GUIDE,
  buildToolRememberText,
  clientAdapters,
  genericAdapter,
  getLatestToolResults,
} from './client-adapters.js';

const log = createLogger('server:deepseek');

const PROVIDER_ID: ModelProvider = 'deepseek-web';

/* ------------------------ DeepSeek 网页后端接口默认配置 ------------------------ */
/** 聊天完成接口（网页前端自己调用的内部接口，非官方开放 API） */
const DEEPSEEK_API_URL = 'https://chat.deepseek.com/api/v0/chat/completion';
/** 创建会话接口：body 传 {} 即可获得新会话 ID */
const DEEPSEEK_SESSION_URL = 'https://chat.deepseek.com/api/v0/chat_session/create';
/** 历史会话列表接口 */
const DEEPSEEK_SESSIONS_URL = 'https://chat.deepseek.com/api/v0/chat_session/fetch_page?lte_cursor.pinned=false';
/** 历史消息接口（获取 current_message_id 作为下轮的 parent_message_id） */
const DEEPSEEK_HISTORY_URL = 'https://chat.deepseek.com/api/v0/chat/history_messages';
/** 更新会话标题接口（新建会话后把 title 设为会话 id，便于识别网关创建的会话） */
const DEEPSEEK_TITLE_URL = 'https://chat.deepseek.com/api/v0/chat_session/update_title';
/** PoW 挑战接口 */
const POW_CHALLENGE_URL = 'https://chat.deepseek.com/api/v0/chat/create_pow_challenge';
/** 网页版 model_type（2026-08 网站实测为 'default'） */
const DEEPSEEK_MODEL_TYPE = 'default';
/** 思考模式（V4 网页默认开启，与 localStorage.thinkingEnabled 一致） */
const DEEPSEEK_THINK = true;
/** 422 自愈最大重试次数 */
const MAX_ATTEMPTS = 6;
/** 首次提问：单条 prompt 总长超过该值就拆分为多条独立请求（防止过大触发 DeepSeek 输入限制） */
const MAX_SINGLE_PROMPT_LEN = 12_000;
/** 拆分后单条「记住」段的最大长度（极端兜底，超过才截断，避免单条仍过大） */
const MAX_SEGMENT_LEN = 20_000;
/** 辅助接口（PoW/会话/历史）专用超时：这类请求应秒回，挂起时快速失败，避免拖死主流程 */
const AUX_TIMEOUT = 10_000;
/** 工具嗅探窗口：带 tools 时 RESPONSE 前多少字符内未出现工具标记则判定为普通回答（保持流式） */
const TOOL_SNIFF_LIMIT = 400;

/* ------------------------------ 辅助函数 ------------------------------ */

/** 只保留「站点自定义头」（x-*），丢弃浏览器禁止手动设置的系统头（host/origin/cookie 等）。 */
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
 * - 只接受 https://chat.deepseek.com 域内的地址（避免跨域被拦）
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
 * 解析 DeepSeek 在 HTTP 200 状态下的「业务错误」响应 JSON。
 * 例：{"code":0,"msg":"","data":{"biz_code":1,"biz_msg":"invalid chat session id","biz_data":null}}
 * 这类响应不是 SSE 内容，会被解析器忽略导致"空内容"误判，需提前识别为业务错误。
 * 正常聊天响应是 SSE（以 event:/data: 开头），此处返回 null。
 */
function parseBizError(body: string): { code: number; msg: string } | null {
  if (!body || !body.trim().startsWith('{')) return null;
  try {
    const j = JSON.parse(body);
    const code = Number(j?.data?.biz_code ?? j?.code ?? 0);
    const msg = String(j?.data?.biz_msg ?? j?.msg ?? '');
    if (code !== 0 && msg) return { code, msg };
  } catch {
    /* 不是 JSON，按内容解析 */
  }
  return null;
}

/** 毫秒级时间戳（toLocaleString + .毫秒），保证每次回答前缀唯一 */
function formatTsMs(): string {
  const now = new Date();
  return `${now.toLocaleString('zh-CN', { hour12: false })}.${String(now.getMilliseconds()).padStart(3, '0')}`;
}

/**
 * 从回答内容（纯文本或 JSON 字符串）里提取「会话: <会话id>」，供追问复用会话。
 * 每条回答返回前都会加上「回答时间: <时间戳>\n会话: <会话id>\n」前缀，
 * 追问时从历史 assistant 内容里解析出会话 id，无需哈希匹配。
 */
function extractSessionId(text: string | null | undefined): string | null {
  if (!text) return null;
  const m = /会话[:：]\s*([A-Za-z0-9][A-Za-z0-9-]{9,})/.exec(text);
  return m?.[1] ?? null;
}

/** 段超长截断：保留前部并在末尾标注省略（仅作为极端兜底，防止单条仍过大） */
function truncateSegment(segment: string, maxLen: number): string {
  if (segment.length <= maxLen) return segment;
  return `${segment.slice(0, maxLen)}\n\n…（内容过长已截断，省略 ${segment.length - maxLen} 字符）`;
}

/**
 * 从回答的 RESPONSE 文本里解析工具调用（支持多个 ```tool_call / ```json 代码块、
 * 裸 JSON 对象或数组），返回所有命中的工具调用；无则返回 null。
 */
function parseToolCallBlocks(
  text: string,
): { name: string; arguments: Record<string, unknown> }[] | null {
  if (!text) return null;
  const results: { name: string; arguments: Record<string, unknown> }[] = [];
  const push = (json: unknown): void => {
    const items = Array.isArray(json) ? json : [json];
    for (const item of items) {
      if (!item || typeof item !== 'object') continue;
      const obj = item as Record<string, unknown>;
      const name = String(obj?.name ?? obj?.function ?? '');
      const args = obj?.arguments ?? obj?.parameters ?? obj?.args;
      if (name) {
        results.push({
          name,
          arguments: args && typeof args === 'object' ? (args as Record<string, unknown>) : {},
        });
      }
    }
  };
  // 1) 所有 ```tool_call / ```json 代码块（支持一次输出多个）
  const blockRe = /```(?:tool_call|json)\s*([\s\S]*?)```/gi;
  let m: RegExpExecArray | null;
  while ((m = blockRe.exec(text)) !== null) {
    try {
      push(JSON.parse(m[1].trim()));
    } catch {
      /* 跳过无法解析的代码块 */
    }
  }
  // 2) 无代码块命中 → 尝试整段 JSON（对象或数组）
  if (results.length === 0) {
    const candidate = text.trim();
    if (candidate.startsWith('[') || candidate.startsWith('{')) {
      try {
        push(JSON.parse(candidate));
      } catch {
        const obj = /\{[\s\S]*\}/.exec(candidate);
        if (obj) {
          try {
            push(JSON.parse(obj[0]));
          } catch {
            /* ignore */
          }
        }
      }
    }
  }
  return results.length > 0 ? results : null;
}

/** 从回答内容（JSON 字符串 {THINK, RESPONSE} 或纯字符串）提取 RESPONSE 纯文本 */
function extractResponseText(content: string): string {
  try {
    const chunks = JSON.parse(content) as Record<string, string> | string;
    return typeof chunks === 'string' ? chunks : (chunks.RESPONSE ?? '');
  } catch {
    return content;
  }
}

type DeepSeekFragment = { type: string; content: string };

/**
 * 创建 DeepSeek SSE 片段解析器（2026-08 网站实测协议）：
 * - event: ready / update_session / finish / done 等（忽略 event 行，只取 data 行）
 * - data: {"v":{"response":{"fragments":[{type:"THINK"|"TEXT",content,...},...]}}}  → 片段快照
 * - data: {"p":"response/fragments/<idx>/content","o":"APPEND","v":"<文本>"}         → 片段内容追加
 * - data: {"v":"<文本>"}                                                              → 追加到当前片段
 * 传入 onChunk 后，每处理一个 data 块都会把「新增的答案增量」实时回调（用于流式）。
 */
function createDeepSeekParser(
  onChunk?: (delta: string) => void,
): {
  processLine: (line: string) => void;
  /** 返回「自本次解析以来的新增增量」（基于 sentLength），供流式逐块推送 */
  answer: () => unknown;
  /** 返回「完整内容」（忽略 sentLength，取全部片段内容），供流式结束后记录回答哈希 */
  fullAnswer: () => unknown;
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

  const answer = (): unknown => {
    // 只有「整条流都没有类型信息」（老协议）才回退到全部内容；
    // 只要有类型信息（THINK/RESPONSE 等），就严格按「非 THINK」过滤。
    // 否则流式过程中会误把思考内容当答案输出（此时 a 为空，a||all 会错回退到 THINK）。
    const hasTyped = fragments.some((f) => f.type !== '');
    if (!hasTyped) return fragments.map((f) => f.content).join('');

    // 按片段 type 分组返回「新增」增量对象（基于 sentLength 只返回新增部分）
    const chunks: Record<string, string> = {};
    fragments.forEach((f, i) => {
      if (!sentLength[i]) sentLength[i] = 0;
      const cur = f.content;
      if (cur.length > sentLength[i]) {
        const delta = cur.slice(sentLength[i]);
        chunks[f.type] = delta;
        sentLength[i] = cur.length;
      }
    });
    return chunks;
  };

  // 完整内容：忽略 sentLength，返回所有片段的完整文本（按 type 分组，过滤 THINK 之外为答案）。
  // 注意 answer() 在流式过程中反复调用、sentLength 持续推进，流式结束后再调 answer() 只返回
  // 最后一块增量（几乎为空），因此记录「回答哈希」时必须用 fullAnswer() 取完整回答。
  const fullAnswer = (): unknown => {
    const hasTyped = fragments.some((f) => f.type !== '');
    if (!hasTyped) return fragments.map((f) => f.content).join('');
    const chunks: Record<string, string> = {};
    fragments.forEach((f) => {
      if (f.content) chunks[f.type] = f.content;
    });
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
          const cur = answer() as Record<string, string>;
          if (Object.keys(cur).length > 0) {
            onChunk(JSON.stringify(cur));
          }
        }
      }
    }
    // event: 行可选处理（ready / update_session / finish / done 等）
  };

  return { processLine, answer, fullAnswer };
}

/* ------------------------------ DeepSeekService ------------------------------ */

export class DeepSeekService {
  /** 会话 -> 最新消息 ID 缓存（作为下一轮的 parent_message_id，保证对话连续性） */
  private sessionMap: Record<string, { current_message_id: number }> = {};
  /** 最近一次流式执行的原始文本累计（供空响应诊断） */
  private lastRawSnippet = '';
  /** 本地保存的会话 ID（解析不到历史会话时的兜底） */
  private currentSessionId: string | null = null;

  constructor(
    private readonly bridge: WebSocketBridge,
    private readonly credentials: CredentialStore,
    private readonly executeTimeoutMs = 120_000,
  ) {}

  /* ------------------------- 凭证与执行通道 ------------------------- */

  /** 确保持有访问凭证：有缓存直接用，否则下发 CAPTURE_REQUEST 让插件从页面捕获并等待。 */
  private async ensureToken(): Promise<string> {
    const cached = this.credentials.getToken(PROVIDER_ID);
    if (cached) return cached;
    try {
      await this.bridge.requestCredentials(PROVIDER_ID, 10_000);
    } catch {
      /* 超时或插件未响应，继续走下方检查 */
    }
    const token = this.credentials.getToken(PROVIDER_ID);
    if (!token) {
      throw Object.assign(
        new Error('未获取到 DeepSeek 访问凭证：请先在浏览器打开 chat.deepseek.com 并登录，且插件已连接'),
        { status: 502, code: 'deepseek_credentials_missing' },
      );
    }
    return token;
  }

  /** 通过插件在网页 context 执行一次请求（非流式），返回完整响应状态与体。 */
  private async executeOnce(
    req: {
      method?: string;
      url: string;
      headers: Record<string, string>;
      body?: string | null;
    },
    timeoutMs?: number,
  ): Promise<ExecuteResultPayload> {
    const res = await this.bridge.sendExecute(
      {
        provider: PROVIDER_ID,
        method: req.method,
        url: req.url,
        headers: req.headers,
        body: req.body,
      },
      timeoutMs ?? this.executeTimeoutMs,
    );
    // 辅助接口（PoW/会话/历史）仅在失败时记录响应片段，避免每请求刷屏
    if (req.url !== DEEPSEEK_API_URL && !res.ok) {
      log.warn(
        `辅助接口失败 ${req.method ?? 'POST'} ${req.url} -> ${res.status} body=${(res.body ?? '').slice(0, 200)}`,
      );
    }
    return res;
  }

  /** 通过插件在网页 context 执行流式请求：onRaw 实时收到原始文本块（可能切行）。 */
  private async executeStreaming(
    req: { method?: string; url: string; headers: Record<string, string>; body?: string | null },
    onRaw: (data: string) => void,
  ): Promise<ExecuteResultPayload> {
    // 累计原始文本（供空响应诊断），同时实时转发给调用方
    let raw = '';
    const res = await this.bridge.sendExecute(
      {
        provider: PROVIDER_ID,
        method: req.method,
        url: req.url,
        headers: req.headers,
        body: req.body,
        streaming: true,
      },
      this.executeTimeoutMs,
      (data) => {
        raw += data;
        onRaw(data);
      },
    );
    this.lastRawSnippet = raw;
    return res;
  }

  /** 把原始文本块按行切分喂给 parser（维护跨块的行 buffer，块边界可能切在行中间）。 */
  private pumpTextToParser(
    text: string,
    processLine: (line: string) => void,
    lineBuf: { v: string },
  ): void {
    lineBuf.v += text;
    let nl: number;
    while ((nl = lineBuf.v.indexOf('\n')) >= 0) {
      const line = lineBuf.v.slice(0, nl);
      lineBuf.v = lineBuf.v.slice(nl + 1);
      processLine(line);
    }
  }

  /** 流结束后把缓冲区剩余内容（可能没有换行结尾）交给 parser。 */
  private flushParser(lineBuf: { v: string }, processLine: (line: string) => void): void {
    if (lineBuf.v.trim()) processLine(lineBuf.v);
    lineBuf.v = '';
  }

  /* ------------------------- PoW（反爬） ------------------------- */

  /** 端到端：经执行通道拉取挑战 → 本地求解 → 组 x-ds-pow-response 头；失败返回 null。 */
  private async getPowHeader(token: string, settingsToken?: string): Promise<string | null> {
    // 尝试最多 2 次：首次挂起/失败时换新 challenge 重试（每次带短超时，避免拖死主流程）；
    // 仍失败则返回 null，调用方降级（不带 pow 头直接发 completion，若被拒会快速暴露）。
    for (let attempt = 1; attempt <= 2; attempt++) {
      try {
        const res = await this.executeOnce(
          {
            method: 'POST',
            url: POW_CHALLENGE_URL,
            headers: this.defaultHeaders(token, settingsToken),
            body: JSON.stringify({ target_path: '/api/v0/chat/completion' }),
          },
          AUX_TIMEOUT,
        );
        if (!res.ok || !res.body) {
          log.warn(`PoW 挑战获取失败(HTTP ${res.status}) 第${attempt}次`);
          continue;
        }
        let json: any;
        try {
          json = JSON.parse(res.body);
        } catch {
          log.warn('PoW 挑战响应非 JSON');
          continue;
        }
        // 兼容多种响应包裹：data.biz_data.challenge / data.challenge / challenge
        const ch = json?.data?.biz_data?.challenge ?? json?.data?.challenge ?? json?.challenge;
        if (!ch) {
          log.warn('PoW 挑战响应缺少 challenge 字段');
          continue;
        }
        const challenge = {
          algorithm: String(ch.algorithm ?? 'DeepSeekHashV1'),
          challenge: String(ch.challenge ?? ''),
          salt: String(ch.salt ?? ''),
          expireAt: Number(ch.expire_at ?? ch.expireAt ?? ch.expires_at ?? 0),
          difficulty: Number(ch.difficulty ?? 144000) || 144000,
          signature: String(ch.signature ?? ''),
          targetPath: String(ch.target_path ?? '/api/v0/chat/completion'),
        };
        const start = Date.now();
        const answer = solvePow(challenge.challenge, challenge.salt, challenge.expireAt, challenge.difficulty);
        log.debug(`PoW solved in ${Date.now() - start}ms difficulty=${challenge.difficulty}`);
        if (answer < 0) {
          log.warn('PoW 求解失败（difficulty 范围内未找到答案）');
          continue;
        }
        return buildPowHeader(challenge, answer);
      } catch (err) {
        log.warn(`PoW 流程失败（第 ${attempt} 次，超时${AUX_TIMEOUT}ms）`, err);
      }
    }
    return null;
  }

  /** 组装 DeepSeek 默认请求头（客户端标识 + Bearer 鉴权 + 可选 settingsToken）。 */
  private defaultHeaders(token: string, settingsToken?: string | null): Record<string, string> {
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
    return headers;
  }

  /* ------------------------- 会话管理 ------------------------- */

  /** 通过 /api/v0/chat_session/create 新建会话，返回会话 ID；失败返回 null。 */
  private async createSession(token: string, settingsToken?: string): Promise<string | null> {
    try {
      const res = await this.executeOnce(
        {
          method: 'POST',
          url: DEEPSEEK_SESSION_URL,
          headers: this.defaultHeaders(token, settingsToken),
          body: '{}',
        },
        AUX_TIMEOUT,
      );
      if (!res.ok || !res.body) return null;
      let json: any;
      try {
        json = JSON.parse(res.body);
      } catch {
        return null;
      }
      // 兼容多种响应结构，取第一个存在的会话 ID 字段（实测 data.biz_data.chat_session.id）
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
      const sessionId = candidates.find((v: unknown) => typeof v === 'string' && (v as string).length > 10) ?? null;
      log.debug('创建会话', { sessionId });
      return sessionId;
    } catch (err) {
      log.warn('创建会话异常', err);
      return null;
    }
  }

  /**
   * 把会话标题设置为会话 id（POST /api/v0/chat_session/update_title）。
   * 检查业务状态（HTTP 200 但 biz_code!=0 视为失败），失败重试一次；均失败仅告警，不影响主流程。
   */
  private async updateSessionTitle(
    token: string,
    settingsToken: string | undefined,
    sessionId: string,
  ): Promise<void> {
    for (let attempt = 1; attempt <= 2; attempt++) {
      try {
        const res = await this.executeOnce(
          {
            method: 'POST',
            url: DEEPSEEK_TITLE_URL,
            headers: this.defaultHeaders(token, settingsToken),
            body: JSON.stringify({ chat_session_id: sessionId, title: sessionId }),
          },
          AUX_TIMEOUT,
        );
        const body = (res.body ?? '').slice(0, 200);
        if (!res.ok) {
          log.warn(`设置会话标题失败(HTTP ${res.status}) session=${sessionId} body=${body}`);
          if (attempt < 2) {
            await new Promise((r) => setTimeout(r, 300));
            continue;
          }
          return;
        }
        // 业务状态：成功为 code=0 且 data.biz_code=0（HTTP 200 不代表真正生效）
        let bizOk = false;
        try {
          const j = JSON.parse(res.body ?? '');
          bizOk = Number(j?.code) === 0 && Number(j?.data?.biz_code) === 0;
        } catch {
          /* 非 JSON 响应，按 HTTP 成功处理 */
        }
        if (bizOk || !res.body) {
          log.info('已将会话标题设为会话 id', { sessionId });
          return;
        }
        // 业务失败：可能是会话刚创建未就绪，重试一次
        log.warn(`设置会话标题业务失败 session=${sessionId} body=${body}`);
        if (attempt < 2) {
          await new Promise((r) => setTimeout(r, 300));
          continue;
        }
        return;
      } catch (err) {
        log.warn(`设置会话标题异常（第 ${attempt} 次）`, { sessionId, error: String(err) });
        if (attempt < 2) {
          await new Promise((r) => setTimeout(r, 300));
          continue;
        }
      }
    }
  }

  /** 获取历史会话列表，返回最近的会话 ID（无则 null）。 */
  private async fetchSessions(token: string): Promise<string | null> {
    try {
      const res = await this.executeOnce(
        {
          method: 'GET',
          url: DEEPSEEK_SESSIONS_URL,
          headers: this.defaultHeaders(token),
        },
        AUX_TIMEOUT,
      );
      if (!res.ok || !res.body) return null;
      let json: any;
      try {
        json = JSON.parse(res.body);
      } catch {
        return null;
      }
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
          updatedAt: Number(it?.updated_at ?? it?.updatedAt ?? 0),
        }))
        .filter((s) => s.id.length > 10)
        .sort((a, b) => b.updatedAt - a.updatedAt);
      return sessions[0]?.id ?? null;
    } catch (err) {
      log.warn('获取历史会话异常', err);
      return null;
    }
  }

  /** 获取指定会话的历史消息，并把 current_message_id 写入 sessionMap（供下轮 parent_message_id）。 */
  private async fetchHistory(token: string, sessionId: string): Promise<void> {
    try {
      const url =
        `${DEEPSEEK_HISTORY_URL}?chat_session_id=${encodeURIComponent(sessionId)}&cache_version=8`;
      const res = await this.executeOnce(
        {
          method: 'GET',
          url,
          headers: this.defaultHeaders(token),
        },
        AUX_TIMEOUT,
      );
      if (!res.ok || !res.body) return;
      let json: any;
      try {
        json = JSON.parse(res.body);
      } catch {
        return;
      }
      const currentMessageId = json?.data?.biz_data?.chat_session?.current_message_id ?? 0;
      if (typeof currentMessageId === 'number') {
        this.sessionMap[sessionId] = { current_message_id: currentMessageId };
      }
    } catch (err) {
      log.warn('获取历史消息异常', { sessionId, error: String(err) });
    }
  }

  /**
   * 新建会话且已生成回答后，把标题设为会话 id。
   * 空会话（无任何消息）设置标题会被 DeepSeek 拒绝（biz_code=5 EMPTY_CHAT_SESSION），
   * 因此必须在首答成功后（会话已有消息）再调用。
   */
  private async titleIfNewSession(built: {
    isNewSession?: boolean;
    powCtx?: { token: string; settingsToken?: string | null };
    body: { chat_session_id?: unknown };
  }): Promise<void> {
    if (!built.isNewSession) return;
    await this.updateSessionTitle(
      built.powCtx?.token ?? '',
      built.powCtx?.settingsToken ?? undefined,
      String(built.body.chat_session_id ?? ''),
    );
  }

  /* ------------------------- 请求组装 ------------------------- */

  /**
   * 构造 DeepSeek 聊天请求。
   * 优先复用内容脚本捕获的「接口模板」（URL/请求头/body 结构），未捕获到模板时回退默认常量。
   */
  private async buildRequest(
    messages: ChatMessage[],
    options?: ChatOptions,
    opts?: { forceNewSession?: boolean },
  ): Promise<{
    url: string;
    headers: Record<string, string>;
    body: Record<string, unknown>;
    segments?: string[];
    powCtx?: { token: string; settingsToken?: string | null };
    /** 本次是否新建了会话（新建会话需在首答后设标题） */
    isNewSession: boolean;
  }> {
    const template: CapturedChatApi | null = this.credentials.getApiTemplate(PROVIDER_ID);
    const token = await this.ensureToken();
    const settingsToken = this.credentials.getSettingsToken(PROVIDER_ID);
    const url = resolveDeepSeekUrl(template?.url);

    // ⚠️ 不能直接复制网页全部请求头：host/origin/referer/cookie 等 forbidden headers 会触发
    // "Failed to fetch"。只保留 x-* 自定义头 + Content-Type + Bearer 鉴权。
    const headers: Record<string, string> = {
      'x-client-bundle-id': 'com.deepseek.chat',
      'x-client-platform': 'web',
      'x-client-version': '2.4.0',
      'x-client-locale': 'zh_CN',
      'x-client-timezone-offset': '28800',
      ...pickSafeHeaders(template?.headers),
    };
    headers['Content-Type'] = 'application/json';
    if (token) headers['Authorization'] = `Bearer ${token}`;
    if (settingsToken) headers['x-settings-token'] = settingsToken;
    const hif = this.credentials.getHifHeaders(PROVIDER_ID);
    if (hif?.leim) headers['x-hif-leim'] = hif.leim;
    if (hif?.dliq) headers['x-hif-dliq'] = hif.dliq;

    // DeepSeek 反爬 PoW：每次请求需携带 x-ds-pow-response（实时求解，无法复用）
    const pow = await this.getPowHeader(token, settingsToken ?? undefined);
    if (pow) headers['x-ds-pow-response'] = pow;

    const tpl = template?.template as
      | {
          chat_session_id?: string;
          parent_message_id?: string | null;
          model_type?: string;
          thinking_enabled?: boolean;
          search_enabled?: boolean;
        }
      | undefined;

    // ---------- 对话归属与会话选择 ----------
    // 首次提问（messages 无 assistant：结构为 [system, user环境, user提问]）→ 新建会话，
    //   并把 system+环境+提问 作为完整 prompt 发出（带「只需记住」前缀），让 DeepSeek 记住上下文。
    // 追问（messages 含 assistant 历史）→ 直接从历史最后一条 assistant 回答里解析「会话: <id>」
    //   复用会话：每条回答返回前都会加上「回答时间: <毫秒时间戳>\n会话: <会话id>\n」前缀，
    //   会话 id 随回答携带到客户端，追问时原样解析即可（无需哈希匹配）。
    // forceNewSession=true 时跳过匹配直接新建（由上层在会话失效时调用重试）。
    const assistantMsgs = messages.filter((m) => m.role === 'assistant');
    const isFirstTurn = assistantMsgs.length === 0;
    // 追问：从历史 assistant 回答里「从后往前」找第一个能解析出「会话: <id>」的。
    // 工具调用轮次的 assistant content 为 null（无前缀），会被跳过，回退到最近一次正常回答前缀。
    let historySessionId: string | null = null;
    if (!isFirstTurn) {
      for (let i = assistantMsgs.length - 1; i >= 0; i--) {
        const sid = extractSessionId(assistantMsgs[i].content);
        if (sid) {
          historySessionId = sid;
          break;
        }
      }
    }

    // 本次是否新建会话
    let isNewSession = false;
    let chatSessionId: string;
    if (options?.sessionId) {
      chatSessionId = options.sessionId;
    } else if (opts?.forceNewSession) {
      // 强制新建（会话失效重试）：跳过匹配
      chatSessionId = (await this.createSession(token, settingsToken ?? undefined)) ?? crypto.randomUUID();
      isNewSession = true;
    } else if (historySessionId) {
      // 追问：复用历史回答里携带的会话 id（客户端原样保留前缀即可命中）
      chatSessionId = historySessionId;
    } else if (!isFirstTurn && this.currentSessionId) {
      // 追问但解析不到历史前缀（如「首次提问直接触发工具调用」：工具调用轮的 assistant
      // content=null、无「回答时间/会话」前缀，下一轮从历史里找不到带前缀的 assistant）：
      // 用最近一次会话兜底，避免工具结果被拆分到新会话导致上下文断裂。
      chatSessionId = this.currentSessionId;
    } else {
      // 首次提问 / 解析不到会话 id：新建会话
      chatSessionId = (await this.createSession(token, settingsToken ?? undefined)) ?? crypto.randomUUID();
      isNewSession = true;
    }
    if (chatSessionId) await this.fetchHistory(token, chatSessionId);
    // 记录最近会话（解析失败时的兜底）
    this.currentSessionId = chatSessionId;

    // ---------- 适配器选择 ----------
    // 不同 IDE（VS Code Copilot / Trae 等）的 messages 结构不同，选择对应适配器把消息
    // 翻译成 DeepSeek 的 prompt 分段（只拆三大类：系统提示词整段 / 用户环境 / 用户提问）。
    const adapter = clientAdapters.find((a) => a.matches(messages)) ?? genericAdapter;
    const hasTools = !!options?.tools && options.tools.length > 0;

    // ---------- prompt 构造 ----------
    // 首次提问：适配器产出基础分段（系统提示词整段 + 环境 + 提问，带「只需记住」前缀），
    //   再把工具定义「记住」段插到提问段之前。
    // 追问：适配器产出当前轮 prompt（提问 + 当次工具结果），历史由 DeepSeek 会话维护。
    let prompt: string;
    let segments: string[] | undefined;
    if (isFirstTurn) {
      const base = adapter.buildFirstTurnSegments(messages);
      // 工具定义「记住」段（网关通用，不依赖 IDE）插入到提问段（最后一段）之前
      const segs =
        hasTools && base.length > 0
          ? [...base.slice(0, base.length - 1), buildToolRememberText(options.tools!), ...base.slice(-1)]
          : base;
      const joined = segs.join('\n');
      // 拆分为多条独立请求的条件：段数 > 1（如 system+环境+提问、或带工具段）且总长超过阈值 → 拆分防过大
      const shouldSplit =
        segs.length > 1 && (joined.length > MAX_SINGLE_PROMPT_LEN || segs.length > 3);
      if (shouldSplit) {
        // 拆分：返回段列表，由 executeBuilt 逐条构造请求并动态续接 parent
        segments = segs.map((s) => truncateSegment(s, MAX_SEGMENT_LEN));
        prompt = joined; // 占位（拆分时不真正发送单条），仅用于日志长度参考
      } else {
        prompt = truncateSegment(joined, MAX_SINGLE_PROMPT_LEN);
      }
    } else {
      prompt = adapter.buildFollowUpPrompt(messages);
    }

    // ---------- 工具（function calling）注入与阶段引导 ----------
    // 首次提问的工具定义已作为「记住」段插入；仅当「追问且新建会话」时（新会话没有首次的工具记忆）
    // 在提问前补上，保证新会话也有工具可用。
    if (hasTools) {
      if (!isFirstTurn && isNewSession) {
        prompt = `${buildToolRememberText(options.tools!)}\n\n${prompt}`;
      }
      // 阶段引导（两阶段决策，提高工具调用倾向）：
      //   阶段1：真实提问前先判断「是否需要调用工具」；
      //   阶段2：拿到当次工具结果后判断「能否直接回答 / 是否还需工具」，直到任务完成。
      const hasToolResults = getLatestToolResults(messages).length > 0;
      const guide = hasToolResults ? TOOL_PHASE2_GUIDE : TOOL_PHASE1_GUIDE;
      if (segments && segments.length > 0) {
        // 首次拆段：引导并入最后一段（提问段）之前
        const last = segments.length - 1;
        segments[last] = `${guide}\n\n${segments[last]}`;
      } else {
        prompt = `${guide}\n\n${prompt}`;
      }
    }

    // 实测 /api/v0/chat/completion 的 schema 用单个 prompt 字符串（不是 messages 数组）
    const body: Record<string, unknown> = {
      chat_session_id: chatSessionId,
      // 新建会话的第一条消息没有父消息（parent=null）；复用会话用上次的 current_message_id 续接
      parent_message_id: isNewSession ? null : (this.sessionMap[chatSessionId]?.current_message_id ?? null),
      model_type: tpl?.model_type ?? DEEPSEEK_MODEL_TYPE,
      prompt,
      ref_file_ids: [],
      thinking_enabled: tpl?.thinking_enabled ?? DEEPSEEK_THINK,
      search_enabled: tpl?.search_enabled ?? false,
      action: null,
      preempt: false,
    };

    if (segments) {
      return {
        url,
        headers,
        body,
        segments,
        powCtx: { token, settingsToken: settingsToken ?? null },
        isNewSession,
      };
    }
    return { url, headers, body, isNewSession };
  }

  /* ------------------------- 执行 + 422 自愈 ------------------------- */

  /**
   * 执行一次 DeepSeek 聊天请求（含自愈循环 / 错误处理 / 流式读取的公共逻辑）。
   * onChunk 存在时按流式读取并逐块回调，否则收集完整答案。
   */
  private async executeWithRetry(
    url: string,
    headers: Record<string, string>,
    body: Record<string, unknown>,
    onChunk?: (delta: string) => void,
  ): Promise<string> {
    // 422 自愈时会修改 currentBody 的属性（补字段），但引用本身不变，用 const
    const currentBody: Record<string, unknown> = { ...body };
    // 行缓冲（跨 EXECUTE_CHUNK 块边界，可能切在行中间）
    const lineBuf = { v: '' };

    for (let attempt = 1; attempt <= MAX_ATTEMPTS; attempt++) {
      lineBuf.v = '';
      const bodyStr = JSON.stringify(currentBody);
      const req = { method: 'POST' as const, url, headers, body: bodyStr };
      let res: ExecuteResultPayload;
      let parser: ReturnType<typeof createDeepSeekParser> | null = null;

      if (onChunk) {
        // 流式：解析器带 onChunk，每个 data 块实时产出增量
        parser = createDeepSeekParser(onChunk);
        res = await this.executeStreaming(req, (data) => this.pumpTextToParser(data, parser!.processLine, lineBuf));
        this.flushParser(lineBuf, parser.processLine);
      } else {
        // 非流式：完整响应体一次喂给解析器
        res = await this.executeOnce(req);
        parser = createDeepSeekParser();
        this.pumpTextToParser(res.body ?? '', parser.processLine, lineBuf);
        this.flushParser(lineBuf, parser.processLine);
      }

      // 鉴权失败
      if (res.status === 401 || res.status === 403) {
        log.warn(`鉴权失败 HTTP ${res.status}`);
        throw Object.assign(
          new Error('DeepSeek 登录状态失效或未登录：请先在浏览器打开 chat.deepseek.com 并登录'),
          { status: 502, code: 'deepseek_unauthorized' },
        );
      }

      // 422：尝试解析「missing field `X`」自动补全并重试（服务端完成 schema 学习）
      if (res.status === 422) {
        const text = res.body ?? '';
        const missing = parseMissingField(text);
        if (missing && attempt < MAX_ATTEMPTS) {
          currentBody[missing] = guessDefault(missing);
          log.warn(`自动补全字段 ${missing}=${JSON.stringify(currentBody[missing])} 后重试(第${attempt + 1}次)`, {
            responseBody: text.slice(0, 300),
          });
          continue;
        }
        throw Object.assign(
          new Error(`DeepSeek API 请求失败 (422): ${text.slice(0, 300)}`),
          { status: 502, code: 'deepseek_api_error' },
        );
      }

      if (!res.ok) {
        const text = res.body ?? '';
        throw Object.assign(
          new Error(`DeepSeek API 请求失败 (${res.status}): ${text.slice(0, 200)}`),
          { status: 502, code: 'deepseek_api_error' },
        );
      }

      // 200 但响应体是业务错误 JSON（如 invalid chat session id）：提前识别，避免误判为"空内容"
      const bizError = parseBizError(onChunk ? this.lastRawSnippet : (res.body ?? ''));
      if (bizError) {
        log.warn(`DeepSeek 业务错误：${bizError.msg}`);
        throw Object.assign(new Error(`DeepSeek 业务错误: ${bizError.msg}`), {
          status: 502,
          code: 'deepseek_biz_error',
          bizMsg: bizError.msg,
        });
      }

      // 成功：取解析结果（对象含 THINK/RESPONSE 内容，或老协议的无类型拼接字符串）。
      // 流式时 answer() 返回的是「最后一块增量」（sentLength 已推进），不完整；
      // 客户端已通过 onChunk 收到全部增量，这里的返回值仅用于记录回答哈希 → 用 fullAnswer()。
      const content = onChunk ? parser.fullAnswer() : parser.answer();
      if (!content || (typeof content === 'string' && content.length === 0)) {
        // 解析为空：dump 原始响应与状态，便于定位（真实响应可能是空体 / 错误 JSON / 未知格式）
        const rawSnippet = onChunk ? this.lastRawSnippet : (res.body ?? '');
        log.error('DeepSeek 解析结果为空（deepseek_empty_response）', {
          status: res.status,
          ok: res.ok,
          rawLength: rawSnippet.length,
          raw: rawSnippet.slice(0, 2000),
        });
        throw Object.assign(
          new Error(
            `DeepSeek 未返回内容（status=${res.status}，原始响应: ${rawSnippet.slice(0, 200)}）`,
          ),
          { status: 502, code: 'deepseek_empty_response' },
        );
      }
      return JSON.stringify(content);
    }

    throw Object.assign(new Error('DeepSeek 请求重试次数用尽'), {
      status: 502,
      code: 'deepseek_retry_exhausted',
    });
  }

  /**
   * 执行 buildRequest 的结果：
   * - 无 segments：单请求直接执行（含 422 自愈 / 流式回调 / 业务错误识别）
   * - 有 segments：逐段构造独立请求（同一会话内依次续接 parent，每条独立求解 PoW）发送，
   *   前 N-1 段只让 DeepSeek "记住"（响应丢弃），最后一段的响应返回。
   */
  private async executeBuilt(
    built: {
      url: string;
      headers: Record<string, string>;
      body: Record<string, unknown>;
      segments?: string[];
      powCtx?: { token: string; settingsToken?: string | null };
      isNewSession?: boolean;
    },
    onChunk?: (delta: string) => void,
  ): Promise<string> {
    // 单请求：直接执行
    if (!built.segments || built.segments.length <= 1) {
      log.info('发起请求', {
        url: built.url,
        chat_session_id: built.body.chat_session_id,
        model_type: built.body.model_type,
        promptLen: String(built.body.prompt).length,
      });
      return this.executeWithRetry(built.url, built.headers, built.body, onChunk);
    }

    // 拆分请求：逐段构造并执行，最后一段返回内容
    const sessionId = String(built.body.chat_session_id ?? '');
    const { token, settingsToken } = built.powCtx ?? {};
    let content = '';
    const tips=[
      "创建会话中...\n",
      "手忙脚乱中...\n",
      "灵感激发中...\n",
      "思路整理中...\n",
      "上下文加载中...\n",
    ]
    for (let i = 0; i < built.segments.length; i++) {
      const isLast = i === built.segments.length - 1;
      // 非最后一段时，推送思考状态
      if (!isLast && onChunk) {
        const thinkMessage =tips[i % tips.length] ;
        onChunk(JSON.stringify({ THINK: thinkMessage }));
      }
      // 每条请求独立求解 PoW（x-ds-pow-response 无法复用）
      const headers = { ...built.headers };
      if (token) {
        const pow = await this.getPowHeader(token, settingsToken ?? undefined);
        if (pow) headers['x-ds-pow-response'] = pow;
      }
      // 同会话续接：首段 parent=null（新会话），后续用执行后最新的 current_message_id
      const parentMessageId =
        i === 0 ? null : (this.sessionMap[sessionId]?.current_message_id ?? null);
      const body: Record<string, unknown> = {
        ...built.body,
        prompt: built.segments[i],
        parent_message_id: parentMessageId,
      };
      log.info(isLast ? '发起请求' : '发送上下文(记住)', {
        url: built.url,
        chat_session_id: sessionId,
        model_type: body.model_type,
        promptLen: String(body.prompt).length,
        seq: `${i + 1}/${built.segments.length}`,
      });
      content = await this.executeWithRetry(built.url, headers, body, isLast ? onChunk : undefined);
      // 更新会话内 current_message_id，供下一条续接
      if (token) await this.fetchHistory(token, sessionId);
    }
    return content;
  }

  /**
   * 给回答内容（JSON 字符串 {THINK, RESPONSE}）的 RESPONSE 前加前缀：
   * 「回答时间: <毫秒时间戳>\n会话: <会话id>\n」。纯字符串时整体加前缀。
   * 会话 id 直接写进回答，追问时从历史 assistant 内容里解析出来复用，无需哈希匹配。
   */
  private attachAnswerPrefix(content: string, ts: string, sessionId: string): string {
    const prefix = `回答时间: ${ts}\n会话: ${sessionId}\n`;
    try {
      const chunks = JSON.parse(content) as Record<string, string> | string;
      if (typeof chunks === 'string') return `${prefix}${chunks}`;
      if (typeof chunks.RESPONSE === 'string') {
        return JSON.stringify({ ...chunks, RESPONSE: `${prefix}${chunks.RESPONSE}` });
      }
      return content;
    } catch {
      return `${prefix}${content}`;
    }
  }

  /**
   * 解析回答内容中的工具调用；命中则返回 OpenAI 标准 tool_calls（可多个），否则 null。
   * 仅在请求带 tools 时启用（方案 A：prompt 注入模拟 function calling）。
   */
  private parseToolCall(content: string, options?: ChatOptions): ToolCall[] | null {
    if (!options?.tools?.length) return null;
    const calls = parseToolCallBlocks(extractResponseText(content));
    if (!calls || calls.length === 0) return null;
    // 诊断：对照工具定义检查缺失的必填参数（DeepSeek 网页版无原生约束，可能漏填必填字段）
    for (const c of calls) {
      const def = options.tools.find((t) => t?.function?.name === c.name);
      const required =
        (def?.function?.parameters as { required?: string[] } | undefined)?.required ?? [];
      if (required.length > 0) {
        const missing = required.filter((k) => !(k in c.arguments));
        if (missing.length > 0) {
          log.warn(`工具调用缺少必填参数 name=${c.name} missing=[${missing.join(', ')}]`, {
            arguments: JSON.stringify(c.arguments),
          });
        }
      }
    }
    const base = Date.now().toString(36);
    return calls.map((c, i) => ({
      id: `call_${base}_${i}`,
      type: 'function',
      function: { name: c.name, arguments: JSON.stringify(c.arguments) },
    }));
  }

  /* ------------------------- 对外接口（ChatHandler 调用） ------------------------- */

  /** 非流式聊天 */
  async chat(model: ModelProvider, messages: ChatMessage[], options?: ChatOptions): Promise<ChatResult> {
    const built = await this.buildRequest(messages, options);
    const ts = formatTsMs();
    const sessionId = String(built.body.chat_session_id ?? '');
    try {
      const content = await this.executeBuilt(built);
      // 新建会话：回答已生成（会话有内容），再设置标题为会话 id（空会话会被 EMPTY_CHAT_SESSION 拒绝）
      await this.titleIfNewSession(built);
      // 工具调用：回答命中工具调用格式则转成 OpenAI 标准 tool_calls 返回（content 置空）
      const toolCalls = this.parseToolCall(content, options);
      if (toolCalls) {
        log.info('检测到工具调用', { names: toolCalls.map((t) => t.function.name) });
        return { content: undefined, tool_calls: toolCalls, model, finishedReason: 'tool_calls' };
      }
      // 返回前加「回答时间 + 会话」前缀（会话 id 随回答携带，供追问解析复用）
      const finalContent = this.attachAnswerPrefix(content, ts, sessionId);
      return { content: finalContent, model, finishedReason: 'stop' };
    } catch (err) {
      // 会话失效（invalid chat session id）：本地/历史会话已过期，强制新建会话重试一次
      if ((err as { bizMsg?: string })?.bizMsg === 'invalid chat session id') {
        log.warn('会话失效（invalid chat session id），强制新建会话重试');
        // 清除本地保存的失效会话 + 模板中的失效会话 ID，避免后续请求继续复用无效会话
        this.currentSessionId = null;
        const api = this.credentials.getApiTemplate(PROVIDER_ID);
        if (api?.template?.chat_session_id) {
          this.credentials.upsert(PROVIDER_ID, {
            apiTemplate: { ...api, template: { ...api.template, chat_session_id: undefined } },
          });
        }
        const rebuilt = await this.buildRequest(messages, options, { forceNewSession: true });
        const content = await this.executeBuilt(rebuilt);
        await this.titleIfNewSession(rebuilt);
        const toolCalls = this.parseToolCall(content, options);
        if (toolCalls) {
          log.info('检测到工具调用', { names: toolCalls.map((t) => t.function.name) });
          return { content: undefined, tool_calls: toolCalls, model, finishedReason: 'tool_calls' };
        }
        const finalContent = this.attachAnswerPrefix(content, ts, String(rebuilt.body.chat_session_id ?? ''));
        return { content: finalContent, model, finishedReason: 'stop' };
      }
      throw err;
    }
  }

  /**
   * 流式收尾：带 tools 时按嗅探模式决定输出。
   * - tool 模式：解析缓冲的完整 RESPONSE → 命中工具调用则发 tool_calls delta，否则补发普通回答
   * - sniff 阶段流结束（回答很短）：按普通回答补发
   * - stream 模式：内容已实时输出，无需处理
   */
  private finishStream(
    model: ModelProvider,
    content: string,
    prefix: string,
    options: ChatOptions | undefined,
    onChunk: (delta: string) => void,
    toolState: { mode: 'sniff' | 'tool' | 'stream'; sniffBuf: string; respBuf: string[] },
  ): ChatResult {
    if (options?.tools?.length) {
      if (toolState.mode === 'tool') {
        const responseText = toolState.respBuf.join('');
        const toolCalls = this.parseToolCall(responseText, options);
        if (toolCalls) {
          log.info('检测到工具调用', { names: toolCalls.map((t) => t.function.name) });
          onChunk(JSON.stringify({ tool_calls: toolCalls }));
          return { content: undefined, tool_calls: toolCalls, model, finishedReason: 'tool_calls' };
        }
        // 工具模式但解析失败（JSON 不完整等）：补发为普通回答（带前缀）
        if (responseText) onChunk(JSON.stringify({ RESPONSE: `${prefix}${responseText}` }));
        return { content, model, finishedReason: 'stop' };
      }
      // sniff 阶段流就结束（回答很短）：按普通回答补发（带前缀）
      if (toolState.sniffBuf) {
        onChunk(JSON.stringify({ RESPONSE: `${prefix}${toolState.sniffBuf}` }));
      }
    }
    return { content, model, finishedReason: 'stop' };
  }

  /** 流式聊天：onChunk 逐块收到增量（JSON 字符串 {THINK, RESPONSE}） */
  async chatStream(
    model: ModelProvider,
    messages: ChatMessage[],
    onChunk: (delta: string) => void,
    options?: ChatOptions,
  ): Promise<ChatResult> {
    const built = await this.buildRequest(messages, options);
    const ts = formatTsMs();
    const sessionId = String(built.body.chat_session_id ?? '');
    // 前缀：回答时间 + 会话 id（供追问复用会话）
    const prefix = `回答时间: ${ts}\n会话: ${sessionId}\n`;
    const hasTools = !!options?.tools && options.tools.length > 0;
    let prefixSent = false;
    // 工具模式决策：带 tools 时先「嗅探」RESPONSE 开头，判断是否工具调用。
    //   - 命中工具标记（```tool_call/```json 或 {"name":）→ 转工具缓冲模式，流结束后解析 tool_calls；
    //   - 否则（普通回答，如 AI 结合工具结果回答）→ 补发已缓冲内容后转实时流式，保持逐字输出。
    // 这样第二轮（带 tool 结果的正常回答）也能流式，而不是整体缓冲到最后一次性输出。
    const toolState: { mode: 'sniff' | 'tool' | 'stream'; sniffBuf: string; respBuf: string[] } = {
      mode: hasTools ? 'sniff' : 'stream',
      sniffBuf: '',
      respBuf: [],
    };
    const wrappedOnChunk = (delta: string): void => {
      let chunks: Record<string, string>;
      try {
        chunks = JSON.parse(delta) as Record<string, string>;
      } catch {
        /* 非 JSON 块，直接透传 */
        onChunk(delta);
        return;
      }
      // THINK 思考始终实时转发（保持流式）
      if (chunks.THINK) onChunk(JSON.stringify({ THINK: chunks.THINK }));
      const resp = chunks.RESPONSE;
      if (!resp) return;
      if (toolState.mode === 'stream') {
        // 已判定普通回答：实时流式输出（首个块带前缀）
        onChunk(JSON.stringify({ RESPONSE: (prefixSent ? '' : prefix) + resp }));
        prefixSent = true;
        return;
      }
      if (toolState.mode === 'tool') {
        // 工具缓冲模式：累积完整 RESPONSE，流结束后解析
        toolState.respBuf.push(resp);
        return;
      }
      // sniff：累积并判断
      toolState.sniffBuf += resp;
      if (
        /```tool_call|```json/i.test(toolState.sniffBuf) ||
        /^\s*\{\s*"name"\s*:/.test(toolState.sniffBuf)
      ) {
        toolState.mode = 'tool';
        toolState.respBuf.push(toolState.sniffBuf);
        toolState.sniffBuf = '';
        return;
      }
      if (toolState.sniffBuf.length >= TOOL_SNIFF_LIMIT) {
        // 前 N 字符无工具标记 → 普通回答：补发已缓冲内容，转实时流式
        toolState.mode = 'stream';
        onChunk(JSON.stringify({ RESPONSE: (prefixSent ? '' : prefix) + toolState.sniffBuf }));
        prefixSent = true;
        toolState.sniffBuf = '';
      }
    };
    try {
      const content = await this.executeBuilt(built, wrappedOnChunk);
      // 新建会话：回答已生成（会话有内容），再设置标题为会话 id（空会话会被 EMPTY_CHAT_SESSION 拒绝）
      await this.titleIfNewSession(built);
      return this.finishStream(model, content, prefix, options, onChunk, toolState);
    } catch (err) {
      // 会话失效（invalid chat session id）：本地/历史会话已过期，强制新建会话重试一次
      if ((err as { bizMsg?: string })?.bizMsg === 'invalid chat session id') {
        log.warn('会话失效（invalid chat session id），强制新建会话重试');
        // 清除本地保存的失效会话 + 模板中的失效会话 ID，避免后续请求继续复用无效会话
        this.currentSessionId = null;
        const api = this.credentials.getApiTemplate(PROVIDER_ID);
        if (api?.template?.chat_session_id) {
          this.credentials.upsert(PROVIDER_ID, {
            apiTemplate: { ...api, template: { ...api.template, chat_session_id: undefined } },
          });
        }
        // 重置工具嗅探状态（首轮可能已累积部分响应）
        toolState.mode = hasTools ? 'sniff' : 'stream';
        toolState.sniffBuf = '';
        toolState.respBuf.length = 0;
        const rebuilt = await this.buildRequest(messages, options, { forceNewSession: true });
        const content = await this.executeBuilt(rebuilt, wrappedOnChunk);
        await this.titleIfNewSession(rebuilt);
        return this.finishStream(model, content, prefix, options, onChunk, toolState);
      }
      throw err;
    }
  }
}
