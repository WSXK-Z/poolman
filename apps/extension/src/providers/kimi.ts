import { createDomProvider } from './dom-provider';

/** Kimi 网页版（kimi.moonshot.cn） */
export const kimiProvider = createDomProvider({
  id: 'kimi-web',
  name: 'Kimi',
  url: 'https://kimi.moonshot.cn',
  matches: ['https://kimi.moonshot.cn/*'],
  models: ['kimi', 'moonshot-v1-8k'],
  loginPattern: '登录|登\\s*录|注册|sign\\s*in|log\\s*in',
  dom: {
    inputSelectors: ['textarea[placeholder*="输入"]', 'textarea', 'div[contenteditable="true"]'],
    contentEditable: false,
    sendSelectors: ['button[type="submit"]', '[data-testid="send-button"]'],
    sendLabelPattern: '发送|send',
    stopLabelPattern: '停止|stop',
    resultSelectors: ['[class*="assistant"] [class*="markdown"]', '[class*="markdown"]', '[class*="message-content"]'],
  },
});
