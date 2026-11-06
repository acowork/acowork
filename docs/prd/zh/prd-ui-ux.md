# ACowork Desktop App — UI/UX 产品需求文档

> 版本：v2.0 | 修订日期：2026-04-27
> 关联设计文档：`docs/design/14-desktop-app.md`
> 关联实施计划：S1 用户模式任务定义（归档于 `docs/_internal/archive/plan/zh/plan-p5.md`，本地查阅）
>
> **v2.0 修订说明**：本文档 v1.0 描述的布局（顶部标题栏带 Gateway 指示器、左侧导航栏 Chat/Models/Skills/Settings、右侧系统托盘动态状态、Models 独立视图、Vault/Providers 设置 Tab 等）与当前代码实现严重脱节。v2.0 依据 `apps/acowork-desktop` 实际代码逐节重写，以代码为准。
> 主要差异：导航栏实为 5 个视图（Chat/Projects/Docs/Harness/Settings）+ 顶部头像；Gateway 状态移到底部状态栏；系统托盘仅含 Quit 菜单；无独立 Models 视图（Provider 管理在 Harness 视图）；Settings 为 Profile/General/Appearance/Gateway/Nodes 五个 Tab；聊天区为「Agent 列表 + 会话 + 可选文件编辑器 + 结果面板 + 右侧 40px 工具条」。

---

## 1. 文档目的

本文档定义 ACowork Desktop App 的 **用户模式** 全部页面交互规格，作为 Phase 5 S1 前端开发的唯一实现依据。开发者模式 UI（Debug/DevMode）在本文档中作为结果面板的一个 Tab 描述，完整协议见 `docs/design/10-debug-protocol.md`。

本文档基于 `apps/acowork-desktop/src` 前端代码与 `apps/acowork-desktop/src-tauri` 后端代码的当前实现整理。前端技术栈：React 19 + TypeScript + Vite + Tailwind CSS v4 + Zustand；桌面壳：Tauri v2。

---

## 2. 设计系统

### 2.1 设计 Token

基于 Tailwind CSS v4 `@theme`，定义项目级设计变量（见 `src/styles/globals.css`）。注意：实际代码中主要使用 **语义化表面色**（`chat-area` / `modal-surface` / `nav-surface` 等），而非 shadcn 的 `primary`/`accent` 灰度变量。

```css
/* 语义表面色（自动随 .dark 翻转，无需 dark: 前缀） */
--color-chat-area:        hsl(0 0% 98%);      /* 主工作区（浅），dark = zinc-900 */
--color-modal-surface:    hsl(0 0% 100%);     /* 对话框/卡片表面（浅），dark = #27272A */
--color-modal-overlay:    hsl(0 0% 0% / 0.5); /* 模态遮罩（主题无关）*/
--color-nav-surface:      hsl(240 8% 94%);    /* Agent 列表容器（浅），dark = #2E2E2E */
--color-nav-control:      hsl(240 5% 87%);    /* 输入/按钮静态底（浅）*/
--color-nav-control-hover:hsl(240 5% 82%);    /* 主要操作 hover */
--color-nav-item-hover:   hsl(240 6% 90%);    /* 列表行 hover */
--color-nav-divider:      hsl(0 0% 78%);      /* 列表分隔线 */
--color-editor-canvas:    #FFFFFF;            /* 文件编辑器画布，dark = #1E1E1E（与 Monaco 对齐）*/

/* 聊天消息表面 */
--color-chat-bubble:      hsl(240 5.9% 90.6% / 0.4);  /* 助手/思考/错误/系统气泡 */
--color-chat-title:       hsl(240 4.8% 86.1% / 0.5);  /* 块标题 */
--color-chat-body:        hsl(0 0% 98% / 0.8);        /* 块正文（与面板同底，视觉合并）*/
--color-chat-border:      hsl(240 5.9% 90.6%);        /* 容器边框 */
--color-chat-badge:       hsl(240 5.9% 90.6%);        /* 发送者角色标签 */
--color-chat-user:        color-mix(in srgb, var(--color-accent) 90%, transparent); /* 用户气泡 */
--color-chat-user-text:   hsl(240 100% 100%);          /* 用户气泡文字（恒为深色）*/

/* 强调色（用户可配置，默认蓝）*/
--color-accent:           #3B82F6;

/* 间距 */
--spacing-nav:            52px;   /* 左侧导航栏宽度 */
--spacing-agent-list:     240px;  /* Agent 列表默认宽度 */
--spacing-results:        320px;  /* 结果面板默认宽度（代码实际默认 340px，见 §4.1）*/
--spacing-chat-min:       360px;  /* 聊天面板最小宽度（代码实际 288px，见 §4.1）*/

/* 字号（可缩放，见 §6.5）*/
--text-xs:    0.75rem;
--text-sm:    0.875rem;
--text-base:  1rem;
--text-lg:    1.125rem;
--text-xl:    1.25rem;

/* 圆角（4 级）*/
--radius-sm:  4px;   /* 按钮、输入框、标签 */
--radius-md:  6px;   /* 卡片容器、对话框、消息块 */
--radius-lg:  8px;   /* 弹层、横幅 */
--radius-xl:  12px;  /* 面板容器、布局外壳 */

/* 动画 */
--duration-fast:   150ms;
--duration-normal: 250ms;
--duration-slow:   400ms;

/* 暗色模式：Tailwind 类名方式（.dark 类 + @custom-variant），
   颜色通过上述语义 token 自动翻转。 */
```

### 2.2 窗口规格（`src-tauri/tauri.conf.json`）

| 属性 | 值 |
|------|-----|
| 默认尺寸 | 1200 × 800 |
| 最小尺寸 | 1024 × 600 |
| 透明度 | `transparent: true` |
| 标题栏 | `titleBarStyle: "Overlay"` + `hiddenTitle: true`（macOS 使用原生红绿灯） |
| 启动可见性 | `visible: false`，React 首帧渲染完成后由 `getCurrentWindow().show()` 显示（避免白屏/装饰闪烁） |
| 单实例 | 是（Tauri 内建） |
| 关闭行为 | **隐藏到托盘**（`CloseRequested` 拦截：`window.hide()` + `api.prevent_close()`，仅当窗口可见时；见 §3.3）。真正退出需通过托盘 "Quit ACowork" 菜单 |

### 2.3 响应式

当前实现**不使用 CSS 媒体查询断点**。面板宽度通过**拖拽手柄**手动调节并持久化到 `localStorage`：

| 面板 | 默认 | 最小 | 最大 | 持久化 key |
|------|------|------|------|-----------|
| Agent 列表（侧栏） | 240px | 100px（拖至 <100 折叠为 64px 仅头像） | 400px | `acowork-sidebar-width` |
| 结果面板（右侧） | 340px | 200px | 600px | `acowork-right-width` |
| 文件编辑器 | 450px | 200px | 900px（动态上限保证聊天 ≥288px） | `acowork-file-width` |

窗口 resize 时：侧栏与结果面板保持绝对宽度，会话与文件面板按可用空间比例缩放（变化 <5% 忽略以避免抖动）。

---

## 3. 全局组件

### 3.1 顶部标题栏（`TitleBar.tsx`，高 32px）

```
┌──────────────────────────────────────────────────────────────────┐
│            ACowork                          [— □ ✕]（仅 Win/Linux）│
└──────────────────────────────────────────────────────────────────┘
```

| 元素 | 位置 | 说明 |
|------|------|------|
| "ACowork" 品牌文本 | 左 | `text-xs`，`data-tauri-drag-region` 支持原生拖拽，双击最大化由系统处理 |
| 窗口控制按钮 | 右 | **仅 Windows/Linux** 渲染自定义 最小化/最大化/关闭 按钮；macOS 使用原生红绿灯（左侧留 80px） |

**注意**：标题栏**没有** Gateway 状态指示器。Gateway 连接状态显示在底部状态栏与 SplashScreen（见 §3.4、§7）。

### 3.2 左侧导航栏（`NavBar.tsx`，宽 52px）

```
┌────┐
│ 👤 │  ← 用户头像（点击 → Settings → Profile Tab）
├────┤
│ 💬 │  ← Chat（默认视图）
│ 📋 │  ← Projects（看板图标，当前为占位）
│ 📄 │  ← Docs（文档图标，当前为占位）
│ 🧩 │  ← Harness（拼图图标，Provider/模型管理）
├────┤
│    │  （弹性留白）
├────┤
│ ⚙️ │  ← Settings（底部）
└────┘
```

**导航项**（`NavView = "chat" | "projects" | "docs" | "harness" | "settings"`）：

| 项 | 图标（未选中/选中） | 说明 |
|----|-------------------|------|
| 用户头像 | 40px 头像（`UserAvatar`） | 点击跳转 Settings → Profile Tab；hover 有圆环 |
| Chat | 气泡（描边/实心） | 默认视图 |
| Projects | 看板（描边/实心） | 目前渲染 "TODO" 占位 |
| Docs | 文档（描边/实心） | 目前渲染 "TODO" 占位 |
| Harness | 拼图（描边/实心） | Provider / 搜索 / MCP / Embedding / LSP 管理 |
| Settings | 齿轮（描边/实心） | 底部，与顶部群组以 `flex-1` 分隔 |

| 规则 | 说明 |
|------|------|
| 选中态 | 图标切换为实心（filled）变体，`currentColor` 着色 |
| 未选中态 | 描边（outline）变体 |
| Hover | `NavButton` 圆角背景高亮 |
| Tooltip | `position="right"`，悬停显示 |
| 无障碍 | `role="navigation"` + `aria-label` |

所有视图切换通过 `AppLayout.currentView` 状态驱动；非 Chat 视图右侧保留 40px 空占位以保持窗口视觉对称。

### 3.3 系统托盘（`src-tauri/src/tray/`）

**当前实现极简**：

| 状态 | 行为 |
|------|------|
| 菜单 | 仅一项 **"Quit ACowork"**（点击后先杀死本地 Gateway 进程树再退出，见 `tray/events.rs`） |
| 左键点击 | 恢复并聚焦主窗口（`unminimize → show → set_focus`，类微信） |
| 右键点击 | 系统自动弹出菜单 |
| Tooltip | 静态 `"ACowork"`（无动态状态） |
| 图标 | 内嵌 `icon.png` |

**关闭窗口 = 隐藏到托盘**：`CloseRequested` 事件拦截（窗口可见时）→ `window.hide()` + `prevent_close()`，应用驻留托盘；仅托盘 Quit 菜单（或系统退出）真正结束进程。这使托盘承担**窗口恢复入口 + 常驻宿主**双重角色。

**注意**：文档 v1.0 中描述的 "Show Dashboard / Agent Chat / Status / Start Gateway" 等动态菜单项与彩色状态图标**均未实现**。

### 3.4 底部状态栏（`AppLayout.tsx` 内联，高 24px）

位于窗口最底部，承载全局状态信号：

| 元素 | 说明 |
|------|------|
| 状态药丸 | `error`（红）/ `warning`（琥珀）/ `info`（灰），点击复制全文；hover Tooltip 显示完整内容 |
| Agent + 上下文药丸 | 选中 Agent 运行中且结果面板折叠/非 Status Tab 时显示：`Agent: {名称}` + `Context: {usage}% | {tokens}/{窗口}`（≥90% 高亮）。`{窗口}` = 该会话生效窗口（per-session 覆盖存在时为其覆盖值，否则 per-agent 链解析值，ADR-074） |
| MQTT 调试控件 | `MqttDebugControls`（开发者用，显示 MQTT 连接状态） |
| 文件状态簇 | 文件编辑器打开时，绝对定位在文件面板下方，显示光标位置 / LSP 状态等 |

**Gateway 断连信号**：Gateway 状态为 `error` 时，底部状态栏显示红色状态药丸，同时标题栏渲染 `GatewayStatusChip`（见 §3.5）。

### 3.5 Gateway 断连指示芯片（`GatewayStatusChip.tsx`）

仅在 `gatewayStatus === "error"` 时渲染（稳态掉线；启动期由 SplashScreen 负责），挂在标题栏 "ACowork" 右侧：

```
┌─────────────────────────────────────────────────────────────┐
│ ACowork  (◉ 网关不可达  2 ▾)                    [搜索][窗口] │
└─────────────────────────────────────────────────────────────┘
```

- 琥珀色实心胶囊（`bg-amber-500`，dark `bg-amber-600`）+ 白字，`h-6`，hover 加深
- **呼吸动画**：Wifi 图标 `animate-pulse`（只作用于图标，标签不闪）
- **常驻不自动消失** —— 休眠唤醒换网络后断线会持续存在，直到用户切换地址或重试成功
- 有候选地址时显示数量角标，不展开浮层也知道"有东西可点"
- 点击展开浮层（`w-80`，点外部 / Esc 关闭）：状态文案 → 候选地址纵向列表（每行一个 `btn-accent` 实心按钮，右侧显示延迟 ms，`max-h-56` 纵向滚动）→ 底部操作条
- 本地模式（`gatewayMode === "local"`）：显示 "本地网关未运行" / 启动中显示 "正在启动本地网关"，提供 **[启动]** + **[重试]**
- 远程模式：显示 "网关不可达"，提供 **[重试]**

---

## 4. Chat 视图（默认视图）

### 4.1 视图结构

```
┌────┬────────────┬─────────────┬─────────────────┬──────────┬────┐
│    │ Agent List │ Chat Panel  │ FileEditor(可选)│ Results  │    │
│ N  │ (240px可拖)│  (弹性)     │ (450px,有文件  │ Panel    │ R  │
│ A  │            │             │  时出现)        │ (340px   │ I  │
│ V  │ [搜索框]   │ [会话Tab条] │                 │ 可折叠)  │ G  │
│ 5  │ ☀AgentA    │ [工具栏]    │                 │ [Tab头]  │ H  │
│ 2  │  AgentB    │ [消息流]    │                 │ Workspace│ T  │
│ p  │  AgentC    │             │                 │ Status   │ 4  │
│ x  │            │ [输入区]    │                 │ Memory   │ 0  │
│    │ [+ 添加]   │             │                 │ Tools    │ p  │
│    │            │             │                 │ Debug    │ x  │
│    │            │             │                 │ Setup    │    │
└────┴────────────┴─────────────┴─────────────────┴──────────┴────┘
```

**面板次序**（`AppLayout.tsx`，Chat 视图）：

1. `NavBar`（52px）
2. `AgentList`（可拖拽调宽）
3. 分隔条（1px 拖拽手柄）
4. `ChatPanel`（`flex-1` 弹性）
5. `FileEditorPanel`（**仅当有打开文件时**渲染，`openFiles.length > 0`）
6. `ResultsPanel`（`!resultsCollapsed` 时渲染，可折叠、可调宽）
7. `RightNavBar`（40px，工具条）

### 4.2 Agent 列表（`AgentList.tsx`，默认 240px）

#### 数据源

| 数据 | 来源 | 刷新时机 |
|------|------|---------|
| Agent 列表 | Zustand `agentStore.agents`（内部调用 Tauri `list_agents` 命令） | 初始加载 + 安装/卸载/克隆/创建后 + 每 30s 轮询 |
| 会话标题 | `fetchLatestSession(id)`（运行中的 Agent 异步拉取最新会话标题） | Agent 变为 running 且标题未缓存时 |
| 会话活动点 | `chatStore.agentStates` 各会话 `sessionStatus` 非 idle | 实时（MQTT 事件驱动） |

#### Agent 条目

```
┌────────────────────────────────────────────┐
│ [40px头像]  Weather Agent           🟢点  │
│             <最新会话标题 / zzzz 休眠动画>  │
└────────────────────────────────────────────┘
```

| 元素 | 说明 |
|------|------|
| 头像 | `AgentAvatar`，优先 manifest `avatar` / `builtin_avatar`，否则首字母渐变 |
| 名称 | `profile.displayName ?? display_name ?? name`，单行截断 |
| 第二行 | 运行中：最新会话标题（未加载时骨架屏 pulse）；未运行/休眠：`zzzz` 休眠动画 |
| 活动点 | 运行中 + 有会话处于 streaming/waiting_approval/paused 时，头像右下角强调色圆点（ADR-014） |
| 选中态 | `bg-[var(--color-accent)]/90 text-white` |
| 分隔线 | 非最后一项下方 `nav-divider` 细线 |

#### 条目交互

| 操作 | 触发 | 行为 |
|------|------|------|
| 单击 | 左键 | 选中 Agent，ChatPanel 加载其会话 |
| 双击 | 左键×2 | 停止的 Agent 直接启动（`startAgentAndSyncUI`） |
| 右键 | 右键 | 弹出上下文菜单（见下） |

**右键上下文菜单**（条件渲染）：

| 菜单项 | 条件 | 行为 |
|--------|------|------|
| ▶ Start | `!running` | 启动（去重：同一 Agent 防止连点重复触发） |
| ▶ Start in Debug | `!running` | 以 DevMode 启动（`startAgentAndSyncUI(id, true)`） |
| ⏹ Stop | `running` | 确认后停止 |
| Details | 始终 | 打开 `AgentDetailDialog` |
| Clone | 始终 | 打开 `CloneDialog`，成功后自动选中克隆体 |
| Publish | 始终 | 打开 `PublishWizard`（导出 .agent 包） |
| 🗑 Uninstall | 所有 Agent（含 System Agent，ADR-077 取消其特权） | 确认后卸载 |

**Stop / Uninstall / 危险操作确认对话框**：统一 `ConfirmDialog` 组件，`destructive` 红色样式，Esc 关闭，取消为默认焦点。

**底部添加区**：底部 `[+]` 按钮，弹出菜单（popover）：

```
┌────────────────────────────┐
│ ✨ Create Agent            │  → 打开 CreateWizard（在线创建）
│ ＋ Install Agent           │  → 打开 .agent 文件选择并安装
└────────────────────────────┘
```

- 多节点场景：安装前先 `fetchNodes()` 解析在线节点；>1 个在线节点时菜单切换为**节点选择器**（ADR-055 §6.13.3）
- 安装中按钮显示加载态，成功后 Toast + 自动选中新 Agent
- `com.acowork.system` 是随 Gateway 分发的默认 Agent，与普通 Agent 一样走安装/启动/卸载流程（ADR-077）

### 4.3 Chat Panel（`ChatPanel.tsx`，弹性宽度）

#### 4.3.1 顶部结构（自上而下）

| 区域 | 内容 |
|------|------|
| 会话 Tab 条（`SessionTabBar`） | 多会话标签切换（当前 Agent 的 `openSessionIds`），每个 Tab 含标题 + 关闭按钮；含 "新建会话" 入口 |
| 工具栏 | **模型选择**（Layers 图标，弹层列模型 + 能力标记 + "Add Model"）、**推理强度**（Brain 图标，Auto/Off/Low/Medium/High）、**Workspace**（文件夹图标）、**Skills**（拼图/技能）、**上传**（文件/图片，50MiB 上限前置校验）、**发送/停止** 等 |
| 消息流 | 虚拟化列表（`VirtualMessageList`），滚动快照恢复 |
| 输入区 | 多行 `textarea`（Enter 发送 / Shift+Enter 换行 / IME 组合 300ms 防误发），附 发送/停止 按钮与上下文菜单 |

工具栏按钮在窄宽时**按需折叠为纯图标**（从最左按钮开始折叠文字，`data-toolbar-btn` DOM 测量驱动），保证不重叠。

#### 4.3.2 空状态 / 未运行状态

| 状态 | 占位 |
|------|------|
| 未选择 Agent / 无会话 | "选择一个 Agent 开始对话"（居中图标 + 文案） |
| Agent 已停止 | "启动 Agent" 按钮（居中，点击启动） |
| Agent 运行中但未 ready | "Connecting to agent..." 提示（MQTT 未连接时） |

#### 4.3.3 消息流与消息类型

消息由 `VirtualMessageList` 虚拟化渲染，按会话分组（`messageFolder.ts`）。主要消息类型（见 `lib/types.ts`）：

| 类型 | 呈现 |
|------|------|
| `user` | 用户气泡（`chat-user` 强调色 90% 混合），右侧 |
| `assistant` | Markdown 渲染（`chat-bubble` 表面），含代码块（`CodeBlock`）、Mermaid（`MermaidBlock`）、表格、LaTeX |
| `think` / `thought` | 思考块（可折叠） |
| `tool_call` / `tool_result` | 工具调用块（可展开参数/结果 JSON） |
| `error` | 错误消息块 |
| `system` | 系统消息（会话创建/清理等，小字居中） |
| `compaction` | 上下文压缩卡片（`CompactionCard`） |

附加消息组件：`AskQuestionCard`（Agent 向用户提问）、`RetryWaitBanner`、`DebugPausedBanner`、`ExploreBlock`（探索/搜索）、`StreamingSourceBlock`、`ThinkBlock`。

**附件**：用户可粘贴/上传文件、图片（`UserWithAttachmentsBubble`），顶部 `AttachedContextChips` 显示已挂载上下文；Agent 引用文件显示 `DocumentChip`。

#### 4.3.4 流式输出

流式事件经 **MQTT**（非 WebSocket）推送（ADR-033），`chatStore` 维护各会话消息增量：

1. 收到 chunk → 逐字追加到当前消息
2. 收到 done → 结束流式，更新 token 统计（`tokenUsage`、`contextUsage`）
3. 工具调用事件 → 插入/填充工具块

MQTT 连接状态由 Rust `rumqttc` eventloop 维护并推送 `mqtt-status` 事件；前端仅反映状态（睡眠唤醒时前端可触发 `force_reconnect_mqtt` 主动重连）。

#### 4.3.5 输入区

| 规则 | 说明 |
|------|------|
| 发送 | **Enter**（Shift+Enter 换行）；发送中再次 Enter → 消息入队等待下一轮，不打断流式 |
| 停止 | 必须点击 Stop 按钮（Enter 不触发停止，避免误触） |
| IME 防护 | `onCompositionEnd` 后 300ms 内的 Enter 视为输入法确认，不发送 |
| 多行 | `max-h-48` 内滚动 |
| 禁用条件 | Gateway 未连接 / Agent 未运行 / MQTT 断开时禁用，placeholder 相应变化 |
| 粘贴 | 粘贴图片/文件自动上传（50MiB 上限前置校验） |
| 右键菜单 | 自定义复制/粘贴菜单（Tauri WebView 无原生菜单） |

### 4.4 结果面板（`ResultsPanel.tsx` + `RightNavBar.tsx`，默认 340px）

右侧结果面板由 **40px 工具条（RightNavBar）** + 内容区组成，多 Tab 结构：

| Tab | 图标 | 显示条件 | 内容 |
|-----|------|---------|------|
| Workspace | 文件夹 | Agent 运行中 | `WorkspaceExplorer`（文件树 / 浏览 / 定位） |
| Status | 仪表盘 | 始终 | Agent 运行状态、Token 统计、模型/Provider、会话统计（见下） |
| Memory | 数据库 | Agent 运行中 | `MemoryPanel`（记忆管理） |
| Debug | 虫 | Agent 运行中 | `DebugPanel`（DevMode 调试，见 §4.4.2） |
| Setup | 齿轮 | Agent 运行中 | `AgentSetupTab`（Agent 配置；内置工具 / MCP / 提示词与技能三张卡片在此，见 §4.4.3） |

**交互规则**：
- 点击已激活 Tab 的工具条按钮 → 折叠面板；再点 → 展开
- Agent 停止时，Workspace/Memory/Debug/Setup 按钮隐藏，面板自动跳回 Status（见 `AppLayout` 生命周期 effect）
- 右侧栏共 **5 个 Tab**：原「工具」Tab 已下线，其内置工具 / MCP 卡片并入 Setup Tab；Debug 的提示词与技能卡片也迁至 Setup Tab
- Agent 进入调试模式时自动切到 Debug Tab
- 顶部标题栏显示当前 Tab 名称，左侧为拖拽调宽手柄

#### 4.4.1 Status Tab

显示当前会话/Agent 统计：Token 用量（`tokenUsage`）、上下文占用（`contextUsage`，含 ADR-028 历史累计兜底）、迭代次数、模型/Provider、推理强度、温度、会话数、压缩状态等。上下文数字行支持 **per-session 上下文窗口覆盖**（ADR-074）：编辑入口在 `ContextUsageIcon` 弹层内，常用档位 32K/64K/96K/128K/200K + K 单位数字输入 + "恢复为 agent 默认（继承）"；设定值超过模型窗口时实际生效取 `min(设定值, 模型窗口)` 并就地提示。

#### 4.4.2 Debug Tab（DevMode）

| 状态 | 呈现 |
|------|------|
| Agent 未运行 | "无 Agent 处于调试模式" 空态 |
| 运行中但 DevMode 关闭 | "Enable Debug" 按钮（`enable_agent_debug` 运行时开启，免重启） |
| DevMode 开启 + 远程 Gateway | "调试在远程模式不可用"（ADR-048 D6：调试 RPC 依赖本地 MQTT） |
| DevMode 开启 + 本地 + 未连接 | "调试连接已断开" |
| 就绪 | `DebugPanel`：控制条（Pause/Resume、Step、Stop、Restart、Exit Debug、Re-execute）+ 状态卡（迭代/阶段/Token/会话状态）+ 上下文快照列表（可展开各 section、在线编辑、patch） |

#### 4.4.3 Setup Tab 卡片

Setup Tab 是全 Agent 级配置的唯一入口，7 张 level-1 折叠卡片自上而下：

| # | 卡片 | 内容 |
|---|------|------|
| 1 | Agent Info | 头像（ADR-017 服务端）、显示名 |
| 2 | Session Parameters | 会话语言、最大迭代/会话数、Shell 审批、压缩比阈值 |
| 3 | Model Parameters | 最大输出 Token、上下文窗口、温度 |
| 4 | Chat Session 路由 | 路由模型 / 超时 / 历史预算（ADR-099） |
| 5 | 提示词与技能 | Agent 包 `prompts/` / `skills/` 浏览与编辑 + reload（原 Debug Tab 顶部卡片，ADR-063 §3.7） |
| 6 | 内置工具 | 内置工具开关（ADR-029，含 ADR-094 的 `load_skill`） |
| 7 | MCP 服务器 | Agent 包建议的 MCP 分组（ADR-104）+ 服务器开关（ADR-069 / ADR-090） |

默认只展开**前 3 张**卡片，其余默认折叠（顺序与默认值由 `agentSetupCards.ts` 一处定义）。卡片展开状态为组件本地状态，不做持久化。

### 4.5 文件编辑器（`FileEditorPanel.tsx`，可选）

- **仅当有打开文件时**渲染（`openFiles.length > 0`）
- 默认宽 450px（首次打开自动按可用空间 50% 计算），可拖拽 200–900px
- 基于 Monaco Editor，支持多文件 Tab、Markdown/图片/HTML 预览、LSP（语言服务器）、全局搜索、符号搜索、GoToFile
- 底部状态簇显示光标/选区/LSP 状态（绝对定位在全局状态栏上方）

---

## 5. Harness 视图（Provider / 模型管理）

导航栏 Harness 入口（拼图图标）进入 `HarnessPage`，5 个 Tab：

| Tab | 内容 |
|-----|------|
| Providers | Provider API Key 管理（`AddProviderFlow` / 编辑对话框），模型能力配置（`ModelMultiSelect`）、全局默认模型（`GlobalCompactModelCard`） |
| Search | 联网搜索配置（`SearchTab`） |
| MCP | MCP 服务器管理（`McpTab`，预设 `MCP_PRESETS`） |
| Embedding | 嵌入模型配置（`EmbeddingModelTab`） |
| LSP | LSP 服务器管理（`LspTab`） |

**数据源**：API Key 走 Tauri `list_keys` / `add_key` / `remove_key` / `update_key` 命令（vault）；Provider 列表来自 Gateway 的 `offline_providers.json`（`fetchProviders()`）；模型列表来自 `fetchProviderModels(providerId)`。

**注意**：文档 v1.0 中的独立 "Models 视图" 与 "Vault/Providers 设置 Tab" 已废弃——Provider 管理集中在 Harness 视图。

---

## 6. Settings 视图（`SettingsPage.tsx`）

5 个 Tab（Tab 切换用 CSS `display` 保留组件状态）：

| Tab | 说明 |
|-----|------|
| Profile | 用户身份：显示名、头像（上传/内置头像）、语言、时区、城市、职业 |
| General | 日志：Gateway 日志级别、前端日志级别、日志文件大小/数量、删除日志；数据目录（只读，来自 `/api/config`）；关于；重置引导（Reset Onboarding） |
| Appearance | 主题（light/dark/system）、强调色（`ACCENT_PRESETS` 色板）、内容宽度（40–100%）、字号（S–XXL）、窗口透明度（滑块 0–100%） |
| Gateway | 运行模式（local/remote）、本地状态与 启动/停止/重启、远程 URL + 测试连接、已连接 Agents 列表（含模型信息、Debug 徽标） |
| Nodes | 节点拓扑（`GET /api/nodes`）：node_id / 在线状态 / OS / 架构 / 版本 / hostname / Agent 数（只读表格） |

### 6.1 Appearance 详细

| 设置 | 取值 | 说明 |
|------|------|------|
| 主题 | light / dark / system | system 跟随 macOS 外观（`matchMedia` 监听，`settingsStore.osTheme` 同步） |
| 强调色 | 预设色板（`ACCENT_PRESETS`） | 写 `--color-accent` + `accent-{id}` 类，联动玻璃毛玻璃色调、消息气泡、选中态 |
| 内容宽度 | 40/50/60/70/80/90/100% | 控制主内容区最大宽度 |
| 字号 | S(0.75)/M(0.875)/L(1.0)/XL(1.125)/XXL(1.25) | 写 `--ui-font-size`；全局快捷键 Ctrl+= / Ctrl+- 步进 |
| 透明度 | 0–100% | macOS 原生 `NSVisualEffectView`（`set_window_effect`）+ CSS 玻璃 tint 双层；macOS 有最小不透明度下限保证一致性 |

### 6.2 Gateway 详细

- **运行模式**：local（Tauri 托管启动 Gateway 子进程）/ remote（连接用户配置 URL）
- local：状态指示（Running/Starting/Stopped）、[Start]（外部启动时）/ [Restart] [Stop]（Tauri 托管时）、版本
- remote：URL 输入（blur/Enter 保存）、[Apply]、状态指示、[Test Connection]
- **Connected Agents**：列出 `/api/agents` 中 `running || connected` 的 Agent，每行显示名称、`provider/model`、Debug 徽标

---

## 7. 首次启动引导（`OnboardingFlow.tsx`）

### 7.1 流程概览

```
Step 1: 欢迎 ──→ Step 2: Gateway ──→ Step 3: API Key ──→ Step 4: 身份 ──→ Step 5: 安装 Agent
                                                                    │
                                                          跳过/完成 → 主界面
```

- 进度条：5 段式（`bg-zinc-800` 已过 / `bg-zinc-200` 未到）
- 引导状态持久化：`localStorage["acowork_onboarding"] = "completed"`
- 顶部为进度条 + "Step X of 5" 文案
- 模态覆盖全屏（`fixed inset-0 z-50`），居中 `max-w-md`

### 7.2 Step 1: 欢迎

品牌 Logo + 欢迎语 + **[开始配置]**（→ Step 2）+ **[跳过引导]**（直接完成，进入主界面）。

### 7.3 Step 2: Gateway 连接

**与 v1.0 差异**：增加了 **本地/远程模式选择**（RadioGroup）：

- **本地模式**（推荐）：显示状态（Starting/Connected/Not started/Failed），失败显示 ErrorBox + [Start Local Gateway] 按钮；连接成功后 [下一步] 可用
- **远程模式**：URL 输入（placeholder 默认 `http://127.0.0.1:19876`）+ [Apply] + 状态指示 + [Test Connection]
- `canProceed = status === "connected"`（两种模式一致）

### 7.4 Step 3: API Key 配置

**与 v1.0 差异**：Provider 下拉来自 **动态 Provider 列表**（Gateway `fetchProviders()`），配置项更多：

```
┌──────────────────────────────────────────────────────────────┐
│  🔑 [Provider ▼]                                             │
│  API Key    [password 输入]                                  │
│  Base URL   [文本输入]                                        │
│  模型多选    [ModelMultiSelect：能力筛选/自定义模型输入]        │
│                [保存]（保存后 "Saved"）                       │
│  ────────────────────────────────────────────                │
│  🏠 本地 Provider（无需 Key）：列出本地 Provider 名称           │
└──────────────────────────────────────────────────────────────┘
```

- `needsApiKey(provider)` 决定是否显示 Key 输入框；本地 Provider 无需 Key
- Base URL 随 Provider 切换自动填充其 `api` 端点
- 模型多选：`ModelMultiSelect` 复用 Harness 组件，保存完整模型列表（非仅默认）
- 保存：Tauri `add_key` 命令，成功后派发 `models-added` 事件（ChatPanel 监听刷新模型列表）
- **[下一步]** 始终可用（可跳过，无 v1.0 的"至少一个 Provider 可用"硬约束）

### 7.5 Step 4: 身份信息

| 字段 | 必填 | 控件 |
|------|------|------|
| 称谓 (name) | 是 | 文本 |
| 语言 | 是 | 下拉（zh-CN / zh-TW / en / ja / ko） |
| 时区 | 是 | 下拉（Asia/Shanghai / Asia/Tokyo / America/New_York / America/Los_Angeles / Europe/London / UTC） |
| 城市 | 否 | 文本 |
| 职业 | 否 | 文本 |

- `requiredFilled = name && language && timezone`，未填时 [下一步] 禁用
- 完成时调用 `createUser(...)`（fire-and-forget），同步本地 `userProfileStore`，无头像则随机分配内置头像

### 7.6 Step 5: 安装第一个 Agent

**与 v1.0 差异**：推荐 Agent 列表为 **6 个内置角色**（非 v1.0 的 Weather/Calendar），且支持**批量多选安装**：

| 资源名 | 名称 | 角色 | 描述 |
|--------|------|------|------|
| software-architect-agent | Architect | Software Architect | 系统设计、架构评审、技术规划、风险评估 |
| senior-engineer-agent | SSE | Senior Software Engineer | 代码评审、架构设计、调试、重构、测试、文档 |
| quality-assurance-agent | QA | Quality Assurance Manager | 质量策略、测试计划、缺陷管理、发布验收 |
| project-manager-agent | PM | Project Manager | 需求分析、任务拆解、进度跟踪、风险管理 |
| product-manager-agent | Product | Product Manager | 产品策略、用户研究、PRD 撰写、路线图、发布规划 |
| document-manager-agent | Docs | Document Manager | 文档收集、组织、撰写、转换、知识库维护 |

- 每项为 checkbox 卡片（名称 · 角色 / 描述），默认全部勾选；提供 [全选]/[清空]
- **[安装所选 (N)]**：`waitBootstrapReady()`（轮询 `/api/bootstrap` 至 READY）→ `runBounded(items, 3)` 并发 3 提交 `install_bundled_agent` → 每项 `wait_agent_installed` 轮询 → 每项状态徽标（pending/submitted/completed/failed，含 operation_id）
- **[从文件安装]**：打开 `.agent` 文件选择，`install_agent` 命令
- **[完成]**：无论是否安装都可用，设 `onboarding_completed`，进入主界面

---

## 8. 错误处理 UX

### 8.1 Toast 通知（`ToastProvider.tsx`）

所有非致命错误/成功通过 Toast 显示。

| 属性 | 值 |
|------|-----|
| 位置 | 右下角 |
| 类型 | success / error / warning / info |
| 堆叠 | 最多 3 条，新的推入，旧的提前消失 |
| 自动消失 | success 较短 / error 较长 |

### 8.2 加载状态

| 组件 | 加载态 |
|------|--------|
| Agent 列表首载 | 居中 Spinner（`animate-spin`） |
| Agent 会话标题 | 第二行骨架屏（`animate-pulse` 矩形条） |
| 安装/启动 | 按钮变 loading / 禁用 + Toast 反馈 |
| 面板数据 | Tab 内 `loading` 文案 |

### 8.3 网络/连接错误

- Gateway `error` → 标题栏 `GatewayStatusChip`（§3.5）+ 底部状态栏红色药丸
- MQTT 断开（Agent 运行中）→ 底部状态栏 warning 药丸（"Realtime connection lost, retrying..."），休眠中的 Agent 不提示（预期行为）
- 各异步操作失败 → Toast + 错误详情

---

## 9. 动画与过渡

| 场景 | 动画 |
|------|------|
| 导航/列表选中态 | 背景色过渡 150ms |
| 面板折叠/展开 | 宽度即时切换（无过渡动画） |
| Toast | 从右滑入 + 淡入 / 淡出 |
| 流式打字 | 无闪烁光标（块级渲染） |
| Agent 状态变更 | 无颜色渐变（即时切换） |
| 欢迎/启动 | SplashScreen 淡入（700ms translate+opacity）、LoadingDots 每 400ms |
| 强调色切换 | 即时生效 |

**prefers-reduced-motion**：CSS 层 `transition-all` 在系统减少动画时可由 Tailwind 媒体查询降级。

---

## 10. 键盘快捷键

| 快捷键 | 上下文 | 行为 |
|--------|--------|------|
| `Enter` | 输入区 | 发送消息（Shift+Enter 换行） |
| `Ctrl/Cmd + =` | 全局 | 字号增大（步进 S→XXL） |
| `Ctrl/Cmd + -` | 全局 | 字号减小 |
| `Escape` | 对话框/弹层 | 关闭 |
| `F5` / `Ctrl+R` / `Ctrl+N` 等浏览器快捷键 | 全局 | **被屏蔽**（防止页面刷新/重载，见 `main.tsx` BLOCKED_SHORTCUTS） |
| `Ctrl+Shift+P` | 全局 | 被屏蔽（浏览器打印） |

**注意**：v1.0 中 `Ctrl/Cmd + Enter`、`Ctrl+N` 安装、`Ctrl+,` Settings、`Ctrl+Shift+D` DevMode、`Ctrl+R` 刷新列表等**均未实现**。

---

## 11. 前端 ↔ 后端契约汇总

### 11.1 Tauri 命令（`invoke(...)`，经 `withGlobalTauri`）

| 前端操作 | 命令 | 说明 |
|---------|------|------|
| 列出 Agent | `list_agents` | `agentStore.fetchAgents()` |
| 安装 Agent | `install_agent` | `{ packagePath, devMode, nodeId }` |
| 安装内置 Agent | `install_bundled_agent` | `{ resourceName, devMode }` → `OperationAck` |
| 等待安装完成 | `wait_agent_installed` | `{ agentId, timeoutSecs }` |
| 卸载 Agent | `uninstall_agent` | `{ agentId }` |
| 启动 Agent | `start_agent` | `{ agentId, devMode }` |
| 停止 Agent | `stop_agent` | `{ agentId }` |
| 重启调试 | `restart_agent_in_debug` | `{ agentId }` |
| 克隆 Agent | `clone_agent` | |
| 创建 Agent | `create_agent` | |
| 发布/导出 | `prepare_publish` / `build_publish` / `export_package` | |
| Vault Key | `list_keys` / `add_key` / `remove_key` / `update_key` / `list_search_keys` / `add_search_key` | |
| 调试 | `enable_agent_debug` / `disable_agent_debug` / `debug_rpc` | |
| Gateway | `set_gateway_config` / `get_gateway_config` / `init_local_gateway` / `start_local_gateway` / `stop_local_gateway` / `get_local_gateway_status` / `get_bootstrap` | |
| MQTT | `connect_mqtt` / `disconnect_mqtt` / `force_reconnect_mqtt` / `get_mqtt_status` / `mqtt_subscribe_agent_session` / `mqtt_unsubscribe_agent_session` / `mqtt_publish_control` | 实时消息/控制走 MQTT（ADR-033），**非 WebSocket** |
| 文件 | `upload_file` / `get_file_size` / `upload_agent_file` / `upload_user_avatar_file` / `update_agent_manifest_avatar` | |
| 系统 | `reveal_in_file_explorer` / `set_window_effect` / 剪贴板 | |

### 11.2 Gateway HTTP API（`fetch(...)` 直连）

| 前端操作 | 方法 | 路径 |
|---------|------|------|
| 健康检查 | GET | `/health` |
| 引导状态 | GET | `/api/bootstrap` |
| 配置 | GET/PUT | `/api/config` |
| 节点 | GET | `/api/nodes` |
| Agent 列表 | GET | `/api/agents` |
| Agent 详情 | GET | `/api/agents/{id}` |
| Agent 模型 | GET | `/api/agents/{id}/model` |
| Agent 状态 | GET | `/api/agents/{id}/status` |
| 删除日志 | DELETE | `/api/logs` |
| 创建用户 | POST | `/api/users`（`createUser`） |
| Provider | GET | `/api/providers` |
| Provider 模型 | GET | `/api/providers/{id}/models` |

---

## 12. 状态管理（Zustand Store 一览）

| Store | 职责 |
|-------|------|
| `settingsStore` | theme/osTheme/accentColor/fontSize/contentWidth/opacity/gatewayMode/gatewayUrl/logLevel，持久化 localStorage |
| `gatewayStore` | status/health/localState + checkHealth/startLocalGateway/stopLocalGateway/checkLocalStatus |
| `agentStore` | agents(含 meta/profile/sessions/sessionTitle/tokenTotals)、selectedAgentId、fetchAgents/selectAgent/install/uninstall/start/stop/clone |
| `chatStore` | agentStates → sessionStates（messages/tokenUsage/contextUsage/sessionStatus/model/provider/inputValue）、mqttConnected、MQTT 事件处理 |
| `debugStore` | 调试会话状态、snapshots、connect/disconnect/resume/pause/step/stop/restart/rewind/reExecute/patchContext |
| `layoutStore` | activePanelTab/resultsCollapsed/filePanelBounds |
| `workspaceStore` | 工作区状态、locateRequest |
| `fileEditorStore` | openFiles/activeFileId |
| `fileTreeStore` | 文件树 |
| `editorStatusStore` | 光标/选区/LSP 状态 |
| `statusBarStore` | 状态栏 message/type/visible/setStatus/clearStatus |
| `userProfileStore` | 用户 profile（displayName/avatar） |
| `skillStore` | 技能 |
| `mcpStore` | MCP 服务器 |

---

## 13. 无障碍

| 规则 | 实现 |
|------|------|
| 键盘导航 | 所有交互元素可通过 Tab 聚焦 |
| ARIA 标签 | NavBar `role="navigation"`、Agent 列表 `role="list"/"listitem"`、拖拽手柄 `role="separator"`、输入框/按钮 `aria-label` |
| 焦点指示器 | 可聚焦元素 `focus-visible:ring` |
| 对比度 | 主要文本 zinc-700/zinc-900（浅）/ zinc-300（深），≥4.5:1 |
| 工具提示 | `Tooltip` 组件（延迟、方向可控） |

---

## 14. 与设计文档的关系

| 文档 | 关系 |
|------|------|
| `docs/design/14-desktop-app.md` | 架构、技术选型、窗口管理 — 本文档在其基础上细化交互 |
| `docs/design/10-debug-protocol.md` | 开发者模式/调试协议 — Debug Tab 交互依据 |
| `docs/design/13-skill-system.md` | Skill 系统 — ChatPanel 工具栏 Skills 入口、SkillsPanel |
| `docs/_internal/archive/plan/zh/plan-p5.md` | S1 任务定义（归档） |
