# AI Gateway 产品使用指南

> 复用您已登录的 AI 网页会话（DeepSeek / ChatGPT / Claude / Gemini / Kimi / 通义千问），对外提供 **OpenAI 兼容 API** 的本地网关。零 API Key、零云端依赖、全部在本地运行。

⚠️ **免责声明**：本项目仅供学习研究。自动化操作 AI 网页版可能违反相关服务条款，存在账号风险，请谨慎使用。

---

## 1. 产品简介

AI Gateway（也称 poolman）是一个本地化的 API 网关，它将您已经登录的 AI 网页版（如 DeepSeek、ChatGPT 等）转换为标准的 OpenAI 兼容接口。您可以使用任何支持 OpenAI API 的客户端（如 Cursor、Chatbox、Postman 等）通过本网关调用 AI 能力，而无需购买官方 API Key。

**核心优势**：
- ✅ 零成本：复用已有网页账号，无需额外付费。
- ✅ 隐私安全：所有请求在本地处理，不经过第三方服务器。
- ✅ 统一接口：所有平台均适配为 OpenAI 格式，无缝切换。
- ✅ 轻量级：仅需一个 Chrome 浏览器和 Node.js 环境（或使用桌面端打包版本）。

---

## 2. 快速开始

有两种使用方式：**使用桌面端打包版本（推荐）** 或 **从源码运行**。

### 2.1 使用桌面端打包版本（推荐）

#### 2.1.1 环境准备
- **操作系统**：Windows / macOS / Linux
- **Chrome 浏览器**（用于运行扩展，必须已登录目标 AI 网站）

#### 2.1.2 下载与安装
从 [Releases](https://github.com/your-repo/ai-gateway/releases) 页面下载对应操作系统的最新版本安装包（例如 `poolman_0.1.13_x64-setup.exe`），然后按照安装向导完成安装。

#### 2.1.3 启动桌面端
双击桌面快捷方式或从开始菜单启动应用（应用名称为 `poolman`）。启动后，桌面端会自动启动内置的网关服务：
- HTTP 服务监听 `http://127.0.0.1:13213`
- WebSocket 服务监听 `ws://127.0.0.1:3001`

> 端口可在桌面端设置界面中修改，修改后所有 API 调用需使用新端口。

#### 2.1.4 加载浏览器扩展
桌面端安装包已包含浏览器扩展，但需要您手动加载：
1. 打开 Chrome，访问 `chrome://extensions/`（桌面端「帮助」页的「打开扩展管理页」按钮会调用浏览器打开该页面；找不到浏览器时会复制地址，粘贴到地址栏即可）
2. 开启“开发者模式”
3. 点击“加载已解压的扩展程序”，选择桌面端安装目录下的 `chrome-mv3` 文件夹（通常位于 `C:\Program Files\poolman\resources\chrome-mv3` 或类似路径）

加载成功后，扩展图标会出现在浏览器右上角。

#### 2.1.5 登录 AI 网页
**重要**：扩展需要读取您已登录的 AI 网页会话，请先在浏览器中手动登录您要使用的 AI 服务（如 chat.deepseek.com、chatgpt.com 等）。登录后无需额外操作，扩展会自动识别。

### 2.2 从源码运行（开发/自定义部署）

适合需要自定义修改或调试的用户。

#### 2.2.1 环境准备
- Node.js 18+
- pnpm 8+
- Chrome 浏览器
- Git

#### 2.2.2 克隆与安装
```bash
git clone https://github.com/your-repo/ai-gateway.git
cd ai-gateway
pnpm install
```

#### 2.2.3 启动开发模式
```bash
# 同时启动桌面端与插件（使用 pnpm 递归并行）
pnpm dev
# 或分别启动
pnpm --filter @ai-gateway/desktop dev
pnpm --filter @ai-gateway/extension dev
```

- 网关监听 `http://127.0.0.1:13213`（HTTP）和 `ws://127.0.0.1:3001`（WebSocket）
- 插件构建产物位于 `apps/extension/.output/chrome-mv3`，在 Chrome 扩展程序页面加载此文件夹即可。

#### 2.2.4 构建生产版本
```bash
pnpm build
# 桌面端前端产物：apps/desktop/dist
# 插件产物：apps/extension/dist
```

---

## 3. 配置管理

AI Gateway 无需配置 API Key。您可以通过环境变量或桌面端界面调整行为。

### 3.1 环境变量（内置网关）

配置读取自 `apps/desktop/src-tauri/src/config.rs`，支持以下环境变量：

| 变量名 | 说明 | 默认值 |
|--------|------|--------|
| `HOST` | 监听地址 | `127.0.0.1` |
| `PORT` | HTTP 服务端口 | `13213` |
| `WS_PORT` | WebSocket 端口 | `3001` |
| `REQUEST_TIMEOUT_MS` | 非流式请求的总超时（毫秒） | `120000` |
| `STREAM_IDLE_TIMEOUT_MS` | 流式请求的静默容忍（毫秒）：连续这么久没有新数据才判定上游挂死；期间持续有数据则一直等待 | `180000` |
| `CORS_ORIGIN` | 允许的跨域来源 | `*` |
| `LOG_LEVEL` | 日志级别（`trace`/`debug`/`info`/`warn`/`error`）；插件调用详情等高频日志只在 `debug` 及以下输出 | `info` |

### 3.2 桌面端配置

启动桌面端后，可在“配置管理”界面调整端口、超时时间等，修改后需重启服务生效。

### 3.3 网页渠道并发（会话池）

网页渠道的会话本身是有状态的（同一个网页会话不能并发），但不同会话之间可以并行。网关按**渠道并发数**为每个渠道预建同等数量的网页会话，每个在途请求独占一个会话，因此可并发的请求数就是并发数。

- **会话标题**：池内会话在网页端统一命名为 `poolman_<编号>`（如 `poolman_1`），便于辨认与回收。网页端在会话还没有内容时拒绝改名（应答 `EMPTY_CHAT_SESSION`），因此命名发生在这个会话**首轮对话成功之后**，每个会话只改一次。
- **持久化与复用**：会话信息记录在数据库 `web_sessions` 表；再次启动时先与网页端会话列表核对，仍然存在的直接复用，只有缺失的才补建，不会重复创建。
- **并发数配置**：写在模型行的配置里，键为 `concurrency`（同一渠道多行取最大值，未配置时默认 5，上限 16）。例如把 `models` 表中 `deepseek-web` 渠道任一模型行的 `config` 改为 `{"concurrency": 8}`，重启后该渠道会预建 8 个会话。
- **请求无状态**：每轮请求都把整段对话拼进提示词，网关不再按对话维护网页会话，调用方传入的 `session_id` 不参与网页会话选择。

---

## 4. 调用 API

AI Gateway 提供完全兼容 OpenAI 的 `/v1/chat/completions` 和 `/v1/models` 接口，默认基地址为 `http://127.0.0.1:13213/v1`。

### 4.1 查看可用模型

```bash
curl http://127.0.0.1:13213/v1/models
```

返回示例：
```json
{
  "object": "list",
  "data": [
    { "id": "deepseek-web", "object": "model" },
    { "id": "gpt-4o", "object": "model" },
    { "id": "claude-3-7-sonnet", "object": "model" },
    { "id": "gemini-2.5-pro", "object": "model" },
    { "id": "kimi", "object": "model" },
    { "id": "qwen-max", "object": "model" }
  ]
}
```

### 4.2 非流式调用

```bash
curl http://127.0.0.1:13213/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "deepseek-web",
    "messages": [
      {"role": "user", "content": "你好，请介绍一下自己"}
    ]
  }'
```

### 4.3 流式调用（SSE）

```bash
curl -N http://127.0.0.1:13213/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o",
    "stream": true,
    "messages": [
      {"role": "user", "content": "讲一个简短的笑话"}
    ]
  }'
```

### 4.4 使用 OpenAI SDK

您可以使用任意语言的 OpenAI SDK，只需将 `baseURL` 指向网关地址，并传入任意占位 API Key（例如 `sk-placeholder`）：

**Python 示例**：
```python
from openai import OpenAI

client = OpenAI(
    base_url="http://127.0.0.1:13213/v1",
    api_key="sk-placeholder"  # 任意值
)

response = client.chat.completions.create(
    model="deepseek-web",
    messages=[{"role": "user", "content": "你好"}],
    stream=False
)
print(response.choices[0].message.content)
```

**JavaScript (Node.js) 示例**：
```javascript
import OpenAI from 'openai';

const client = new OpenAI({
    baseURL: 'http://127.0.0.1:13213/v1',
    apiKey: 'sk-placeholder'
});

const response = await client.chat.completions.create({
    model: 'gpt-4o',
    messages: [{ role: 'user', content: '你好' }],
    stream: false
});
console.log(response.choices[0].message.content);
```

---

## 5. 支持的平台与模型映射

| 对外模型名 | 内部 Provider | 对应网页 |
|------------|---------------|----------|
| `gpt-4o` / `gpt-4` / `gpt-3.5-turbo` | `chatgpt-web` | chatgpt.com |
| `claude-3-7-sonnet` / `claude-3-opus` | `claude-web` | claude.ai |
| `deepseek-web`（统一入口） | `deepseek-web` | chat.deepseek.com |
| `gemini-2.5-pro` / `gemini-pro` | `gemini-web` | gemini.google.com |
| `kimi` / `moonshot-v1-8k` | `kimi-web` | kimi.moonshot.cn |
| `qwen-max` / `qwen-plus` | `qwen-web` | tongyi.aliyun.com |

> **注意**：DeepSeek 网页版仅支持单一模型（实际为网页当前选中的版本），对外统一使用 `deepseek-web`。已废弃的 API 模型名（如 `deepseek-chat`）仍作为别名映射到 `deepseek-web`。

如需添加新的平台，可在 `apps/extension/src/providers` 中新增适配器并注册。

---

## 6. 桌面管理工具（可选）

桌面端应用（Tauri + Vue）提供了图形化管理界面，方便您在不使用命令行的情况下进行配置和监控。

### 6.1 启动桌面端

```bash
cd apps/desktop
pnpm dev
```

### 6.2 主要功能

- **密钥管理**：查看当前所有平台的会话状态（登录/未登录），无需手动输入 Key。
- **会话管理**：浏览历史对话记录，支持搜索和筛选。
- **配置管理**：调整服务端口、超时时间、日志级别等。
- **状态监控**：实时查看服务健康状态、连接数和错误日志。

### 6.3 构建生产版本

```bash
cd apps/desktop
pnpm build:all   # 构建扩展 + 桌面端
pnpm tauri build  # 生成安装包（Windows 为 NSIS）
```

生成的安装包位于 `src-tauri/target/release/bundle/`。

---

## 7. 常见问题

### 7.1 扩展无法识别已登录的会话？

- 确保您已在 Chrome 中登录目标 AI 网页，且会话未过期。
- 检查扩展是否正确加载（`chrome://extensions` 中无错误提示）。
- 尝试刷新 AI 网页，然后重新发送请求。

### 7.2 调用返回 404 或超时？

- 确保本地服务已启动（`http://127.0.0.1:13213` 可访问）。
- 检查防火墙是否拦截了端口。
- 查看服务日志（终端输出或桌面端日志）确认是否收到请求。

### 7.3 模型返回的内容不完整或报错？

- 可能是 AI 网页端有反爬机制，可以尝试减少请求频率。
- 检查网络连接，确保 Chrome 可以正常访问 AI 网站。
- 更新扩展代码（`apps/extension/src/providers` 中的选择器可能需要适配网页更新）。

### 7.4 如何更新扩展？

- 使用桌面端时，重新下载安装最新版本即可覆盖。
- 从源码运行时，拉取最新代码后执行 `pnpm build`，然后在 `chrome://extensions` 中点击“重新加载”。

### 7.5 能否同时使用多个平台？

可以。您可以在同一请求中切换不同的 `model` 参数，网关会自动路由到对应的网页。

### 7.6 性能怎么样？

每次请求通过 DOM 操作模拟交互，延迟约 2~5 秒，不适合高并发场景。建议用于个人开发测试或低频率使用。

---

## 8. 故障排查

| 问题现象 | 可能原因 | 解决方法 |
|----------|----------|----------|
| 服务启动失败，端口被占用 | 13213/3001 端口已被其他程序使用 | 修改 `PORT` 和 `WS_PORT` 环境变量或桌面端配置 |
| 扩展加载后无反应 | 未登录或会话过期 | 重新登录目标 AI 网页 |
| 请求返回 `500 Internal Server Error` | 网页返回异常或选择器失效 | 查看终端/桌面端错误日志，更新对应 provider |
| 桌面端构建失败 | Rust 工具链缺失 | 安装 [Tauri 系统依赖](https://tauri.app/start/prerequisites/) |
| 请求返回空内容 | 网页返回的响应里没有正文帧（如网页端报错、会话失效） | 在 `data/gateway.log` 搜 `插件调用结果`（每次插件往返的状态、正文长度与正文开头）与 `网页回答为空`（未解析出正文的那次原始响应） |
| 流式响应中断 | 网络不稳定或网页端限制 | 检查网络，尝试非流式调用 |
| 桌面端找不到扩展资源 | 安装路径不正确或资源未打包 | 重新安装，或手动复制 `chrome-mv3` 到指定位置 |

如果问题仍未解决，请提交 Issue 或查阅项目根目录下的 `链路设计.md` 了解更多实现细节。

---

## 9. 相关链接

- [项目 GitHub 仓库](https://github.com/your-repo/ai-gateway)（请替换为实际地址）
- [Tauri 文档](https://tauri.app/)
- [OpenAI API 文档](https://platform.openai.com/docs/api-reference)
- [Naive UI 组件库](https://www.naiveui.com/)

## 10. 在 VS Code 中使用 AI Gateway

您可以在 VS Code 中配置支持自定义 OpenAI 端点的 AI 扩展（如 Continue、Codeium、或 Copilot Chat 的代理）来使用 AI Gateway。

### 10.1 配置 Continue 扩展

若使用 [Continue](https://continue.dev/) 扩展，可在 `~/.continue/config.json` 或项目根目录的 `.continue/config.json` 中添加以下配置：

```json
{
  "models": [
    {
      "title": "AI Gateway",
      "provider": "openai",
      "model": "deepseek-web",
      "apiBase": "http://127.0.0.1:13213/v1",
      "apiKey": "sk-placeholder"
    }
  ]
}
```

### 10.2 配置 VS Code Copilot 自定义模型

VS Code Copilot 支持通过 `settings.json` 添加自定义模型端点。您可以将 AI Gateway 配置为一个自定义模型，以便在 Copilot Chat 中使用。

在您的 VS Code 设置（`settings.json`）中，添加以下配置到 `github.copilot.chat.models` 数组中（如果该设置不存在，则创建）：

```json
{
  "github.copilot.chat.models": [
    {
      "name": "AGI",
      "vendor": "customendpoint",
      "apiKey": "${input:chat.lm.secret.-38053104}",
      "apiType": "chat-completions",
      "models": [
        {
          "id": "deepseek-web",
          "name": "deepseek-web",
          "url": "http://127.0.0.1:13213/v1",
          "toolCalling": true,
          "vision": true,
          "maxInputTokens": 128000,
          "maxOutputTokens": 16000
        }
      ]
    }
  ]
}
```

> 注意：`apiKey` 字段可填写任意占位值，实际认证由网页会话提供。`url` 需指向 AI Gateway 的基础地址（默认 `http://127.0.0.1:13213/v1`）。您可以根据需要添加更多模型或修改现有模型参数。

保存设置后，重新加载 VS Code 窗口即可使用该模型。

### 10.3 项目配置文件说明

AI Gateway 的模型映射定义在 `packages/shared/src/types/models.ts` 中，您可以添加自定义模型别名。例如：

```typescript
// packages/shared/src/types/models.ts
export const ModelMapping = {
  // ... existing mappings ...
  'my-custom-model': 'deepseek-web', // 将自定义模型名映射到已有 provider
} as const;
```

> 如需添加全新的 AI 平台，请在 `apps/extension/src/providers` 中创建适配器，并参考现有实现编写 `chat` 方法。

---

