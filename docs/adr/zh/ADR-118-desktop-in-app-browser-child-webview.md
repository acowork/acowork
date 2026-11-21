# ADR-118：桌面端内置浏览器 — 子 Webview 承载第三方页面

**状态**：已定案并落地（2026-11-18）
**日期**：2026-11-18
**决策者**：大鱼（决策授权：由 Senior Engineer 按「最主流、一劳永逸、不要止血」的原则自行定案）
**前置**：
- [ADR-009：Gateway 工作区隔离](./ADR-009-gateway-workspace-isolation.md)（「谁读谁的资产」的既有边界；本 ADR 的权限收窄与之同源）
- [ADR-112：工作区预览能力票据](./ADR-112-workspace-preview-capability-ticket.md)（工作区 `.html` 以真实 HTTP 文档渲染，是本 ADR 迁移到新宿主的对象）
- [ADR-116：托管应用框架](./ADR-116-hosted-application-framework.md)（同为「第三方文档跑在 Desktop 内」的既有先例）

---

## 1. 决策摘要

### 1.1 一句话

**把「内置网页预览」从 `<iframe>` 换成同窗口的**子 Webview**（Tauri 2.12 `Webview::add_child`）。** `iframe` 里显示不了 github.com 不是权限或配置问题，而是引擎对**被嵌入文档**强制执行 `X-Frame-Options` / `CSP frame-ancestors`，前端没有任何绕过手段；子 Webview 把目标页作为**顶层文档**加载，这类响应头对它不适用——这也是 Cursor / VS Code Simple Browser 的做法。

### 1.2 关键决策表

| # | 决策 | 结论 |
|---|---|---|
| D1 | 渲染载体 | **同窗口子 Webview**（`Window::add_child`）。不是 `iframe`，不是独立 `BrowserWindow`：前者被 XFO 硬拦，后者会让内置浏览器与主窗口的登录态、焦点、托盘生命周期分叉 |
| D2 | 导航策略 | 策略**按 webview 角色参数化**（`NavigationRole::{App, Preview}`），`navigation_policy.rs` 是唯一实现。`App` 行为零变化（原 allow-list 逐条搬移），`Preview` 是补集：允许 `http/https` 顶层导航，其余协议一律拒绝 |
| D3 | 会话生命周期 | Rust 侧 `PreviewRegistry` 持有 `label → {url,title,phase,visible,bounds}`，**上限 3 个存活会话**、LRU 驱逐（`MAX_LIVE_SESSIONS`）；前端按**标签页身份**（`OpenFile.id`）绑定 label，切 tab **只停放不销毁**，关 tab 才销毁 |
| D4 | 权限边界 | `capabilities/default.json` 由 `windows: ["main"]` 改为 `webviews: ["main"]`。`windows` 键会让**该窗口的所有 webview**（含承载第三方站点的预览）继承 IPC 权限；`dev/ci.sh` 新增 `run_desktop_ipc_scope_redline` 红线钉死这条 |
| D5 | 弹窗与下载 | 页面内 `window.open` / `target="_blank"`：Rust 返回 `NewWindowResponse::Deny` 并发 `preview://new-window`，前端**开新预览标签**（无工作区上下文时退回系统浏览器）；`on_download` 一律拒绝（没有落盘目的地 UI，宁可拒绝也不把文件丢到用户找不到的地方），但**拒绝不再静默**：发 `preview://download`，前端用 toast 提供「在浏览器中打开」（下载 URL 浏览器取不到时——`blob:`/`data:` 这类页面内生成的文件——动作指向页面本身） |
| D6 | `unstable` feature | **接受**。`add_child` 目前只在 `tauri` 的 `unstable` feature 下可用；用 `=2.12.3` 精确锁版本 + 红线 + 集成测试把这个依赖面收窄，见 §5 |
| D7 | 明确不做 | 不做 iframe 兜底（两套渲染路径＝两套坐标系/两套权限，正是要消灭的东西）；不做独立窗口内的浏览器（`BrowserWindow` 方案见 §6 备选）；不做 `file://` / `data:` / `javascript:` 预览（Preview 角色策略拒绝）；不做下载管理 |

### 1.3 事实底座（全部实测，非推断）

1. `GET https://github.com/acowork/ACowork` 返回 `200`，但带 `X-Frame-Options: deny` 与 `Content-Security-Policy: frame-ancestors 'none'` ⇒ 任何 `iframe` 渲染结果都是空白。这条约束由**浏览器内核**在「把文档装进 frame」时执行，页面 JS、`sandbox` 属性、CSP 放宽都改变不了它。
2. Cursor / VS Code Simple Browser 不是 `iframe`，而是**顶层 webview**（Electron `WebContentsView`）加载同一 URL。XFO 只约束「被嵌入的 frame」，对顶层文档不生效——所以它们能打开。
3. 改造前的实现还有两个真实缺陷（同一次改造一并消灭）：
   - `UrlPreviewView` 的 5 秒「加载慢」提示**永不触发**：XFO 拒绝时 iframe 的 `onLoad` 事件照样派发，`loading` 被置 `false`，提示被清掉——用户看到的是「空白且没有任何解释」。
   - 「在浏览器中打开」是 `target="_blank"` 的 `<a>`，Tauri v2 默认拒绝新建窗口且仓库没有 `new_window_req_handler` ⇒ 点击大概率无反应。
4. Tauri 2.12 的 `add_child` 在 Windows 上**同步调用会死锁**（WebView2 的 controller 创建回调需要 UI 线程的消息泵，而同步等待把泵堵住）。因此所有创建路径必须是 `async` 命令，由 tokio 工作线程发起。
5. `capability.windows` 的语义是「匹配窗口内**所有** webview 都获得该 capability」（`tauri-utils` acl/capability 文档），`webviews` 才是按 label 精确限定。
6. 子 Webview 是**原生层**，永远合成在 HTML 之上：`z-index` 无效、Dom portal 盖不住它。这条事实决定了 D3/D4 之外的三个 UI 约束（§4.2）。

---

## 2. 架构

```
React                                     Rust (src-tauri)
─────────────────────────────────────     ────────────────────────────────────────
UrlPreviewView                            browser/host.rs   ── Tauri commands
  ├─ 工具栏（back/forward/refresh/地址栏）    preview_open / set_bounds / set_visible
  └─ WebPreviewHost                        preview_navigate / reload / history
       ├─ useNativeOverlayCoverage         preview_close / close_all / list
       ├─ previewSessionStore（label↔tab）  browser/registry.rs ── PreviewRegistry
       └─ lib/previewHost.ts（类型化 IPC）     label/url/title/phase/visible/bounds
                                            LRU ≤ MAX_LIVE_SESSIONS
navigation_guard（插件级 on_navigation）  ← navigation_policy::is_allowed(role, url)
                                                   ▲
                                     Window::add_child(WebviewBuilder("preview-N", External(url)))
                                                   │  原生子 Webview（顶层文档，XFO 不适用）
                                                   ▼
                                           github.com / 工作区 .html 票据 URL
```

### 2.1 前端分层（为什么是三份而不是一份）

| 模块 | 唯一职责 | 不做什么 |
|---|---|---|
| [lib/previewHost.ts](apps/acowork-desktop/src/lib/previewHost.ts) | 命令与事件契约、错误码解析、URL 规范化 | 不碰 DOM、不持状态 |
| [stores/previewSessionStore.ts](apps/acowork-desktop/src/stores/previewSessionStore.ts) | `sessionKey → label → PreviewState` 投影 | 不发起 IPC |
| [components/editor/WebPreviewHost.tsx](apps/acowork-desktop/src/components/editor/WebPreviewHost.tsx) | 测量 → 建会话/复用 → 推几何与可见性 | 不渲染任何可见 UI（原生表面盖住一切） |
| [components/editor/UrlPreviewView.tsx](apps/acowork-desktop/src/components/editor/UrlPreviewView.tsx) | 工具栏与错误卡（必须落在 webview 矩形**之外**） | 不拥有会话生命周期 |

### 2.2 坐标系

`PreviewBounds` 是**窗口客户区逻辑像素**——与 `getBoundingClientRect()` 同一空间，所以不需要滚动/DPI 换算；前端只做**取整**（分数坐标会让 WebView2 渲染发虚，且会在拖拽 resize 时按帧刷 IPC）。`getBoundingClientRect()` 在 `previewGeometry.ts` 里被压成四个数字的纯函数，便于单测。

---

## 3. 决策细节

### D1 同窗口子 Webview

三条硬要求同时成立才选它：(a) 能显示被 XFO 拒绝的站点；(b) 与主窗口共享同一份登录态与生命周期（托盘隐藏/显示、单实例、DPI 缩放一起走）；(c) 面板几何由 React 布局决定，webview 必须跟着 `ResizeObserver` 走。

`add_child` 三者全中：它是**本窗口的一个 webview**，坐标由我们给，cookie/存储与主窗口共享同一个 profile。

### D2 角色化导航策略

`navigation_guard` 是**插件级** `on_navigation`，Tauri 会把它串在**每个** webview 上（`tauri/src/manager/webview.rs`）。原实现只有一份「app 白名单」，如果不参数化，预览 webview 的外部 URL 会被 app 规则直接拦掉——表现是「webview 已创建但永远白屏」，且不报错。

因此 `navigation_policy.rs` 抽出 `is_allowed(role, url)`：

| 角色 | 规则 | 判据 |
|---|---|---|
| `App` | 原白名单（dev server / gateway / 内置页面），其余拒绝 | 与改造前**逐条等价**（`navigation_guard.rs` 只剩调用） |
| `Preview` | `http` / `https` 允许，其余协议拒绝；**本应用自身的面**拒绝（`tauri.localhost` / `asset.localhost`，以及 debug 构建下 `devUrl` 那一个 loopback 端口） | 补集语义：预览是「通向外部世界」的入口，不需要知道内网地址。拒绝自家是同一句话的另一半——预览不是「第二个主窗口」。loopback **按端口**拒绝而不是整段拒绝：agent 在本机起的 dev server（其它端口）恰恰是这个能力最有用的场景，desktop 只是投影，见 §Consequences |

角色由 `webview.label()` 决定（`PREVIEW_LABEL_PREFIX = "preview-"`），不依赖前端自报。`open_preview` 命令在**创建之前**用同一策略校验 URL，于是非法协议得到 `blocked_scheme:` 的明确错误，而不是一个空白 webview。

### D3 会话生命周期与会话键

| 事件 | 行为 | 理由 |
|---|---|---|
| 打开标签 | `preview_open` → 分配 `preview-N` → 前端绑定 `OpenFile.id → label` | 一次创建，之后复用 |
| 切走标签 | `preview_set_visible(false)`（**停放**） | 页面不重载，滚动位置/表单/视频不丢 |
| 切回标签 | `preview_set_bounds` + `set_visible(true)` | 只换几何与可见性 |
| 关闭标签 | `preview_close`（由 `registerFileDisposer` 触发） | 会话不能比它的标签页活得更久 |
| 超过 3 个存活 | `registry.overflow()` → 销毁最久未活跃者 | 每个预览都是独立渲染进程（WebView2），无上限就是内存无上限 |
| 复用到的 label 已被驱逐 | 后端返回 `unknown_preview:` → 前端**丢弃绑定并重建一次**（最多 2 次，之后报错） | 用户看到的是「页面重新加载」，而不是永久空白 |

`sourceUrls[sessionKey]` 记录「最后一次来自 props 的 URL」：重挂载时只有 props **真的变了**才导航，否则会把用户在预览里点进去的页面拽回原地址。

### D4 权限收窄（安全红线）

`windows: ["main"]` → `webviews: ["main"]`。前者会让 `add_child` 出来的预览 webview 继承 `shell:allow-open`、`fs`、`dialog` 等权限——即**任何被渲染的第三方站点都能调用应用 IPC 面**。`run_desktop_ipc_scope_redline` 三条 grep 守住：

1. `capabilities/*.json` 中不得出现 `"windows": ["…"]`；
2. `default.json` 必须含 `"webviews"`；
3. 任何 capability 不得出现 `preview` label（预览必须零权限）。

### D5 弹窗、下载与「真的出不去」

- `window.open`：`Deny` + `preview://new-window` 事件 → 前端 `openUrl(agentId, url)` 开新预览标签（有工作区上下文时），否则 `openExternal(url)`。理由：不产生无法回收的原生窗口，且与浏览器「新标签」预期一致。
- 下载：`on_download` 返回拒绝。没有下载面板就没有可信的落盘位置，静默把文件丢到 `%TEMP%` 是更坏的行为。
- 但「拒绝」不等于「静默」：拒绝的同时发 `preview://download`（`{url, from}`），`usePreviewBridge` 弹一条 warning toast，动作是「在浏览器中打开」。差别是可见的——此前点下载链接毫无反应，用户读作「页面坏了」；现在他知道发生了什么，并且有一条能完成的出路。
- 动作给的是**可打开的 URL**，不一定是下载 URL：`blob:` / `data:` 是页面自己在内存里生成的，离开那个页面就没有意义，`shell:allow-open` 的默认 scope（`mailto:` / `tel:` / `http(s)`）也会拒绝它——照搬下载 URL 会造出一个点了没反应的按钮，正是这条决策要消除的东西。因此 `downloadActionUrl` 按序择一：下载 URL 是 `http(s)` 就用它，否则用页面自身 URL（`from` 就是映射过去的那把键），两者都不可打开就**不给动作**（只留警告文案，不留死按钮）。
- 「在浏览器中打开」：改走 `openExternal`（`plugin-shell` 的 `open`，`shell:allow-open` 只授给主 webview），不再用 `target="_blank"` 的 `<a>`——那条路径在 Tauri v2 里默认被拒且无 handler。

### D6 `unstable` 的接受与收口

`add_child` 目前只在 `tauri` 的 `unstable` feature 下导出。这不是「用不用 beta API」的偏好问题：做不到 D1 就没有内置浏览器，只剩「iframe（打不开）+ 系统浏览器（出应用）」两种残缺形态。

收口手段：
- `tauri = { version = "=2.12.3", features = [..., "unstable"] }` —— 精确锁版本，避免 patch 升级悄无声息地改 `add_child` 语义；
- `browser/` 目录是**唯一**接触 `add_child` / `WebviewBuilder` 的地方，命令签名与 DTO 都在这里，升级时改动面已知；
- 集成测试覆盖注册表与策略（与 GUI 无关，可回归）；升级 checklist 见 §5.2。

---

## 4. 后果

### 4.1 正面

1. **github.com / google.com / 银行站点等被 XFO、CSP 拒绝嵌入的页面全部可预览**，与 Cursor 行为一致——这是本 ADR 的原始诉求。
2. 三个历史缺陷一并消失：假「加载慢」提示（不再有 iframe `onLoad` 判据）、点不动的「在浏览器中打开」、以及在面板内无法解释的空白页。
3. 导航策略变成**可参数化的纯函数**，有 8 个用例的独立测试；权限泄漏面被一条红线永久钉住。
4. 工作区 `.html` 预览（ADR-112）与外部 URL 预览**共用同一宿主**：一套坐标、一套生命周期、一套权限——不再存在「两个预览器行为不同」。

### 4.2 负面 / 必须接受的约束

1. **原生层永远在 HTML 之上**。由此产生三条不可协商的 UI 规则：
   - 工具栏、加载指示、错误卡**必须落在 webview 矩形之外**，不能覆盖在预览区上；
   - 任何 HTML 浮层（模态、下拉、右键菜单、灯箱）打开时**必须隐藏** webview，否则弹窗不可见且点击穿透到页面 —— 由 `useNativeOverlayCoverage` 用 `document.elementFromPoint` 命中检测驱动（15 处 portal 逐个埋点的方案会随新增浮层静默回归）；
   - 命中检测的已知盲区：`pointer-events: none` 的浮层对 `elementFromPoint` 不可见。本应用没有任何**需要拦截输入**的浮层使用该组合；新增时须显式调用 `preview_set_visible(false)`。
2. **内存**：每个存活预览是一个独立渲染进程。上限 3 是显式取舍（不是可调参数）。
3. **每个存活会话都是「真浏览器」**：它会跑第三方 JS，共享主窗口 profile。安全边界靠 D4（零 IPC 权限）而不是靠沙箱隔离——预览 webview 与主应用在**进程内**仍是同一 WebView2 profile。
4. **`unstable` 依赖**：见 D6 的收口手段。
5. 平台覆盖：Windows 先落地并验证；macOS/Linux 走同一条 `add_child` 路径但**未在本轮实测**（§5.2 列为升级/扩展 checklist）。

### 4.3 未被本轮覆盖（诚实清单）

- `dev/ci.sh` 的 ADR-076 identity-injection 红线在 HEAD 上本就失败（`core/acowork-gateway/src/app/mcp_proxy.rs:146` 命中），与本改造无关，需单独处置。
- 桌面端 Rust 集成测试（`capability_scope` / `preview_registry` / `navigation_policy`）**不在** `dev/ci.sh` 的 `core` 模式里执行：它们属于 `apps/acowork-desktop/src-tauri`，与 `core/` workspace 分离。当前靠开发者本地 `cargo test`；是否给 Desktop 加 CI job 是独立决定。
- GUI 冒烟（真实 `add_child` 创建、DPI 拖拽、浮层遮挡）需人手动执行：本机 lib 单测二进制在 Windows 上无法启动（`0xc0000139 STATUS_ENTRYPOINT_NOT_FOUND`：测试目标没有打包 manifest，`comctl32` 绑定到 System32 的 v5.82，缺 `rfd` 静态导入的 `TaskDialogIndirect`），这是改造前就存在的环境问题；`[[test]]` 集成测试不受影响（本轮 18 例全部通过）。

---

## 5. 验证

### 5.1 本轮实测

| 项 | 结果 |
|---|---|
| `cargo check --all-targets`（src-tauri） | 通过（含 `unstable` feature 全量重编译） |
| `cargo test --test navigation_policy` | 8 passed |
| `cargo test --test preview_registry` | 8 passed |
| `cargo test --test capability_scope` | 2 passed |
| `npx tsc --noEmit`（前端） | 无新增错误（既有 5 个第三方依赖缺失错误与本次无关） |
| `npm run check:i18n` | 1999 处调用点全部可解析、5 个语言包占位符合规 |
| vitest（新增 5 个文件） | 31 passed：`previewGeometry` 7 / `previewSessionStore` 7 / `WebPreviewHost` 7 / `UrlPreviewView` 7 / `useNativeOverlayCoverage` 3 |
| `dev/ci.sh redlines` — `run_desktop_ipc_scope_redline` | OK |

### 5.2 升级 / 扩展到其它平台时的 checklist

1. 确认 `tauri` 版本仍导出 `WebviewWindow::add_child` 与 `NewWindowResponse`（`unstable`）；
2. 确认 Windows 上 `add_child` 仍必须异步（否则死锁）；
3. macOS / Linux 上重复 §5.1 的 Rust 测试 + 手动 GUI 冒烟（几何、遮挡、登录态）；
4. 若 Tauri 提供稳定的多 webview API，删除 `unstable` feature 并重跑本节全部项。

---

## 6. 备选方案（为什么没选）

| 方案 | 结论 | 否决理由 |
|---|---|---|
| 保留 `iframe`，靠放宽 `sandbox` / `X-Frame-Options` 绕过 | 否决 | 物理不可能：响应头由目标站点决定，引擎在被嵌入侧执行。这是「止血」的典型形态（假提示、按钮、外部浏览器兜底都治不了空白页） |
| 独立 `BrowserWindow` / 系统浏览器内嵌 | 否决 | 内置浏览器的价值在于**在面板里**预览工作区页面与文档链接；弹窗式窗口破坏面板级布局语义，且托盘/单实例/登录态都要额外同步 |
| Electron 迁移（Cursor 同款） | 否决 | 为一个功能重写整个桌面外壳，成本与风险都远超收益；Tauri 已提供等价原语 |
| 用本地反代剥掉 `X-Frame-Options`（Gateway/reverse proxy 改写响应头） | 否决 | 把「渲染一个页面」变成「代理并改写第三方流量」：登录、cookie、CSRF、TLS 全都要穿过我们，且只能对自家代理的站点生效——覆盖面远小于子 Webview |
| 只做「在系统浏览器中打开」 | 否决 | 等于取消内置预览；且用户明确要求与 Cursor 对齐 |
| 每个 portal 浮层逐个注册「我打开了」 | 否决（采纳为辅助判断依据） | 15 处 portal 且会继续增长；漏一处就是「弹窗被页面盖住」的不可恢复故障。命中检测是当前唯一不会随新浮层回归的判据 |

---

## 7. 关联

- 代码：[browser/mod.rs](apps/acowork-desktop/src-tauri/src/browser/mod.rs)、[browser/host.rs](apps/acowork-desktop/src-tauri/src/browser/host.rs)、[browser/registry.rs](apps/acowork-desktop/src-tauri/src/browser/registry.rs)、[navigation_policy.rs](apps/acowork-desktop/src-tauri/src/navigation_policy.rs)、[navigation_guard.rs](apps/acowork-desktop/src-tauri/src/navigation_guard.rs)
- 前端：[WebPreviewHost.tsx](apps/acowork-desktop/src/components/editor/WebPreviewHost.tsx)、[UrlPreviewView.tsx](apps/acowork-desktop/src/components/editor/UrlPreviewView.tsx)、[previewHost.ts](apps/acowork-desktop/src/lib/previewHost.ts)、[previewSessionStore.ts](apps/acowork-desktop/src/stores/previewSessionStore.ts)、[useNativeOverlayCoverage.ts](apps/acowork-desktop/src/hooks/useNativeOverlayCoverage.ts)、[usePreviewBridge.ts](apps/acowork-desktop/src/hooks/usePreviewBridge.ts)
- 权限与红线：[capabilities/default.json](apps/acowork-desktop/src-tauri/capabilities/default.json)、[dev/ci.sh](dev/ci.sh)（`run_desktop_ipc_scope_redline`）
- 设计：[14-desktop-app.md](../design/zh/14-desktop-app.md) §7.5
- 后续规划（浏览器自动化 / 自研 Browser MCP 的调研与设计）：[browser-automation-plan.md](../../plan/zh/browser-automation-plan.md)
- 相关 ADR：[ADR-112](./ADR-112-workspace-preview-capability-ticket.md)（工作区 HTML 票据 URL 渲染，本 ADR 的宿主）、[ADR-116](./ADR-116-hosted-application-framework.md)（托管应用同样承载第三方文档）