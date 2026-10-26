# Desktop App（桌面应用）

> 版本：v3.4 | 更新日期：2026-04-16

---

ACowork Desktop App 是基于 Tauri 的桌面客户端，作为用户与 Agent 交互的主界面。Desktop App 与 Gateway 是**独立进程**，通过 Gateway Service API 通信。

## 1. 定位与职责

Desktop App 是 ACowork 平台的**用户界面层**，不承载任何平台核心逻辑。它的职责是：

| 职责 | 说明 |
|------|------|
| Agent 交互 | 用户与 Agent 的对话界面，消息收发 |
| Agent 管理 | 安装、卸载、克隆、创建、启停 Agent |
| 调试面板 | 开发者模式下对 Agent Runtime 的步进调试、录制回放 |
| 配置管理 | Gateway 配置、API Key 管理（Vault）、Provider 配置 |
| 系统托盘 | Gateway 状态指示、快捷操作 |

**Desktop App 不做的事：**
- 不运行 Agent 逻辑（Agent Runtime 独立进程）
- 不管理 Agent 生命周期（Gateway 负责）
- 不存储 API Key（Gateway Vault 负责）
- 不代理 LLM 调用（Agent Runtime 直连）

## 2. 架构概览

```
┌─────────────────────────────────────────────────────────────────┐
│                    ACowork Desktop App (Tauri v2)                │
│                                                                  │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │  WebView Frontend (React)                                 │  │
│  │                                                           │  │
│  │  ┌────────┐ ┌──────────┐ ┌──────────┐ ┌───────────────┐  │  │
│  │  │ Agent  │ │ Chat     │ │ Execution│ │ Settings      │  │  │
│  │  │ List   │ │ Panel    │ │ Results  │ │ (Vault/Config)│  │  │
│  │  └────────┘ └──────────┘ └──────────┘ └───────────────┘  │  │
│  │  ┌────────┐ ┌──────────┐ ┌──────────┐ ┌───────────────┐  │  │
│  │  │ Debug  │ │ Skill    │ │ Manifest │ │ Publish       │  │  │
│  │  │ Panel  │ │ Editor   │ │ Editor   │ │ Wizard        │  │  │
│  │  └────────┘ └──────────┘ └──────────┘ └───────────────┘  │  │
│  └───────────────────────────────────────────────────────────┘  │
│                            │ Tauri IPC (invoke)                  │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │  Rust Backend                                             │  │
│  │                                                           │  │
│  │  ┌──────────────┐  ┌──────────────┐  ┌───────────────┐  │  │
│  │  │ Gateway      │  │ Debug        │  │ Tray          │  │  │
│  │  │ Client       │  │ Protocol     │  │ Manager       │  │  │
│  │  │ (HTTP/MQTT)  │  │ Client       │  │               │  │  │
│  │  │              │  │ (HTTP+MQTT)  │  │               │  │  │
│  │  └──────────────┘  └──────────────┘  └───────────────┘  │  │
│  └───────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
          │                                    │
          │ Gateway Service API                │ Debug Protocol
          │ (HTTP REST + MQTT)                  │ (HTTP RPC + MQTT events)
         ▼                                    ▼
┌─────────────────────┐           ┌─────────────────────────┐
│  Gateway (独立进程)  │           │  Agent Runtime          │
│                     │           │  (DevMode)              │
│  ┌───────────────┐  │           │                         │
│  │ Key Vault     │  │           │  主循环受调试器控制       │
│  │ Lifecycle     │  │           │  消息可编辑重执行        │
│  │ Intent Router │  │           │  Skill 热加载           │
│  │ Package Mgr   │  │           │  录制/回放引擎          │
│  └───────────────┘  │           └─────────────────────────┘
└─────────────────────┘
```

### 2.1 Desktop App 与 Gateway 的通信

Desktop App 通过 **Gateway HTTP API** 与 Gateway 交互：

| 操作 | HTTP 方法 | 路径 | 说明 |
|------|----------|------|------|
| Gateway 状态 | GET | `/health` | 健康检查 |
| Agent 列表 | GET | `/api/agents` | 已安装的 Agent 列表 |
| Agent 安装 | POST | `/api/agents/install` | 安装 .agent 包 |
| Agent 卸载 | DELETE | `/api/agents/:id` | 卸载 Agent |
| Agent 克隆 | POST | `/api/agents/clone` | 克隆 Agent |
| Agent 启停 | POST | `/api/agents/:id/start` | 启动 Agent |
| Agent 启停 | POST | `/api/agents/:id/stop` | 停止 Agent |
| 发送消息 | POST | `/api/agents/:id/message` | 向 Agent 发送用户消息 |
| 流式响应 | WebSocket | `/api/agents/:id/stream` | 订阅 Agent 的流式输出 |
| Vault 操作 | GET/POST | `/api/vault/*` | API Key 管理 |
| 配置操作 | GET/PUT | `/api/config/*` | Gateway 配置 |

> 注：Gateway 原有的 Socket API（给 Agent Runtime 用的）保持不变。Desktop App 使用 HTTP API，Gateway 需要额外暴露一层 HTTP 接口供 Desktop App 调用。这是两个不同的消费端，协议分层：

```
Gateway
├── Socket API (端口 A)    ← Agent Runtime 使用（现有设计）
└── HTTP API (端口 B)      ← Desktop App + CLI 使用（新增）
```

### 2.2 Desktop App 与 Agent Runtime 的通信（DevMode）

开发者模式下，Desktop App 直接通过 **Debug Protocol**（WebSocket）连接到目标 Agent Runtime：

```
Desktop App  ──WebSocket──>  Agent Runtime (DevMode, ws://127.0.0.1:19877)
```

Debug Protocol 的完整定义见 [10-debug-protocol.md](./10-debug-protocol.md)。

## 3. 页面布局

Desktop App 采用**左中右四栏布局**，根据当前模式（用户模式 / 开发者模式）动态调整可见性和内容。

### 3.1 整体布局

```
┌──────────────────────────────────────────────────────────────────────┐
│  ACowork                            [Developer Mode ○]  [— □ ✕] │
├────┬──────────────┬────────────────────────┬────────────────────────┤
│    │              │                        │                        │
│ 📱 │   Agent      │     Chat Panel         │   Execution            │
│ 💬 │   List       │                        │   Results              │
│ 🤖 │              │                        │                        │
│ ⚙️ │  ┌────────┐  │  ┌──────────────────┐  │   用户模式:            │
│    │  │Agent A  │  │  │ User:            │  │   - 工具调用结果        │
│    │  │Agent B  │  │  │ Assistant:       │  │   - 执行耗时           │
│    │  │Agent C  │  │  │ Tool:            │  │   - Token 用量         │
│    │  │...      │  │  │ Assistant:       │  │                        │
│    │  └────────┘  │  └──────────────────┘  │   开发者模式:           │
│    │              │                        │   - 调试控制台          │
│    │              │  ┌──────────────────┐  │   - 单步执行详情        │
│    │              │  │ Input: [_______] │  │   - 断点状态            │
│    │              │  └──────────────────┘  │   - 录制控制            │
│    │              │                        │                        │
└────┴──────────────┴────────────────────────┴────────────────────────┘
```

### 3.2 各区域说明

#### 3.2.1 导航栏（最左侧，固定宽度 48px）

垂直图标导航，点击切换中间内容区：

| 图标 | 功能 | 说明 |
|------|------|------|
| 💬 Chat | 聊天视图 | 默认视图，包含 Agent List + Chat + Results |
| 🤖 Models | 模型管理 | Provider 列表、模型配置、API Key 状态 |
| 📋 Skills | 技能列表 | 当前 Agent 的 Skills 列表、编辑入口 |
| ⚙️ Settings | 设置 | Gateway 连接配置、全局偏好、关于 |

#### 3.2.2 Agent 列表（第二栏，宽度 240px）

- 显示所有已安装的 Agent（来自 Gateway `/api/agents`）
- 每个 Agent 条目：图标/名称 + 运行状态指示器
- 点击选择当前交互的 Agent
- 右键菜单：启动/停止、查看详情、克隆、卸载、设置
- 底部：`+ 创建 Agent` / `+ 从文件安装` 按钮
- **开发者模式额外**：Agent 旁显示 `dev` 标签（开发态 Agent）

#### 3.2.3 聊天面板（第三栏，弹性宽度）

当前选中 Agent 的对话界面：

- **消息流**：显示对话历史（system/user/assistant/tool 消息）
- **输入区**：底部输入框，支持多行输入和快捷键发送
- **工具调用展示**：内联展示 tool_call 和 tool_result，可展开/折叠
- **流式输出**：LLM 响应实时流式显示
- **消息操作**：
  - 用户模式：复制消息内容、重新生成
  - 开发者模式额外：编辑消息内容、从该消息重执行（Re-execute from here）

#### 3.2.4 执行结果区（第四栏，宽度 320px，可折叠）

**用户模式下**：
- 工具调用摘要：工具名、参数、耗时、状态（成功/失败）
- 当前会话 Token 用量统计（prompt/completion/total）
- 当前 Agent 运行状态

**开发者模式下**（替代上述内容）：
- 调试控制栏：Resume / Pause / Step / Stop
- 单步执行详情：当前迭代轮次、阶段（Phase）、LLM 输入/输出
- 断点面板：已设置的断点列表、添加/删除断点
- Provider 切换器：下拉选择当前 Provider + 模型
- 录制控制：开始录制 / 停止录制 / 加载回放

### 3.3 开发者模式切换

用户模式与开发者模式通过顶部工具栏的 toggle 切换。切换逻辑：

```
用户模式
  │
  └── 开启 Developer Mode
      │
      ├── Desktop App 向 Agent Runtime 发送 DebuggerAttach
      │   （如果当前 Agent 未以 DevMode 运行，先通知 Gateway 重启为 DevMode）
      │
      ├── 执行结果区切换为调试面板
      │
      ├── 聊天面板消息增加 Edit / Re-execute 操作
      │
      └── 导航栏额外显示 Skills 编辑、Manifest 编辑入口
```

开发者模式的完整能力定义见 [10-debug-protocol.md](./10-debug-protocol.md)。

### 3.4 窗口管理

| 行为 | 说明 |
|------|------|
| 关闭窗口 | 隐藏到系统托盘（不退出进程） |
| 系统托盘图标 | 显示 Gateway 连接状态（已连接/未连接/错误） |
| 左键托盘图标 | 显示/聚焦主窗口 |
| 右键托盘菜单 | 显示 Dashboard / Agent Chat / 状态 / 退出 |
| 最小尺寸 | 1024 x 600 |
| 默认尺寸 | 1200 x 800 |

## 4. 用户模式功能

### 4.1 首次启动引导

```
Step 1: 欢迎
  "欢迎使用 ACowork，让我们快速配置你的环境"

Step 2: Gateway 连接
  ├─ 自动检测本地 Gateway（尝试连接默认地址）
  ├─ 检测成功 → 跳到 Step 3
  └─ 检测失败 → 提示启动 Gateway 或配置地址

Step 3: API Key 配置
  ├─ 从文件导入
  ├─ 手动输入（Provider + Key）
  └─ 后续在 Settings 中管理

Step 4: 身份信息采集（必填 → 选填）
  ├─ 必填：称谓 / 语言 / 时区
  ├─ 选填：城市 / 职业 / 沟通偏好
  └─ 完成后调用 Gateway HTTP API 写入系统 Agent

Step 5: 安装第一个 Agent
  ├─ 从本地仓库选择
  ├─ 拖放 .agent 文件安装
  └─ 跳过（之后手动安装）

→ 进入主界面
```

> Step 4 的数据流向：Desktop App → `POST /api/system/identity` → Gateway → UserProfile 模块 → 持久化存储。详见 [18-user-identity-simplified.md](./18-user-identity-simplified.md)。

### 4.2 Agent 管理

| 操作 | 入口 | 说明 |
|------|------|------|
| 安装 | Agent 列表底部 `+` / 拖放 .agent 文件 | 调用 Gateway 安装 API |
| 卸载 | Agent 右键菜单 | 确认后调用 Gateway 卸载 API |
| 启动/停止 | Agent 右键菜单 / 状态指示器 | 调用 Gateway Lifecycle API |
| 查看详情 | Agent 右键菜单 | 显示 manifest 信息、运行状态、版本 |
| 克隆 | Agent 右键菜单（开发者模式） | 见第 5.1 节 |
| 从零创建 | Agent 列表底部（开发者模式） | 见第 5.2 节 |

### 4.3 对话

- 用户输入消息 → Desktop App 调用 Gateway `/api/agents/:id/message` → Gateway 通过 Intent Router 转发给 Agent Runtime
- Agent 响应通过 WebSocket 流式推送回 Desktop App
- 对话历史存储在 Agent 的私有 SQLite 中，Desktop App 不持久化对话数据

### 4.4 设置页面

| 分类 | 内容 |
|------|------|
| Gateway | 连接地址、健康状态、版本信息 |
| Providers | Provider 列表、默认 Provider、模型配置 |
| Vault | API Key 管理（增删改，通过 Gateway Vault API） |
| 外观 | 主题（亮/暗）、语言、字体大小 |
| 通用 | 日志级别、数据目录位置、更新检查 |

### 4.5 工作区管理

#### 工作区选择器交互规格

**当前工作区选择**：
- 下拉菜单中，当前选中的工作区条目显示 checkmark (✓) 图标
- 下拉按钮显示当前选中工作区的别名/路径（而非固定显示第一个）
- 选择新工作区后，调用 `PUT /api/agents/{agent_id}/workspaces/current` 持久化选择
- 同时更新选择统计（`select_count`、`last_selected_at`），用于权重排序

**Agent 切换联动**：
- 当用户切换到不同 Agent 时，自动重新加载该 Agent 的工作区列表
- 工作区列表是**一份带归属标记的全局数组**（`workspacesAgentId` 记录它描述的是哪个 instance）。启动另一个 Agent 会为那个 Agent 取列表并覆盖这一格，因此读方只在归属与当前选中 Agent 相符时才渲染、才允许点选；取列表失败时也要用空列表占下这一格——"这个 Agent 的列表未知"与"这是别的 Agent 的列表"不是一回事，后者能被点中并把不属于当前 Agent 的 workspace id 写进会话。归属不符时重新取一次，界面在一帧内自愈
- 恢复该 Agent 上次的当前工作区选择

**状态管理**：
- 使用独立的 `workspaceStore`（Zustand）管理工作区状态
- 包含 `workspaces` 列表、`currentWorkspaceId`、`loading` 状态

## 5. 开发者模式功能

开发者模式在用户模式基础上叠加调试能力。所有调试协议的详细定义见 [10-debug-protocol.md](./10-debug-protocol.md)。

### 5.1 Agent 克隆

Desktop App 的 Agent 列表右键菜单提供"克隆"选项（开发者模式可见）：

```
用户右键 Agent A → 克隆
       │
       ▼
弹出克隆对话框:
  ├─ 源 Agent: com.example.weather (com.example.weather)
  ├─ 克隆模式:
  │   ○ 骨架克隆（全部包文件——manifest、prompts、skills、assets 及任何包内目录）
  │   ● 完整克隆（+ workspace/config + 对话记录 + SQLite 记忆库；
  │     files/ 和 logs/ 不复制）
  ├─ 新 Agent ID: [com.example.weather    ]  ← 默认与源相同；包名可重复
  │    （唯一性在 instance_id 上，ADR-073）
  ├─ 新 Agent 名称: [留空则用「名称 (克隆)」]
  └─ [取消]  [克隆]
       │
       ▼
调用 Gateway /api/agents/:instance_id/clone
  body: { new_agent_id, new_name, mode }
       │
       ▼
Agent 列表刷新，新 Agent 出现并标记为 dev: true
```

骨架 / 完整的边界只在 [10-debug-protocol.md §7.3](./10-debug-protocol.md) 定义一次；
对话框只负责提供两个选项，不重复描述各自包含什么。

### 5.2 从零创建

Agent 列表底部"创建 Agent"按钮在开发者模式下打开创建向导：

```
Step 1: 基本信息 — agent_id, name, description, author
Step 2: LLM 配置 — 选择 Provider + 模型（从 Vault 中已有 Key 选择）
Step 3: 权限声明 — 勾选所需权限模板
Step 4: 选择模板 — 空白 / 天气 / 日历 / 自定义
Step 5: 生成 → 调用 Gateway API 创建工作区 → 新 Agent 标记 dev: true → 自动进入 DevMode
```

### 5.3 Skill 编辑器

导航栏"Skills"视图在开发者模式下提供编辑能力：

```
┌─ Skills ──────────────────────────────────────────┐
│                                                    │
│  当前 Agent: com.example.weather-dev                │
│                                                    │
│  ┌──────────────┐  ┌──────────────────────────┐   │
│  │ Skills 列表   │  │ SKILL.md 编辑器           │   │
│  │              │  │                          │   │
│  │ ● weather-   │  │ ---                      │   │
│  │   query      │  │ name: weather-query      │   │
│  │              │  │ description: ...         │   │
│  │ ○ news-      │  │ triggers:               │   │
│  │   digest     │  │   - 天气                 │   │
│  │              │  │ ---                      │   │
│  │ [+ 新建]     │  │                          │   │
│  └──────────────┘  │ # Weather Query Skill    │   │
│                    │                          │   │
│                    │ ...                      │   │
│                    └──────────────────────────┘   │
│                                                    │
│  [🔄 Reload to Runtime]  [▶ Test in Chat]         │
└────────────────────────────────────────────────────┘
```

- `Reload to Runtime`：保存后通过 Debug Protocol 发送 `DebuggerReloadSkills`
- `Test in Chat`：热加载后自动在 Chat 面板发送一条触发消息

### 5.4 发布向导

开发者模式下，Agent 列表右键菜单的"发布"选项打开发布向导：

```
Step 1: 检查 — manifest 完整性、SKILL.md 格式、prompts 存在性
Step 2: 清理 — 移除 dev 标记、清空 recordings/、重置 config/
Step 3: 打包 — 生成 .agent ZIP 文件
Step 4: 签名 — 调用 acowork-sign 签名
Step 5: 分发 — 本地安装 / 导出到文件 / 上传仓库
```

详细流程见 [10-debug-protocol.md](./10-debug-protocol.md) 第 8 节。

## 6. 系统托盘

### 6.1 托盘图标状态

| 状态 | 图标样式 | Tooltip |
|------|---------|---------|
| Gateway 已连接 | 正常图标 | `ACowork — Connected` |
| Gateway 未连接 | 灰色图标 | `ACowork — Disconnected` |
| Agent 正在运行 | 蓝色脉冲 | `ACowork — 2 Agents Running` |
| Agent 执行中 | 绿色脉冲 | `ACowork — Working` |
| 错误 | 红色图标 | `ACowork — Error` |

### 6.2 托盘右键菜单

```
┌──────────────────┐
│ Show Dashboard   │
│ Agent Chat       │
│──────────────────│
│ Status: Running  │  (disabled, 仅展示)
│──────────────────│
│ Start Gateway    │  (未连接时显示)
│ Stop Gateway     │  (已连接时显示)
│──────────────────│
│ Quit ACowork    │
└──────────────────┘
```

### 6.3 Gateway 健康轮询

Desktop App 后台定期（每 5s）调用 Gateway `/health` 端点：

```
健康检查
  │
  ├─ Gateway 在线 → 更新托盘状态为 Connected
  │
  ├─ Gateway 离线 → 更新托盘状态为 Disconnected
  │   └─ 主窗口显示"Gateway 未连接"横幅，引导用户启动
  │
  └─ 连续 3 次失败 → 降级为每 30s 轮询（减少资源消耗）
```

## 7. 技术选型

### 7.1 Tauri v2

| 组件 | 选择 | 理由 |
|------|------|------|
| **框架** | Tauri v2 | 安全模型更好（capability-based）、插件系统成熟、Rust 生态一致 |
| **IPC** | Tauri Commands（invoke） | 前后端类型安全通信 |
| **前端框架** | React 19 + TypeScript | 生态最成熟、组件库丰富、Tauri + React 已验证 |
| **构建工具** | Vite | 快速 HMR、Tauri 官方推荐 |
| **UI 组件库** | shadcn/ui + Tailwind CSS | 可定制性强、无运行时依赖、tree-shakable |
| **状态管理** | Zustand | 轻量、TypeScript 友好、适合中等复杂度应用 |
| **HTTP + MQTT** | `fetch` + `mqtt.js`（或 Tauri Command `debug_rpc`） | 流式消息（MQTT 订阅 `acowork/agents/{id}/sessions/{sid}/messages/#`）、Debug Protocol（`debug_rpc` + `debug-event` 事件） |

### 7.2 Tauri Plugins

| Plugin | 用途 |
|--------|------|
| `tauri-plugin-shell` | 调用外部命令（acowork-sign 等 CLI 工具） |
| `tauri-plugin-store` | 持久化 Desktop App 自身配置（Gateway 地址、窗口状态等） |
| `tauri-plugin-single-instance` | 防止多实例 |
| `tauri-plugin-dialog` | 文件选择对话框（安装 .agent 包） |
| `tauri-plugin-notification` | 系统通知（Agent 完成长任务时） |

### 7.3 前端目录结构

```
apps/acowork-desktop/
├── src-tauri/                  # Rust 后端
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── capabilities/           # Tauri 权限声明
│   ├── icons/                  # 应用图标
│   └── src/
│       ├── main.rs             # 入口
│       ├── lib.rs              # Tauri Builder 配置
│       ├── commands/           # Tauri Commands（前端调用的 Rust 函数）
│       │   ├── mod.rs
│       │   ├── gateway.rs      # Gateway API 封装
│       │   ├── agent.rs        # Agent 管理操作
│       │   ├── debug.rs        # Debug Protocol 封装
│       │   ├── vault.rs        # Vault/Key 管理
│       │   └── settings.rs     # 配置管理
│       ├── gateway_client.rs   # Gateway HTTP 客户端
│       ├── debug_client.rs     # Debug Protocol WebSocket 客户端
│       ├── state.rs            # 共享状态（Arc<RwLock>）
│       └── tray/               # 系统托盘
│           ├── mod.rs
│           ├── menu.rs
│           ├── icon.rs
│           └── events.rs
│
└── web/                        # React 前端
    ├── package.json
    ├── vite.config.ts
    ├── tsconfig.json
    ├── index.html
    └── src/
        ├── main.tsx
        ├── App.tsx
        ├── components/         # UI 组件
        │   ├── layout/         # 布局（四栏）
        │   ├── chat/           # 聊天面板
        │   ├── agent-list/     # Agent 列表
        │   ├── results/        # 执行结果区
        │   ├── debug/          # 调试面板（开发者模式）
        │   ├── skills/         # Skill 编辑器
        │   ├── settings/       # 设置页面
        │   └── common/         # 通用组件
        ├── hooks/              # 自定义 Hooks
        ├── stores/             # Zustand 状态
        ├── lib/                # 工具函数、类型定义
        └── styles/             # Tailwind 样式
```

### 7.4 Cargo.toml 依赖

```toml
[package]
name = "acowork-desktop"
version = "0.1.0"
edition = "2024"

[build-dependencies]
tauri-build = { version = "2", features = [] }

[dependencies]
tauri = { version = "2", features = ["tray-icon", "image-png"] }
tauri-plugin-shell = "2"
tauri-plugin-store = "2"
tauri-plugin-single-instance = "2"
tauri-plugin-dialog = "2"
tauri-plugin-notification = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros", "sync", "time", "net"] }
tokio-tungstenite = "0.26"    # WebSocket (Debug Protocol)
anyhow = "1"
directories = "6"
# Desktop App 不依赖 acowork 内部 crate —— 通过 Gateway HTTP API / Debug WebSocket 与平台通信，
# 所有数据结构按 API 契约独立定义，保持 UI 层与平台核心的解耦。

[features]
default = ["custom-protocol"]
custom-protocol = ["tauri/custom-protocol"]
```

## 8. Gateway HTTP API（新增）

Gateway 需要新增一层 HTTP API 供 Desktop App 和 CLI 使用。这与现有的 Socket API（Agent Runtime 用）是两个独立的消费端。

### 8.1 为什么需要 HTTP API

| 维度 | Socket API（现有） | HTTP API（新增） |
|------|-------------------|-----------------|
| 消费者 | Agent Runtime | Desktop App / CLI |
| 传输层 | Unix Socket / Named Pipe | HTTP (localhost) |
| 通信模式 | 长连接 + 双向推送 | 请求/响应 + WebSocket 流式 |
| 用途 | 进程间实时通信 | 用户界面操作 |

Socket API 是底层 IPC 协议，不适合 WebView 直接调用。HTTP API 是面向用户操作的抽象层，两者共享 Gateway 内部逻辑。

### 8.2 HTTP 接口定义

```rust
// Gateway HTTP API 路由（新增）
pub enum HttpRoute {
    // 健康检查
    Get("/health") -> HealthResponse,

    // Agent 管理
    Get("/api/agents") -> AgentListResponse,
    Post("/api/agents/install") -> AgentInstallResponse,         // body: .agent 文件路径
    Delete("/api/agents/:id") -> AgentUninstallResponse,
    Post("/api/agents/:instance_id/clone") -> AgentCloneResponse, // body: { mode, new_agent_id }
    Post("/api/agents/:id/start") -> AgentStartResponse,
    Post("/api/agents/:id/stop") -> AgentStopResponse,
    Get("/api/agents/:id") -> AgentDetailResponse,

    // 对话
    Post("/api/agents/:id/message") -> MessageResponse,          // body: { content }
    Get("/api/agents/:id/stream") -> WebSocketUpgrade,          // 流式对话

    // Vault
    Get("/api/vault/keys") -> KeyListResponse,
    Post("/api/vault/keys") -> KeyAddResponse,                   // body: { provider, key }
    Delete("/api/vault/keys/:provider") -> KeyDeleteResponse,

    // 配置
    Get("/api/config") -> ConfigResponse,
    Put("/api/config") -> ConfigUpdateResponse,

    // 系统信息
    Get("/api/status") -> StatusResponse,
}
```

### 8.3 HTTP Server 实现

Gateway 新增 HTTP Server（使用 Axum），监听 `http://127.0.0.1:19876`：

```rust
// Gateway 进程同时监听：
// 1. Socket API（给 Agent Runtime 用）
// 2. HTTP API（给 Desktop App / CLI 用）
// 两者共享同一个 Gateway 内部状态
```

HTTP 端口可配置，默认 `19876`。仅监听 `127.0.0.1`，不对外暴露。

## 9. 与现有文档的关系

| 文档 | 关系 |
|------|------|
| [01-overview.md](./01-overview.md) | Desktop App 是总纲中"未来扩展"的具体化 |
| [04-gateway.md](./04-gateway.md) | Gateway 新增 HTTP API，其余组件不变 |
| [06-communication.md](./06-communication.md) | Socket API 保持不变，HTTP API 是新增的消费者层 |
| [10-debug-protocol.md](./10-debug-protocol.md) | Desktop App 的开发者模式完全依赖 Debug Protocol |
| [12-tool-system.md](./12-tool-system.md) | Desktop App 不涉及 Tool 执行，仅展示结果 |
| [13-skill-system.md](./13-skill-system.md) | Desktop App 的 Skill 编辑器是 Skill 生命周期的 UI 入口 |

## 10. 设计决策记录

| 决策 | 选择 | 理由 |
|------|------|------|
| Desktop App 与 Gateway 独立 | 独立进程 | Gateway 可独立运行支持 CLI-only 用户；职责清晰 |
| Gateway 新增 HTTP API | Axum HTTP | Socket API 不适合 WebView 直接调用；HTTP 是 Desktop App / CLI 的标准消费接口；与现有 Axum 选型一致 |
| 布局方案 | 左中右四栏 | 导航 + Agent 列表 + 聊天 + 结果区，信息层次清晰；开发者模式复用同一布局叠加调试面板 |
| 前端框架 | React + TypeScript | 生态最成熟；Tauri + React 可行性已验证 |
| UI 组件库 | shadcn/ui + Tailwind | 无运行时依赖、可定制、tree-shakable |
| 系统托盘 | 关闭隐藏不退出 | 标准桌面应用行为；Gateway 常驻，Desktop App 作为其 GUI 前端也应常驻 |
| 单实例 | tauri-plugin-single-instance | 避免多窗口混乱，第二次启动聚焦已有窗口 |
| Gateway 状态检测 | 轮询 /health | 简单可靠；5s 间隔足够；失败后降级到 30s |
