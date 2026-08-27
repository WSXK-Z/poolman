import { createDomProvider } from './dom-provider';

/** ChatGPT 网页版（chatgpt.com） */
export const chatgptProvider = createDomProvider({
  id: 'chatgpt-web',
  name: 'ChatGPT',
  url: 'https://chatgpt.com',
  matches: ['https://chatgpt.com/*', 'https://chat.openai.com/*'],
  models: ['gpt-4o', 'gpt-4o-mini', 'gpt-4', 'gpt-3.5-turbo'],
  loginPattern: 'log\\s*in|sign\\s*up|登录|注册',
  dom: {
    inputSelectors: ['#prompt-textarea', 'div[contenteditable="true"][role="textbox"]', 'textarea'],
    contentEditable: true,
    sendSelectors: ['[data-testid="send-button"]', 'button[type="submit"]'],
    sendLabelPattern: 'send',
    stopLabelPattern: 'stop',
    resultSelectors: ['[data-message-author-role="assistant"]', '[class*="markdown"]'],
  },
});
