import { defineConfig } from 'wxt';

export default defineConfig({
  srcDir: 'src',
  modules: ['@wxt-dev/module-vue'],
  manifest: {
    name: 'AI Gateway',
    description: '复用已登录的 AI 网页会话，对外提供 OpenAI 兼容 API（零 API Key）',
    permissions: ['storage', 'tabs', 'scripting'],
    host_permissions: [
      'https://chat.deepseek.com/*',
      'https://chatgpt.com/*',
      'https://chat.openai.com/*',
      'https://claude.ai/*',
      'https://gemini.google.com/*',
      'https://kimi.moonshot.cn/*',
      'https://tongyi.aliyun.com/*',
      'https://yuanbao.tencent.com/*',
    ],
    action: {
      default_title: 'AI Gateway',
    },
  },
});
