import type { ChatMessage, ChatOptions, ChatResult, ModelProvider, PublicModel } from '@ai-gateway/shared';
import { createLogger } from '@ai-gateway/utils';
import { browser } from 'wxt/browser';
import {
  type AIProvider,
  buildPrompt,
  domChatInPage,
  domCheckAuthInPage,
  ensureTab,
  runInPage,
  type DomChatConfig,
} from './base';

const log = createLogger('ext:provider');

export interface DomProviderOptions {
  id: ModelProvider;
  name: string;
  url: string;
  matches: string[];
  models: PublicModel[];
  /** 登录态检测关键词（正则） */
  loginPattern: string;
  /** 通用 DOM 聊天配置（prompt 会在调用时注入） */
  dom: Omit<DomChatConfig, 'prompt'>;
  /** DeepSeek 类“深度思考”预点击（可选） */
  deepThink?: boolean;
}

/**
 * 基于通用 DOM 注入脚本创建平台适配器。
 * 各平台只需提供页面选择器与匹配规则，即可获得完整的 chat / checkAuth 实现。
 */
export function createDomProvider(opts: DomProviderOptions): AIProvider {
  const domConfig: Omit<DomChatConfig, 'prompt'> = opts.deepThink
    ? {
        ...opts.dom,
        preClickSelector: opts.dom.preClickSelector,
        preClickLabelPattern: opts.dom.preClickLabelPattern ?? '深度思考|DeepThink',
        preClickActivePattern: opts.dom.preClickActivePattern ?? 'active|开启',
      }
    : opts.dom;

  return {
    id: opts.id,
    name: opts.name,
    url: opts.url,
    matches: opts.matches,
    getModels: () => opts.models as unknown as string[],

    async open(): Promise<number> {
      // 用户手动“打开”平台时，激活并聚焦标签页
      return ensureTab(opts.url, true);
    },

    async checkAuth(): Promise<boolean> {
      // 不自动打开标签页：仅当平台页面已打开时才检测登录态
      try {
        const tabs = await browser.tabs.query({ url: `${opts.url}/*` });
        const tab = tabs.find((t) => t.id != null);
        if (!tab?.id) return false;
        const authed = await runInPage<boolean>(tab.id, domCheckAuthInPage, [
          { loginPattern: opts.loginPattern, inputSelectors: opts.dom.inputSelectors },
        ]);
        log.info(`checkAuth ${opts.id} -> ${authed}`);
        return authed;
      } catch (err) {
        log.warn(`checkAuth ${opts.id} failed`, err);
        return false;
      }
    },

    async chat(model: ModelProvider, messages: ChatMessage[], options?: ChatOptions): Promise<ChatResult> {
      const tabId = await ensureTab(opts.url);
      const prompt = buildPrompt(messages);
      const started = Date.now();
      const content = await runInPage<string>(tabId, domChatInPage, [
        {
          ...domConfig,
          prompt,
          maxWaitMs: options?.timeoutMs ?? domConfig.maxWaitMs,
        } satisfies DomChatConfig,
      ]);
      log.info(`chat ${opts.id} done in ${Date.now() - started}ms`);
      return {
        content,
        model,
        finishedReason: 'stop',
      };
    },
  };
}
