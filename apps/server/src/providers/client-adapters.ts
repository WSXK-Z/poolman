/**
 * IDE / 客户端适配器：把某个 AI IDE（如 VS Code Copilot / Trae 等）的 messages 结构
 * 翻译成 DeepSeek 的 prompt 分段。
 *
 * 不同 IDE 的「系统提示词 / 用户环境 / 用户提问」结构各不相同：
 *   - VS Code Copilot：system 整段 + 环境 user（含 environment/workspace/memory）+ 含 <userRequest> 的提问
 *   - Trae 等其他 IDE：结构可能不同，各写一个适配器即可，无需改动 DeepSeekService。
 *
 * 只拆分三大类（系统提示词 / 用户环境 / 用户提问），不再对 system 内部按标签块细分。
 */
import type { ChatMessage, Tool } from '@ai-gateway/shared';

/** 工具调用输出格式指令（注入 prompt）：让模型以固定 JSON 块输出工具调用 */
export const TOOL_CALL_INSTRUCTION = `你是一个能够自主调用工具完成任务的中级智能体。收到用户请求后，默认应【主动】判断并调用合适的工具来执行任务（读取/修改文件、执行命令、搜索代码、检索信息等），而不是仅凭已有知识直接回答。
调用工具时你必须【只】输出一个由 \`\`\`tool_call 围起来的 JSON 块，格式严格如下：
\`\`\`tool_call
{"name":"<工具名>","arguments":{<按参数 JSON Schema 填入实际参数>}}
\`\`\`
要求：
1. arguments 必须包含该工具【所有必填参数】的合法值，一个都不能少；
2. 参数类型必须与 JSON Schema 一致：字符串用双引号包裹，数字不加引号，布尔用 true/false，数组用 [...]，对象用 {...}；
3. 只能调用「可用工具」列表中已定义的名称，不要编造工具名或参数；
4. 只输出这一个代码块，不要包含任何其它文字或解释；
5. 任务涉及读写文件、执行终端命令、运行测试、搜索代码、检索资料等时，一律调用工具完成，不要跳过工具直接回答；
6. 对需要精确匹配文本的编辑类工具（如 replace_string_in_file / insert_edit_into_file），必须先调用读取工具获取文件【最新】内容，确保 oldString 与文件当前实际内容逐字符一致（含缩进、换行、注释），再执行编辑，严禁基于过时/记忆中的旧内容构造 oldString；
7. 只有任务确实不需要任何工具（纯问答、闲聊）时，才直接回答用户。`;

/** 阶段1引导：真实提问前先判断是否需要调用工具（提高工具调用倾向） */
export const TOOL_PHASE1_GUIDE = `请先判断：针对以下用户问题，你默认应该【主动调用工具】来完成任务，不要直接回答。只要任务可能涉及以下任一项，就必须调用工具：
- 读取、查看、修改文件/目录/代码；
- 执行终端命令、运行脚本或测试；
- 搜索代码、检索资料、获取外部信息。
需要调用工具 → 输出一个由 \`\`\`tool_call 围起来的 JSON 块（规范工具调用）。
只有当任务是纯知识问答或闲聊、完全不涉及任何工具时，才直接回答用户。`;

/** 阶段2引导：拿到工具结果后判断是否还需工具 / 任务是否完成（循环直到任务真正完成） */
export const TOOL_PHASE2_GUIDE = `结合下面提供的工具结果，以及之前的对话上下文，请判断任务是否已经可以完成：
- 如果任务还需要调用工具才能完成（例如：修改文件、执行终端命令、继续检索信息等）→ 输出一个由 \`\`\`tool_call 围起来的 JSON 块（规范工具调用），继续执行，直到任务真正完成；
- 只有任务已全部完成、不再需要任何工具时，才直接给出最终答案；
- 不要为了省事而跳过必要的工具调用直接回答，也不要凭空编造工具的执行结果。`;

/** 把 OpenAI tools 定义渲染成人类可读文本（注入 prompt 供模型了解可用工具） */
export function renderTools(tools: Tool[]): string {
  return (tools ?? [])
    .map((t, i) => {
      const fn = t?.function ?? {};
      const name = String(fn?.name ?? '');
      const desc = String(fn?.description ?? '');
      const params = (fn?.parameters ?? {}) as {
        properties?: Record<string, unknown>;
        required?: string[];
      };
      const props = params.properties ? JSON.stringify(params.properties) : '{}';
      const required =
        Array.isArray(params.required) && params.required.length > 0
          ? params.required.join(', ')
          : '（全部可选）';
      return (
        `${i + 1}. 名称: ${name}\n` +
        `   描述: ${desc}\n` +
        `   必填参数: ${required}\n` +
        `   参数(JSON Schema): ${props}`
      );
    })
    .join('\n');
}

/** 工具「记住」段文本（由网关统一追加，不依赖 IDE） */
export function buildToolRememberText(tools: Tool[]): string {
  return `以下是本次可调用的工具，你只需记住，不需要回答我：\n${TOOL_CALL_INSTRUCTION}\n\n可用工具：\n${renderTools(tools)}`;
}

/**
 * 取消息末尾连续的工具结果（当次工具调用的返回），不含之前轮次的工具结果。
 * 客户端每轮把最新工具结果追加在消息末尾，之前的 tool 消息属于更早轮次，不应重复携带。
 */
export function getLatestToolResults(messages: ChatMessage[], max = 5): ChatMessage[] {
  const out: ChatMessage[] = [];
  for (let i = messages.length - 1; i >= 0; i--) {
    if (messages[i].role !== 'tool') break;
    out.unshift(messages[i]);
    if (out.length >= max) break;
  }
  return out;
}

/**
 * 把当次的工具结果（role='tool'）渲染成文本，供追问 prompt 拼接。
 * 只带消息末尾连续的工具结果 + 当前提问，不把历史轮次的工具结果重复带上。
 */
export function renderToolResults(messages: ChatMessage[], max = 5): string {
  const parts: string[] = [];
  for (const t of getLatestToolResults(messages, max)) {
    if (t.content) {
      const label = t.tool_call_id ? `工具(${t.tool_call_id})返回的结果：` : '工具返回的结果：';
      parts.push(`${label}\n${t.content}`);
    }
  }
  return parts.join('\n\n');
}

/**
 * IDE / 客户端适配器接口。
 * 只拆分三大类：系统提示词（整段）/ 用户环境 / 用户提问，具体如何识别取决于各 IDE。
 */
export interface ClientAdapter {
  readonly id: string;
  /** 该消息集合是否属于本适配器（通过特征识别，如 <userRequest> 标记） */
  matches(messages: ChatMessage[]): boolean;
  /** 首次提问基础分段：[系统提示词记住段?, 用户环境记住段?, 提问段]（system 整段不细分） */
  buildFirstTurnSegments(messages: ChatMessage[]): string[];
  /** 追问 prompt（当前提问 + 当次工具结果） */
  buildFollowUpPrompt(messages: ChatMessage[]): string;
}

/** VS Code Copilot 适配器：识别含 <userRequest> 标记的 Copilot 消息结构 */
export const vscodeCopilotAdapter: ClientAdapter = {
  id: 'vscode-copilot',
  matches(messages: ChatMessage[]): boolean {
    return messages.some((m) => (m.content ?? '').includes('<userRequest>'));
  },
  buildFirstTurnSegments(messages: ChatMessage[]): string[] {
    const system = messages.find((m) => m.role === 'system')?.content;
    // 用户环境：第一条不含 <userRequest> 的 user（含 environment/workspace/memory）
    const envUser = messages.find(
      (m) => m.role === 'user' && !(m.content ?? '').includes('<userRequest>'),
    )?.content;
    // 提问：最后一条含 <userRequest> 的 user
    const lastTurnUser = [...messages]
      .reverse()
      .find((m) => m.role === 'user' && (m.content ?? '').includes('<userRequest>'));
    const req = lastTurnUser?.content?.match(/<userRequest>\s*([\s\S]*?)\s*<\/userRequest>/);

    // 非标准结构兜底：直接把最后一条 user 内容作为提问
    if (!system && !lastTurnUser) {
      return [[...messages].reverse().find((m) => m.role === 'user')?.content ?? ''];
    }

    const parts: string[] = [];
    // 系统提示词整段一条（不再按标签块细分）
    if (system) parts.push(`以下是你要遵守的系统提示词，你只需记住，不需要回答我：\n${system}`);
    if (envUser) parts.push(`以下是用户的环境信息，你只需记住，不需要回答我：\n${envUser}`);
    if (req?.[1]?.trim()) parts.push(`用户的问题：\n${req[1].trim()}`);
    return parts;
  },
  buildFollowUpPrompt(messages: ChatMessage[]): string {
    const lastTurnUser = [...messages]
      .reverse()
      .find((m) => m.role === 'user' && (m.content ?? '').includes('<userRequest>'));
    if (lastTurnUser) {
      const lastReq = lastTurnUser.content?.match(/<userRequest>\s*([\s\S]*?)\s*<\/userRequest>/);
      const question = lastReq?.[1]?.trim() ?? lastTurnUser.content ?? '';
      const toolResults = renderToolResults(messages);
      return toolResults ? `${toolResults}\n\n${question}` : question;
    }
    return genericAdapter.buildFollowUpPrompt(messages);
  },
};

/** 通用适配器（标准 OpenAI 风格，无 <userRequest>）：system 整段 + 最后一条 user 作为提问 */
export const genericAdapter: ClientAdapter = {
  id: 'generic',
  matches(): boolean {
    return true;
  },
  buildFirstTurnSegments(messages: ChatMessage[]): string[] {
    const system = messages.find((m) => m.role === 'system')?.content;
    const parts: string[] = [];
    if (system) parts.push(`以下是你要遵守的系统提示词，你只需记住，不需要回答我：\n${system}`);
    const lastUser = [...messages].reverse().find((m) => m.role === 'user');
    if (lastUser?.content) parts.push(`用户的问题：\n${lastUser.content}`);
    return parts.length > 0 ? parts : [''];
  },
  buildFollowUpPrompt(messages: ChatMessage[]): string {
    const parts: string[] = [];
    const toolResults = renderToolResults(messages);
    if (toolResults) parts.push(toolResults);
    const lastUser = [...messages].reverse().find((m) => m.role === 'user');
    if (lastUser?.content) parts.push(`用户：\n${lastUser.content}`);
    return parts.join('\n\n');
  },
};

/** 全部适配器（按顺序匹配，通用适配器兜底） */
export const clientAdapters: ClientAdapter[] = [vscodeCopilotAdapter, genericAdapter];
