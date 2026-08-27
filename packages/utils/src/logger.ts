export type LogLevel = 'debug' | 'info' | 'warn' | 'error';

const LEVEL_ORDER: Record<LogLevel, number> = {
  debug: 10,
  info: 20,
  warn: 30,
  error: 40,
};

/** 读取环境变量（浏览器环境安全降级），避免依赖 Node 类型 */
function resolveDefaultLevel(): LogLevel {
  const maybeProcess = (globalThis as { process?: { env?: Record<string, string | undefined> } }).process;
  const level = maybeProcess?.env?.LOG_LEVEL as LogLevel | undefined;
  return level ?? 'info';
}

let currentLevel: LogLevel = resolveDefaultLevel();

/** 设置全局日志级别 */
export function setLogLevel(level: LogLevel): void {
  currentLevel = level;
}

function shouldLog(level: LogLevel): boolean {
  return LEVEL_ORDER[level] >= LEVEL_ORDER[currentLevel];
}

function formatTime(): string {
  return new Date().toISOString();
}

function write(level: LogLevel, scope: string, message: string, ...args: unknown[]): void {
  if (!shouldLog(level)) return;
  const prefix = `${formatTime()} [${level.toUpperCase()}] (${scope})`;
  const line = `${prefix} ${message}`;
  const stream = level === 'error' ? console.error : level === 'warn' ? console.warn : console.log;
  if (args.length > 0) {
    stream(line, ...args);
  } else {
    stream(line);
  }
}

export interface Logger {
  debug(message: string, ...args: unknown[]): void;
  info(message: string, ...args: unknown[]): void;
  warn(message: string, ...args: unknown[]): void;
  error(message: string, ...args: unknown[]): void;
  child(scope: string): Logger;
}

/** 创建带作用域前缀的日志器 */
export function createLogger(scope: string): Logger {
  return {
    debug: (m, ...a) => write('debug', scope, m, ...a),
    info: (m, ...a) => write('info', scope, m, ...a),
    warn: (m, ...a) => write('warn', scope, m, ...a),
    error: (m, ...a) => write('error', scope, m, ...a),
    child: (sub) => createLogger(`${scope}:${sub}`),
  };
}

/** 默认根日志器 */
export const logger = createLogger('ai-gateway');
