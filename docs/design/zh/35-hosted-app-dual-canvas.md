# 设计文档 35：托管应用双画布（用户界面 / Agent 工作态 / 聊天预览）

**版本**：1.0（2026-11-19）
**状态**：已接受，待实现（首个 `visual` 应用：FigCraft）
**权威决策**：[ADR-118](../../adr/zh/ADR-118-hosted-app-dual-canvas.md)
**前置**：[ADR-116](../../adr/zh/ADR-116-hosted-application-framework.md) · [设计文档 34](./34-hosted-application-framework.md)
**English counterpart**：[Design Doc 35](../en/35-hosted-app-dual-canvas.md)

> 中文文档为 source of truth，英文 counterpart 按约定同 commit 维护。
>
> 本文是 ADR-116 之上的**工作态契约**。进程、反代、ticket、catalog 仍以文档 34 为准；本文规定用户画布、Agent 工作态、聊天预览如何分离，以及新应用必须遵守的清单。

---

## 1. 定位

一个托管应用对外呈现三种东西，允许缺省，不允许混用：

| 面 | 谁在用 | 状态在哪 | 入口 |
|---|---|---|---|
| **用户画布** | 当前登录用户 | 该用户的 Desktop webview（及用户在应用数据目录下的文件） | 左侧导航 → `AppFrame` → `/api/apps/{id}/ui/` |
| **Agent 工作态** | 某个 Agent 实例的某一条聊天 | **应用进程内**按键分表的会话 | Runtime → `{mcp.gateway_path}` → `POST /mcp` |
| **聊天预览** | 用户想看 Agent 画到哪了 | 只读投影，权威仍是 Agent 工作态 | 聊天卡片 → 常驻栏 iframe → 带 canvas 查询的 `/ui/` |

DOC / PM 是 **library**：Agent 写入共享持久库，用户打开那份资源即看到结果，不需要第三面的活画布。FigCraft 及同类是 **visual**：Agent 的稿在会话里，用户不打开预览就看不见现场。

平台聊天**默认不是共编**。共编只发生在应用自己的 UI 里再放聊天 / Agent 的时候（那时才允许 MCP attach 用户画布）。

---

## 2. 架构

```
Desktop                                              Gateway                         App process (one per app_id)
┌─────────────────────────────┐                     ┌──────────────┐                ┌─────────────────────────────┐
│ 左侧导航 AppFrame           │  ticket + /ui/      │ app::proxy   │                │ 用户静态 bundle             │
│  = 用户画布（不接 MCP）      │────────────────────►│              │───────────────►│ 不持有 Agent 会话            │
│                             │                     │              │                │                             │
│ 聊天转录：画布卡片           │                     │ app::mcp_    │  X-MCP-Actor   │ Agent 会话表                │
│  （无 iframe）               │                     │  proxy       │  X-MCP-Conv.   │  key = (actor, conversation)│
│                             │                     │              │───────────────►│  Headless / 等价工作态      │
│ 编辑器栏：预览 AppFrame      │  ticket + /ui/?canvas│              │                │  POST /mcp → 该会话         │
│  = 只读查看器                │────────────────────►│              │───────────────►│  GET /api/canvas/… 投影     │
└─────────────────────────────┘                     └──────────────┘                └─────────────────────────────┘
        ▲                                                                                    │
        │  theme / locale postMessage                                                        │
        └────────────────────────────────────────────────────────────────────────────────────┘
                              Agent Runtime 只走 MCP，不碰用户 iframe
```

单进程、多会话。画布隔离不靠多进程。

---

## 3. `app.toml` 增量

文档 34 的段全部保留。新增：

```toml
[ui.canvas]
# library = 共享持久库（DOC / PM）。用户打开资源即看见 Agent 的写入。
# visual  = 会话内画布（FigCraft 及同类）。必须提供聊天预览通道。
# none    = 无用户 webview 工作态。
kind = "visual"

# visual 时：预览 iframe 的 UI 路径（相对该应用 /ui/）。
# 应用把 query 里的 canvas 会话 id 投影为只读视图。
preview_path = "/?canvas={canvas_id}"
```

| 字段 | 默认 | 说明 |
|---|---|---|
| `ui.canvas.kind` | `library`（有 webview 时）或 `none` | 未声明时，内置 PM/DOC 视为 `library`；纯 MCP 服务视为 `none` |
| `ui.canvas.preview_path` | 无 | 仅 `visual` 需要。`{canvas_id}` 由平台替换为会话键的 URL-safe 形式 |

`[mcp].auto_inject = true` 仍是进入 `acowork/global/mcps` 的开关（文档 34）。`visual` 应用要让 Agent 用工具，必须打开它。

---

## 4. Agent 工作态

### 4.1 键

```
canvas_id = app_id + actor_id + conversation_id
```

- `actor_id`：`X-MCP-Actor`，Gateway 已校验为已安装 instance_id。
- `conversation_id`：`X-MCP-Conversation`，Runtime 在每次 MCP HTTP 请求上带当前聊天会话 id。
- 缺 Actor：写工具拒绝（与 DOC 匿名只读同一精神，具体只读集合由应用自己定）。
- 缺 Conversation：写工具拒绝。禁止复用「进程里仅有的一份」。

同一 Agent、两条聊天 = 两份工作态。用户的左侧画布**没有**这个键，也不进入这张表。

### 4.2 权威与寿命

- 权威：应用进程内存（可另持久化到该应用数据目录 `app-data/{id}/canvases/{canvas_id}/`，键仍是上面那个）。
- 创建：该键上第一次写工具。
- 销毁：应用自己的 GC（空闲超时、会话结束通知——后者是后续平台能力，本版不阻塞）。进程重启可以丢未导出的 visual 会话；需要保住的应用自己落盘。
- `tools/call` **不得**要求预览 iframe 在线。

### 4.3 MCP HTTP

沿用文档 34：单 `POST /mcp`，Streamable HTTP 无状态子集。应用把 JSON-RPC 交给**该请求键对应的** `Server` / 工具分发，而不是全局单例。

Gateway `/api/apps/{id}/mcp`（及声明的 `gateway_path`）继续只做反代 + Actor 校验。工具列表、截图、导出都是应用的事。

### 4.4 与 library 应用的关系

DOC / PM 的「工作态」是库里的文档 / 任务，本来就按资源 id 寻址，不是一块匿名画布。它们：

- 继续用 `X-MCP-Actor` 做写入署名（已有）；
- **不必**为每条聊天建一块画布；
- **不必**实现预览 iframe；
- 聊天里现有的「打开这份文档 / 任务」已满足「看见结果」。

新的 library 应用走同一条：共享库 + 用户打开资源，不要为了「统一」去实现 visual 预览。

---

## 5. 用户画布

- 入口、ticket、sandbox、身份注入：文档 34 原样。
- Desktop 离开该导航时可以卸 iframe（用户稿若只活在 WASM 里，应用自己负责草稿恢复；这是应用问题，不是平台 MCP 问题）。
- **禁止**把该 iframe 登记成 MCP Backend。
- 主题：见 §7。

---

## 6. 聊天预览（仅 `kind = visual`）

### 6.1 转录卡片

当本会话出现该应用 MCP 的 `tools/call`（成功即可）时，聊天插入（或更新）一张**非窗口化行内的**画布卡片：

- 应用名、可选标题、最近一次 `screenshot` / 导出缩略图（有则示，无则占位）；
- 动作：「打开画布」。

卡片是普通消息 / 附件结构，随转录窗口化滚动。**卡片内不挂 iframe。**

### 6.2 常驻 iframe

「打开画布」把预览挂到**文件编辑器栏**（与点开 workspace 文件同一槽；若当时没有打开文件，就展开该栏）。要求：

1. 使用现有 `AppFrame`（同一 sandbox：无 `allow-same-origin`，有 `allow-downloads`）。
2. `src` = Gateway `/api/apps/{id}/ui/` + `preview_path` 展开 + fragment 里的 ticket（ticket 仍走 fragment，不进 query）。
3. `canvas_id` 进 query 或应用约定的 preview 参数；它标识 Agent 会话，不是用户 id。
4. 该 iframe 在用户关掉预览或切到另一会话预览之前保持挂载。切聊天会话则卸掉或换成那条会话的 canvas。
5. 第一次该应用 MCP 工具成功后 **可以**自动打开预览；闲聊不得预加载 WASM。

### 6.3 预览如何跟上工作态

预览是投影，推荐两种，应用二选一或组合：

| 方式 | 做法 | 何时用 |
|---|---|---|
| 快照 | 每次写工具后更新文档字节 / PNG；预览页轮询或经 `/api/canvas/{canvas_id}/snapshot` 取 | 实现成本最低 |
| 注入控制 | adapter 在返回的 `index.html` 注入脚本：补 `?control`、听 `acowork` theme、把快照喂给页面已有的文件打开 API | 需要像真的编辑器一样看稿时 |

禁止把预览 iframe 登记为 MCP 的唯一 Backend。可以额外 attach 作显示，断线不得影响工具。

### 6.4 为什么不在气泡里

`VirtualRowList` 只挂视口附近的行。FigCraft 一类 WASM 在行里会随滚动反复启动，状态还会和进程内权威分叉。预览必须放在转录**外面**的常驻栏。

---

## 7. 主题与语言

ADR-116 已规定 `AppFrame` 在 load / `ready` 时 `postMessage`：

```json
{ "channel": "acowork", "type": "theme", "theme": "light|dark", "accent": "#…", "locale": "zh-CN" }
```

本契约补两条：

1. Desktop 在 `documentElement` 的 light/dark class 变化时**再推一次**（用户画布和预览 iframe 都要收到）。
2. 应用侧：hosted adapter 注入的脚本监听该消息，映射到应用自己的主题命令（FigCraft：`view.themeLight` / `view.themeDark`，或 `ui.set { theme }`）。默认 `system` 会跟着操作系统走，**跟不着** ACowork 壳，visual 应用在 iframe 内应改用壳推下来的值。

accent 各应用可以忽略。locale 能接就接，不接也不违反契约。

**不要**为了主题去改上游 `index.html` 或 UI crate；注入发生在 adapter serve `/ui/` 的时候。

---

## 8. 交接与共编

**交接（默认）**：Agent 把结果写成应用能打开的资源（`.figcraft`、文档 id、导出 PNG）。用户在左侧画布打开 = 用户行为，此后是用户稿。

**共编（显式、后期）**：应用 UI 内嵌聊天，用户打开的就是那块画布。那时才允许对该用户画布 attach。平台左侧导航 + 平台聊天这条路径永远不自动共编。

---

## 9. Hosted adapter（改动收口）

上游仓库（或 fork）里，ACowork 特有的代码集中在一个二进制 / crate，例如 `apps/{name}-acowork`：

| 职责 | 放这里 | 不要放 |
|---|---|---|
| ADR-116 CLI（`--host` / `--port` / `--port-file` / `--gateway-health-url`） | adapter `main` | 上游 desktop `main` |
| `GET /health`、`GET /ui/*`、`POST /mcp` | adapter | 上游 |
| 按 `(actor, conversation)` 的会话表 | adapter | 上游全局单例 MCP |
| 预览 snapshot API | adapter | 上游 |
| 给 `index.html` 注入 control / theme / canvas 脚本 | adapter 响应路径 | 上游 `index.html` 源文件 |
| `packaging/acowork/app.toml` | adapter 打包目录 | 上游发行配置 |

可以 **依赖** 上游 MCP / 引擎 crate（如 `figcraft-mcp::Server::handle`），实现自己的 `Backend` trait 指向 Headless 会话。不要改上游 tools 表去迁就 HTTP。

路径适配（Web 没有真实文件系统时，把 `open_file` 的 path 读成字节再走页面/引擎的 `{name, data}`）也放在 adapter 的 Backend 里。

---

## 10. 平台壳要补的能力

应用遵守本文即可被托管；下面几条是 **ACowork Desktop / Runtime** 的义务，避免每个应用各写一套聊天 UI。

| # | 层 | 义务 |
|---|---|---|
| P1 | Runtime | MCP HTTP 请求带 `X-MCP-Conversation: {session_id}` |
| P2 | Gateway | `app::mcp_proxy` 转发该头（不要当伪造身份剥掉） |
| P3 | Desktop `AppFrame` | 主题 class 变化时重推 theme / locale |
| P4 | Desktop 聊天 | `kind=visual` 且本会话对该 app 有 MCP 调用 → 画布卡片 + 打开编辑器栏预览 |
| P5 | Desktop 布局 | 预览走 `AppFrame`，`src` 按 `preview_path` 展开；与用户左侧 `hostedAppId` 导航互不覆盖（用户画布和预览可同时存在：一个在 `view=app`，一个在聊天的文件栏） |

用户在左侧打开 FigCraft 与在聊天打开 Agent 预览是两个 `AppFrame` 实例、两个 ticket、两个 URL。允许并存。

---

## 11. 新应用清单

做一个新的托管应用时，按顺序勾：

**文档 34（托管）**

1. `app.toml`：`id` / `process` / `mcp` / `ui` / `permissions` / `package`
2. generic 进程：`/health` + `/ui/` + `POST /mcp`
3. 只监听 `127.0.0.1`；经 Gateway 反代
4. `mcp.auto_inject` 按是否要进 Agent catalog 选择
5. Desktop 不 import 该应用前端

**本文（工作态）**

6. 选定 `ui.canvas.kind`
7. `library`：MCP 写共享库，用 `X-MCP-Actor` 署名；用户打开资源看结果
8. `visual`：进程内会话表，键 `(actor, conversation)`；预览 URL；adapter 注入主题
9. MCP **不** attach 用户左侧 iframe
10. 不把 webview 放进聊天气泡
11. 胶水只进 hosted adapter
12. 多用户：无会话则报错，不落到「当前那一块」

**FigCraft 对照（首个 visual）**

- 用户画布：已安装的 `/ui/` WASM，左侧导航。
- Agent 工作态：`figcraft-mcp` Headless（或等价 Session），按键分表；`auto_inject = true`。
- 预览：聊天卡片 + 编辑器栏 `AppFrame`；注入脚本喂快照 / `?control`。
- 主题：注入监听 `acowork` theme，调 `view.themeLight` / `view.themeDark`。
- 不改 `crates/mcp`、`crates/engine`、`apps/figcraft-web/index.html`。

---

## 12. 安全

延续文档 34：

- 预览 URL 的 `canvas_id` 不是凭证。页面仍要 ticket；应用服务端用 `X-App-User` + canvas 键做授权：只有「该预览是这个用户正在看的这条聊天里那个 Agent 的稿」才返回快照。实现上：Desktop 打开预览时带 session ticket；应用把 ticket 用户与 canvas 的 conversation 所属关系校验清楚，或只把 snapshot 绑在本次 ticket 所 mint 的用户上，canvas 由 query 指定且必须属于该用户可见的会话（平台可在 mint 预览 ticket 时把 canvas 编进 ticket 域——若未做，应用至少校验用户已登录）。
- MCP 路径继续剥 `X-App-User` 等，Agent 不能靠自报用户去读别人的用户画布。
- 预览 sandbox 与用户画布相同，无 `allow-same-origin`。

首版允许简化：snapshot 接口要求合法 ticket，且 `canvas_id` 的 `actor` 对当前 Gateway 是已安装 Agent；同一 Gateway 上的用户互相不可见性靠「不知道 canvas_id」。收紧授权是后续项，不得在文档里把「猜到 id 就能看别人的稿」写成特性。

---

## 13. 验证（落地时）

1. 用户 A 左侧打开应用、画自己的稿；用户 B 的 Agent `tools/call` **不得**出现在 A 的左侧画布上。
2. 同一用户两条聊天、同一 Agent：两份工作态，预览各自打开互不覆盖写入。
3. 关掉预览 iframe 后再 `tools/call` 仍成功，再打开预览能看到最新快照。
4. 聊天滚动、折叠 explore 块，不得卸载预览栏里的 iframe。
5. ACowork 切换 light/dark，用户画布与预览（若开着）跟壳走，不跟 OS。
6. `auto_inject = false` 时 catalog 无该 server；`true` 时 `GET` 全局 MCP 列表含 `{mcp.gateway_path}`。
7. Adapter 对上游 `index.html` 的 diff 为空（注入只在 HTTP 响应里）。

---

## 14. 边界

- 会话结束通知、canvas GC 协议：未做。应用可自订空闲超时。
- 预览 ticket 绑定 canvas：§12 收紧项。
- 应用内共编：不在本版实现范围。
- memory 面板型 UI：仍排除。
- Mobile：本版 Desktop 先行；移动端是否挂预览栏另议，红线 1–3（用户画布、Agent 权威在进程、转录不挂 iframe）仍然适用。
