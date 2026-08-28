/**
 * 服务端凭证存储（CredentialStore）。
 *
 * 连接器化重构后，插件内容脚本捕获的凭证/接口模板通过 CREDENTIAL_SYNC 上报到这里，
 * 由服务端统一存储、决策并在发起请求时使用。插件不再参与任何业务逻辑，只作"捕获代理"。
 *
 * 数据来源：CREDENTIAL_SYNC 消息（bridge/handler.ts 收到后调用 upsert）。
 * 消费方：DeepSeekService（组装请求/求解 PoW 时读取 token/settingsToken/hif/apiTemplate）。
 */
import type { CapturedChatApi, ModelProvider } from '@ai-gateway/shared';

/** 单个平台在服务端持有的凭证集合 */
export interface ProviderCredentials {
  /** 访问凭证（Authorization: Bearer <token>），来源 localStorage.userToken */
  token?: string | null;
  /** 设置令牌（x-settings-token 请求头来源），来源 localStorage.settingsJwt */
  settingsToken?: string | null;
  /** DeepSeek 完成接口需要的会话级头（x-hif-leim / x-hif-dliq） */
  hifHeaders?: { leim?: string; dliq?: string };
  /** 捕获到的聊天接口模板（URL/请求头/body 结构，用于自适配转发） */
  apiTemplate?: CapturedChatApi | null;
  /** 最后更新时间戳 */
  updatedAt: number;
}

/** 供 /health 或调试输出的精简视图 */
export interface CredentialSummary {
  provider: string;
  tokenCaptured: boolean;
  settingsTokenCaptured: boolean;
  templateCaptured: boolean;
  updatedAt: number;
}

export class CredentialStore {
  private store = new Map<ModelProvider, ProviderCredentials>();

  /**
   * 合并更新某个平台的凭证（插件上报 CREDENTIAL_SYNC 时调用）。
   * 只覆盖消息里携带的字段；未携带的字段保持原值，避免覆盖掉更完整的历史捕获。
   */
  upsert(provider: ModelProvider, creds: Partial<ProviderCredentials>): void {
    const prev = this.store.get(provider) ?? ({} as ProviderCredentials);
    const next: ProviderCredentials = { ...prev, ...creds, updatedAt: Date.now() };
    this.store.set(provider, next);
  }

  /** 获取某个平台的完整凭证集合 */
  get(provider: ModelProvider): ProviderCredentials | undefined {
    return this.store.get(provider);
  }

  /** 获取访问凭证；无则返回 null */
  getToken(provider: ModelProvider): string | null {
    return this.store.get(provider)?.token ?? null;
  }

  /** 获取设置令牌；无则返回 null */
  getSettingsToken(provider: ModelProvider): string | null {
    return this.store.get(provider)?.settingsToken ?? null;
  }

  /** 获取会话级头（x-hif-leim / x-hif-dliq） */
  getHifHeaders(provider: ModelProvider): { leim?: string; dliq?: string } | undefined {
    return this.store.get(provider)?.hifHeaders;
  }

  /** 获取聊天接口模板 */
  getApiTemplate(provider: ModelProvider): CapturedChatApi | null {
    return this.store.get(provider)?.apiTemplate ?? null;
  }

  /** 是否已持有访问凭证 */
  hasToken(provider: ModelProvider): boolean {
    return !!this.store.get(provider)?.token;
  }

  /** 汇总各平台凭证状态（供 /health 展示） */
  summary(): Record<string, CredentialSummary> {
    const out: Record<string, CredentialSummary> = {};
    for (const [provider, creds] of this.store) {
      out[provider] = {
        provider,
        tokenCaptured: !!creds.token,
        settingsTokenCaptured: !!creds.settingsToken,
        templateCaptured: !!creds.apiTemplate,
        updatedAt: creds.updatedAt,
      };
    }
    return out;
  }

  /** 清空指定平台（或全部）凭证 */
  clear(provider?: ModelProvider): void {
    if (provider) {
      this.store.delete(provider);
    } else {
      this.store.clear();
    }
  }
}
