import { createDomProvider } from './dom-provider';

/** Claude 网页版（claude.ai） */
export const claudeProvider = createDomProvider({
  id: 'claude-web',
  name: 'Claude',
  url: 'https://claude.ai',
  matches: ['https://claude.ai/*'],
  models: ['claude-3-7-sonnet', 'claude-3-5-sonnet', 'claude-3-opus'],
  loginPattern: 'log\\s*in|sign\\s*up|登录|注册',
  dom: {
    inputSelectors: ['div[contenteditable="true"].ProseMirror', 'div[contenteditable="true"][role="textbox"]'],
    contentEditable: true,
    sendSelectors: ['button[aria-label*="Send" i]', 'button[data-testid="send-button"]'],
    sendLabelPattern: 'send',
    stopLabelPattern: 'stop',
    resultSelectors: ['[data-testid="user-message"] + div [class*="markdown"]', '[class*="assistant-message"]', '[class*="markdown"]'],
  },
});
