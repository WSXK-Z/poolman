import { createDomProvider } from './dom-provider';

/** 通义千问 网页版（tongyi.aliyun.com） */
export const qwenProvider = createDomProvider({
  id: 'qwen-web',
  name: '通义千问',
  url: 'https://tongyi.aliyun.com',
  matches: ['https://tongyi.aliyun.com/*'],
  models: ['qwen-max', 'qwen-plus'],
  loginPattern: '登录|登\\s*录|注册|sign\\s*in|log\\s*in',
  dom: {
    inputSelectors: ['textarea[placeholder*="输入"]', 'textarea', 'div[contenteditable="true"]'],
    contentEditable: false,
    sendSelectors: ['button[type="submit"]', 'button[aria-label*="发送" i]'],
    sendLabelPattern: '发送|send',
    stopLabelPattern: '停止|stop',
    resultSelectors: ['[class*="markdown"]', '[class*="message-content"]', '[class*="answer"]'],
  },
});
