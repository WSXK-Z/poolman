/** 通过平台 URL、模型列表和登录检测规则构造轻量供应商元信息。 */
import type { ModelProvider, PublicModel } from '@ai-gateway/shared';
import { createLogger } from '@ai-gateway/utils';
import { browser } from 'wxt/browser';
import {
  type AIProvider,
  domCheckAuthInPage,
  ensureTab,
  runInPage,
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
  /** 登录态检测使用的输入框选择器 */
  dom: { inputSelectors: string[]; [key: string]: unknown };
}

/**
 * 基于通用 DOM 注入脚本创建平台适配器。
 * 各平台只需提供页面选择器与匹配规则，即可获得完整的 chat / checkAuth 实现。
 */
export function createDomProvider(opts: DomProviderOptions): AIProvider {
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
  };
}
