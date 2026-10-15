/**
 * 供应商页面元信息与通用标签页辅助逻辑。
 *
 * AIProvider 只为状态轮询和页面管理提供平台元数据；对话请求由桌面适配器处理，
 * 扩展不从此接口直接生成或解析聊天内容。
 */
import type { ModelProvider } from '@ai-gateway/shared';
import { browser } from 'wxt/browser';

/** 统一 AI 平台适配器接口 */
export interface AIProvider {
  id: ModelProvider;
  name: string;
  /** 平台页面地址 */
  url: string;
  /** 标签页匹配模式（host_permissions 需覆盖） */
  matches: string[];
  /** 检测是否已登录 */
  checkAuth(): Promise<boolean>;
  /** 平台支持的对外模型名 */
  getModels(): string[];
  /** 打开平台页面，返回 tabId */
  open(): Promise<number>;
}

/** 适配器业务错误（code 可回传给服务端） */
export class ProviderError extends Error {
  constructor(
    message: string,
    public readonly code = 'provider_error',
  ) {
    super(message);
    this.name = 'ProviderError';
  }
}

/**
 * 查找或创建平台的标签页。
 * @param activate 为 true 时激活并聚焦该标签页（用户手动“打开”平台时用）；
 *                 自动化调用（chat/checkAuth）用 false，避免打断用户。
 */
export async function ensureTab(url: string, activate = false): Promise<number> {
  // tabs.query 需要完整的 Chrome 匹配模式（如 https://chat.deepseek.com/*）
  const pattern = url.endsWith('/') ? `${url}*` : `${url}/*`;
  const tabs = await browser.tabs.query({ url: pattern });
  const existing = tabs.find((t) => t.id != null);
  if (existing?.id != null) {
    if (activate) {
      await browser.tabs.update(existing.id, { active: true });
      if (existing.windowId != null) {
        await browser.windows.update(existing.windowId, { focused: true }).catch(() => {});
      }
    }
    return existing.id;
  }
  const tab = await browser.tabs.create({ url, active: activate });
  if (tab.id == null) throw new ProviderError('无法创建标签页');
  return tab.id;
}

/** 在指定标签页的 MAIN world 中执行自包含函数 */
export async function runInPage<T>(
  tabId: number,
  func: (...args: any[]) => T | Promise<T>,
  args?: any[],
): Promise<T> {
  const results = await browser.scripting.executeScript({
    target: { tabId },
    world: 'MAIN',
    func: func as (...args: any[]) => unknown,
    args: (args ?? []) as any[],
  });
  return (results[0]?.result ?? null) as T;
}

/* ------------------------------------------------------------------ */
/* 以下为自包含注入函数：会被序列化后在页面 MAIN world 中执行，          */
/* 严禁引用任何外部变量。                                               */
/* ------------------------------------------------------------------ */

export interface DomChatConfig {
  /** 要发送的提示词 */
  prompt: string;
  /** 输入框候选选择器 */
  inputSelectors: string[];
  /** 输入框是否为 contenteditable（否则按 textarea 处理） */
  contentEditable: boolean;
  /** 发送按钮候选选择器 */
  sendSelectors: string[];
  /** 发送按钮 aria-label / 文本匹配模式 */
  sendLabelPattern?: string;
  /** 停止生成按钮的 aria-label / 文本匹配模式 */
  stopLabelPattern?: string;
  /** 回复内容候选选择器（取最后一个匹配项的 innerText） */
  resultSelectors: string[];
  /** 可选：生成前点击的元素（如“深度思考”开关），传 selector 或文本匹配 */
  preClickSelector?: string;
  preClickLabelPattern?: string;
  preClickActivePattern?: string;
  /** 最长等待时间 */
  maxWaitMs?: number;
  /** 回复稳定判定窗口 */
  stabilizationMs?: number;
}

/**
 * 通用 DOM 聊天执行函数（自包含）。
 * 流程：定位输入框 -> 填入并触发事件 -> 点击发送 -> 轮询等待“停止”按钮消失且内容稳定 -> 返回最后回复文本。
 */
export function domChatInPage(config: DomChatConfig): Promise<string> {
  const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
  const waitFor = async (pred: () => boolean, timeout: number, interval = 300): Promise<boolean> => {
    const start = Date.now();
    while (Date.now() - start < timeout) {
      try {
        if (pred()) return true;
      } catch {
        /* ignore */
      }
      await sleep(interval);
    }
    return false;
  };

  return new Promise<string>((resolve, reject) => {
    void (async () => {
      try {
        const maxWait = config.maxWaitMs ?? 180_000;
        const stabilization = config.stabilizationMs ?? 2_000;

        // 1. 定位输入框（等待页面就绪）
        const findInput = (): HTMLElement | null => {
          for (const sel of config.inputSelectors) {
            const el = document.querySelector<HTMLElement>(sel);
            if (el) return el;
          }
          return null;
        };
        const inputReady = await waitFor(() => !!findInput(), 20_000, 500);
        if (!inputReady) throw new Error('未找到输入框，请确认 AI 页面已打开并登录');
        const input = findInput()!;

        input.focus();
        if (!config.contentEditable && input instanceof HTMLTextAreaElement) {
          const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')?.set;
          setter?.call(input, config.prompt);
          input.dispatchEvent(new Event('input', { bubbles: true }));
          input.dispatchEvent(new Event('change', { bubbles: true }));
        } else {
          (input as HTMLElement).textContent = config.prompt;
          input.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertText', data: config.prompt }));
        }

        // 2. 可选的生成前点击（如 DeepSeek 深度思考开关）
        if (config.preClickSelector || config.preClickLabelPattern) {
          const pre = config.preClickSelector
            ? document.querySelector(config.preClickSelector)
            : Array.from(document.querySelectorAll<HTMLElement>('button,div[role="button"]')).find((el) =>
                new RegExp(config.preClickLabelPattern || '', 'i').test(el.textContent || ''),
              );
          if (pre) {
            const active =
              (pre as HTMLElement).getAttribute('data-active') === 'true' ||
              (pre as HTMLElement).getAttribute('aria-pressed') === 'true' ||
              (pre as HTMLElement).classList.contains('active');
            const activeRe = config.preClickActivePattern ? new RegExp(config.preClickActivePattern, 'i') : null;
            const isActive = active || (activeRe ? activeRe.test((pre as HTMLElement).textContent || '') : false);
            if (!isActive) (pre as HTMLElement).click();
          }
        }

        await sleep(250);

        // 3. 点击发送
        let sendBtn: HTMLButtonElement | null = null;
        for (const sel of config.sendSelectors) {
          const el = document.querySelector<HTMLButtonElement>(sel);
          if (el) {
            sendBtn = el;
            break;
          }
        }
        const sendLabelPattern = config.sendLabelPattern;
        if (!sendBtn && sendLabelPattern) {
          sendBtn =
            Array.from(document.querySelectorAll<HTMLButtonElement>('button')).find((b) =>
              new RegExp(sendLabelPattern, 'i').test(
                (b.getAttribute('aria-label') || '') + ' ' + (b.textContent || ''),
              ),
            ) ?? null;
        }
        if (!sendBtn) throw new Error('未找到可用的发送按钮');
        if (sendBtn.disabled) {
          await waitFor(() => !sendBtn!.disabled, 10_000, 300);
        }
        sendBtn.click();

        // 4. 等待回复完成：停止按钮出现后消失，且内容稳定
        const stopRe = config.stopLabelPattern ? new RegExp(config.stopLabelPattern, 'i') : null;
        const hasStop = () => {
          if (!stopRe) return false;
          return Array.from(document.querySelectorAll<HTMLElement>('button,[role="button"]')).some((b) =>
            stopRe.test((b.getAttribute('aria-label') || '') + ' ' + (b.textContent || '')),
          );
        };

        const getLastText = (): string => {
          for (const sel of config.resultSelectors) {
            const nodes = document.querySelectorAll<HTMLElement>(sel);
            const last = nodes[nodes.length - 1];
            if (last) {
              const t = last.innerText.trim();
              if (t) return t;
            }
          }
          return '';
        };

        const start = Date.now();
        let lastText = '';
        let stableFor = 0;
        let sawStop = false;

        while (Date.now() - start < maxWait) {
          await sleep(500);
          const cur = getLastText();
          const generating = hasStop();
          if (generating) sawStop = true;
          if (!generating && sawStop && cur.length > 0) {
            if (cur === lastText) {
              stableFor += 500;
              if (stableFor >= stabilization) break;
            } else {
              lastText = cur;
              stableFor = 0;
            }
          } else {
            lastText = cur || lastText;
            stableFor = 0;
          }
        }

        const content = lastText.trim() || getLastText().trim();
        if (!content) throw new Error('未能获取到回复内容（可能页面结构已变化）');
        resolve(content);
      } catch (err) {
        reject(err instanceof Error ? err : new Error(String(err)));
      }
    })();
  });
}

/** 通用登录态检测函数（自包含） */
export function domCheckAuthInPage(args: { loginPattern: string; inputSelectors: string[] }): boolean {
  const loginRe = new RegExp(args.loginPattern, 'i');
  const hasLoginBtn = Array.from(document.querySelectorAll('a,button,[role="button"]')).some((el) =>
    loginRe.test(el.textContent || ''),
  );
  const hasChatInput = args.inputSelectors.some((sel) => !!document.querySelector(sel));
  // 有聊天输入框视为已登录；否则以“是否存在登录/注册按钮”作为判断依据
  return hasChatInput || !hasLoginBtn;
}
