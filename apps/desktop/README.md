# Poolman 桌面端

Tauri + Vue 3 + TypeScript 构建的 AI 网关管理工具桌面端应用。

## 项目简介

桌面端是 AI Gateway 的核心管理界面，提供以下功能模块：

- **密钥管理**（`KeysView`）：管理各类 AI 服务商的 API 密钥
- **会话管理**（`SessionView`）：查看和管理 AI 对话会话
- **配置管理**（`ConfigView`）：调整应用全局配置
- **状态监控与供应商管理**（`StatusView`）：查看插件连接与供应商登录状态，并在桌面端启用或停用已登录供应商。桌面端停用会阻止网关 API 使用该供应商，不会更改插件端的连接设置。
- **插件管理**（`PluginManagementView`）：管理多个已连接插件的请求分配状态；供应商详情可控制网页连接和桌面端启用状态，并通过供应商适配器查询、删除网页端会话。目前 DeepSeek 网页端会话管理使用 DeepSeek Web API。

应用同时集成了浏览器扩展（位于 `../extension`），支持在桌面端与浏览器端之间共享配置和状态。

## 技术栈

- **前端框架**：Vue 3 (Composition API + `<script setup>`)
- **UI 组件库**：Naive UI
- **桌面框架**：Tauri v2 (Rust 后端)
- **构建工具**：Vite
- **语言**：TypeScript

## 环境要求

- **Node.js**：>= 18（推荐使用 Volta 管理版本）
- **pnpm**：>= 8
- **Rust**：最新稳定版（用于 Tauri 编译）
- **Windows**：需安装 Visual Studio Build Tools 或 MSVC
- **macOS / Linux**：需安装相应的系统依赖（参考 [Tauri 文档](https://tauri.app/start/prerequisites/)）

## 安装与依赖

在项目根目录执行：

```bash
pnpm install
```

这将安装所有子项目的依赖，包括桌面端、扩展和服务端。

若仅需安装桌面端依赖，可进入 `apps/desktop` 目录后执行：

```bash
pnpm install
```

## 开发模式

### 启动开发服务器

在 `apps/desktop` 目录下运行：

```bash
pnpm dev
```

该命令会：
1. 构建浏览器扩展（`pnpm build:extension`）
2. 启动 Vite 开发服务器（默认端口 `1420`）
3. 打开 Tauri 应用窗口，并自动加载开发服务器

> **注意**：首次运行会下载 Tauri 相关的 Rust 依赖，耗时较长，请耐心等待。

### 开发调试

- **前端调试**：使用浏览器 DevTools（Tauri 窗口内可右键 -> 检查）
- **后端调试**：Rust 代码可通过 `println!` 或日志输出，也可配置 VS Code 的 Rust 调试器
- **扩展协同**：桌面端会加载 `../extension/.output/chrome-mv3` 目录下的扩展资源，修改扩展代码后需重新构建（或重启 `pnpm dev`）

## 构建与打包

### 构建前端资源

```bash
pnpm build
```

该命令会执行 `vue-tsc` 类型检查，然后使用 Vite 打包前端资源到 `dist` 目录。

### 构建完整应用（含扩展）

```bash
pnpm build:all
```

该命令会：
1. 构建浏览器扩展（`pnpm build:extension`）
2. 构建桌面端前端（`pnpm build`）
3. 生成 Tauri 可执行文件（输出到 `src-tauri/target/release`）

### 打包安装程序

Tauri 默认配置为 Windows NSIS 安装包，运行：

```bash
pnpm tauri build
```

生成的安装包位于 `src-tauri/target/release/bundle/nsis/`。

> 若需生成其他平台包（如 `.app`、`.deb`），请修改 `src-tauri/tauri.conf.json` 中的 `bundle.targets` 配置。

## 配置说明

### 应用配置（`tauri.conf.json`）

| 字段 | 说明 |
|------|------|
| `productName` | 产品名称（`poolman`） |
| `version` | 应用版本 |
| `identifier` | 应用唯一标识（`com.wushi.poolman`） |
| `app.windows` | 主窗口默认尺寸和标题 |
| `bundle.resources` | 打包时额外包含的资源（此处将扩展输出目录映射为 `chrome-mv3`） |
| `bundle.icon` | 应用图标路径 |

### 开发与构建命令

- `beforeDevCommand`：开发模式启动前执行的命令（此处为 `pnpm build:extension && pnpm dev`）
- `devUrl`：开发服务器的地址
- `beforeBuildCommand`：构建前执行的命令（此处为 `pnpm build:all`）
- `frontendDist`：前端打包输出目录

## 扩展集成

桌面端会打包并加载位于 `../extension/.output/chrome-mv3` 的浏览器扩展资源。

- **开发时**：需先构建扩展（`pnpm build:extension`）或执行 `pnpm build:all`
- **运行时**：扩展与桌面端通过共享配置和消息通道协同工作
- **修改扩展**：修改扩展代码后，需重新构建扩展，然后重启桌面端应用

> 如需单独调试扩展，可进入 `apps/extension` 目录使用 `wxt` 命令开发。

## 目录结构

```
apps/desktop/
├── src/                     # 前端源码
│   ├── assets/              # 静态资源
│   ├── views/               # 页面组件
│   │   ├── ConfigView.vue   # 配置页面
│   │   ├── KeysView.vue     # 密钥管理
│   │   ├── SessionView.vue  # 会话管理
│   │   └── StatusView.vue   # 状态监控
│   ├── App.vue              # 根组件
│   ├── main.ts              # 入口文件
│   └── config.ts            # 前端配置
├── src-tauri/               # Tauri 后端（Rust）
│   ├── src/                 # Rust 源码
│   │   ├── main.rs          # 主入口
│   │   ├── lib.rs           # 库模块
│   │   ├── apikey.rs        # API 密钥管理
│   │   ├── config.rs        # 配置管理
│   │   ├── session.rs       # 会话管理
│   │   └── ...
│   ├── Cargo.toml           # Rust 依赖
│   └── tauri.conf.json      # Tauri 配置
├── index.html               # HTML 模板
├── package.json             # Node 依赖
├── vite.config.ts           # Vite 配置
└── README.md                # 本文档
```

## 常见问题

### 1. 开发时窗口空白或无法加载

- 确保 Vite 开发服务器已启动（检查 `http://localhost:1420` 是否可访问）
- 检查 `tauri.conf.json` 中的 `devUrl` 是否与 Vite 端口一致
- 尝试关闭防火墙或代理，或设置 `Tauri` 的 `csp` 配置

### 2. 扩展未加载或功能异常

- 确认扩展已构建（`../extension/.output/chrome-mv3` 存在）
- 若扩展代码有更新，需重新构建并重启桌面端
- 检查浏览器扩展的日志（在 Tauri 窗口内无法直接查看，可打开浏览器扩展开发者模式查看）

### 3. 打包失败

- 检查 Rust 工具链是否完整（`rustc --version`）
- 确保已安装 Tauri 系统依赖（参考 [Tauri 文档](https://tauri.app/start/prerequisites/)）
- Windows 下需安装 `Microsoft Visual C++ Build Tools`

### 4. 图标未更新

- 替换 `src-tauri/icons/` 下的图标文件后，需重新构建应用
- 确保图标格式和尺寸符合 Tauri 要求（参考 `tauri.conf.json` 中的 `bundle.icon`）

## 贡献指南

1. 在根目录执行 `pnpm install` 安装依赖
2. 进入 `apps/desktop` 进行开发
3. 提交前运行 `pnpm build` 确保无类型错误
4. 如有 Rust 修改，需使用 `cargo fmt` 和 `cargo clippy` 保持代码风格

## 相关链接

- [Tauri 文档](https://tauri.app/)
- [Vue 3 文档](https://vuejs.org/)
- [Naive UI 文档](https://www.naiveui.com/)
- [项目根目录 README](../../README.md)
