# ADR-118：托管应用的双画布 —— 用户界面与 Agent 工作态分离

**状态**：已接受（2026-11-19；用户定案「左侧导航是用户自己的画布；聊天里 Agent 操作 Agent 自己的画布；聊天要能打开 iframe 看结果；共编只发生在应用界面内再放聊天的时候」）
**日期**：2026-11-19
**决策者**：用户

**关联**：
- [ADR-116](./ADR-116-hosted-application-framework.md)（托管应用框架：进程 / 反代 / catalog / iframe 壳）
- [设计文档 34](../../design/zh/34-hosted-application-framework.md)（ADR-116 的完整契约）
- [设计文档 35](../../design/zh/35-hosted-app-dual-canvas.md)（本 ADR 的完整契约、通道与新应用清单）
- [ADR-009](./ADR-009-gateway-workspace-isolation.md)（Gateway 不读应用数据目录）
- [ADR-069](./ADR-069-mcp-tool-level-optin.md)、[ADR-073](./ADR-073-agent-instance-identity-decomposition.md)（MCP catalog + `X-MCP-Actor` = instance_id）
- [ADR-104](./ADR-104-agent-manifest-declaration-surface.md)（`[[mcp_servers]]` 声明面）
- 聊天转录窗口化：[设计文档 14](../../design/zh/14-desktop-app.md) 与 `VirtualRowList` 约束（transcript 行不在 DOM 里）

> 中文文档为 source of truth，英文 counterpart 按约定同 commit 维护。

---

## 1. 决策摘要

### 1.1 一句话

**托管应用的用户界面和 Agent 工作态是两份东西。** 左侧导航打开的 webview 是当前登录用户的画布，纯用户行为，MCP **不** attach 上去。聊天里 Agent 创建或修改该应用的文件时，操作的是按 `(app_id, X-MCP-Actor, conversation_id)` 隔离的 Agent 工作态。用户要看见结果，聊天给出一张卡片，点开后在**聊天旁的常驻栏**挂一块只读预览 iframe——不是消息气泡里的 iframe，也不是用户左侧那块画布。

### 1.2 关键决策表

| # | 决策 | 结论 |
|---|---|---|
| 1 | 用户画布 | 左侧导航 `ui.mode = webview` 的 iframe 只服务当前用户。打开、编辑、保存都是用户行为，与 Agent 无关 |
| 2 | Agent 工作态 | MCP `tools/call` 只改 Agent 自己的会话。权威状态在应用进程内，按 `(app_id, actor, conversation)` 分表，**不**在 Desktop iframe 里 |
| 3 | 进程 | 每个 `app_id` 仍是 Gateway 一个 generic 进程（ADR-116）。隔离靠会话表，不靠按用户再拉一份进程 |
| 4 | 聊天预览 | 转录里只放卡片（缩略图 +「打开画布」）。iframe 挂在文件编辑器那一列（或同等常驻栏）。**禁止**把应用 WASM/webview 放进 `VirtualRowList` 行 |
| 5 | 预览 ≠ 后端 | 预览 iframe 是 Agent 工作态的查看器。Agent 没人看着也能画；iframe 卸掉不得让 `tools/call` 失败 |
| 6 | 交接 | 用户要自己改 Agent 的结果：应用导出资源（文件、文档 id），用户在左侧画布打开。打开之后是用户画布，Agent 不再碰，除非走决策 7 |
| 7 | 共编 | 仅当应用**自己的 UI** 内嵌聊天 / Agent 时，才允许 MCP attach 到用户画布。平台聊天默认不提供共编 |
| 8 | 应用种类 | `library`（DOC / PM）：共享持久库，用户打开资源即看见 Agent 写过的东西，不强制预览 iframe。`visual`（FigCraft 及同类）：工作态是一块画布，必须提供聊天预览。`none`：无用户 webview |
| 9 | 主题 | Desktop `AppFrame` 已有 `postMessage`（theme / locale / accent）。应用页监听即可。ACowork 浅色/深色变化时壳必须再推一次 |
| 10 | 改动收口 | 适配代码集中在应用的 **hosted adapter**（如 `figcraft-acowork`）：MCP HTTP、按 Actor 的会话、预览 URL、HTML 注入。禁止为接平台去改上游引擎 / 上游 MCP crate / 上游 UI 源码 |
| 11 | Catalog | `mcp.auto_inject = true` 时走 ADR-116 已有的 `catalog_apps()` → `acowork/global/mcps`。Gateway 不解析工具语义 |

### 1.3 不变量（红线）

1. **MCP 默认不 attach 用户 webview。** 用户左侧打开的 iframe 不是 Agent 的后端。
2. **Agent 工作态的权威在应用进程。** Desktop iframe 崩溃、切走、虚拟列表卸行，不得丢稿、不得让下一发 `tools/call` 找不到会话。
3. **聊天转录里不挂活的应用 iframe。** 行是窗口化的，滚出视口即不在 DOM。
4. **预览 iframe 只读 Agent 会话。** 不得误接到另一个用户、另一个 Agent、另一条会话，也不得接到左侧用户画布。
5. **多用户共用一个应用进程时，会话键必须包含 Actor。** 缺会话时返回明确错误，禁止静默落到「当前唯一那块画布」。
6. **Gateway 仍然零应用业务**（ADR-116 / ADR-009）：不读应用数据目录、不解释工具、不渲染画布。
7. **共编是应用内功能，不是平台聊天默认。**

### 1.4 明确不做

- 不把 Agent 工具打到用户正在左侧看的那块 iframe 上。
- 不按用户或按 Agent 再 spawn 一份应用进程来「隔离画布」。
- 不把 FigCraft / 同类 WASM 当作 MCP 的唯一后端。
- 不为接 ACowork 去 fork 上游应用的引擎、命令表或 Web UI 源文件；胶水留在 adapter。
- 不把 memory 右侧面板塞进本契约（仍是 ADR-116 排除项）。

---

## 2. 背景

ADR-116 解决了「应用如何被托管」：独立进程、反代、catalog、左侧导航槽、沙箱 iframe。它没有回答 **Agent 改的是哪一份状态**。

把 MCP attach 到用户当前打开的 webview，表面上能「看见 Agent 在画」，但：

- 用户自己的稿被 Agent 抢走；
- 多用户 / 多 Desktop 会串到同一块 attach 点；
- Agent 能否工作取决于那块 iframe 还在不在——和 DOC 相反：用户没打开 Docs 视图，Agent 照样 `doc_write`。

用户定案与 DOC 同类：**打开应用界面是纯用户行为；Agent 走 MCP 操作自己的工作态；要看结果，聊天给一个打开预览的机制。** 共编留到应用界面自己放聊天时再做。

---

## 3. 身份与键

| 通道 | 身份 | 用途 |
|---|---|---|
| `/api/apps/{id}/ui/*` + `/api/*` | app session ticket → `X-App-User` | 用户画布、预览 iframe 的页面与 API |
| `{mcp.gateway_path}` | 机器凭证 + `X-MCP-Actor`（instance_id）+ `X-MCP-Conversation`（会话 id） | Agent 工具。Gateway 校验 Actor 已安装，剥掉客户端自报的用户身份（ADR-116 MCP 路径已如此） |

Agent 工作态键：`(app_id, actor_id, conversation_id)`。同一 Agent 开两条聊天 = 两份稿。缺 `X-MCP-Conversation` 时应用 MUST 拒绝写工具（可允许只读探测），MUST NOT 复用「进程里仅有的那一份」。

Runtime 在 MCP HTTP 请求上转发当前会话 id（头名 `X-MCP-Conversation`）。Gateway `app::mcp_proxy` 原样转发该头（它不是伪造身份头）。

---

## 4. 后果

**正面**

- 后续 visual 应用（白板、表格、原型）复用同一套：用户画布、Agent 会话表、聊天卡片、常驻预览 iframe。
- 多用户不再抢用户 iframe；Agent 之间按 Actor × 会话隔离。
- 上游同步冲突面收在 adapter。

**负面**

- Runtime 需带上 `X-MCP-Conversation`（新头，小改）。
- Desktop 聊天要识别「该应用的 MCP 被调用」并给出卡片 / 打开预览，这是平台壳的能力，不是每个应用自己画聊天 UI。
- `library` 与 `visual` 要在 `app.toml` 声明，避免 DOC 被强制弹出画布 iframe。

**回滚**

- 关闭应用的 `mcp.auto_inject` 即从 catalog 消失。
- 预览栏不实现时，Agent 仍可工作，用户只能看 tool_result 文本 / 截图——功能降级，不破坏红线 1–3。

---

## 5. 落地顺序

完整步骤与 API 见 [设计文档 35](../../design/zh/35-hosted-app-dual-canvas.md)。首个 `visual` 应用是 FigCraft（ACowork fork）。PM / DOC 保持 `library`，不强制预览 iframe。
