# ADR-116: 托管应用框架（Application，而非插件）——以 PM / DOC 为蓝本

**状态**：已接受（2026-11-15；用户定案"与其说是插件，更像是一个应用"，授权以 PM / DOC 为蓝本搭完整框架并完成验证）
**日期**：2026-11-15
**决策者**：用户

**关联**：
- [ADR-064](./ADR-064-pm-standalone-process.md)、[ADR-070](./ADR-070-doc-standalone-process-and-tree-storage.md)、[ADR-101](./ADR-101-memory-standalone-mcp-service.md)、[ADR-084](./ADR-084-user-standalone-process.md)（独立进程范式四连——本 ADR 把它抽象为声明式应用框架）
- [ADR-009](./ADR-009-gateway-workspace-isolation.md)（Gateway 边界红线，本 ADR 延续为"Gateway 不读 app 数据目录"）
- [ADR-104](./ADR-104-agent-manifest-declaration-surface.md)（声明式 manifest 的范式来源；本 ADR 明确**不复用** `AgentManifest` 类型）
- [ADR-112](./ADR-112-workspace-preview-capability-ticket.md)（HMAC capability ticket——app session ticket 复用同一安全模型）
- [ADR-069](./ADR-069-mcp-tool-level-optin.md)、[ADR-090](./ADR-090-mcp-lsp-install-ownership-node.md)（catalog 的两层结构：机器级推送 + per-agent activation）
- [设计文档 34](../design/zh/34-hosted-application-framework.md)（完整契约、API 清单与验证步骤）
- `AGENTS.md` §Conventions（Desktop = 纯展示层；Gateway 边界 = 通信 + 资源管理 + 反代）

---

## 1. 决策摘要

### 1.1 一句话

**把 PM / DOC 已落地的"独立进程 + 独立 HTTP 服务 + MCP HTTP + Gateway 反代"抽象为声明式的「托管应用（Hosted Application）」框架**：每个应用自带进程、HTTP 服务、前端页面和数据目录；平台只负责注册、托管、路由、身份注入、MCP catalog 发布、导航槽位和一组**窄的**平台接口。平台**不**提供 `vscode.*` 式的宿主 SDK 让应用改写平台状态——这是"应用"与"插件"的分水岭。

### 1.2 关键决策表

| # | 决策 | 结论 |
|---|---|---|
| 1 | 定位 | **应用，不是插件**。应用拥有自己的 UI / 数据 / 业务逻辑；平台拥有契约与准入。禁止"往宿主 UI 注入菜单/命令/状态栏"的贡献点，禁止宿主 API 修改 agent 配置、停 Runtime、改写 catalog |
| 2 | 首批范围 | **PM + DOC**。二者已是 ADR-064/070 的独立进程，作为蓝本与首批注册应用完成端到端验证。**memory 排除**：它的 UI 是右侧导航栏嵌入面板，属于另一类 UI 契约，本轮不动其 supervisor / proxy / catalog 路径 |
| 3 | 声明层 | 新增 `acowork-core::app`：`app.toml`（`AppManifest`）+ `AppRegistry`。内置 PM/DOC 声明编译进 core；用户安装声明从 `{data_dir}/apps/*.toml` 叠加；启停状态持久化到 `apps_disabled.json`。id / 路径 / binary 在信任边界强校验 |
| 4 | 进程 | `runtime = "legacy"`（PM/DOC）沿用已验证的 ADR-064 supervisor（含 `--gateway-url`、token env、身份注入测试套件）；`runtime = "generic"` 走框架配置驱动 supervisor（`app::supervisor`）。两者统一投影到 `resolve_app_port()`，禁止每个应用各写一套 Rust supervisor |
| 5 | 网络 | 应用只监听 `127.0.0.1`，桌面端与 Agent **一律经 Gateway**：`/api/apps/{id}/ui/*`（静态 bundle）、`/api/apps/{id}/api/*`（应用 REST）、`{mcp.gateway_path}`（MCP）。`/api/pm\|doc/*` 保留为字节级兼容别名，Desktop 与 agent manifest 零改动 |
| 6 | MCP | 仍以 HTTP（Streamable HTTP 无状态子集，单 `POST /mcp`）为主。`build_available_mcps()` 中 pm/doc 两处硬编码改为遍历 registry：禁用应用立即从 catalog 消失，新应用声明 `auto_inject` 即自动进入 catalog，无需改代码。memory 保持 ADR-101 原路径不动 |
| 7 | 前端 | 应用自己 serve `/ui/`；Desktop 只提供 iframe 容器 + `postMessage` 窄桥（theme / locale / accent / ready / notify）。`sandbox` **带 `allow-same-origin`**（见 §1.4）：不带时 iframe 是 opaque origin，`localStorage` / `IndexedDB` 一律抛 `SecurityError`，应用存不住偏好、存不住用户自加字体。应用拿到的是 **Gateway 的源**，不是 Desktop 窗口的源，父窗口 DOM 与 Bearer token 仍不可达 |
| 8 | 认证 | 主窗口用已登录的 Bearer 换短期 HMAC **app session ticket**，经 iframe URL **fragment**（不进 query、日志、Referer）传给应用页；应用页把 ticket 放进 `X-App-Session` 头，Gateway 逐请求校验并注入可信身份。可选一次性交换为 `HttpOnly; SameSite=Strict; Path=/api/apps/{id}/` cookie |
| 9 | 身份 | 反代剥离客户端自报的 `X-Actor` / `x-actor-admin` / `x-app-id` / `Authorization` / cookie，注入 `X-App-Id` / `X-App-User` / `X-Actor` / `x-actor-admin=false` / `X-App-Platform-Url`。延续 ADR-064 Phase 3 的反伪造语义 |
| 10 | 平台接口 | `/api/platform/{identity,context,agents,notify}` 最小集合，按 `app.toml` 声明的 `permissions.platform` **默认拒绝**。应用后端凭 supervisor 注入的 internal service token + `X-App-Id` 调用；webview 永远拿不到平台凭证 |
| 11 | 失败隔离 | 应用无进程 / 重启中 → `503 + Retry-After: 2`（与现有 proxy 契约一致）；应用崩溃不影响 Gateway / Runtime；启用状态变更触发 `acowork/global/mcps` hot push |
| 12 | 回滚 | 禁用应用（平台侧准入）或从 `{data_dir}/apps/` 移除声明即回滚；`/api/pm\|doc/*` 兼容别名保证存量契约不受影响 |

### 1.3 不变量（红线，建议加入 `AGENTS.md`）

1. **Gateway 零应用业务**：不读应用数据目录、不解析应用工具语义、不承载应用 REST 实现。
2. **Desktop 零构建耦合**：不 import 应用前端代码；应用页面只能运行时经 Gateway 反代的 iframe 加载。
3. **应用不碰平台内部**：不直连 Runtime HTTP、MQTT broker `:19875`、Vault 或别的应用数据目录；唯一出口是 `/api/platform/*`。
4. **身份由平台注入，不由客户端自报**；应用 webview 不持有用户 Bearer token，token 不进 localStorage、不进 URL query。
5. **应用之间不互相依赖**（不直连、不共享数据目录）；协作只能经平台 catalog / 事件。
6. **平台不提供改写平台状态的宿主 API**——这是"应用"定位的核心护栏。
7. memory 的右侧面板嵌入形态未纳入本框架；在其 UI 契约明确前，不得把 memory 声明为 `app.toml` 应用。

### 1.4 iframe sandbox 与 `allow-same-origin`

`AppFrame` 的 `sandbox` 是 `allow-scripts allow-same-origin allow-forms allow-popups allow-downloads`。

**为什么必须给 `allow-same-origin`**：不透明源（opaque origin）下浏览器把 iframe 当作来自一个谁也不是的源，`localStorage.setItem` 与 `indexedDB.open` 直接抛 `SecurityError`（实测：在完全相同的 sandbox 里跑探针，两者都是 `SecurityError`）。托管应用是有状态的编辑器：界面语言、"不再提示缺字体"、用户自己添加的字体（IndexedDB，几 MB 的 CJK 字体放不进 localStorage）全都要落盘。没有这个属性，这些能力在 ACowork 里永远坏掉，而同一个包在浏览器里正常。

**它换不来什么**：iframe 的源是 Gateway（`http://127.0.0.1:19876`），不是 Desktop 窗口的源（Tauri `tauri://localhost` / dev `localhost:5173`）。父窗口 DOM 读不到；Bearer token 存在 Desktop 源的 `localStorage`，读不到；Gateway API 只认 Bearer，应用手里只有自己的 app session ticket（URL fragment 传入），拿不到别人的会话。

**已知天花板（可接受）**：所有托管应用共享同一个 Gateway 源，因此应用 A 与应用 B 的 `localStorage` 同源——一个恶意应用理论上能读到另一个应用存的偏好。缓解路径（按需再做，不是现在）：宿主在注入脚本里给 `localStorage` 加应用 id 前缀垫片，或给每个应用单独的源（独立端口 / 独立 proxy 前缀）。写在这里，是为了下次有人问"为什么应用之间能互相看见存储"时不用重新推导。

---

### 1.5 托管应用的系统字体

Craft 文档（`.docx` / `.pptx` / `.xlsx`）**只写字体名，不嵌字形**：正文说"宋体"、"Calibri"，字体程序不在文件里。渲染器必须自己有那份字体，而 wasm 里读不到 `%WINDIR%\Fonts`，浏览器端的 `queryLocalFonts`（`Load System Installed Fonts…` 的实现）在 WebView2 上不可用、也需要用户手势授权。结果就是：中文文档在应用里全是方块，还要用户自己去装字库。（PDF 不受影响——它把字体子集嵌在文件里，所以 PdfCraft 天然没有这个问题。）

**机制**（不改 `craft-web-host`、不新增 bridge 消息类型）：

```
应用首帧就绪 ──fontsWanted {have:[families]}──▶ AppFrame
AppFrame ──system_fonts(have)──▶ Tauri 命令（读 %WINDIR%\Fonts，只返回缺的）
AppFrame ──fonts {fonts:[{name,data(base64)}]}──▶ 应用桥
应用桥 ──window.<app>.execute("font.add",{data,name})──▶ 应用引擎（现有命令）
```

- **时机**：应用自己报 `ready`（canvas 的 `<app>:ready`）时握手一次，不等用户操作，也不等"文档报缺字体"——那样用户已经看见方块了。`have` 由应用自己的 `font.list` 提供，所以第二次以后是零传输（装过的字体在应用自己的 IndexedDB 里），宿主不维护任何"已注入"状态。
- **默认集**（按序串行灌，先到先生效，只在本机首次打开时传一次）。同一个"角色"在三个平台上是不同的字体，所以各自一张表：
  - Windows：SimSun（宋体）、DengXian（等线）、Microsoft YaHei（微软雅黑）、Calibri、Arial、Times New Roman，约 58 MB（`%WINDIR%\Fonts`）
  - macOS：Songti SC（宋体）、PingFang SC（苹方，顶上微软雅黑的位置）、Hiragino Sans GB、Helvetica、Arial、Times New Roman（`/System/Library/Fonts`、`Supplemental`、`/Library/Fonts`、`~/Library/Fonts`）
  - Linux：Noto Serif CJK SC（宋体的替身）、Noto Sans CJK SC、Liberation Sans / Serif、Noto Sans / Serif、DejaVu Sans（`/usr/share/fonts`、`/usr/local/share/fonts`、`~/.local/share/fonts`、`~/.fonts`，递归找）
  - 表里没有的字体（或这台机器没装的）静默跳过；名字猜错也只是少注入一个，不会报错。
- **不变量**：宿主不猜应用支持什么。应用有 `font.add` 才生效；没有（gridcraft / pdfcraft / vectorcraft / lightcraft）桥就无声 no-op，包在浏览器里打开的行为完全不变。
- **天花板**：默认集不含黑体 / 楷体 / 仿宋；应用字体库此前会丢掉字体自带的 localized family name（`宋体` 装成 `SimSun` 后按 `宋体` 查不到，于是"缺字体"提示照旧、正文走 script fallback），已在 wordcraft 修（`fontdb/user.rs` 与 bundled / system 路径一样保留 `local_names`）。DeckCraft 用的是更早的一套 fontdb，没有这一层，字形正确但仍会报"缺字体"名字——留作后续。


## 2. 背景与动机

### 2.1 现状：框架事实存在，但没有"框架"

在本次改动前，PM / DOC / user / memory 已各自是独立进程，且四套 `lifecycle/*_supervisor.rs` 逐字同构（共用 `acowork-core::supervisor`）。但"有哪些服务"这件事被硬编码在五处：`gateway.toml` 的 `[pm]`/`[doc]` 段、`build_available_mcps()` 里三个 `if let`、三个 `http/*_proxy.rs`、Desktop `NavBar.tsx` 的静态导航、`ExtensionsView.tsx` 里恒空的 `EXTENSIONS = []`（注释明确写着 `core/acowork-gateway/src/http/extensions.rs` 待建）。新增一个同类能力需要改五处 Rust + 前端。

### 2.2 定位修正

用户明确指出"与其说是插件，更像是一个应用"。VSCode 式插件的主要交互面是宿主 API（`vscode.*`），可以读写编辑器状态、通过 `contributes.*` 往宿主 UI 注入贡献点。ACowork 的这类能力天然是**完整应用**：自带页面、数据、业务逻辑，只是被平台托管。因此平台接口必须收窄为"上下文读取 + 少量显式能力"，而不是宿主 SDK。

---

## 3. 设计

### 3.1 五个契约面

| # | 通道 | 方向 | 认证 |
|---|---|---|---|
| ① | `/api/apps/{id}/ui/*` | Desktop iframe → 应用静态 bundle | 公开静态字节 |
| ② | `{mcp.gateway_path}`（`/api/pm/mcp`、`/api/doc/mcp`） | Agent Runtime → 应用 MCP | node token / `X-MCP-Actor` 校验（保持 ADR-064） |
| ③ | `/api/apps/{id}/api/*` | Desktop / iframe → 应用 REST | app session ticket |
| ④ | `/api/platform/*` | 应用后端 → 平台 | internal service token + `X-App-Id` + 声明权限 |
| ⑤ | supervisor ↔ `/health` | 平台 → 应用进程 | spawn / port-file / 反向探活（ADR-018） |

MCP **刻意不放在 `/api/apps/{id}/` 通配下**：Agent 侧继续走已验证的 `{mcp.gateway_path}`，避免把机器凭证路径与 ticket 路径混在一起。

### 3.2 关键实现落点

| 层 | 文件 |
|---|---|
| 声明与校验 | `core/acowork-core/src/app.rs`（`AppManifest`、`AppRegistry`、`app_shell_html`） |
| 运行时状态投影 | `core/acowork-gateway/src/app/registry.rs`（`resolve_app_port` / `app_status` / `app_mcp_url`） |
| 通用 supervisor | `core/acowork-gateway/src/app/supervisor.rs`（配置驱动、模板占位、密钥走 env 不走 argv） |
| 反代与身份注入 | `core/acowork-gateway/src/app/proxy.rs` |
| 发现与控制面 | `core/acowork-gateway/src/app/api.rs`（`GET /api/apps`、enable/disable、session-ticket） |
| 平台接口 | `core/acowork-gateway/src/app/platform.rs` |
| 会话票据 | `core/acowork-gateway/src/app/ticket.rs`（独立 HMAC 域，与 ADR-112 预览票据互不验签） |
| 应用自带页面 | `core/acowork-pm/src/ui.rs`、`core/acowork-doc/src/ui.rs` |
| Desktop | `apps/acowork-desktop/src/lib/apps-api.ts`、`src/components/apps/AppFrame.tsx`、`src/views/ExtensionsView.tsx` |

### 3.3 安全要点

- ticket = `v1.<expires>.<user_hex>.<mac_hex>`，MAC 覆盖 `(expires, app_id, user_id)`，长度前缀防字段混淆；换一个 app id 无法重放；密钥进程内随机，Gateway 重启即全部失效。
- `auth_middleware::is_public_path()` 仅对 `/api/apps/{id}/{ui|api|session}/...` 放行；`/api/apps`、`/api/apps/{id}/enable|disable|session-ticket` 仍在 bearer 闸门内。所有公开通道在 `app::proxy` handler 内二次校验，静态页面不含敏感数据。
- cookie 版本 `Path=/api/apps/{id}/` 精确隔离；header 版本（`X-App-Session`）优先，规避跨站 iframe 的 SameSite 限制。

---

## 4. 借鉴与刻意不借鉴（VSCode 模式对照）

**借鉴**：声明式 manifest；lazy activation 提示（`onStartup` / `onNavigate` / `onMcpCall`）；独立进程隔离（extension host 的思想）；权限声明与默认拒绝；签名与版本（后续可复用 `acowork-sign` 的 APK v2 Ed25519 Signing Block）。

**刻意不借鉴**：`contributes.menus / commands / views` 注入宿主 UI；`vscode.*` 读写宿主状态；进程内 `activate()` 钩子；共享 DOM / window；插件市场与宿主版本强绑定。应用只有一个贡献点：导航槽位 + MCP server + 声明的平台权限。

---

## 5. 落地与验证

### 5.1 已完成

- 声明层、通用 supervisor、通用反代、apps API、platform API、ticket、catalog 泛化、PM/DOC `/ui/` 与 `/app-context`、Desktop 工作台与 iframe 桥。
- PM / DOC 作为首批注册应用，`/api/pm|doc/*` 兼容别名保持不变，`acowork/global/mcps` 改为 registry 驱动。
- memory 的 supervisor / proxy / catalog 分支未改动。

### 5.2 验证清单

1. `cargo check -p acowork-core -p acowork-gateway -p acowork-pm -p acowork-doc` 通过。
2. 单元测试：`acowork-core::app`（4）、`acowork-gateway::app`（11，其中 5 个为 `app::framework_tests` 对真实 `build_router()` 的 router 集成测试）、`acowork-pm::ui`（1）、`acowork-doc::ui`（1）全部通过。
3. Desktop：新增 `apps-api.test.ts`（6）/ `AppFrame.test.tsx`（4）共 10 个框架契约测试，加上 `capsule` / `listHeaderHeight` / `sidebarRowFontSize` / `overlayScrollCoverage` 共 42 个视图结构测试，全部通过。
4. 冒烟命令与期望结果见设计文档 34 §6；隔离实例启动的完整 live smoke 属于交付后验证项，不在本批单元测试结论内。

---

## 6. 遗留与后续

- **P2 前端迁移**：PM / DOC 现有主视图仍是 Desktop 内置 React 组件（`src/views/pm|doc`）。框架已提供 webview 通道与 `/ui/` 页面，但把完整业务 UI 迁进应用包风险最大，建议先拿 DOC 试点。当前并存是有意的过渡态。
- **P3 打包与安装**（契约已定义在设计文档 34 §6，代码未实现）：`.app` 包格式（ZIP + Signing Block，含 `bin/` 与 `ui/`）、`[package]` 段（`app_api` / `platforms` / `entry`）、复用 `acowork-sign` 通用层的签名与证书指纹锁定、`POST /api/apps/install` 与卸载/回滚、`apps/{id}/versions/{v}` 与 `app-data/{id}` 的包数据分离、二进制三级寻址（替换 `sibling_binary` 单一来源），以及三处运行时联动（`AppRegistry::reload()`、动态 supervisor、catalog hot push）。落地顺序 P3.1 → P3.3，其中 P3.1 可独立验证（把 PM 换 id 装成 `pm-local` 与内置 pm 并存）。
- **其他 P3**：SSE 事件通道、Vault secrets、应用市场、动态进程启停（当前 enable/disable 是平台侧准入，进程生命周期在 Gateway 启动期决定）。
- **memory**：右侧导航栏嵌入 UI 属于"面板型"应用，需要另立 UI 贡献点契约，之后再纳入。
- `ExtensionsView` 的 i18n 命名空间仍未建立（原有 TODO 保留）；本轮未改 5 个语言包。
- 建议 CI 增补两条 red line：`dev/ci.sh` 的 `run_all_redlines` 中检查 Gateway 不读 app 数据目录、Desktop 不 import 应用前端代码。
- **双画布**（本 ADR 只定了托管壳，没定 Agent 改哪一份状态）：[ADR-121](./ADR-121-hosted-app-dual-canvas.md) / [设计文档 35](../../design/zh/35-hosted-app-dual-canvas.md)。左侧导航 = 用户画布；MCP 默认不 attach；聊天预览走常驻栏 iframe。
