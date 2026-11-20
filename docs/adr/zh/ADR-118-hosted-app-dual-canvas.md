# ADR-118：托管应用的双画布 —— 用户界面与 Agent 工作区文件分离

**状态**：已接受（2026-11-19；用户定案「左侧导航是用户自己的画布；聊天里 Agent 操作的是自己工作区里的文件，和生成图片一样先落盘再预览修改；聊天要能打开 iframe 看该文件；共编只发生在应用界面内再放聊天的时候」。同日修订：Agent 的稿不是会话内存，权威是工作区文件。）
**日期**：2026-11-19
**决策者**：用户

**关联**：
- [ADR-116](./ADR-116-hosted-application-framework.md)（托管应用框架：进程 / 反代 / catalog / iframe 壳）
- [设计文档 34](../../design/zh/34-hosted-application-framework.md)（ADR-116 的完整契约）
- [设计文档 35](../../design/zh/35-hosted-app-dual-canvas.md)（本 ADR 的完整契约、通道与新应用清单）
- [ADR-009](./ADR-009-gateway-workspace-isolation.md)（Gateway 不读应用数据目录；工作区文件走 Runtime）
- [ADR-069](./ADR-069-mcp-tool-level-optin.md)、[ADR-073](./ADR-073-agent-instance-identity-decomposition.md)（MCP catalog + `X-MCP-Actor` = instance_id）
- [ADR-104](./ADR-104-agent-manifest-declaration-surface.md)（`[[mcp_servers]]` 声明面）
- 聊天转录窗口化：[设计文档 14](../../design/zh/14-desktop-app.md) 与 `VirtualRowList` 约束（transcript 行不在 DOM 里）

> 中文文档为 source of truth，英文 counterpart 按约定同 commit 维护。

---

## 1. 决策摘要

### 1.1 一句话

**左侧导航是用户的应用界面；Agent 的稿是它工作区里的一个文件。** 和生成图片同一条路：先在工作区创建文件，再基于这个文件设计、保存、预览，完成后再决定是否上传 DOC、发给别人或让用户拷走。MCP 改的是这份文件，不是一块只活在会话里的匿名画布，也不是用户左侧正在看的那块。聊天预览 = 打开这份工作区文件（`visual` 类型用应用自己的 iframe 查看器）。

### 1.2 关键决策表

| # | 决策 | 结论 |
|---|---|---|
| 1 | 用户画布 | 左侧导航 `ui.mode = webview` 的 iframe 只服务当前用户。打开、编辑、保存都是用户行为，与 Agent 无关 |
| 2 | Agent 的稿 | **工作区文件是权威。** Agent 创建 `*.figcraft`（或该应用的等价文件）落在自己的 workspace 里；MCP 的 open/save/edit 都针对这条路径。不是 `(conversation)` 内存会话 |
| 3 | 引擎 | 应用进程里可以有 Headless / 打开中的文档，那只是这份文件的编辑器缓存。进程重启以磁盘文件为准重新打开。预览 iframe 不是这份文件的后端 |
| 4 | 聊天预览 | 转录里放文件卡片（和图片附件同一类：路径、缩略图、「打开」）。点开后在**文件编辑器栏**用该类型的查看器预览这份工作区文件。`visual` 的查看器是应用 iframe。**禁止**把应用 WASM 放进 `VirtualRowList` 行 |
| 5 | 预览 ≠ 后端 | 关掉预览不得让下一发 MCP 失败。Agent 继续 `open_file` / `save_file` 工作区路径即可 |
| 6 | 交接 | 文件已经在工作区。之后可以：用户在左侧 FigCraft 打开同一文件（或拷贝）、`doc` 上传、发给其他 Agent。打开进用户画布之后，除非走决策 7，Agent 不再 attach 那块 UI |
| 7 | 共编 | 仅当应用**自己的 UI** 内嵌聊天 / Agent 时，才允许 MCP attach 到用户画布。平台聊天默认不提供共编 |
| 8 | `library` vs `visual` | 都是「先有一份可打开的产物」。`library`（DOC / PM）的产物在应用自己的库里，用户打开资源即看见。`visual`（FigCraft、图片类设计稿）的产物在 Agent **工作区文件**里，预览要用对应查看器（FigCraft iframe / 图片查看器）。区分的是**预览通道**，不是「一个有文件一个没有」 |
| 9 | 主题 | Desktop `AppFrame` 已有 `postMessage`（theme / locale / accent）。应用页监听即可。ACowork 浅色/深色变化时壳必须再推一次 |
| 10 | 改动收口 | 胶水集中在 **hosted adapter**：MCP HTTP、把工具路径限制在调用方 Agent 的 workspace、把工作区文件投影给预览 iframe。禁止为接平台去改上游引擎 / MCP crate / UI 源码 |
| 11 | Catalog | `mcp.auto_inject = true` 时走 ADR-116 `catalog_apps()` → `acowork/global/mcps`。Gateway 不解析工具语义 |
| 12 | 文件隔离 | 多用户 / 多 Agent 不靠「会话表里的匿名画布」，靠 **每个 Agent 自己的 workspace**。FigCraft MCP 的 automation root = 该 Actor 的工作区根目录。工具不得读到别人的工作区 |

### 1.3 不变量（红线）

1. **MCP 默认不 attach 用户 webview。** 用户左侧打开的 iframe 不是 Agent 的后端。
2. **Agent 的稿是工作区文件。** 不得把只存在于进程内存、没有对应文件的会话当成权威。引擎缓存可以丢，文件不能丢。
3. **聊天转录里不挂活的应用 iframe。** 行是窗口化的，滚出视口即不在 DOM。
4. **预览打开的是那条工作区路径。** 不得误接到另一个 Agent 的文件，也不得接到左侧用户画布里未保存的本地状态。
5. **文件访问限制在调用方 Agent 的 workspace。** 缺路径或路径越界则报错，禁止落到「进程里当前打开的那一份」。
6. **Gateway 仍然零应用业务**（ADR-116 / ADR-009）：不读应用数据目录、不解释工具、不渲染画布。工作区字节走既有 workspace 文件 API。
7. **共编是应用内功能，不是平台聊天默认。**

### 1.4 明确不做

- 不把 Agent 工具打到用户正在左侧看的那块 iframe 上。
- 不按用户或按 Agent 再 spawn 一份应用进程来「隔离画布」；隔离靠工作区目录。
- 不把 FigCraft / 同类 WASM 当作 MCP 的唯一后端（WASM 是查看器 / 用户编辑器）。
- 不把「当前聊天会话 id」当成稿的主键。会话里出现的是**指向工作区文件的引用**。
- 不为接 ACowork 去 fork 上游应用的引擎、命令表或 Web UI 源文件；胶水留在 adapter。
- 不把 memory 右侧面板塞进本契约（仍是 ADR-116 排除项）。
- 不强迫 DOC/PM 先在工作区写一份再同步进库——它们的产物主键就是库里的资源 id。

---

## 2. 背景

ADR-116 解决了「应用如何被托管」，没有回答 **Agent 改的是哪一份状态**。

错误方向有两条：

1. **Attach 用户 iframe**：用户自己的稿被抢走，多 Desktop 串画布，iframe 一卸工具就断。
2. **会话内匿名画布**：稿只活在 `(actor, conversation)` 的内存里。和平台其余产物（图片、代码、Markdown）不一致——那些都是工作区文件；也没法交给 DOC、别的 Agent 或用户，除非再发明一次导出。

用户定案对齐已有的**生成图片**流程：

1. Agent 在自己的 workspace 里创建一个文件；
2. 在这个文件上改、保存、预览、再改；
3. 做完可以上传 DOC、发给别人、或用户用左侧应用打开。

FigCraft 的 MCP 已有 `open_file` / `save_file` 和 automation root，本来就是按路径工作。工作区路径就是稿的身份。

---

## 3. 身份与键

| 通道 | 身份 | 用途 |
|---|---|---|
| `/api/apps/{id}/ui/*` + `/api/*` | app session ticket → `X-App-User` | 用户画布；预览 iframe 的页面 |
| 工作区文件 API | 已登录用户 + Agent + `workspace_id` + rel path | 预览 / 用户打开 Agent 产物（既有） |
| `{mcp.gateway_path}` | 机器凭证 + `X-MCP-Actor`（instance_id） | Agent 工具。Gateway 校验 Actor 已安装 |

**稿的主键**：`(agent_id / instance_id, workspace_id, relative_path)`。聊天消息只引用这条路径。同一文件可以在多条聊天里被提到；两条聊天默认应使用不同文件，除非 Agent 显式打开同一路径。

Runtime 必须让应用 MCP 知道**该 Actor 当前 workspace 的根目录**（头或平台接口，见设计文档 35），以便 `open_file` / `save_file` 落在工作区而不是应用进程自己的匿名目录。FigCraft automation read/write root 设成这个根。

不把 `conversation_id` 当作文件主键。工具参数里的 `path` 才是。

---

## 4. 后果

**正面**

- 和图片、源码、附件同一套：工作区有文件 → 聊天能引用 → 编辑器栏能预览 → 可复制、可上传、可发给别人。
- 多 Agent 隔离就是 workspace 隔离，不必再做一份「匿名画布表」。
- 进程里的 Headless 只是打开该文件的编辑器；崩溃后从文件恢复。

**负面**

- Adapter 必须能解析 Actor → workspace 根，并把工具路径限制在里面。
- Desktop 要把 `.figcraft`（及后续同类扩展名）纳入工作区可预览类型，查看器走应用 iframe。
- DOC/PM 仍走库资源，不要为了「统一成文件」倒退回把 PRD 写进 workspace `.md`。

**回滚**

- 关闭 `mcp.auto_inject` 即从 catalog 消失。
- 预览器未做时，用户仍可在工作区树里看到文件、用截图 tool_result 看外观——不破坏「文件是权威」这条红线。

---

## 5. 落地顺序

完整步骤与 API 见 [设计文档 35](../../design/zh/35-hosted-app-dual-canvas.md)。首个 `visual` 应用是 FigCraft：工作区 `*.figcraft` + MCP open/save + 编辑器栏 iframe 预览。PM / DOC 保持 `library`。
