# AI Gateway

复用你已登录的 AI 网页会话（DeepSeek / ChatGPT / Claude / Gemini / Kimi / 通义千问），对外提供 **OpenAI 兼容 API** 的本地网关。零 API Key、零云端依赖、全部在本地运行。

> ⚠️ 本项目仅供学习研究。自动化操作 AI 网页版可能违反相关服务条款，存在账号风险，请谨慎使用。

## 架构

```
外部调用方 (OpenAI 客户端 / Cursor / Chatbox)
        │  HTTP / SSE  (localhost:13213)
        ▼
  Tauri 桌面应用 (Vue3 + NaiveUI + Rust)
   - 提供 OpenAI 兼容 HTTP 服务（基于 Rust 的 axum 或类似）
   - 通过 WebSocket 与 Chrome 插件通信
        │  WebSocket  (localhost:3001)
        ▼
  Chrome 插件 (WXT + Vue3 + NaiveUI)
   - background: WebSocket 客户端、消息路由
   - providers: 各平台 DOM 适配器（chrome.scripting 注入）
        │  chrome.scripting.executeScript (MAIN world)
        ▼
  AI 网页（已登录的浏览器会话）
```

## 目录

```
apps/desktop      Tauri 桌面应用（Vue3 + NaiveUI + Rust）
apps/extension    Chrome 插件（WXT + Vue3 + NaiveUI）
packages/shared   共享类型与 Zod 校验（通信协议）
packages/utils    通用工具（日志/加密/重试）
scripts           辅助脚本
```

## 快速开始

环境要求：Node.js 18+、pnpm 8+、Chrome、Rust（用于桌面应用）

```bash
# 1. 安装依赖
pnpm install

# 2. 开发模式：同时启动桌面应用和插件
pnpm dev
# 或分别启动
pnpm --filter @ai-gateway/desktop dev
pnpm --filter @ai-gateway/extension dev
```

- **桌面应用**：启动后提供 HTTP 服务，默认监听 `http://127.0.0.1:13213`，WebSocket 桥接监听 `ws://127.0.0.1:3001`。同时打开 GUI 窗口用于状态查看和配置。
- **插件**：在 Chrome 扩展程序页（`chrome://extensions`）开启“开发者模式”，加载已解压扩展，选择 `apps/extension/.output/chrome-mv3`（开发模式）或 `apps/extension/dist`（构建后）。插件会自动连接桌面应用的 WebSocket。

## 调用示例

```bash
# 列出模型
curl http://127.0.0.1:13213/v1/models

# 非流式
curl http://127.0.0.1:13213/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-web","messages":[{"role":"user","content":"你好"}]}'

# 流式 (SSE)
curl -N http://127.0.0.1:13213/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-4o","stream":true,"messages":[{"role":"user","content":"讲个笑话"}]}'
```

也可直接使用任意 OpenAI SDK，把 `baseURL` 指向 `http://127.0.0.1:13213/v1`，填入任意假 key。

## 支持平台与模型映射

| 对外模型名 | 内部 Provider | 网页 |
| --- | --- | --- |
| `gpt-4o` / `gpt-4` / `gpt-3.5-turbo` | `chatgpt-web` | chatgpt.com |
| `claude-3-7-sonnet` / `claude-3-opus` | `claude-web` | claude.ai |
| `deepseek-web`（网页版统一入口） | `deepseek-web` | chat.deepseek.com |
| `gemini-2.5-pro` / `gemini-pro` | `gemini-web` | gemini.google.com |
| `kimi` / `moonshot-v1-8k` | `kimi-web` | kimi.moonshot.cn |
| `qwen-max` / `qwen-plus` | `qwen-web` | tongyi.aliyun.com |

> DeepSeek 网页版对外统一暴露 `deepseek-web`（网页没有 API 那样的模型选择器，实际使用网页当前选中的 V4 模型）。已停用的 API 模型名 `deepseek-chat` / `deepseek-reasoner` 仍作为别名映射到 `deepseek-web`。

> 模型映射定义在 `packages/shared/src/types/models.ts`；新增平台只需在 `apps/extension/src/providers` 新增适配器并在 `index.ts` 注册。

## 生产部署

```bash
pnpm build
# 桌面应用构建为各平台可执行文件（参见 apps/desktop/src-tauri/tauri.conf.json）
cd apps/desktop
pnpm tauri build
```

构建产物位于 `apps/desktop/src-tauri/target/release`。

## 常用命令

| 命令 | 说明 |
| --- | --- |
| `pnpm dev` | 并行启动桌面应用与插件开发模式（pnpm 递归并行） |
| `pnpm build` | 全量构建（按依赖顺序，pnpm 递归） |
| `pnpm typecheck` | 先构建共享包，再做全量类型检查 |
| `pnpm lint` | ESLint 检查 |
| `pnpm build:all` | 按依赖顺序构建（shared → utils → desktop → extension） |
| `pnpm package` | 打包发布产物 |

## 风险说明

1. 合规：自动化操作网页版可能违反服务条款，存在封号风险。
2. 稳定性：AI 网站前端频繁更新，`providers/` 下的选择器需持续维护。
3. 性能：每次请求通过 DOM 操作完成，延迟约 2-5 秒，不适合高并发。
4. 资源：AI 网页标签页会保持打开，占用一定内存。
