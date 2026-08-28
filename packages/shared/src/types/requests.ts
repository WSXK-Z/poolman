export type ChatRole = 'system' | 'user' | 'assistant' | 'tool';

export interface ChatMessage {
  role: ChatRole;
  content: string | null;     
  name?: string;
  tool_calls?: ToolCall[];          // assistant 角色使用
  tool_call_id?: string;            // tool 角色使用
}

export interface ChatOptions {
  temperature?: number;
  topP?: number;
  maxTokens?: number;
  stream?: boolean;
  timeoutMs?: number;
  /** 固定会话 ID：若指定，则复用该会话（如 DeepSeek 网页会话），不再自动查找/新建 */
  sessionId?: string;
  tools?: Tool[];                 
  tool_choice?: 'auto' | 'none' | { type: 'function'; function: { name: string } }; 
}

export interface TokenUsage {
  prompt_tokens: number;
  completion_tokens: number;
  total_tokens: number;
}

export interface ChatResult {
  content?: string;                 
  tool_calls?: ToolCall[];
  model: string;
  usage?: TokenUsage;
  finishedReason?: string;
}

// ===== 工具定义 =====
export interface FunctionDefinition {
  name: string;
  description?: string;
  parameters?: Record<string, unknown>; // JSON Schema 对象
}
export interface Tool {
  type: 'function';
  function: FunctionDefinition;
}
export interface ToolCall {
  id: string;
  type: 'function';
  function: {
    name: string;
    arguments: string; // JSON 字符串
  };
}
