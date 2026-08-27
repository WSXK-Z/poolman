import type { ModelProvider } from '@ai-gateway/shared';
import { ProviderError } from './base';
import { deepseekProvider } from './deepseek';
import { chatgptProvider } from './chatgpt';
import { claudeProvider } from './claude';
import { geminiProvider } from './gemini';
import { kimiProvider } from './kimi';
import { qwenProvider } from './qwen';
import type { AIProvider } from './base';

/** Provider 注册表：ModelProvider 标识 -> 适配器实例 */
const registry = new Map<ModelProvider, AIProvider>();

function register(provider: AIProvider): void {
  registry.set(provider.id, provider);
}

register(deepseekProvider);
register(chatgptProvider);
register(claudeProvider);
register(geminiProvider);
register(kimiProvider);
register(qwenProvider);

/** 根据 Provider 标识获取适配器 */
export function getProvider(id: ModelProvider): AIProvider {
  const provider = registry.get(id);
  if (!provider) {
    throw new ProviderError(`不支持的 Provider: ${id}`, 'unsupported_provider');
  }
  return provider;
}

/** 列出所有注册的 Provider */
export function listProviders(): AIProvider[] {
  return Array.from(registry.values());
}
