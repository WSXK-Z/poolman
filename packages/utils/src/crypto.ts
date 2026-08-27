const ALPHABET = '0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ';

function getRandomValues(len: number): Uint8Array {
  const bytes = new Uint8Array(len);
  globalThis.crypto.getRandomValues(bytes);
  return bytes;
}

/** 生成安全的随机 ID（默认 21 位，类 nanoid）。浏览器 / Node 通用。 */
export function randomId(length = 21): string {
  const bytes = getRandomValues(length);
  let out = '';
  for (let i = 0; i < length; i++) {
    out += ALPHABET[bytes[i]! % ALPHABET.length];
  }
  return out;
}

/** 生成带前缀的请求 ID，如 req_xxxx */
export function requestId(prefix = 'req'): string {
  return `${prefix}_${randomId(16)}`;
}

/** 生成不含特殊字符的短期 Token（如 ws auth） */
export function generateToken(bytes = 32): string {
  const arr = getRandomValues(bytes);
  return Array.from(arr, (b) => b.toString(16).padStart(2, '0')).join('');
}

/** 常量时间字符串比较（避免时序攻击） */
export function safeEqual(a: string, b: string): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) {
    diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  }
  return diff === 0;
}
