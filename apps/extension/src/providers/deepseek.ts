/**
 * DeepSeek 网页版适配器（连接器化后仅保留元信息）。
 *
 * 重构后，请求组装 / PoW 求解 / SSE 解析 / 422 自愈 / 会话管理等业务逻辑
 * 已全部迁移到桌面端（apps/desktop/src-tauri/src/extension/adapters/deepseek）。
 * 插件只承担两类机械职责：
 *   1) 内容脚本捕获凭证/接口模板 → 同步服务端（CREDENTIAL_SYNC）
 *   2) 收到 EXECUTE_REQUEST 后在网页 context 执行 fetch 并回传原始流
 *
 * 因此这里的 chat / chatStream 不再被 messageRouter 调用（DeepSeek 走 EXECUTE 通道），
 * 仅保留元信息供 UI 展示 / 标签页查找 / open()。
 */
import type { ModelProvider } from '@ai-gateway/shared';
import { backgroundState } from '~/background/state';
import { ensureTab, type AIProvider } from './base';

const PROVIDER_ID: ModelProvider = 'deepseek-web';

export const deepseekProvider: AIProvider = {
  id: PROVIDER_ID,
  name: 'DeepSeek',
  url: 'https://chat.deepseek.com',
  matches: ['https://chat.deepseek.com/*'],
  getModels: () => ['deepseek-web'],

  async open(): Promise<number> {
    // 用户手动“打开”平台时，激活并聚焦标签页
    return ensureTab('https://chat.deepseek.com', true);
  },

  async checkAuth(): Promise<boolean> {
    // 连接器化后凭证的权威存储与服务端一致（经 CREDENTIAL_SYNC 同步）；这里读本地副本判断
    return !!backgroundState.getToken(PROVIDER_ID);
  },
};
