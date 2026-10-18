/**
 * 模型映射：对外暴露的 OpenAI 风格模型名 -> 插件内部 Provider 标识。
 * 新增 AI 平台时，在此追加映射并在 apps/extension/src/providers 中注册适配器。
 */
export const ModelMapping = {
  // ChatGPT
  'gpt-4o': 'chatgpt-web',
  'gpt-4o-mini': 'chatgpt-web',
  'gpt-4': 'chatgpt-web',
  'gpt-3.5-turbo': 'chatgpt-web',
  // Claude
  'claude-3-7-sonnet': 'claude-web',
  'claude-3-5-sonnet': 'claude-web',
  'claude-3-opus': 'claude-web',
  // DeepSeek 网页版：统一入口（使用网页当前选中的 V4 模型，思考模式为页面内开关）。
  // 注意：deepseek-v4-flash / deepseek-v4-pro 是官方 API 的模型名，网页版没有对应的模型选择，故不作为对外模型暴露。
  'deepseek-web': 'deepseek-web',
  // 已停用的 DeepSeek API 模型名（映射到网页版，向后兼容）
  'deepseek-chat': 'deepseek-web',
  'deepseek-reasoner': 'deepseek-web',
  // Gemini
  'gemini-2.5-pro': 'gemini-web',
  'gemini-2.0-flash': 'gemini-web',
  'gemini-pro': 'gemini-web',
  // Kimi
  'kimi': 'kimi-web',
  'moonshot-v1-8k': 'kimi-web',
  // Qwen
  'qwen-max': 'qwen-web',
  'qwen-plus': 'qwen-web',
  // 腾讯元宝
  'yuanbao-web': 'yuanbao-web',
} as const;

/** 对外暴露的模型名 */
export type PublicModel = keyof typeof ModelMapping;

/** 插件内部 Provider 标识（由模型映射推导） */
export type ModelProvider = (typeof ModelMapping)[keyof typeof ModelMapping];

/** 所有 Provider 的列表 */
export const PROVIDERS: ModelProvider[] = Array.from(new Set(Object.values(ModelMapping)));

/** 由 Provider 反查其支持的对外模型名 */
export function modelsForProvider(provider: ModelProvider): PublicModel[] {
  return (Object.entries(ModelMapping) as [PublicModel, ModelProvider][])
    .filter(([, p]) => p === provider)
    .map(([model]) => model);
}

export interface ProviderStatus {
  provider: ModelProvider;
  connected: boolean;
  authed: boolean;
  models: string[];
  lastChecked: number;
  error?: string;
  /** 是否已捕获访问凭证（API 转发平台） */
  tokenCaptured?: boolean;
  /** 是否已捕获接口模板（API 转发平台） */
  templateCaptured?: boolean;
}

export type ProviderStatusMap = Partial<Record<ModelProvider, ProviderStatus>>;
