import { createDomProvider } from './dom-provider';

/** Gemini 网页版（gemini.google.com） */
export const geminiProvider = createDomProvider({
  id: 'gemini-web',
  name: 'Gemini',
  url: 'https://gemini.google.com',
  matches: ['https://gemini.google.com/*'],
  models: ['gemini-2.5-pro', 'gemini-2.0-flash', 'gemini-pro'],
  loginPattern: 'log\\s*in|sign\\s*up|登录|注册',
  dom: {
    inputSelectors: [
      'rich-textarea div[contenteditable="true"]',
      'div[contenteditable="true"][role="textbox"]',
      'div.ql-editor[contenteditable="true"]',
    ],
    contentEditable: true,
    sendSelectors: ['button.send-button', 'button[aria-label*="Send" i]', 'button[type="submit"]'],
    sendLabelPattern: 'send',
    stopLabelPattern: 'stop',
    resultSelectors: ['message-content', '[class*="model-response"]', '[class*="markdown"]'],
  },
});
