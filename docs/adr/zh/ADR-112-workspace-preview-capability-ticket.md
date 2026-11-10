# ADR-112：工作区静态资源的预览能力票据（capability URL）

- **编号**：ADR-112
- **状态**：已实施
- **日期**：2026-11-10
- **决策者**：Nicholas（需求与方案确认）
- **关联**：ADR-009（Gateway 边界 — 文件字节仍由 Runtime 拥有，Gateway 只做反向代理）、ADR-055 L2-7（`/workspace-files/*` 静态代理与远端 Node 拓扑）、ADR-087（workspace 文件内容在 `use` 层、不可在 `view` 层外泄）、ADR-076 §决策 3（`multi_user` 下 Bearer 网关全站鉴权）、ADR-108（右侧面板整合 — 预览落在右侧工作区）
- **英文 counterpart**：[docs/adr/en/ADR-112-workspace-preview-capability-ticket.md](../en/ADR-112-workspace-preview-capability-ticket.md)（同 commit 维护）

---

## 摘要

**一句话**：工作区里的 HTML 文件不再以 Blob URL + `<base href>` 渲染，而是由 Gateway 签发一张**绑定到 `(agent, workspace)` 的短期能力票据**，让页面以真正的 `http://` 文档加载，并复用聊天 URL 点击所用的同一个带地址栏内部浏览器（`UrlPreviewView`）渲染。

**一句话（为什么）**：页面的 CSS / JS / 字体 / 图片是**浏览器发起的子资源请求**，而浏览器请求无法携带 `Authorization` 头。原来那条路径把 `styles.css` 之类指到了需要 Bearer 的 `/workspace-files/*` 上，于是 HTML 骨架能画出来（HTML 文本在 Blob 里），样式与脚本**全部 401** —— 用户看到的就是"按本地文件渲染、布局一塌糊涂"。

---

## 背景（事实取证）

1. **症状与真实原因不一致。** 用户描述为"看起来像是按照本地文件渲染的"，但预览走的并不是 `file://`：文档是 `blob:` URL（`HtmlPreviewView`），子资源是 `http://…/workspace-files/…`。差别只在鉴权，不在协议或 origin。

2. **网关日志直接证明是 401。** `~/.acowork/acowork-gateway/data/logs/20261110_112918.log` 第 21946–21969 行（11:39:14，打开 `ACoworkWebSite/index.html`）：`styles.css?v=9`、`main.js`、`assets/logo.svg`、`assets/logo-gray.svg`、`assets/fonts/Inter-Variable.woff2`、`JetBrainsMono-Variable.woff2` 逐个 `status=401 Unauthorized`，且 `origin=<none>`（blob 文档发起的子资源请求不带 Origin）。

3. **现场复现同一结论。** 对运行中的网关直接请求同一路由、不带 token：
   `GET /workspace-files/…/AGENTS.md` → `401 {"error":"unauthorized","message":"missing bearer token"}`。

4. **机制在代码里是显式的。** `auth_middleware::is_public_path` 未放行 `/workspace-files`，`multi_user` 下整站需 Bearer；`<link>` / `<script>` / `@font-face` 都无法携带该头。这与 `workspaceFileBlob.ts` 文件头早已写明的"`<img>` 不能带 Authorization"是同一类缺陷，只是 HTML 预览这条路径漏了。

5. **已有正确的对照路径。** 图片 / PDF / Office / Markdown 预览都用 `useWorkspaceFileBlob` → 带 Bearer 的 `fetch` → Blob URL，所以它们从来不出问题：它们**不产生子资源请求**。问题只在"一个页面需要被浏览器自己去取 30 个文件"的时候出现。

6. **`?ticket=` 方案不成立（关键约束）。** 相对路径解析会**保留 path、丢弃 query**：文档 URL 上的 `?ticket=x` 在 `styles.css` 解析后消失。因此票据必须落在**路径**上，这是唯一能覆盖子资源的形态。

7. **不能简单把 `/workspace-files` 加入公开路径。** 那会让本机任意进程无凭证读取任意 agent 的全部 workspace 文件，也直接破坏 ADR-087 §方案 G（workspace 文件内容不得在 `view` 层外泄；`workspace/file` 读被定为 `use`）。

---

## 决策

### D1 — HTML 预览改为真正的 HTTP 文档

`text/html` 的 preview 分支不再自行取文本、建 Blob、注入 `<base>`；改为向 Gateway 换取一张票据，把 `http://<gateway>/workspace-preview/<ticket>/<agent>/<ws>/<path>` 交给 `UrlPreviewView`。

一条被删除的复杂度：`HtmlPreviewView.tsx`、`htmlPreviewTransform.ts`（含根相对路径/`srcset` 重写与自测脚本）、`check:html-preview` 构建步骤。浏览器本来就负责相对路径解析，那套重写是在用客户端代码模拟一个它做不到的事。

### D2 — 票据放在**路径**上，不是 query

`GET /workspace-preview/{ticket}/{agent_id}/{workspace_id}/{*path}`。相对子资源（`styles.css`、`assets/x.svg`、CSS 内的 `url(...)`）自动继承整段前缀，文档与全部依赖共用一次鉴权，无需任何重写。

### D3 — 单一实现，两种鉴权入口

`workspaces::proxy_workspace_file` 是唯一的字节路径（路径逐段编码、`workspace_id` 透传、Runtime 侧 `resolve_within` 越权防护）。`/workspace-files/*`（Bearer）与 `/workspace-preview/*`（票据）都是它的入口，鉴权差异在入口层解决，字节路径不写第二份。

### D4 — 票据的语义与护栏

格式 `v1.<expires_at>.<mac_hex>`，`mac = HMAC-SHA256(secret, context|exp|len:agent|len:workspace)` 截断 128 位；agent / workspace **不存进票据**，校验时由请求路径重算，因此一张票据无法被改用到另一个工作区；字段做长度前缀，避免 id 内含有分隔符时发生拼接歧义。

护栏（不得在不修 ADR 的情况下放宽）：
- **只读**：仅 `GET`，只返回文件字节，永远不授权写。
- **绑定**：一张票据只对一对 `(agent_id, workspace_id)` 有效。
- **有寿命**：8 小时（`TICKET_TTL_SECS`）。
- **不提权**：签发路由 `POST /api/agents/{id}/workspaces/preview-ticket` 在 `permission::classify` 中定为 `use`，与 `GET /workspaces/file` **同级**——票据只是把已存在的读权换成浏览器可携带的形式，不是新权限。若定为 `manage`，guest 能读 JSON 却无法渲染页面，会出现"只有 HTML 预览对 guest 失效"的诡异分裂。
- **不落盘**：密钥为进程内随机值（`AppState::new` 生成），重启即令所有在外票据失效。
- **不入日志**：票据在路径里，访问日志中间件会把它替换为 `***`（`workspace_preview::redact_preview_ticket`，与 ADR-089 D2 对 `?access_token=` 的规则一致）。否则一次页面加载就会把一张 8 小时有效的凭证写进日志文件六次以上，而日志正是用户报障时要交出的东西。

### D5 — 每次导航重新签发，不缓存票据

`lib/workspacePreviewUrl.ts` 在**每次导航前**签发一张新票据，同一 `(agent, workspace)` 的并发调用共享同一次签发。

不做跨导航缓存，依据的是一条事实而不是偏好：签名密钥是进程内的，网关重启即令所有在外票据失效，而**客户端无法观测这次重启**——HTTP 层没有进程身份，`/health` 也不带（只有 `status` / `version` / `checks`）。任何客户端缓存都会在重启之后继续把一张校验不过的票据交给 iframe，用户看到的只是一页嵌在预览里的 403 JSON，比原来的 401 瀑布更难诊断。签发是网关本地的纯 HMAC 运算（不经过 Node／Runtime），代价是一次亚毫秒请求，因此宁可重签。

（曾考虑给 `/health` 加进程 boot id 让客户端比对——那要改一条被全平台依赖的公开响应；也考虑用 HEAD 预检票据——那要多一次往返和 CORS 依赖。两条都不如"不缓存"简单。）

签发失败**不回退**到旧 blob 路径（那会把 401 缺陷悄悄复活且无法与正常预览区分），而是渲染带"重试"的错误卡；重试必然拿到新票据，因为没有缓存可失效。

---

## 后果

**正面**
- 工作区 HTML 与外部网址共用同一个渲染面（带地址栏、前进/后退/刷新、在浏览器中打开），渲染结果与真实站点一致：CSS、JS、字体、图片、`<iframe>`、`<video>` 全部可用。
- 少两个模块、少一个构建步骤，`<html>` 的相对路径语义不再由我们模拟。
- 能力票据与预签名 URL 同一范式，可复用到将来其它"浏览器必须直连"的静态资源场景。

**代价 / 已知边界**
- 引入了一个新的公开前缀。它换取的是"仅能 GET 读某个 workspace 的文件子树，8 小时后失效，URL 不可猜"，与调用者手动读文件的权限上限一致；但拿到 URL 的人（本机任意进程 / 同网段，取决于网关绑定）在有效期内可读该子树。这是本 ADR 唯一扩大出去的面，须以此为前提做后续设计。
- **根相对路径仍然不支持**：`<img src="/assets/x.png">` 会打到网关根目录而 404。这需要预览独占一个 origin／端口才能解决（例如专用静态服务器），代价远大于收益；且此形态在旧方案下同样失败（401），没有回归。静态站点应当使用相对路径——这也正是任何静态托管的既有约定。
- 票据出现在地址栏里。这是有意的：它就是可复制的真实地址，且 8 小时后自动失效。
- **预览渲染的是磁盘上的文件，不是编辑器里未保存的缓冲区。** 旧 blob 方案渲染 `activeFile.content`，含未保存的修改；新方案由浏览器直接 GET 文件字节——这是"真实 HTTP 文档"的必然代价，要让未保存内容也走 HTTP 就得额外写盘或加一个内存服务端点，不值得。当前行为：文件 dirty 时预览仍是上次保存的内容（待办：dirty 时在预览区给出提示，或在切到预览前自动保存）。

---

## 实施

- **Gateway**：`http/workspace_preview.rs`（票据签发/校验、日志脱敏、两个路由、错误映射、21 个测试）；`http/workspaces.rs` 抽出 `proxy_workspace_file`；`http/routes.rs` 注册 + `AppState.workspace_preview_secret`；`http/auth_middleware.rs::is_public_path` 放行该前缀；`http/permission.rs::classify` 定为 `use`。
- **Desktop**：新增 `lib/workspacePreviewUrl.ts`、`components/editor/WorkspaceHtmlPreview.tsx`；`FileEditorPanel` 的 `text/html` 分支改接；删除 `HtmlPreviewView.tsx` / `htmlPreviewTransform.ts` / `scripts/check-html-preview.mts` 及 `check:html-preview` 构建步骤；清理 `htmlPreviewTsModuleWarning`，新增 `htmlPreviewFailed` / `htmlPreviewRetry`（5 个语言文件同步）。
- **测试**：网关票据单测覆盖过期、篡改 MAC、跨 workspace 重放、字段拼接歧义、换密钥失效、HMAC 的 RFC 4231 已知答案，以及"校验先于代理"（伪造票据 403、真票据落到代理层 503）；桌面侧覆盖每次导航重签/并发合并/失败不留缓存，以及组件对"HTTP 文档而非 blob"、错误卡、重试、切换文件时丢弃过期响应的约束。
