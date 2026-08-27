import { browser } from 'wxt/browser';
import type { ModelProvider, ProviderStatusMap } from '@ai-gateway/shared';
import { createLogger, errorMessage } from '@ai-gateway/utils';
import { GatewayConnection } from '~/background/connection';
import { bindDiagnosticSender, diag } from '~/background/diagnostics';
import { handleServerMessage } from '~/background/messageRouter';
import { backgroundState } from '~/background/state';
import { getProvider, listProviders } from '~/providers';
import {
  DEFAULT_SETTINGS,
  type AppSettings,
  type InternalMessage,
  type InternalResponse,
} from '~/shared/constants';

const log = createLogger('ext:background');

let connection: GatewayConnection | null = null;

function getStatus(): ProviderStatusMap {
  return backgroundState.providers;
}

function startConnection(): void {
  connection?.stop();
  connection = new GatewayConnection({
    url: backgroundState.settings.serverUrl,
    token: backgroundState.settings.serverToken || undefined,
    autoReconnect: backgroundState.settings.autoReconnect,
    getStatus,
    onMessage: (msg) => void handleServerMessage(msg, (resp) => connection?.send(resp)),
  });
  // 诊断信息通过该连接上报给服务端
  bindDiagnosticSender((msg) => connection?.send(msg));
  connection.start();
}

/** 刷新各平台登录态并写入状态 */
async function refreshProviderStatus(): Promise<void> {
  for (const provider of listProviders()) {
    const enabled = backgroundState.settings.enabledProviders[provider.id] ?? true;
    if (!enabled) {
      backgroundState.setProviderStatus(provider.id, { connected: false, authed: false, models: provider.getModels() });
      continue;
    }
    const authed = await provider.checkAuth();
    backgroundState.setProviderStatus(provider.id, {
      connected: true,
      authed,
      models: provider.getModels(),
      tokenCaptured: !!backgroundState.getToken(provider.id),
      templateCaptured: !!backgroundState.getApiTemplate(provider.id),
    });
  }
}

/** 处理来自 UI（popup/options）的内部消息 */
async function handleInternalMessage(msg: InternalMessage): Promise<InternalResponse> {
  switch (msg.type) {
    case 'GET_STATE':
      return { ok: true, state: backgroundState.snapshot() };
    case 'GET_SETTINGS':
      return { ok: true, data: backgroundState.settings };
    case 'UPDATE_SETTINGS': {
      backgroundState.setSettings(msg.payload);
      await browser.storage.local.set(backgroundState.settings as unknown as Record<string, unknown>);
      if (
        'serverUrl' in msg.payload ||
        'serverToken' in msg.payload ||
        'autoReconnect' in msg.payload
      ) {
        startConnection();
      }
      if ('enabledProviders' in msg.payload) {
        void refreshProviderStatus();
      }
      return { ok: true, data: backgroundState.settings };
    }
    case 'QUICK_TEST': {
      try {
        const provider = getProvider(msg.payload.model);
        const started = Date.now();
        const result = await provider.chat(msg.payload.model, [{ role: 'user', content: msg.payload.prompt }]);
        return { ok: true, result: { content: result.content, elapsedMs: Date.now() - started } };
      } catch (err) {
        return { ok: false, error: errorMessage(err) };
      }
    }
    case 'OPEN_PROVIDER': {
      try {
        const provider = getProvider(msg.payload.provider);
        await provider.open();
        return { ok: true };
      } catch (err) {
        return { ok: false, error: errorMessage(err) };
      }
    }
    case 'GET_LOGS':
      return { ok: true, logs: backgroundState.getLogs() };
    case 'CLEAR_LOGS':
      backgroundState.clearLogs();
      return { ok: true, logs: backgroundState.getLogs() };
    default:
      return { ok: false, error: '未知消息类型' };
  }
}

export default defineBackground(() => {
  log.info('AI Gateway background started');

  // 1. 恢复持久化的捕获结果 + 加载设置
  void backgroundState.loadPersisted();
  void browser.storage.local.get(DEFAULT_SETTINGS as unknown as Record<string, unknown>).then((stored) => {
    const settings: AppSettings = { ...DEFAULT_SETTINGS, ...(stored as Partial<AppSettings>) };
    backgroundState.setSettings(settings);
    startConnection();
    void refreshProviderStatus();
  });

  // 2. 周期刷新登录态（同时用于 SW 心跳）
  setInterval(() => void refreshProviderStatus(), 60_000);

  // 3. 响应 UI 消息 + 内容脚本上报
  browser.runtime.onMessage.addListener(
    (
      message:
        | InternalMessage
        | {
            type: 'TOKEN_CAPTURED';
            payload: { provider: ModelProvider; token: string; url?: string; settingsToken?: string };
          }
        | {
            type: 'API_CAPTURED';
            payload: {
              provider: ModelProvider;
              url: string;
              headers: Record<string, string>;
              template: unknown;
              token: string | null;
              hifHeaders?: { leim?: string; dliq?: string };
            };
          },
    ) => {
      if (message && typeof message === 'object' && 'type' in message) {
        if (message.type === 'TOKEN_CAPTURED') {
          // 内容脚本捕获到平台 Token（持久化到 storage.local，重载不丢）
          if (message.payload?.token) {
            backgroundState.setToken(message.payload.provider, message.payload.token);
          }
          if (message.payload?.settingsToken) {
            backgroundState.setSettingsToken(message.payload.provider, message.payload.settingsToken);
          }
          backgroundState.addLog('info', `捕获到 ${message.payload.provider} 的访问 Token`);
          diag('info', 'capture', `${message.payload.provider} 访问凭证已捕获`, {
            provider: message.payload.provider,
            tokenLength: message.payload.token?.length ?? 0,
            hasSettingsToken: !!message.payload.settingsToken,
          });
          return;
        }
        if (message.type === 'API_CAPTURED') {
          // 内容脚本捕获到聊天接口模板（自适配）
          const tpl = message.payload.template as
            | {
                agent_name?: string;
                model_name?: string;
                think_enabled?: boolean;
                generation_settings?: Record<string, unknown>;
                has_session_id?: boolean;
                has_parent_message_id?: boolean;
                chat_session_id?: string;
                parent_message_id?: string | null;
                model_type?: string;
                thinking_enabled?: boolean;
                search_enabled?: boolean;
              }
            | null
            | undefined;
          if (tpl && typeof tpl === 'object') {
            backgroundState.setApiTemplate(message.payload.provider, {
              provider: message.payload.provider,
              url: message.payload.url,
              headers: message.payload.headers,
              template: tpl,
              token: message.payload.token ?? backgroundState.getToken(message.payload.provider) ?? '',
              capturedAt: Date.now(),
            });
            // DeepSeek 完成接口需要 x-hif-leim / x-hif-dliq（会话级，独立持久化）
            const hif = message.payload.hifHeaders;
            if (hif && (hif.leim || hif.dliq)) {
              backgroundState.setHifHeaders(message.payload.provider, hif);
            }
            if (message.payload.token) {
              backgroundState.setToken(message.payload.provider, message.payload.token);
            }
            backgroundState.addLog('info', `捕获到 ${message.payload.provider} 接口模板: ${message.payload.url}`);
            diag('info', 'capture', `${message.payload.provider} 聊天接口模板已捕获`, {
              provider: message.payload.provider,
              url: message.payload.url,
              headers: message.payload.headers,
              template: tpl,
            });
          }
          return;
        }
        return handleInternalMessage(message as InternalMessage).then((resp) => resp as InternalResponse);
      }
      return Promise.resolve({ ok: false, error: 'invalid message' } as InternalResponse);
    },
  );

  // 4. 服务端连接状态变化记录
  void backgroundState;
});
