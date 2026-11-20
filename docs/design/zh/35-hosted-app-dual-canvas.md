# 设计文档 35：托管应用双画布（用户界面 / 工作区文件 / 预览）

**版本**：1.1（2026-11-19；Agent 的稿改为工作区文件，对齐生成图片流程）
**状态**：已接受，待实现（首个 `visual` 应用：FigCraft）
**权威决策**：[ADR-118](../../adr/zh/ADR-118-hosted-app-dual-canvas.md)
**前置**：[ADR-116](../../adr/zh/ADR-116-hosted-application-framework.md) · [设计文档 34](./34-hosted-application-framework.md)
**English counterpart**：[Design Doc 35](../en/35-hosted-app-dual-canvas.md)

> 中文文档为 source of truth，英文 counterpart 按约定同 commit 维护。
>
> 本文是 ADR-116 之上的**产物契约**。进程、反代、ticket、catalog 仍以文档 34 为准。

---

## 1. 定位

和 Agent **生成一张图片**相同：

1. 在该 Agent 的 workspace 里创建一个文件；
2. 基于这个文件修改、保存、预览、再修改；
3. 做完可以留在工作区、上传 DOC、发给其他 Agent / 用户。

**总归从一个文件开始。** 不是从「这条聊天绑定的内存画布」开始。

三种面允许缺省，不允许把用户正在编的 UI 当成 Agent 的那份文件：

| 面 | 谁在用 | 权威状态 | 入口 |
|---|---|---|---|
| **用户画布** | 当前登录用户 | 用户在应用里打开的稿（用户自己的文件或未保存缓冲） | 左侧导航 → `AppFrame` → `/api/apps/{id}/ui/` |
| **Agent 产物** | 某个 Agent | **该 Agent 工作区里的文件** | Runtime MCP → 应用 `open_file` / `save_file` / 编辑工具 |
| **预览** | 用户看 Agent 画到哪了 | 只读打开那条工作区路径 | 聊天文件卡片 → 编辑器栏查看器（图片用 ImageViewer，`.figcraft` 用应用 iframe） |

`library`（DOC / PM）的产物主键在应用自己的库（文档 id、任务 id），用户打开资源即看见——等价于「文件」，只是不住在 Agent workspace。`visual`（FigCraft、以及以后需要专用查看器的设计稿）的产物主键是 workspace 路径，预览必须走对应查看器。

平台聊天**默认不是共编**。共编只发生在应用自己的 UI 里再放聊天的时候。

---

## 2. 架构

```
Agent workspace (on the Node)          App process (one per app_id)
┌──────────────────────────┐           ┌──────────────────────────────┐
│  ridgeline.figcraft      │◄─ open/ ─►│ Headless / engine：打开该文件 │
│  cover.png               │   save    │ 只是文件的编辑器，不是权威     │
└────────────┬─────────────┘           │ POST /mcp 带 path             │
             │                         └──────────────────────────────┘
             │ 既有 workspace 文件 API
             ▼
Desktop 聊天卡片 → 编辑器栏预览该 path
  png  → ImageViewer
  figcraft → AppFrame /ui/?file=…（只读投影）
左侧导航 AppFrame = 用户自己的稿，不接这条 MCP
```

单应用进程可以同时打开多个工作区文件（按路径缓存）。隔离靠 **Agent workspace 目录**，不靠按用户再拉进程。

---

## 3. `app.toml` 增量

文档 34 的段全部保留。新增：

```toml
[ui.canvas]
# library = 产物在应用自己的库（DOC / PM）。用户打开资源即看见。
# visual  = 产物是 Agent 工作区里的文件，预览要用本应用 iframe（FigCraft）。
# none    = 无专用查看器（普通文件走 Monaco / 图片查看器即可）。
kind = "visual"

# visual：把工作区文件投进预览 iframe 的路径模板。
preview_path = "/?file={workspace_file}"
```

`kind` 区分的是**预览通道**，不是「有没有文件」。图片是 `none`（已有 ImageViewer）；`.figcraft` 是 `visual`（需要 FigCraft iframe）；DOC 文档是 `library`。

`[mcp].auto_inject = true` 仍是进入 catalog 的开关。`visual` 应用要让 Agent 改文件，必须打开它。

---

## 4. Agent 产物 = 工作区文件

### 4.1 主键

```
file_id = (instance_id, workspace_id, relative_path)
```

例如 `agents/<instance>/workspaces/default/designs/home.figcraft`。聊天、预览、MCP `path` 都引用它。

禁止用 `conversation_id` 当主键。一条聊天可以先后改多个文件；一个文件也可以在后来的聊天里被再次打开。

### 4.2 生命周期（对齐生成图片）

| 步骤 | 做什么 |
|---|---|
| 创建 | Agent 在 workspace 写下新文件（FigCraft：`save_file` / `file.save` 到工作区路径；或先 `export` 再 `file_write` 字节）。空文件也是文件 |
| 修改 | MCP 对该 path `open_file`（若尚未打开）→ 编辑工具 → `save_file`。磁盘是权威 |
| 预览 | 用户点卡片 / 自动打开编辑器栏，查看器读工作区文件字节 |
| 再修改 | 同一 path 继续 MCP；预览重新读文件或吃一次 snapshot |
| 交出 | 文件已在工作区：用户拷贝、左侧 FigCraft 打开、`doc` 导入、`intent` 把路径发给别人 |

进程内打开的文档 = 该 path 的缓存。重启后按 path 再 `open_file`。未 `save_file` 的修改允许丢失——与编辑器脏缓冲相同；应用 SHOULD 在每次会改变预览的工具后保存。

### 4.3 路径与根目录

调用方是 `X-MCP-Actor`。Runtime 提供该 Actor **当前会话绑定的 workspace 根**（建议请求头 `X-MCP-Workspace-Root`，值为 Node 上的绝对路径；或 adapter 调平台接口解析）。

Adapter MUST：

- 把 FigCraft（及同类）automation read/write root 设成这个根（或该根下声明的子目录）；
- 拒绝根外路径；
- 工具里的相对路径相对这个根。

缺根则写工具失败，明确报错，不得写入应用进程自己的 data_dir 充数。

### 4.4 MCP HTTP

沿用文档 34：`POST /mcp`。JSON-RPC 进该进程的 MCP server；`open_file` / `save_file` / `create_node` 等作用于**已打开的工作区文件**。同一进程可按 path 保留多个打开文档（FigCraft 本就支持 `document` 参数）。

Gateway 继续只反代 + 校验 Actor。

### 4.5 library 应用

DOC / PM 不要改成「先写 workspace 再上传」。它们的产物就是库记录，MCP 已按资源 id 写。聊天「打开这份文档」已是预览。本节的工作区文件规则针对 **visual 文件类型** 和普通附件。

---

## 5. 用户画布

- 入口、ticket、sandbox：文档 34。
- 用户从工作区打开 Agent 的 `.figcraft` 进左侧 FigCraft = 用户行为，打开后是用户画布上的一份拷贝或同一文件（若用户工作区与 Agent 工作区碰巧相同——默认不是）。
- **禁止**把用户 iframe 登记成 MCP Backend。

---

## 6. 预览

### 6.1 转录卡片

和图片附件相同：工作区相对路径、可选缩略图（FigCraft 可另存 `*.png` 或 MCP `screenshot` 落盘）、动作「打开」。卡片随转录窗口化，**内不挂 iframe**。

### 6.2 编辑器栏

点开走既有「打开工作区文件」槽：

| 扩展名 | 查看器 |
|---|---|
| png / jpg / … | `ImageViewer`（已有） |
| pdf / md / … | 已有预览器 |
| `figcraft`（及 `ui.canvas.kind = visual` 声明的扩展名） | `AppFrame` 加载该应用 `preview_path`，把文件字节交给应用（host `figcraft:open` 或 adapter 注入 `openFile`） |

要求：

1. 同一 `AppFrame` sandbox（无 `allow-same-origin`）。
2. ticket 仍在 fragment。
3. 预览读的是 workspace 文件 API 的字节，不是「进程里匿名会话」。
4. 关掉预览不影响 MCP。
5. 第一次对该文件的成功写工具后 **可以**自动打开预览；不得对每个闲聊预加载 WASM。

`.figcraft` 加入工作区可预览扩展名（`isWorkspacePreviewable`）。

### 6.3 跟上修改

保存到工作区后，预览重新拉字节或接收 snapshot。不要把 iframe 当成唯一打开中的引擎副本。

### 6.4 为什么不在气泡里

同 ADR-118：`VirtualRowList` 会卸行。图片已经是「卡片 + 点开/灯箱」，`.figcraft` 同样。

---

## 7. 主题与语言

`AppFrame` 在 load / `ready` 以及 **light/dark class 变化**时 `postMessage`：

```json
{ "channel": "acowork", "type": "theme", "theme": "light|dark", "accent": "#…", "locale": "zh-CN" }
```

Adapter 注入脚本监听，映射到应用主题命令（FigCraft：`view.themeLight` / `view.themeDark`）。iframe 内不要用 `system`（会跟 OS 而不是 ACowork 壳）。

不要改上游 `index.html`；注入发生在 adapter serve `/ui/` 时。

---

## 8. 交接与共编

文件已在工作区，交接就是用这个路径：

- 用户在工作区树打开 / 复制到自己的目录，再用左侧 FigCraft 打开；
- Agent 调 DOC MCP 上传导出件；
- 把路径发给其他 Agent。

**共编**：应用 UI 内嵌聊天之后才 attach 用户画布。平台聊天路径不自动共编。

---

## 9. Hosted adapter

| 职责 | 放 adapter | 不要放 |
|---|---|---|
| ADR-116 CLI、`/health`、`/ui/`、`POST /mcp` | adapter | 上游 desktop `main` |
| Actor → workspace 根 → automation roots | adapter | 上游 |
| 按 path 打开 / 保存工作区文件 | adapter Backend + 上游 `open_file`/`save_file` | 匿名 Headless 当权威 |
| 预览：把 workspace 字节喂给 iframe | adapter | 上游 `index.html` |
| 主题注入 | adapter 响应 | 上游源文件 |
| `packaging/acowork/app.toml` | adapter 打包目录 | 上游发行配置 |

可以依赖上游 `figcraft-mcp::Server`，Backend 对磁盘上的那份 workspace 文件说话。

---

## 10. 平台壳义务

| # | 层 | 义务 |
|---|---|---|
| P1 | Runtime | MCP 请求带 `X-MCP-Actor`；并带上当前 workspace 根（`X-MCP-Workspace-Root` 或等价） |
| P2 | Gateway | 转发该头；不要当伪造身份剥掉。Workspace **字节**仍走既有 workspace 文件 API，不走 Gateway 读盘 |
| P3 | Desktop `AppFrame` | 主题变化再推一次 |
| P4 | Desktop 工作区 / 聊天 | `.figcraft` 可预览；聊天卡片打开编辑器栏 |
| P5 | Desktop 布局 | 预览 `AppFrame` 与左侧用户 `hostedAppId` 互不覆盖 |

---

## 11. 新应用清单

**文档 34**

1. `app.toml` 进程 / MCP / UI / 权限
2. `/health` + `/ui/` + `POST /mcp`，只绑 `127.0.0.1`
3. Desktop 不 import 应用前端

**本文**

4. 选 `ui.canvas.kind`（预览通道，不是「有没有文件」）
5. `visual`：产物落 Agent workspace；MCP path 限制在该根；声明扩展名与 `preview_path`
6. `library`：产物写应用库；不要为统一而改成先写 workspace
7. MCP 不 attach 用户左侧 iframe
8. 预览不进聊天气泡
9. 胶水只进 adapter
10. 越界路径报错，不落到「当前打开的那一份」

**FigCraft**

- 用户画布：左侧 `/ui/` WASM。
- Agent：workspace `*.figcraft` + Headless/引擎打开该文件 + `auto_inject = true`。
- 预览：工作区文件卡片 + 编辑器栏 iframe。
- 主题：注入监听。
- 不改 `crates/mcp`、`crates/engine`、`apps/figcraft-web/index.html`。

---

## 12. 安全

- 预览读工作区文件：沿用既有 workspace 权限（use 档可读内容）。iframe ticket 不能用来列别人的工作区。
- MCP 路径限制在 `X-MCP-Actor` 的 workspace 根。
- 预览 sandbox 与用户画布相同。

---

## 13. 验证（落地时）

1. Agent 创建 `designs/a.figcraft` 后，工作区树能看见该文件；左侧用户 FigCraft **没有**自动出现这份稿。
2. 关掉预览后再 MCP 编辑、`save_file`，磁盘文件更新；再打开预览是新内容。
3. 路径逃出 workspace 被拒绝。
4. 用户 B 的 Agent 写自己工作区，用户 A 的树里没有该文件。
5. 聊天滚动不卸载编辑器栏里的预览 iframe。
6. ACowork 切换 light/dark，预览 iframe（若开着）跟壳走。
7. Adapter 对上游 `index.html` 的源码 diff 为空。

---

## 14. 边界

- `X-MCP-Workspace-Root` 的最终头名可在实现时与 Runtime 对齐，语义不变。
- 应用内共编：不在本版。
- memory 面板：仍排除。
- Mobile：Desktop 先行；「产物是工作区文件」这条在移动端同样成立，预览栏形态另议。
