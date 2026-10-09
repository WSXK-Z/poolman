import { browser } from 'wxt/browser';
import { MessageType, type ModelProvider, type ProviderStatusMap } from '@ai-gateway/shared';
import { createLogger, errorMessage } from '@ai-gateway/utils';
import { GatewayConnection } from '~/background/connection';
import { bindDiagnosticSender } from '~/background/diagnostics';
import { handleServerMessage } from '~/background/messageRouter';
import { backgroundState } from '~/background/state';
import { sendMessageToTab } from '~/lib/contentScript';
import { getProvider, listProviders } from '~/providers';
import {
  DEFAULT_SETTINGS,
  type AppSettings,
  type InternalMessage,
  type InternalResponse,
} from '~/shared/constants';

const log = createLogger('ext:background');

let connection: GatewayConnection | null = null;
const providerStatusPolls = new Map<ModelProvider, ReturnType<typeof setTimeout> | null>();

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
    // 连接（或重连）建立后，把本地已捕获凭证全量同步给服务端
    onOpen: () => syncCredentialsToServer(),
  });
  // 诊断信息通过该连接上报给服务端
  bindDiagnosticSender((msg) => connection?.send(msg));
  connection.start();
}

/**
 * 向服务端同步凭证（CREDENTIAL_SYNC）。provider 指定则只同步该平台，否则全量。
 * 内容脚本捕获后本地缓存一份（供 UI 展示 / 重连重发），真正存储与使用在服务端。
 */
function syncCredentialsToServer(provider?: ModelProvider): void {
  if (!connection) return;
  const creds = backgroundState.getAllCredentials();
  for (const c of creds) {
    if (provider && c.provider !== provider) continue;
    connection.send({
      id: `credsync_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 6)}`,
      type: MessageType.CREDENTIAL_SYNC,
      timestamp: Date.now(),
      payload: {
        provider: c.provider,
        token: c.token ?? null,
        settingsToken: c.settingsToken ?? null,
        hifHeaders: c.hifHeaders,
        apiTemplate: c.apiTemplate ?? null,
      },
    });
  }
}

/** 检查单个平台登录状态并刷新界面数据 */
async function refreshOneProviderStatus(provider: ReturnType<typeof getProvider>): Promise<boolean> {
  const enabled = backgroundState.settings.enabledProviders[provider.id] ?? true;
  if (!enabled) {
    backgroundState.setProviderStatus(provider.id, {
      connected: false,
      authed: false,
      models: provider.getModels(),
    });
    return false;
  }
  const authed = !!backgroundState.getToken(provider.id);
  backgroundState.setProviderStatus(provider.id, {
    connected: true,
    authed,
    models: provider.getModels(),
    tokenCaptured: !!backgroundState.getToken(provider.id),
    templateCaptured: !!backgroundState.getApiTemplate(provider.id),
  });
  return authed;
}

/** 页面刚打开时凭证可能尚未写入存储，主动请求内容脚本读取凭证并短暂轮询 */
function pollProviderStatus(providerId: ModelProvider): void {
  if (providerStatusPolls.has(providerId)) return;
  const provider = getProvider(providerId);
  let attempts = 0;
  const maxAttempts = 20;
  providerStatusPolls.set(providerId, null);

  const poll = async (): Promise<void> => {
    if (!(backgroundState.settings.enabledProviders[providerId] ?? true)) {
      providerStatusPolls.delete(providerId);
      return;
    }
    try {
      const authed = await refreshOneProviderStatus(provider);
      if (authed) {
        providerStatusPolls.delete(providerId);
        return;
      }
      for (const pattern of provider.matches) {
        const tabs = await browser.tabs.query({ url: pattern });
        const tab = tabs.find((item) => item.id != null);
        if (tab?.id != null) {
          await sendMessageToTab(tab.id, { type: 'CAPTURE_TOKEN', provider: providerId });
          break;
        }
      }
    } catch (err) {
      log.warn(`检查供应商登录状态失败：${providerId}`, errorMessage(err));
    }
    attempts += 1;
    if (attempts >= maxAttempts) {
      providerStatusPolls.delete(providerId);
      return;
    }
    providerStatusPolls.set(providerId, setTimeout(() => void poll(), 2_000));
  };

  void poll();
}

/** 刷新全部平台状态 */
async function refreshProviderStatus(): Promise<void> {
  for (const provider of listProviders()) {
    try {
      await refreshOneProviderStatus(provider);
    } catch (err) {
      log.warn(`刷新供应商状态失败：${provider.id}`, errorMessage(err));
    }
  }
}

/** 处理来自主界面的内部消息 */
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
        for (const provider of listProviders()) {
          if (msg.payload.enabledProviders?.[provider.id] === true) {
            pollProviderStatus(provider.id);
          } else if (msg.payload.enabledProviders?.[provider.id] === false) {
            const timer = providerStatusPolls.get(provider.id);
            if (timer !== null && timer !== undefined) clearTimeout(timer);
            providerStatusPolls.delete(provider.id);
          }
        }
        void refreshProviderStatus();
      }
      return { ok: true, data: backgroundState.settings };
    }
    case 'QUICK_TEST': {
      try {
        const provider = getProvider(msg.payload.model);
        const started = Date.now();
        const result = await provider.chat(msg.payload.model, [{ role: 'user', content: msg.payload.prompt }]);
        return { ok: true, result: { content: result.content ?? '', elapsedMs: Date.now() - started } };
      } catch (err) {
        return { ok: false, error: errorMessage(err) };
      }
    }
    case 'OPEN_PROVIDER': {
      try {
        const provider = getProvider(msg.payload.provider);
        await provider.open();
        pollProviderStatus(provider.id);
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

  // 2. 周期刷新凭证状态（同时用于 SW 心跳）
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
          }
        | {
            type: 'EXECUTE_CHUNK' | 'EXECUTE_RESULT' | 'EXECUTE_ERROR';
            requestId: string;
            data?: string;
            status?: number;
            ok?: boolean;
            statusText?: string;
            body?: string | null;
            error?: string;
            code?: string;
          },
    ) => {
      if (message && typeof message === 'object' && 'type' in message) {
        // content 网页执行器的回包 → 转发为协议消息给服务端（id 复用 EXECUTE_REQUEST 的 id）
        if (
          message.type === 'EXECUTE_CHUNK' ||
          message.type === 'EXECUTE_RESULT' ||
          message.type === 'EXECUTE_ERROR'
        ) {
          if (message.requestId) {
            const base = { id: message.requestId, timestamp: Date.now() };
            if (message.type === 'EXECUTE_CHUNK') {
              connection?.send({ ...base, type: MessageType.EXECUTE_CHUNK, payload: { data: message.data ?? '' } });
            } else if (message.type === 'EXECUTE_RESULT') {
              connection?.send({
                ...base,
                type: MessageType.EXECUTE_RESULT,
                payload: {
                  status: message.status ?? 0,
                  ok: message.ok ?? false,
                  statusText: message.statusText,
                  body: message.body ?? null,
                },
              });
            } else {
              connection?.send({
                ...base,
                type: MessageType.EXECUTE_ERROR,
                payload: { error: message.error ?? 'unknown execute error', code: message.code },
              });
            }
          }
          return;
        }
        if (message.type === 'TOKEN_CAPTURED') {
          // 内容脚本捕获到平台 Token（本地持久化一份供 UI / 重连重发）。
          // 内容脚本每 2s 轮询上报，凭证没变化时跳过同步/日志，避免刷屏与重复同步。
          const hasToken = !!message.payload?.token;
          const hasSettings = !!message.payload?.settingsToken;
          const changed =
            (hasToken && message.payload.token !== backgroundState.getToken(message.payload.provider)) ||
            (hasSettings &&
              message.payload.settingsToken !== backgroundState.getSettingsToken(message.payload.provider));
          if (message.payload?.token) {
            backgroundState.setToken(message.payload.provider, message.payload.token);
          }
          if (message.payload?.settingsToken) {
            backgroundState.setSettingsToken(message.payload.provider, message.payload.settingsToken);
          }
          if (hasToken || hasSettings) {
            const provider = getProvider(message.payload.provider);
            backgroundState.setProviderStatus(message.payload.provider, {
              connected: true,
              authed: !!backgroundState.getToken(message.payload.provider),
              models: provider.getModels(),
              tokenCaptured: !!backgroundState.getToken(message.payload.provider),
              templateCaptured: !!backgroundState.getApiTemplate(message.payload.provider),
            });
          }
          // 连接器化：凭证有变化才同步给服务端（服务端存储与使用）并记录日志
          if (changed) {
            syncCredentialsToServer(message.payload.provider);
            backgroundState.addLog('info', `捕获到 ${message.payload.provider} 的访问 Token`);
          }
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
            // 连接器化：接口模板捕获后立即同步给服务端
            syncCredentialsToServer(message.payload.provider);
            backgroundState.addLog('info', `捕获到 ${message.payload.provider} 接口模板: ${message.payload.url}`);
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
