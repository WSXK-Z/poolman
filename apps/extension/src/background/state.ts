import type { ModelProvider, ProviderStatusMap } from '@ai-gateway/shared';
import { randomId } from '@ai-gateway/utils';
/**
 * 扩展后台共享状态及其持久化边界。
 *
 * 页面凭证和接口模板保存到扩展本地存储，连接状态与诊断日志只驻留内存；
 * UI 通过 snapshot 读取，不直接访问本类内部可变集合。
 */
import { browser } from 'wxt/browser';
import {
  DEFAULT_SETTINGS,
  type AppSettings,
  type CapturedChatApi,
  type ExtensionState,
  type LogEntry,
} from '~/shared/constants';

/** 持久化存储 key（避免 Service Worker 重启 / 扩展重载后丢失捕获结果） */
const STORE_KEYS = {
  id: '__ai_gateway_id',
  tokens: '__ai_gateway_tokens',
  settingsTokens: '__ai_gateway_settings_tokens',
  templates: '__ai_gateway_templates',
  hif: '__ai_gateway_hif',
} as const;

/**
 * Background 全局状态（模块级单例）。
 * Service Worker 中所有模块共享此对象。
 * 捕获的 token / settingsToken / 接口模板会持久化到 browser.storage.local，
 * 扩展重载后自动恢复，无需反复捕获。
 */
class BackgroundState {
  id: string | null = null;
  settings: AppSettings = { ...DEFAULT_SETTINGS };
  connected = false;
  providers: ProviderStatusMap = {};
  /** 各平台访问凭证 */
  private tokens: Partial<Record<ModelProvider, string>> = {};
  /** 各平台 settingsToken（x-settings-token 请求头用，如 localStorage.settingsJwt） */
  private settingsTokens: Partial<Record<ModelProvider, string>> = {};
  /** 各平台聊天接口模板（由内容脚本拦截网页真实请求获得，自适配） */
  private apiTemplates: Partial<Record<ModelProvider, CapturedChatApi>> = {};
  /** 各平台 x-hif-leim / x-hif-dliq（DeepSeek 完成接口需要的会话级头，独立持久化避免模板污染丢失） */
  private hifHeaders: Partial<Record<ModelProvider, { leim?: string; dliq?: string }>> = {};
  private logs: LogEntry[] = [];
  private readonly maxLogs = 200;

  private listeners = new Set<() => void>();

  /** 从 storage.local 恢复捕获结果（扩展启动时调用） */
  async loadPersisted(): Promise<void> {
    try {
      const stored = await browser.storage.local.get([
        STORE_KEYS.id,
        STORE_KEYS.tokens,
        STORE_KEYS.settingsTokens,
        STORE_KEYS.templates,
        STORE_KEYS.hif,
      ]);
      const tokens = stored[STORE_KEYS.tokens] as Partial<Record<ModelProvider, string>> | undefined;
      const settingsTokens = stored[STORE_KEYS.settingsTokens] as Partial<Record<ModelProvider, string>> | undefined;
      const templates = stored[STORE_KEYS.templates] as Partial<Record<ModelProvider, CapturedChatApi>> | undefined;
      const hif = stored[STORE_KEYS.hif] as Partial<Record<ModelProvider, { leim?: string; dliq?: string }>> | undefined;
      if (tokens) this.tokens = { ...this.tokens, ...tokens };
      if (settingsTokens) this.settingsTokens = { ...this.settingsTokens, ...settingsTokens };
      if (templates) this.apiTemplates = { ...this.apiTemplates, ...templates };
      if (hif) this.hifHeaders = { ...this.hifHeaders, ...hif };
      // 用持久化的凭证 / 模板先填一份「已连接」状态：Service Worker 或扩展刚重启时，
      // 页面还没来得及回报，空状态会让桌面端判定「没有可分配的插件」而拒绝刚进来的请求。
      // 真实的登录校验随后由 refreshOneProviderStatus 覆盖。
      const persistedProviders = new Set<ModelProvider>([
        ...(Object.keys(this.tokens) as ModelProvider[]),
        ...(Object.keys(this.apiTemplates) as ModelProvider[]),
      ]);
      for (const provider of persistedProviders) {
        this.setProviderStatus(provider, {
          connected: true,
          authed: true,
          tokenCaptured: !!this.tokens[provider],
          templateCaptured: !!this.apiTemplates[provider],
        });
      }
      if (!this.id) this.saveId(randomId(16));
    } catch {
      /* ignore */
    }
  }

  /** 保存扩展唯一 ID（用于服务端识别插件实例） */
  saveId(id: string): void {
    this.id = id;
    browser.storage.local.set({ [STORE_KEYS.id]: id }).catch(() => { });
  }

  /** 保存平台访问凭证 */
  setToken(provider: ModelProvider, token: string): void {
    this.tokens[provider] = token;
    this.notify();
    void browser.storage.local.set({ [STORE_KEYS.tokens]: this.tokens }).catch(() => { });
  }

  /** 读取平台访问凭证 */
  getToken(provider: ModelProvider): string | undefined {
    return this.tokens[provider];
  }

  /** 保存平台 settingsToken */
  setSettingsToken(provider: ModelProvider, token: string): void {
    this.settingsTokens[provider] = token;
    void browser.storage.local.set({ [STORE_KEYS.settingsTokens]: this.settingsTokens }).catch(() => { });
  }

  /** 读取平台 settingsToken */
  getSettingsToken(provider: ModelProvider): string | undefined {
    return this.settingsTokens[provider];
  }

  /** 保存平台聊天接口模板 */
  setApiTemplate(provider: ModelProvider, api: CapturedChatApi): void {
    // 模板随 provider 整体持久化，避免 Service Worker 休眠后丢失网页观察结果。
    this.apiTemplates[provider] = api;
    this.notify();
    void browser.storage.local.set({ [STORE_KEYS.templates]: this.apiTemplates }).catch(() => { });
  }

  /** 读取平台聊天接口模板 */
  getApiTemplate(provider: ModelProvider): CapturedChatApi | undefined {
    return this.apiTemplates[provider];
  }

  /** 保存平台 x-hif-leim / x-hif-dliq */
  setHifHeaders(provider: ModelProvider, hif: { leim?: string; dliq?: string }): void {
    this.hifHeaders[provider] = { ...this.hifHeaders[provider], ...hif };
    void browser.storage.local.set({ [STORE_KEYS.hif]: this.hifHeaders }).catch(() => { });
  }

  /** 读取平台 x-hif-leim / x-hif-dliq */
  getHifHeaders(provider: ModelProvider): { leim?: string; dliq?: string } | undefined {
    return this.hifHeaders[provider];
  }

  /**
   * 返回所有已捕获凭证（供 WS 连接/重连后全量 CREDENTIAL_SYNC 同步给服务端）。
   * 连接器化后，插件本地只保留这份副本供 UI 展示与重连重发，决策与使用都在服务端。
   */
  getAllCredentials(): Array<{
    provider: ModelProvider;
    token?: string;
    settingsToken?: string;
    hifHeaders?: { leim?: string; dliq?: string };
    apiTemplate?: CapturedChatApi;
  }> {
    const providers = new Set<ModelProvider>([
      ...(Object.keys(this.tokens) as ModelProvider[]),
      ...(Object.keys(this.settingsTokens) as ModelProvider[]),
      ...(Object.keys(this.hifHeaders) as ModelProvider[]),
      ...(Object.keys(this.apiTemplates) as ModelProvider[]),
    ]);
    const out: Array<{
      provider: ModelProvider;
      token?: string;
      settingsToken?: string;
      hifHeaders?: { leim?: string; dliq?: string };
      apiTemplate?: CapturedChatApi;
    }> = [];
    for (const provider of providers) {
      out.push({
        provider,
        token: this.tokens[provider],
        settingsToken: this.settingsTokens[provider],
        hifHeaders: this.hifHeaders[provider],
        apiTemplate: this.apiTemplates[provider],
      });
    }
    return out;
  }

  /** 订阅状态变化（返回取消函数） */
  subscribe(fn: () => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  private notify(): void {
    this.listeners.forEach((fn) => fn());
  }

  setSettings(partial: Partial<AppSettings>): void {
    this.settings = { ...this.settings, ...partial };
    this.notify();
  }

  setConnected(connected: boolean): void {
    this.connected = connected;
    this.notify();
  }

  setProviderStatus(provider: ModelProvider, status: Partial<ProviderStatusMap[ModelProvider]>): void {
    const prev = this.providers[provider] ?? {
      provider,
      connected: false,
      authed: false,
      models: [],
      lastChecked: 0,
    };
    this.providers = {
      ...this.providers,
      [provider]: { ...prev, ...status, lastChecked: Date.now() },
    };
    this.notify();
  }

  replaceProviders(status: ProviderStatusMap): void {
    this.providers = status;
    this.notify();
  }

  addLog(level: LogEntry['level'], message: string): void {
    this.logs = [...this.logs, { id: randomId(8), time: Date.now(), level, message }].slice(-this.maxLogs);
    this.notify();
  }

  clearLogs(): void {
    this.logs = [];
    this.notify();
  }

  getLogs(): LogEntry[] {
    return this.logs;
  }

  /** 构建返回给 UI 的状态快照 */
  snapshot(): ExtensionState {
    return {
      settings: this.settings,
      connected: this.connected,
      providers: this.providers,
      logs: this.getLogs(),
    };
  }
}

export const backgroundState = new BackgroundState();
