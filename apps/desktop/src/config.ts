/** 网关 HTTP 服务地址（可配置，保存到 localStorage）。
 *  - desktop 网关(Rust)：http://127.0.0.1:13213
 *  - TypeScript server：http://127.0.0.1:3000
 */
const KEY = "gateway-base-url";
export const DEFAULT_BASE_URL = "http://127.0.0.1:13213";

export function getBaseUrl(): string {
  return localStorage.getItem(KEY) || DEFAULT_BASE_URL;
}

export function setBaseUrl(url: string): void {
  localStorage.setItem(KEY, url.trim() || DEFAULT_BASE_URL);
}

export function getHealthUrl(): string {
  return `${getBaseUrl().replace(/\/+$/, "")}/health`;
}
/** 桌面端前端共享的应用级常量与配置默认值。 */
