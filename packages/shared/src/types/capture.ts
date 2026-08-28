/**
 * 捕获凭证与接口模板类型（服务端与插件共用）。
 *
 * 内容脚本（mainWorld.content）拦截网页真实请求后，会提取：
 *   - 访问凭证 token（Authorization / x-api-key 等）
 *   - 聊天接口的「完整请求模板」：URL、请求头、请求体结构
 * 这些数据通过 WebSocket 的 CREDENTIAL_SYNC 消息同步给服务端，
 * 由服务端存储、决策并在发起请求时使用（插件只作捕获代理）。
 */

/** 拦截到的平台聊天接口模板（网页前端真实请求的 body 结构，用于自适配转发） */
export interface ChatApiTemplate {
  agent_name?: string;
  model_name?: string;
  think_enabled?: boolean;
  generation_settings?: Record<string, unknown>;
  has_session_id?: boolean;
  has_parent_message_id?: boolean;
  /** 网页真实请求里使用的会话 ID（复用即可，随机 UUID 会被判 invalid） */
  chat_session_id?: string;
  /** 网页真实请求里的父消息 ID */
  parent_message_id?: string | null;
  /** 2026-08 网站真实字段：模型类型（如 'default'） */
  model_type?: string;
  thinking_enabled?: boolean;
  search_enabled?: boolean;
  /** 请求体顶层完整字段名（用于 422 自愈时还原 schema） */
  body_keys?: string[];
}

/** 拦截到的平台聊天接口完整信息 */
export interface CapturedChatApi {
  provider: string;
  url: string;
  headers: Record<string, string>;
  template: ChatApiTemplate;
  token: string;
  capturedAt: number;
}
