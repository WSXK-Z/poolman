/**
 * 腾讯元宝网页版适配器（连接器化后仅保留元信息）。
 *
 * 请求组装 / 流式帧解析 / 会话管理等业务逻辑在桌面端
 * （apps/desktop/src-tauri/src/models/yuanbao_web.rs）完成。插件只承担两类机械职责：
 *   1) 内容脚本捕获凭证/接口模板 → 同步服务端（CREDENTIAL_SYNC）
 *   2) 收到 EXECUTE_REQUEST 后在网页 context 执行 fetch 并回传原始流
 *
 * 元宝对话请求带 `x-uskey` 等页面签名头，只能在页面上下文生成，因此必须经插件执行。
 */
import type { ModelProvider } from '@ai-gateway/shared';
import { backgroundState } from '~/background/state';
import { ensureTab, type AIProvider } from './base';

const PROVIDER_ID: ModelProvider = 'yuanbao-web';

export const yuanbaoProvider: AIProvider = {
  id: PROVIDER_ID,
  name: '腾讯元宝',
  url: 'https://yuanbao.tencent.com',
  matches: ['https://yuanbao.tencent.com/*'],
  getModels: () => ['yuanbao-web'],

  async open(): Promise<number> {
    return ensureTab('https://yuanbao.tencent.com', true);
  },

  async checkAuth(): Promise<boolean> {
    // 元宝的登录态走网页 cookie，聊天请求里没有 bearer token；
    // 捕获到真实聊天接口模板，说明该页面已登录并实际发起过对话。
    return (
      !!backgroundState.getToken(PROVIDER_ID) || !!backgroundState.getApiTemplate(PROVIDER_ID)
    );
  },
};