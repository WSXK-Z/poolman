export type ChatRole = 'system' | 'user' | 'assistant';

export interface ChatMessage {
  role: ChatRole;
  content: string;
  name?: string;
}

export interface ChatOptions {
  temperature?: number;
  topP?: number;
  maxTokens?: number;
  stream?: boolean;
  timeoutMs?: number;
  /** 固定会话 ID：若指定，则复用该会话（如 DeepSeek 网页会话），不再自动查找/新建 */
  sessionId?: string;
}

export interface TokenUsage {
  prompt_tokens: number;
  completion_tokens: number;
  total_tokens: number;
}

export interface ChatResult {
  content: string;
  model: string;
  usage?: TokenUsage;
  finishedReason?: string;
}
