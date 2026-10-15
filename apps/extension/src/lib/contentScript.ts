/**
 * 已打开标签页的内容脚本通信辅助工具。
 *
 * 扩展更新后既有标签页可能没有接收端；检测到此类连接错误时按 manifest
 * 声明补注入脚本并重试，其他错误保持原样向调用方传播。
 */
import { browser } from 'wxt/browser';
import { createLogger, errorMessage } from '@ai-gateway/utils';
import { ProviderError } from '~/providers/base';

/**
 * 标签页内容脚本兜底注入。
 *
 * 已打开的页面不会因为扩展重载/更新/首次安装而自动补注入内容脚本，
 * 此时 tabs.sendMessage 会失败：Could not establish connection. Receiving end does not exist.
 * 这里在发送失败时按 manifest 声明补注入内容脚本，然后重试一次。
 */

const log = createLogger('ext:inject');

/** manifest 中声明的内容脚本条目（只取注入所需字段） */
interface DeclaredContentScript {
  js?: string[];
  world?: string;
}

/** 按 manifest 声明把内容脚本注入指定标签页（isolated / MAIN world 分别注入） */
async function injectDeclaredContentScripts(tabId: number): Promise<void> {
  const entries = (browser.runtime.getManifest().content_scripts ?? []) as DeclaredContentScript[];
  for (const entry of entries) {
    if (!entry.js?.length) continue;
    const world = entry.world === 'MAIN' ? 'MAIN' : 'ISOLATED';
    // 路径直接取自 manifest 声明（注入 API 把路径标成受控字符串类型，这里按原样透传）
    const options = {
      target: { tabId, allFrames: false },
      files: entry.js,
      world,
    } as unknown as Parameters<typeof browser.scripting.executeScript>[0];
    try {
      await browser.scripting.executeScript(options);
    } catch (err) {
      log.warn('补注入内容脚本失败', { tabId, files: entry.js, world, error: errorMessage(err) });
    }
  }
}

/** 是否为「目标页没有接收端」错误（内容脚本未注入） */
function isNoReceivingEnd(text: string): boolean {
  return /Receiving end does not exist|Could not establish connection/i.test(text);
}

/**
 * 给标签页发消息；若因内容脚本缺失失败，则补注入后重试一次；
 * 仍失败则抛出明确的 ProviderError（提示刷新标签页），避免上游只看到浏览器原始报错。
 */
export async function sendMessageToTab(tabId: number, message: unknown): Promise<void> {
  try {
    await browser.tabs.sendMessage(tabId, message);
    return;
  } catch (err) {
    if (!isNoReceivingEnd(errorMessage(err))) throw err;
    log.info('标签页缺少内容脚本，补注入后重试', { tabId });
    await injectDeclaredContentScripts(tabId);
    try {
      await browser.tabs.sendMessage(tabId, message);
    } catch (retryErr) {
      throw new ProviderError(
        `无法与页面脚本通信（请刷新该标签页后重试）: ${errorMessage(retryErr)}`,
        'content_script_unreachable',
      );
    }
  }
}
