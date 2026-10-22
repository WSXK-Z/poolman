# @dp_ui/core 前端 UI 设计文档

> 设计体系 **v1.0.0**（2026-10）· 来源：`dp-ui-core-design-system.html`
> 适用平台：响应式 Web · 桌面应用 · Android 应用
> 目标：面向前端团队的 Token、组件与用法规范。所有组件只消费语义 Token，不直接引用原始色值。

---

## 目录

1. [概览](#1-概览)
2. [设计原则](#2-设计原则)
3. [色彩](#3-色彩)
4. [字体](#4-字体)
5. [间距与栅格](#5-间距与栅格)
6. [形态](#6-形态)
7. [动效](#7-动效)
8. [组件规范](#8-组件规范)
9. [可访问性](#9-可访问性)
10. [使用规范](#10-使用规范)
11. [覆盖与版本](#11-覆盖与版本)
12. [附录：Token 速查](#12-附录token-速查)

---

## 1. 概览

| 指标 | 数值 | 说明 |
| --- | --- | --- |
| Semantic Tokens | **18** | 色彩语义变量，亮/暗双主题同源 |
| Components | **12** | 核心组件族 |
| Platforms | **3** | Web · Desktop · Android |
| Contrast | **AA** | 正文 4.5:1 起 |

### 1.1 规范层级

```
原始色阶 / 尺寸原语  →  语义 Token  →  组件与页面
   oklch 50…900          --accent       Button / Input / …
     --space-*           --border       …
```

- **原始色阶只用于生成语义 Token**，业务与组件代码禁止直接引用。
- 主题切换只替换语义 Token 的值，组件零改动。
- 所有颜色以 **OKLch** 表达，保证跨色相的明度对齐与感知均匀。

> 命名说明：本文档使用设计层语义名（`--bg`、`--surface`、`--accent` 等）。实现层若沿用既有 `--dp-*` 前缀，应保持语义一一对应，并同样禁止在组件内直接引用原始色阶。

---

## 2. 设计原则

四条约束，用于在具体实现出现分歧时做取舍。

| 编号 | 原则 | 要求 |
| --- | --- | --- |
| **P—01** | **克制的对比** | 层级优先由字重、字距与留白建立。颜色只用来标记状态与可操作性，不作为装饰手段。 |
| **P—02** | **一个强调色** | 主色只出现在可操作元素与当前状态上，单屏出现不超过两处。铜金（`--brass`）仅用于编号与分隔等结构性点缀。 |
| **P—03** | **可预测的密度** | 间距一律取 4px 的倍数，桌面默认 8px 节奏。不允许出现 13px、27px 这类脱离阶梯的数值。 |
| **P—04** | **状态先于装饰** | 每个可交互元素必须完整定义 hover、focus、active、disabled 四态。缺态的组件不允许合入主干。 |

---

## 3. 色彩

原始色阶只用于生成语义 Token；业务代码引用语义层，切换主题时无需改组件。

### 3.1 品牌色阶 · Accent

| 色阶 | OKLch |
| --- | --- |
| accent-50 | `oklch(97% 0.02 268)` |
| accent-100 | `oklch(94% 0.04 268)` |
| accent-200 | `oklch(88% 0.07 268)` |
| accent-300 | `oklch(80% 0.10 268)` |
| accent-400 | `oklch(68% 0.14 268)` |
| accent-500 | `oklch(56% 0.17 268)` |
| accent-600 | `oklch(50% 0.17 268)` |
| accent-700 | `oklch(43% 0.16 268)` |
| accent-800 | `oklch(35% 0.13 268)` |
| accent-900 | `oklch(27% 0.09 268)` |

`accent-600` 是亮色主题的语义锚点（`--accent`）。

### 3.2 中性色阶 · Graphite

| 色阶 | OKLch |
| --- | --- |
| neutral-0 | `oklch(100% 0 0)` |
| neutral-50 | `oklch(98% 0.002 260)` |
| neutral-100 | `oklch(96% 0.003 260)` |
| neutral-200 | `oklch(92% 0.005 260)` |
| neutral-300 | `oklch(86% 0.007 260)` |
| neutral-400 | `oklch(74% 0.009 260)` |
| neutral-500 | `oklch(60% 0.010 260)` |
| neutral-600 | `oklch(48% 0.012 260)` |
| neutral-700 | `oklch(38% 0.013 262)` |
| neutral-800 | `oklch(28% 0.014 264)` |

### 3.3 语义 Token（18 个）

| Token | 用途 | Light | Dark |
| --- | --- | --- | --- |
| `--bg` | 页面底色 | `oklch(99.2% 0.002 255)` | `oklch(15.5% 0.012 265)` |
| `--surface` | 卡片与面板 | `oklch(100% 0 0)` | `oklch(18.5% 0.014 265)` |
| `--surface-2` | 次级面板 / 表头 | `oklch(97.4% 0.004 255)` | `oklch(22% 0.015 265)` |
| `--surface-3` | 填充 / 骨架屏 | `oklch(95.2% 0.006 255)` | `oklch(26% 0.016 265)` |
| `--fg` | 正文与标题 | `oklch(17% 0.015 265)` | `oklch(95% 0.005 260)` |
| `--fg-2` | 次级文字 | `oklch(40% 0.012 262)` | `oklch(80% 0.01 260)` |
| `--muted` | 说明 / 占位 | `oklch(56% 0.01 260)` | `oklch(63% 0.012 260)` |
| `--border` | 常规分隔线 | `oklch(91% 0.006 260)` | `oklch(27% 0.014 265)` |
| `--border-strong` | 输入框与可交互描边 | `oklch(83% 0.008 260)` | `oklch(36% 0.016 265)` |
| `--accent` | 主操作与当前状态 | `oklch(50% 0.17 268)` | `oklch(70% 0.14 268)` |
| `--accent-hover` | 主操作悬停 | `oklch(45% 0.18 268)` | `oklch(76% 0.13 268)` |
| `--accent-soft` | 选中底 / 弱强调 | `oklch(95.5% 0.025 268)` | `oklch(24% 0.05 268)` |
| `--accent-fg` | 主色上的文字 | `oklch(99% 0.005 268)` | `oklch(16% 0.02 268)` |
| `--brass` | 结构性点缀（编号 / 分隔） | `oklch(60% 0.10 70)` | `oklch(72% 0.09 70)` |
| `--ok` | 成功 | `oklch(56% 0.14 155)` | `oklch(70% 0.13 155)` |
| `--warn` | 警告 | `oklch(68% 0.14 75)` | `oklch(78% 0.13 75)` |
| `--danger` | 错误与破坏性操作 | `oklch(55% 0.18 25)` | `oklch(66% 0.16 25)` |
| `--info` | 中性提示 | `oklch(58% 0.12 240)` | `oklch(70% 0.11 240)` |

### 3.4 使用规则

- 单屏主色（`--accent`）出现不超过两处：主操作与当前状态。
- 语义色（ok / warn / danger / info）只标记状态，且必须同时提供文案或图标，不能只靠颜色。
- 原始色阶、十六进制色值不得进入组件与业务代码。
- 大面积背景使用 `--bg` / `--surface*`，不使用渐变铺底。

---

## 4. 字体

### 4.1 字体族

| 角色 | 字体栈 | 用途 |
| --- | --- | --- |
| DISPLAY | `-apple-system, BlinkMacSystemFont, "SF Pro Display", "Segoe UI Variable Display", "Segoe UI", system-ui, sans-serif` | 标题、大字号数字 |
| BODY | `-apple-system, BlinkMacSystemFont, "SF Pro Text", "Segoe UI Variable Text", "Segoe UI", system-ui, sans-serif` | 正文与界面文字 |
| MONO | `"JetBrains Mono", "SF Mono", ui-monospace, Menlo, Consolas, monospace` | 数值、代码、Token、编号 |

系统字体栈优先，避免字体加载抖动。

### 4.2 字号阶梯

| 名称 | 字号 / 行高 | 字重 · 字距 | 用途 |
| --- | --- | --- | --- |
| display-2xl | 72 / 0.98 | 600 · -0.035em | 首屏主张（Display 字体） |
| display-xl | 52 / 1.05 | 600 · -0.03em | 页面主标题 |
| display-lg | 38 / 1.15 | 600 · -0.025em | 区块标题 |
| heading-lg | 28 / 1.3 | 600 · -0.02em | 卡片组标题 |
| heading-md | 21 / 1.45 | 600 · -0.015em | 小节标题 |
| heading-sm | 17 / 1.5 | 600 · -0.01em | 列表项标题 |
| body-lg | 17 / 1.65 | 400 | 引导段落与说明 |
| body | 15 / 1.65 | 400 | 默认正文 |
| body-sm | 13 / 1.55 | 400 | 辅助说明、表格单元格、表单提示 |
| caption | 12 / 1.5 | 500 · 0.01em | 元信息与时间戳 |
| overline | 11 / 1.45 | 600 · 0.16em · uppercase | 分组标签（Mono） |

### 4.3 排版规则

- 中文正文行高不低于 **1.6**；英文可使用更紧的 1.5。
- 数字统一使用等宽并开启 `font-variant-numeric: tabular-nums`，保证表格与统计数值纵向对齐。
- 标题使用负字距，字号越大字距越紧；正文字距为 0。
- 全大写仅用于 overline / 表头，并配合 `letter-spacing` 提升可读性。

---

## 5. 间距与栅格

### 5.1 间距阶梯

4px 基准，所有间距取阶梯值。

| Token | 值 | Token | 值 |
| --- | --- | --- | --- |
| `--space-1` | 4px | `--space-8` | 32px |
| `--space-2` | 8px | `--space-10` | 40px |
| `--space-3` | 12px | `--space-12` | 48px |
| `--space-4` | 16px | `--space-16` | 64px |
| `--space-5` | 20px | `--space-20` | 80px |
| `--space-6` | 24px | `--space-24` | 96px |

- 桌面默认 8px 节奏；组件内部常用 8 / 12 / 16 / 20 / 24。
- 不允许出现 13px、27px 等脱离阶梯的数值；布局优先用 Grid/Flex 的 `gap`，不靠 margin 堆叠。

### 5.2 页面栅格

- 内容区最大宽度：**1140px**，水平居中。
- 栅格：**12 列**，列间距 **24px**。
- 左右内边距随断点变化（16 / 24 / 48px）。

### 5.3 断点

| 断点 | 宽度 | 布局约定 |
| --- | --- | --- |
| `sm` | 640px | 单列，16px 边距 |
| `md` | 768px | 两列，24px 边距 |
| `lg` | 1024px | 侧栏出现，24px 边距 |
| `xl` | 1280px | 内容居中，48px 边距 |
| `2xl` | 1536px | 最大宽度锁定 |

### 5.4 版式节奏（文档站基准）

| 区域 | 规格 |
| --- | --- |
| 侧栏 | 252px，右侧 1px 描边，粘性定位 |
| Hero 内边距 | 上 96px / 下 64px |
| 章节 | 上下 80px，底部 1px 分隔线 |
| 章节标题区 | 编号（Mono 12px `--brass`）+ 标题 28px + 描述 15px `--fg-2`（最大 62ch），间距 16px |
| 指标卡 | 4 列，1px 间隙用 `--border` 填充，外层 1px 描边 + 14px 圆角 |

---

## 6. 形态

> 圆角随元素尺寸递增；阴影只用于浮层与悬停反馈；常规面板一律用 1px 描边。

### 6.1 圆角

| Token | 值 | 用途 |
| --- | --- | --- |
| `--radius-xs` | 4px | 色块、焦点环、微型标记 |
| `--radius-sm` | 6px | Tag、小按钮、分页、骨架 |
| `--radius-md` | 10px | 按钮、输入框、Tabs、Toast |
| `--radius-lg` | 14px | 卡片、面板、文档区块 |
| `--radius-xl` | 20px | Dialog 等大浮层 |
| `--radius-full` | 999px | 胶囊、开关轨道、头像 |

### 6.2 阴影

| Token | 值 | 用途 |
| --- | --- | --- |
| `--shadow-1` | `0 1px 2px oklch(20% 0.02 265 / .06), 0 1px 1px oklch(20% 0.02 265 / .04)` | 激活 Tab、开关滑块、轻浮层 |
| `--shadow-2` | `0 4px 14px -4px oklch(20% 0.02 265 / .12), 0 1px 3px oklch(20% 0.02 265 / .06)` | 悬停卡片、Popover |
| `--shadow-3` | `0 18px 44px -16px oklch(20% 0.02 265 / .22), 0 4px 12px -4px oklch(20% 0.02 265 / .08)` | Dialog、Toast 等最高层 |

暗色主题下阴影改为纯黑低透明度（0.4 / 0.55 / 0.7）。

### 6.3 描边与焦点

- 常规分隔线：`1px solid var(--border)`。
- 可交互控件描边：`1px solid var(--border-strong)`。
- 焦点环：`--ring = 0 0 0 3px oklch(50% 0.17 268 / 0.22)`（暗色 0.28），圆角取 `--radius-xs`。
- 禁止使用 `outline: none` 直接移除焦点反馈。

---

## 7. 动效

> 动效只用于表达状态变化与空间关系。超过 320ms 的过渡仅用于大面积浮层入场。

### 7.1 时长

| Token | 值 | 用途 |
| --- | --- | --- |
| `--dur-instant` | 80ms | 按压位移、即时状态 |
| `--dur-fast` | 140ms | hover、颜色变化、Toast 退场 |
| `--dur-base` | 200ms | 主题切换、开关、Toast 入场 |
| `--dur-slow` | 320ms | 面板展开 |
| `--dur-slower` | 480ms | 大面积浮层入场 |

### 7.2 缓动

| 名称 | 曲线 | 用途 |
| --- | --- | --- |
| standard | `cubic-bezier(0.2, 0, 0, 1)` | 默认 |
| enter | `cubic-bezier(0.16, 1, 0.3, 1)` | 入场、开关滑块 |
| exit | `cubic-bezier(0.4, 0, 1, 1)` | 退场 |

### 7.3 典型动效

- **按钮按压**：`translateY(1px)`，80ms。
- **Toast**：入场 200ms（`translateY(8px) scale(.98)` → 原位），退场 140ms（`translateY(4px)` 淡出）。
- **骨架屏**：1.6s 无限循环的横向高光渐变。
- **主题切换**：背景与文字颜色 200ms 过渡。
- **减动效**：`prefers-reduced-motion: reduce` 下动画与过渡时长压到 `0.01ms`，滚动改为 instant。

---

## 8. 组件规范

### 8.1 Button

**变体**：`primary` · `secondary` · `ghost` · `danger`；**尺寸**：sm · md · lg；**状态**：default / hover / active / disabled / focus-visible。

| 属性 | md（默认） | sm | lg |
| --- | --- | --- | --- |
| 高度 | 38px | 30px | 46px |
| 水平内边距 | 16px | 12px | 24px |
| 圆角 | `--radius-md` (10px) | `--radius-sm` (6px) | `--radius-md` |
| 字号 | `--text-sm` (13px) | `--text-xs` (12px) | `--text-base` (15px) |
| 字重 | 500 | 500 | 500 |

| 变体 | 背景 | 文字 | 描边 | Hover |
| --- | --- | --- | --- | --- |
| primary | `--accent` | `--accent-fg` | 透明 | `--accent-hover` |
| secondary | `--surface` | `--fg` | `--border-strong` | `--surface-2` |
| ghost | 透明 | `--fg-2` | 透明 | `--surface-2`，文字 `--fg` |
| danger | 透明 | `--danger` | `--danger` 35% 混合 | `--danger` 10% 混合 |

- 内容布局：`inline-flex` 居中，图标与文字间距 8px。
- 过渡：背景 / 描边 / 文字 140ms；按压位移 80ms。
- 禁用：`opacity: .45`，`cursor: not-allowed`，且保留语义标记。
- IconButton：同高度、正方形，必须提供 `aria-label`。

### 8.2 Input / Textarea

| 属性 | 规格 |
| --- | --- |
| 高度 | 38px（Textarea 最小高度同） |
| 水平内边距 | 12px |
| 描边 | `1px --border-strong` |
| 圆角 | `--radius-md` |
| 背景 / 文字 | `--surface` / `--fg` |
| 字号 | `--text-sm` |
| Placeholder | `--muted` |

| 状态 | 表现 |
| --- | --- |
| hover | 描边 `--fg-2` |
| focus | 描边 `--accent` + `--ring` |
| error | 描边 `--danger`；聚焦时 `0 0 0 3px` danger 22% |
| disabled | 背景 `--surface-2`，文字 `--muted`，`not-allowed` |

**字段结构**：label（12px / 500 / `--fg-2`）→ 控件 → hint（11px / `--muted`）。label 与控件用 `for` / `id` 关联；错误提示用 `aria-describedby` 绑定，颜色改为 `--danger`。

**Select**：与 Input 同规格，`appearance: none`，右侧 5px 自绘箭头（`--muted`），定位 `calc(100% - 16px)`。

### 8.3 Switch

| 属性 | 规格 |
| --- | --- |
| 轨道 | 40 × 22px，`--radius-full`，背景 `--surface-3`，描边 `--border-strong` |
| 滑块 | 16 × 16px 圆形，距边 2px，背景 `--surface`，`--shadow-1` |
| 选中 | 轨道 `--accent`，滑块 `translateX(18px)` |
| 过渡 | 200ms；滑块使用 enter 缓动 |
| 焦点 | 轨道显示 `--ring` |
| 文案 | 轨道右侧间距 12px，13px `--fg-2` |

### 8.4 Tag / Badge

| 属性 | 规格 |
| --- | --- |
| 高度 | 22px |
| 水平内边距 | 8px |
| 圆角 | `--radius-sm` |
| 字号 / 字重 | 11px / 500 |
| 默认 | 背景 `--surface-3`，文字 `--fg-2`，描边 `--border` |
| 状态点 | 5px 圆点，`currentColor`，透明度 0.85 |

| 变体 | 背景 | 文字 |
| --- | --- | --- |
| accent | `--accent-soft` | `--accent` |
| ok | `ok` 14% 混合 | `--ok` |
| warn | `warn` 18% 混合 | `--warn` |
| danger | `danger` 12% 混合 | `--danger` |
| plain | 同默认 | 同默认（不显示状态点） |

### 8.5 Table

- 无斑马纹；行间仅用 1px `--border` 分隔，最后一行去掉分隔线。
- 表头：11px、500、大写、`letter-spacing: .08em`、`--muted`，内边距 8px 12px。
- 单元格：内边距 12px，字号 13px。
- 数值列：右对齐 + Mono + `tabular-nums`。
- 语义：使用 `thead` / `th`；数值列声明对齐方式。

### 8.6 Tabs

| 属性 | 规格 |
| --- | --- |
| 容器 | `inline-flex`，gap 2px，padding 3px，背景 `--surface-2`，圆角 10px，描边 `--border` |
| 标签 | 高 30px，padding 0 12px，圆角 6px，字号 12px / 500，默认 `--muted` |
| hover | 文字 `--fg` |
| 选中 | 背景 `--surface`，文字 `--fg`，`--shadow-1` |
| 切换 | 即时切换，不等待网络 |

### 8.7 Pagination

| 属性 | 规格 |
| --- | --- |
| 按钮 | 最小宽 32px、高 32px，圆角 6px，Mono 12px，文字 `--fg-2` |
| hover | 背景 `--surface-2` |
| 当前页 | 背景 `--accent`，文字 `--accent-fg` |
| 布局 | 与内容同排，gap 2px |

### 8.8 Card / Panel

| 属性 | 规格 |
| --- | --- |
| 背景 | `--surface` |
| 描边 | `1px --border` |
| 圆角 | `--radius-lg` (14px) |
| 内边距 | 20px |
| Hover | 描边 `--border-strong` + `--shadow-2` |
| 标题 | 13px / 600 |
| 正文 | 12px / `--muted` / 行高 1.6，与标题间距 8px |

常规面板不加阴影；阴影只作为悬停或浮层反馈。

### 8.9 Avatar

| 属性 | 规格 |
| --- | --- |
| 尺寸 | 34 × 34px，圆形 |
| 背景 / 文字 | `--surface-3` / `--fg-2` |
| 字号 | 12px / 600 |
| 描边 | 1px `--border` |
| accent 变体 | 背景 `--accent-soft`，文字 `--accent`，无描边 |

### 8.10 Skeleton / Empty

| 属性 | 规格 |
| --- | --- |
| 基础块 | 高 12px，圆角 6px，背景 `--surface-3` |
| 高光 | 横向渐变（surface 60% → 透明），1.6s 无限循环 |
| 原则 | 骨架尺寸与最终内容一致，避免布局跳动 |
| Empty | 保持与列表/表格相同的占位高度，并提供说明文案与主操作 |

### 8.11 Dialog / Drawer

| 属性 | 规格 |
| --- | --- |
| 宽度 | `min(440px, 100vw - 32px)` |
| 圆角 | `--radius-xl` (20px) |
| 背景 / 文字 | `--surface` / `--fg` |
| 阴影 | `--shadow-3` |
| 遮罩 | 纯黑 42% + `blur(3px)` |
| 头部 | padding 20px 24px 0；标题 17px / 600 / -0.015em；描述 13px / `--fg-2` / 1.65 |
| 底部 | 右对齐，gap 8px，padding 20px 24px 24px |
| 行为 | 打开后焦点进入首个可操作元素；Esc 关闭；焦点捕获 |

**使用边界**：破坏性操作必须走 Dialog 二次确认；Drawer 与 Dialog 共用规格与层级。

### 8.12 Toast / Notification

| 属性 | 规格 |
| --- | --- |
| 位置 | 右下角，距视口 24px，纵向 gap 8px，`z-index: 200` |
| 背景 / 文字 | `--fg` / `--bg`（反色） |
| 内边距 | 10px 16px |
| 圆角 | `--radius-md` |
| 字号 | 12px / 500 |
| 阴影 | `--shadow-3` |
| 停留 | 3s 后自动消失 |
| 动效 | 入场 200ms enter；退场 140ms exit |

**使用边界**：结果反馈用 Toast；需要用户决策或确认的操作不要用 Toast。

---

## 9. 可访问性

以下条目为合入前必须通过的检查项。

### 9.1 对比度

- 正文文字与背景 ≥ **4.5:1**。
- 大字号（≥ 21px，或 ≥ 17px 且粗体）≥ **3:1**。
- 图标与描边等非文本元素 ≥ **3:1**。
- 禁用态可低于标准，但需同时具备指针与语义标记。

### 9.2 焦点与键盘

- 焦点环使用 `--ring`，不得用 `outline: none` 直接移除。
- Tab 顺序与视觉顺序一致；弹层内需做焦点捕获。
- Dialog 打开后焦点落入首个可操作元素，Esc 关闭。
- 所有纯图标按钮必须提供 `aria-label`。

### 9.3 动效偏好

- 响应 `prefers-reduced-motion`，关闭位移与缩放。
- 闪烁类动画频率不得高于 **3 次/秒**。
- 加载指示必须同时提供文本或进度语义。

### 9.4 语义与结构

- 表单控件与 label 通过 `for` / `id` 关联。
- 错误信息用 `aria-describedby` 绑定到输入框。
- 表格使用 `thead` / `th`，数值列声明对齐。
- 不使用颜色作为唯一的状态区分手段。

---

## 10. 使用规范

| ✅ 这样做 | ❌ 不要这样 |
| --- | --- |
| 组件内只引用 `--accent` 等语义变量，主题切换零改动。 | 在组件里写死十六进制色值或直接引用原始色阶。 |
| 主操作在单个视图中只出现一次，其余操作降级为次要或幽灵样式。 | 用颜色作为唯一的状态表达，缺少图标或文案。 |
| 间距取 `--space-*` 阶梯值，布局用 Grid，不靠 margin 堆叠。 | 出现 13px、27px 这类脱离间距阶梯的数值。 |
| 表格数值列右对齐并开启 `font-variant-numeric: tabular-nums`。 | 给每张卡片加左侧彩色描边，或让整页铺满渐变。 |
| 浮层阴影使用 `--shadow-2` 或 `--shadow-3`，常规面板不加阴影。 | 用 `outline: none` 移除焦点环而不提供替代方案。 |

---

## 11. 覆盖与版本

### 11.1 组件族实现进度

| 组件族 | 进度 |
| --- | --- |
| Button / IconButton | 100% |
| Input / Textarea | 100% |
| Select / Combobox | 92% |
| Checkbox / Radio / Switch | 88% |
| Tag / Badge | 100% |
| Card / Panel | 84% |
| Table | 70% |
| Tabs | 78% |
| Dialog / Drawer | 64% |
| Toast / Notification | 58% |
| Pagination | 96% |
| Skeleton / Empty | 46% |

进度用于排期与缺口对齐：`Dialog / Drawer`、`Toast / Notification`、`Skeleton / Empty` 是当前主要缺口。

### 11.2 变更记录

| 版本 | 日期 | 变更 |
| --- | --- | --- |
| **1.0.0** | 2026-10 | 建立语义 Token 层、字号与间距阶梯、双主题与 12 个核心组件族规范。 |
| 0.9.0 | 2026-08 | 组件从各业务包收敛到 `@dp_ui/core`，统一命名前缀。 |

---

## 12. 附录：Token 速查

以下变量可直接作为设计体系的单一事实来源；组件只读取这些语义变量。

```css
:root {
  /* 字体 */
  --font-display: -apple-system, BlinkMacSystemFont, "SF Pro Display", "Segoe UI Variable Display", "Segoe UI", system-ui, sans-serif;
  --font-body: -apple-system, BlinkMacSystemFont, "SF Pro Text", "Segoe UI Variable Text", "Segoe UI", system-ui, sans-serif;
  --font-mono: "JetBrains Mono", "SF Mono", ui-monospace, Menlo, Consolas, monospace;

  /* 字号 */
  --text-2xs: 11px;
  --text-xs: 12px;
  --text-sm: 13px;
  --text-base: 15px;
  --text-md: 17px;
  --text-lg: 21px;
  --text-xl: 28px;
  --text-2xl: 38px;
  --text-3xl: 52px;
  --text-4xl: 72px;

  /* 间距 */
  --space-1: 4px;
  --space-2: 8px;
  --space-3: 12px;
  --space-4: 16px;
  --space-5: 20px;
  --space-6: 24px;
  --space-8: 32px;
  --space-10: 40px;
  --space-12: 48px;
  --space-16: 64px;
  --space-20: 80px;
  --space-24: 96px;

  /* 圆角 */
  --radius-xs: 4px;
  --radius-sm: 6px;
  --radius-md: 10px;
  --radius-lg: 14px;
  --radius-xl: 20px;
  --radius-full: 999px;

  /* 动效 */
  --dur-instant: 80ms;
  --dur-fast: 140ms;
  --dur-base: 200ms;
  --dur-slow: 320ms;
  --dur-slower: 480ms;
  --ease-standard: cubic-bezier(0.2, 0, 0, 1);
  --ease-enter: cubic-bezier(0.16, 1, 0.3, 1);
  --ease-exit: cubic-bezier(0.4, 0, 1, 1);
}

/* 亮色主题 */
:root[data-theme="light"] {
  --bg: oklch(99.2% 0.002 255);
  --surface: oklch(100% 0 0);
  --surface-2: oklch(97.4% 0.004 255);
  --surface-3: oklch(95.2% 0.006 255);
  --fg: oklch(17% 0.015 265);
  --fg-2: oklch(40% 0.012 262);
  --muted: oklch(56% 0.01 260);
  --border: oklch(91% 0.006 260);
  --border-strong: oklch(83% 0.008 260);
  --accent: oklch(50% 0.17 268);
  --accent-hover: oklch(45% 0.18 268);
  --accent-soft: oklch(95.5% 0.025 268);
  --accent-fg: oklch(99% 0.005 268);
  --brass: oklch(60% 0.10 70);
  --ok: oklch(56% 0.14 155);
  --warn: oklch(68% 0.14 75);
  --danger: oklch(55% 0.18 25);
  --info: oklch(58% 0.12 240);
  --shadow-1: 0 1px 2px oklch(20% 0.02 265 / 0.06), 0 1px 1px oklch(20% 0.02 265 / 0.04);
  --shadow-2: 0 4px 14px -4px oklch(20% 0.02 265 / 0.12), 0 1px 3px oklch(20% 0.02 265 / 0.06);
  --shadow-3: 0 18px 44px -16px oklch(20% 0.02 265 / 0.22), 0 4px 12px -4px oklch(20% 0.02 265 / 0.08);
  --ring: 0 0 0 3px oklch(50% 0.17 268 / 0.22);
  --grid-line: oklch(20% 0.02 265 / 0.045);
}

/* 深色主题 */
:root[data-theme="dark"] {
  --bg: oklch(15.5% 0.012 265);
  --surface: oklch(18.5% 0.014 265);
  --surface-2: oklch(22% 0.015 265);
  --surface-3: oklch(26% 0.016 265);
  --fg: oklch(95% 0.005 260);
  --fg-2: oklch(80% 0.01 260);
  --muted: oklch(63% 0.012 260);
  --border: oklch(27% 0.014 265);
  --border-strong: oklch(36% 0.016 265);
  --accent: oklch(70% 0.14 268);
  --accent-hover: oklch(76% 0.13 268);
  --accent-soft: oklch(24% 0.05 268);
  --accent-fg: oklch(16% 0.02 268);
  --brass: oklch(72% 0.09 70);
  --ok: oklch(70% 0.13 155);
  --warn: oklch(78% 0.13 75);
  --danger: oklch(66% 0.16 25);
  --info: oklch(70% 0.11 240);
  --shadow-1: 0 1px 2px oklch(0% 0 0 / 0.4);
  --shadow-2: 0 6px 18px -6px oklch(0% 0 0 / 0.55);
  --shadow-3: 0 24px 60px -20px oklch(0% 0 0 / 0.7);
  --ring: 0 0 0 3px oklch(70% 0.14 268 / 0.28);
  --grid-line: oklch(100% 0 0 / 0.05);
}
```

### 12.1 落地检查清单

- [ ] 组件只引用语义 Token，无原始色阶与硬编码色值。
- [ ] 主题切换通过 `data-dp-theme="light|dark"` 与 `DpConfigProvider` 的 `comp.mode` 完成，组件无需条件样式。
- [ ] 所有可交互元素定义 hover / focus-visible / active / disabled 四态。
- [ ] 间距、字号、圆角取自阶梯值。
- [ ] 正文对比度 ≥ 4.5:1，非文本元素 ≥ 3:1。
- [ ] 图标按钮具备 `aria-label`，表单具备 label 与错误描述关联。
- [ ] `prefers-reduced-motion` 下关闭位移与缩放。
- [ ] 浮层使用 `--shadow-2/3`，常规面板只用 1px 描边。
- [ ] 表格数值列右对齐并启用 `tabular-nums`。
