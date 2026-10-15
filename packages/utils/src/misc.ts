/**
 * 跨运行环境通用的小型辅助函数。
 *
 * 只包含不依赖扩展 API、Tauri 或 Node 专属对象的时间和错误格式工具。
 */

/** Promise 版 sleep */
export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** 高精度耗时 */
export async function withTiming<T>(fn: () => Promise<T>): Promise<{ result: T; elapsedMs: number }> {
  const start = performance.now();
  const result = await fn();
  return { result, elapsedMs: Math.round(performance.now() - start) };
}

/** 将任意错误转为可读字符串 */
export function errorMessage(err: unknown): string {
  if (err instanceof Error) return err.message;
  if (typeof err === 'string') return err;
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}
