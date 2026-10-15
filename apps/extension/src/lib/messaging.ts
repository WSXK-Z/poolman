/** Popup 与扩展后台之间的类型化消息请求封装。 */
import { browser } from 'wxt/browser';
import type { InternalMessage, InternalResponse } from '~/shared/constants';

/** 向 background 发送内部消息，失败时抛出错误 */
export async function sendMessage<T extends InternalResponse = InternalResponse>(
  msg: InternalMessage,
): Promise<T> {
  const resp = (await browser.runtime.sendMessage(msg)) as InternalResponse;
  if (!resp.ok) {
    throw new Error((resp as { error: string }).error || '未知错误');
  }
  return resp as T;
}
