export interface RetryOptions {
  retries?: number;
  baseDelayMs?: number;
  maxDelayMs?: number;
  /** 指数退避倍数，默认 2 */
  factor?: number;
  /** 是否对特定错误进行重试 */
  shouldRetry?: (err: unknown, attempt: number) => boolean;
  /** 进度回调 */
  onRetry?: (err: unknown, attempt: number) => void;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * 带指数退避的重试工具。
 * @example
 * const data = await retry(() => fetchSomething());
 */
export async function retry<T>(fn: () => Promise<T>, options: RetryOptions = {}): Promise<T> {
  const {
    retries = 3,
    baseDelayMs = 300,
    maxDelayMs = 5000,
    factor = 2,
    shouldRetry = () => true,
    onRetry = () => {},
  } = options;

  let attempt = 0;
  while (true) {
    try {
      return await fn();
    } catch (err) {
      attempt++;
      if (attempt > retries || !shouldRetry(err, attempt)) {
        throw err;
      }
      const delay = Math.min(baseDelayMs * Math.pow(factor, attempt - 1), maxDelayMs);
      onRetry(err, attempt);
      await sleep(delay);
    }
  }
}
