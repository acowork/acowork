# 设计文档 34：托管应用框架（Hosted Application Framework）

**版本**：1.0（2026-11-15）
**状态**：已实现（PM / DOC 首批验证通过）
**权威决策**：[ADR-116](../../adr/zh/ADR-116-hosted-application-framework.md)
**English counterpart**：[Design Doc 34](../en/34-hosted-application-framework.md)

> 中文文档为 source of truth，英文 counterpart 按约定同 commit 维护。

---

## 1. 定位

应用 = 独立进程 + 独立 loopback HTTP 服务 + 自有前端 bundle + 自有数据目录 + MCP HTTP server。平台 = 注册、托管、路由、身份注入、MCP catalog 发布、导航槽位、窄平台接口。平台**不**注入宿主 UI，也**不**提供修改平台状态的宿主 API。

首批应用：`pm`（18082）、`doc`（18081）。`memory` 不在本框架内——它的 UI 是右侧导航栏嵌入面板，属于另一类贡献点契约。

---

## 2. 架构与数据流

```
Desktop (Tauri)                        Gateway (:19876)                 App process (127.0.0.1:{port})
┌────────────────────┐                ┌──────────────────────┐        ┌──────────────────────────┐
│ ExtensionsView     │  GET /api/apps │ app::api             │        │ /ui/*  自有 bundle      │
│  ├ 列表 / 详情     │◄──────────────►│  app::registry       │        │ /api/* 自有 REST        │
│  └ AppFrame iframe  │  ticket        │  app::proxy          │───────►│ /mcp   MCP HTTP         │
│     (no same-origin)│◄──────────────►│  app::platform       │        │ /health                │
└────────────────────┘                │  app::ticket         │        └────────────────────────┘
                                       │  app::supervisor ───┼── spawn / 探活 / 重启
Agent Runtime ── {mcp.gateway_path} ──►│  *_proxy (兼容别名)  │
                                       │  build_available_mcps│──► MQTT acowork/global/mcps
                                       └──────────────────────┘
```

`/api/pm/*`、`/api/doc/*` 继续由原有 `pm_proxy` / `doc_proxy` 处理，字节级契约不变；新框架路由为 `/api/apps/{id}/*`。

---

## 3. `app.toml` 声明

| 段 | 字段 | 说明 |
|---|---|---|
| 顶层 | `id` / `version` / `name` / `description` / `icon` / `builtin` | `id` 必须 `[a-z][a-z0-9_-]{0,63}`，会作为 URL 段、目录名、cookie path |
| `[process]` | `binary` / `port_hint` / `runtime` / `legacy_slot` / `port_file` / `health_path` / `extra_args` / `env` | `binary` 只允许可执行文件名（禁止 `/`、`..`）；`runtime = legacy \| generic`；模板占位 `{gateway_url}` / `{data_dir}` / `{internal_service_token}` / `{publisher_token}` / `{app_id}` |
| `[mcp]` | `path` / `protocol` / `auto_inject` / `gateway_path` | `gateway_path` 是 Agent 侧经 Gateway 访问的 URL，如 `/api/pm/mcp` |
| `[ui]` | `path` / `mode` / `[ui.nav]` | `mode = webview \| native`；nav 只提供 `label` / `icon` / `order` 一个槽位 |
| `[activation]` | `events` | `onStartup` / `onNavigate` / `onMcpCall`（当前为提示，built-in 随 Gateway 启动） |
| `[permissions]` | `platform[]` | 默认拒绝；可选 `identity.read` / `context.read` / `agents.read` / `notify` |

内置声明编译在 `core/acowork-core/src/app.rs`（`PM_APP_TOML` / `DOC_APP_TOML`）；用户声明从 `{data_dir}/apps/*.toml` 叠加，非法声明跳过并告警。`AppRegistry::load()` 在 `GatewayState::new()` 中调用。

---

## 4. HTTP 契约

### 4.1 控制面（bearer 保护）

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/apps` | `{ "apps": [AppDto...] }`，含 enabled / status / port / mcp_url / ui_path / permissions |
| GET | `/api/apps/{id}` | 单个 AppDto |
| POST | `/api/apps/{id}/enable` \| `/disable` | 平台侧准入开关，写 `apps_disabled.json` 并触发 `acowork/global/mcps` hot push |
| POST | `/api/apps/{id}/session-ticket` | 用已登录身份 mint 8h ticket，返回 `{ ticket, expires_at, ui_url }` |

### 4.2 应用通道（ticket 校验）

| 路径 | 上游映射 | 认证 |
|---|---|---|
| `GET /api/apps/{id}/ui/*` | `/{ui.path}/*` | 公开静态字节 |
| `*/api/apps/{id}/api/{tail}` | `/{tail}` | `X-App-Session: <ticket>` 或 scoped cookie |
| `POST /api/apps/{id}/session` | — | 一次性 ticket → cookie 交换 |
| `mcp` / `enable` / `disable` / `session-ticket` | — | 通配 handler 明确 404，走各自精确路由 |

`auth_middleware::is_public_path()` 只豁免 `ui` / `api` / `session` 三个首段，控制面仍在 bearer 闸门内。

### 4.3 平台接口（App → Platform）

| 路径 | 能力 | 返回 |
|---|---|---|
| GET `/api/platform/identity` | `identity.read` | `user_id` / `role` / `app_id` |
| GET `/api/platform/context` | `context.read` | instance、advertise_host、http/mqtt 端口、启动时间 |
| GET `/api/platform/agents` | `agents.read` | 已安装 agent 的 id/name 列表 |
| POST `/api/platform/notify` | `notify` | `{ accepted: true }`（仅结构化日志，无副作用） |

调用方 = 应用后端，凭证 = supervisor 注入的 internal service token + `X-App-Id`。Gateway 按 registry 的声明权限默认拒绝（403 `capability_not_declared`）。webview 从不直接调用这些端点。

### 4.4 身份注入

反代转发前剥离：`x-actor` / `x-actor-admin` / `x-mcp-actor` / `x-app-id` / `x-app-user` / `x-app-session` / `x-app-platform-url` / `authorization` / `cookie` / `x-acowork-node-token` / 所有 hop-by-hop 头。

注入：`X-App-Id`、`X-App-User`、`X-Actor`（= ticket 中的用户）、`x-actor-admin: false`、`X-App-Session-Verified: true`、`X-App-Platform-Url`（应用据此调用平台接口）。

---

## 5. 会话票据

```
ticket := v1.<expires_at_unix>.<user_id_hex>.<mac_hex_16bytes>
mac    := HMAC-SHA256(secret, "acowork/app-session/v1|<exp>|<len>:<app_id>|<len>:<user_id>")
```

- secret 进程内随机、不落盘、不打印；与 ADR-112 预览票据**不同域**，互不验签。
- MAC 覆盖 app_id，换应用重放失败；校验顺序 MAC 优先，不泄露过期信息。
- 传递：Desktop → iframe URL `#ticket=...`（fragment，不进 query/日志/Referer）；iframe JS → `X-App-Session` 头。
- 可选 cookie：`HttpOnly; SameSite=Strict; Path=/api/apps/{id}/; Max-Age=...`。header 路径优先，规避跨站 iframe 的 SameSite 限制。
- Gateway 重启使所有票据失效，Desktop 重新 mint。

---

## 6. 打包与安装

> 本章定义「应用从哪来、怎么装进来」。§1–§5 只解决「已经在本机的应用怎么被托管」，不含分发。本章是实现前的契约，代码尚未落地，落地顺序见 §6.13。

### 6.1 范围与归属

| 维度 | 结论 | 理由 |
|---|---|---|
| 安装归属 | **Gateway 宿主机**（`{data_dir}` 下） | supervisor 跑在 Gateway 进程内（`gateway/mod.rs`）；远程 node 目前只托管 Runtime，不托管应用的进程与数据 |
| 分发形态 | 单个 `.app` 文件（ZIP + Signing Block） | 与 `.agent` 共用一套签名体系，密码学部分不重写 |
| 信任默认值 | **拒绝未签名包** | 应用会拉起一个本地任意进程，风险面大于 agent 包 |
| memory | 仍排除 | 面板型 UI 契约未定（§1） |

远程分发（把应用推到某个 node 上运行）**不在本章**：它要求 node 侧新增 app supervisor、数据目录与准入语义，属于独立 ADR，不能顺手塞进来。

### 6.2 `.app` 包格式

`.app` 是一个 ZIP，装下应用运行所需的全部内容。与 `.agent` 最大的差别是**内含可执行文件**：

```
<app_id>-<version>.app
├── app.toml                # 必需：§3 全字段 + §6.3 的 [package] 段
├── bin/                    # 必需：平台二进制
│   ├── acowork-pm          #   文件名 = [process].binary
│   └── acowork-pm.exe      #   Windows 版；一个包只含一个平台
├── ui/                     # 可选：应用前端 bundle（[ui].path 指向这里）
│   ├── index.html
│   └── assets/
├── assets/                 # 可选：图标等静态资源
│   └── icon.png
└── LICENSE                 # 可选
```

| 项 | 规则 | 校验点 |
|---|---|---|
| 条目路径 | 禁止 `..`、绝对路径、符号链接、加密条目、嵌套 ZIP | 解压前逐条目扫 |
| `bin/` | 必须存在，且 `bin/{process.binary}` 可执行 | 安装时 |
| 平台 | `[package].platforms` 必须含当前 `(os, arch)` | 安装时 |
| 大小 | 单包 ≤ **200 MB**（`.agent` 是 50 MB；app 要带二进制与前端） | 收包时流式计数，超限即断，不整包 buffer |
| 压缩算法 | 仅 `deflate` / `store` | 解压时 |
| 未知顶层条目 | 忽略（向前兼容），但不参与任何契约 | 解压时 |

**不做多平台 fat 包**：一个包一个平台，平台由 `[package].platforms` 显式声明，不在运行时猜。

### 6.3 `app.toml` 新增 `[package]` 段

```toml
[package]
# 宿主契约版本：Gateway 只接受自己支持的 app_api
app_api = 1
# 目标平台，安装时与当前 (os, arch) 比对
platforms = ["windows-x86_64"]
# 包内入口（相对包根），缺省 bin/{process.binary}
entry = "bin/acowork-pm"
```

`app_api` 是**宿主契约版本**，不是应用版本。应用版本可以按 semver 随便走；只要它只用 §4 那五个契约面，就一直写 `app_api = 1`。Gateway 见到更高的 `app_api` 直接拒绝，而不是「装上了但跑不起来」。

### 6.4 磁盘布局：包与数据必须分开

这是对当前实现的一处**破坏性修正**。现在 `app::supervisor::config_from_manifest` 把应用数据目录算成 `{data_dir}/apps/{id}`，而包也会落在 `apps/` 下——两者会撞，且升级时无法区分「包内容」与「用户数据」。

```
{data_dir}/
├── apps/                          # 平台侧安装账本 + 包内容（应用只读）
│   ├── .staging/{uuid}/           # 安装中转，失败即删
│   ├── .trash/{id}.{ts}/          # 卸载保留 7 天
│   └── {id}/
│       ├── state.json             # current / fingerprint / origin / installed_at
│       └── versions/
│           ├── 1.0.0/             # 包内容原样解压（app.toml / bin/ / ui/）
│           └── 1.0.1/
└── app-data/
    └── {id}/                      # 应用私有数据；升级 / 回滚 / 重装都不动
```

- `apps/{id}/state.json` 是**平台账本**，应用不可读写；`versions/*/` 是包内容，语义上只读。
- `app-data/{id}/` 由 Gateway 创建目录、以 `ACOWORK_APP_DATA_DIR` 传给进程，**从不读取内容**（ADR-009 红线）。
- 常量：`APPS_DIR_NAME = "apps"` 保留，新增 `APP_DATA_DIR_NAME = "app-data"`；`config_from_manifest` 的 `data_dir` 改为 `{data_dir}/app-data/{id}`。

`AppRegistry::load()` 需同时理解两种布局：

| 布局 | 来源 | 优先级 |
|---|---|---|
| `apps/{id}/versions/{current}/app.toml`（配 `state.json`） | 安装 | 高 |
| `apps/*.toml` | 手工 / 本地开发（现状） | 低；同 id 冲突时目录包胜出并 `warn` |

平铺 toml 刻意保留：内置应用与本地开发仍可「放一个 toml 指向已有二进制」，不必先打包。

### 6.5 二进制寻址

当前 `gateway/mod.rs` 用 `lifecycle::process::sibling_binary(&bin_name)`，即二进制必须躺在 Gateway 可执行文件的同级目录。这对内置应用成立（随构建产物一起出去），对**安装的应用完全不成立**——这是安装功能的前置阻塞点。

改为三级解析，命中即停：

1. `{data_dir}/apps/{id}/versions/{current}/{package.entry}`
2. `{data_dir}/apps/{id}/{process.binary}`（平铺 toml 的手工开发布局）
3. `sibling_binary(&bin_name)`（内置 / 随 Gateway 分发的旧行为）

命中但不在允许目录内、或不可执行 → **拒绝启动该应用**（`503`，日志给绝对路径），且**不回退**到下一级。回退会让「装错了版本却跑起来一个旧的」这种故障无法定位。

### 6.6 签名与信任

复用 `acowork-sign` 的通用层，不重写密码学：

| 能力 | 复用方式 |
|---|---|
| Signing Block（Local Files + Central Dir + EoCD 全覆盖） | `signing_block::{SigningBlock, Signer, SectionDigest}` 直接用 |
| ZIP 层插入 / 定位 | `zip_utils::{find_eocd, insert_block_before_cd, find_binary_signing_block}` 直接用 |
| 签名 / 验签 | `sign::sign_package` / `verify::verify_package` 直接用（与包内语义无关） |
| 打包 | **不复用** `packager::build_agent_package`（硬编码 `manifest.toml`、`AgentManifest`、conversations/memory 排除规则）。新增 `build_app_package`，复用上面三层 |

签名身份沿用 `.agent` 的两类（Developer 自签名 / Platform），并加一条 app 专属规则：

| 检查 | 规则 | 失败错误码 |
|---|---|---|
| 完整性 | SHA-256 section digest 比对 | `package_tampered` |
| 来源 | 首次安装记录证书指纹；升级必须同指纹 | `signer_mismatch` |
| 平台应用 | `[package].platform_app = true` 时必须是 Platform 签名 | `platform_signature_required` |
| 未签名包 | 默认拒绝；`apps.allow_unsigned = true` 时才接受并 `warn` | `unsigned_package` |
| 内置应用 | 无包、无签名，随 Gateway 构建分发 | — |

> 与 `.agent` Phase 1「未签名可选」不同，`.app` **默认拒绝未签名**：安装一个应用等于允许一个本地进程长期运行，风险面远大于 agent 包。开发期用 `allow_unsigned` 显式放行，生产建议关闭。

### 6.7 安装流程

硬约束：任一步失败都不留半装状态，且不影响已在运行的其他应用。

```
1  收包     → {data_dir}/apps/.staging/{uuid}.app（流式写，边写边计大小，超限即断）
2  结构校验 → 条目白名单、无 ..、无绝对路径、无符号链接、无加密/嵌套
3  验签     → verify_package() → 证书指纹 + 身份类型
4  解压     → .staging/{uuid}/（逐条目再校验一次路径与大小）
5  声明校验 → app.toml::from_toml() + validate_manifest() + [package] 三检（app_api / platforms / entry）
6  断言     → bin 存在且可执行；ui/ 存在时 [ui].path 能对上
7  冲突检查 → 同 id 已装？（builtin 拒绝覆盖）→ 指纹一致走升级，否则 signer_mismatch
8  落位     → .staging/{uuid}/ → apps/{id}/versions/{version}/（同版本已存在则拒绝，不做隐式覆盖）
9  写账本   → apps/{id}/state.json（原子写）
10 重载     → AppRegistry::reload()
11 起进程   → enabled 则启动/重启 supervisor；失败保留已装状态，标 status=error
12 推目录   → 触发 acowork/global/mcps hot push
```

- 第 8 步的 `rename` 与 `apps/{id}` 同一文件系统（都在 `{data_dir}` 下，成立）。
- 第 11 步失败**不回滚安装**：包是好的，起不来是运行问题；回滚会让用户丢了刚装的东西，还看不到真正的错误。状态落 `error`，UI 显示日志路径。
- 每一步失败都要清 `.staging/{uuid}`（第 1–7 步），已在 `versions/` 下的内容不回删（那是成功安装的另一个版本）。

### 6.8 HTTP 契约

全部在控制面（bearer 保护），与 §4.1 同级：

| 方法 | 路径 | 语义 |
|---|---|---|
| POST | `/api/apps/install` | `Content-Type: application/octet-stream`，body = `.app`；返回 `AppDto` + `installed_version` |
| POST | `/api/apps/install-url` | `{ "url": "...", "sha256": "..." }`；`sha256` **必填**（URL 本身不可信） |
| GET | `/api/apps/{id}/versions` | `{ "current": "1.0.1", "versions": [{ version, installed_at, fingerprint }] }` |
| POST | `/api/apps/{id}/rollback` | `{ "version": "1.0.0" }`；切 current + 重启 + hot push |
| DELETE | `/api/apps/{id}` | `?purge_data=false`（默认）；builtin 返回 403 |

与既有端点划清边界，避免语义混淆：

| 端点 | 作用 | 明确不做 |
|---|---|---|
| `POST /api/apps/{id}/enable` / `disable` | 平台侧**准入**（catalog / 反代 / 导航） | 不装卸包、不动文件 |
| `DELETE /api/apps/{id}` | **卸载**（移入 trash + 删账本） | 默认不删 `app-data/{id}` |
| `POST /api/apps/{id}/rollback` | 切换版本 | 不重新下载、不重新验签 |

错误码是**稳定字符串**，Desktop 据此映射文案，不透传后端英文：

| code | HTTP | 含义 |
|---|---|---|
| `package_too_large` | 413 | 超过 200 MB |
| `package_malformed` | 400 | ZIP 结构非法 / 路径穿越 / 加密条目 |
| `package_tampered` | 400 | digest 不匹配 |
| `unsigned_package` | 400 | 未签名且未开 `allow_unsigned` |
| `signer_mismatch` | 409 | 升级证书指纹与已安装不一致 |
| `app_api_unsupported` | 400 | `app_api` 高于宿主支持 |
| `platform_unsupported` | 400 | `platforms` 不含当前平台 |
| `app_id_reserved` | 409 | 试图覆盖 builtin |
| `version_exists` | 409 | 同版本重复安装 |
| `binary_missing` | 400 | `bin/` 缺失或不可执行 |

### 6.9 升级 / 回滚 / 卸载语义

| 操作 | 包内容 | `app-data/{id}` | 进程 | 可回滚 |
|---|---|---|---|---|
| 升级（同 id 新版本） | 新增 `versions/{new}`，`current` 指向新版本 | 不动 | 重启 | 是 |
| 回滚 | `current` 指回旧版本 | 不动 | 重启 | 是 |
| 卸载（默认） | `apps/{id}` → `.trash/{id}.{ts}` | **保留** | 停止 | 7 天内可手工捞回 |
| 卸载 `purge_data=true` | 同上 | 删除 | 停止 | 否（可重装，数据没了） |

`.trash` 保留 7 天，由 Gateway 启动期的清理任务回收（与 PM 的 `.trash` 策略同构，但**不共享目录**）。

### 6.10 运行时联动

三处不改就会「装上了但用不了」：

1. **`AppRegistry::reload()`**：现在 `load()` 只在 `GatewayState::new()` 调一次，之后不会重读磁盘。新增 `reload()`（读盘 + 整体替换 registry），安装 / 卸载 / 回滚后调用。
2. **动态 supervisor**：现在 generic app 的 supervisor 只在 Gateway 启动期起。需要 `start_app(id)` / `stop_app(id)`，供安装后拉起、卸载前停止、回滚后重启；失败不 panic，落 `status=error`。
3. **catalog hot push**：复用 `enable/disable` 已有的 `mqtt_publisher_trigger.trigger()`，安装成功后触发一次，Agent 侧无需重启即可见。

### 6.11 Desktop 入口

`ExtensionsView` 需要补：

- 顶部「安装应用」→ 选择 `.app` 文件 → `POST /api/apps/install`（流式，带进度）。
- 详情页展示 `installed_version` / 签名指纹 / 安装时间 / 包大小 / 来源（upload / url）。
- 「版本」区块：版本列表 + 当前标记 + 回滚。
- 卸载：**inline 确认**（不用 modal，遵循项目 UI 规范），勾选「同时删除应用数据」。

所有失败按 §6.8 的 `code` 映射为中文文案。

### 6.12 打包工具链

```bash
# 从应用源码目录产出未签名包（内部调 acowork-sign 通用层）
dev/common/build-app.sh <app-src-dir> --version 1.0.1 --platform windows-x86_64

# 签名（复用现有二进制，语义与 .agent 一致）
acowork-sign --key ./keys/app-key.pem --cert ./keys/app-key.crt \
             --input ./build/pm-1.0.1.unsigned.app \
             --output ./build/pm-1.0.1.app

# 验证
acowork-verify ./build/pm-1.0.1.app
```

`build-app.sh` 只做三件事：按 `platforms` 校验二进制架构、把 `ui/` 打进包、调 `build_app_package` 产出未签名包。它**不签名**——密钥不进 CI 明文与构建脚本。

### 6.13 分阶段落地

| 阶段 | 内容 | 前置 |
|---|---|---|
| **P3.1** | 包格式 + `[package]` 段 + 二进制三级寻址 + `app-data` 分离 + `reload()` | 无（改动最小，且解除安装的前置阻塞） |
| **P3.2** | `build_app_package` + `build-app.sh` + 本地上传安装 + 卸载 | P3.1 |
| **P3.3** | 签名强制 + 指纹锁定 + `install-url` + 版本列表 / 回滚 + Desktop 入口 | P3.2 |

P3.1 可独立验证：把 PM 打成一个包，换 id（如 `pm-local`）装进 `apps/pm-local/`，与内置 `pm` 并存；能出现在 `/api/apps`、能启动、MCP 能被 Agent 看到。这一步不依赖签名与解析器，却已经把最容易返工的两个契约（二进制寻址、数据目录分离）钉死。

### 6.14 验证清单

1. 装一个 `.app` → `GET /api/apps` 出现该应用，`status=running`、`port` 非空。
2. 同包再装一次 → `409 version_exists`（不做隐式覆盖）。
3. 篡改包内任一字节 → `400 package_tampered`。
4. 换密钥签同 id 新版本 → `409 signer_mismatch`。
5. 升级 → `current` 变化、`app-data/{id}` 内容不变、进程重启。
6. 回滚 → `current` 回旧版本、数据仍不变。
7. 卸载 → 从 catalog 消失、`apps/{id}` 进 `.trash`、`app-data/{id}` 仍在。
8. `platforms` 不含当前平台 → `400 platform_unsupported`。
9. 包内含 `../evil` 条目 → `400 package_malformed`，且 `.staging` 清理干净。
10. 安装后**不做任何重启**，Agent 侧 `acowork/global/mcps` 已含该应用的 MCP server。

### 6.15 明确不做

- 远程 node 分发（§6.1）。
- 应用市场 / 在线索引 / 自动更新检查。
- 增量升级（每次整包）。
- 应用间依赖解析（不引入 `depends_on`）。
- 多平台 fat 包。
- 包内安装钩子（`preinstall` / `postinstall`）——**拒绝在安装期执行包内任意代码**，与 `.agent` 同一条红线。

---

## 7. 验证

### 7.1 编译与测试

```bash
cd core
cargo check -p acowork-core -p acowork-gateway -p acowork-pm -p acowork-doc
cargo test -p acowork-core    app:: --lib
cargo test -p acowork-gateway app:: --lib     # 含 framework_tests 的 5 个 router 集成测试
cargo test -p acowork-pm      ui::  --lib
cargo test -p acowork-doc     ui::  --lib
```

router 集成测试位于 `core/acowork-gateway/src/app/framework_tests.rs`，不是 `tests/` 下的独立 integration test；它直接对真实 `build_router()` 发起请求，覆盖 app 列表、UI/API 通道身份注入、ticket 跨应用重放、启停准入、平台能力默认拒绝。

Desktop 侧：

```bash
cd ../apps/acowork-desktop
npx tsc --noEmit
# 应用框架的客户端 / iframe 契约（10 个）
npx vitest run src/lib/apps-api.test.ts src/components/apps/AppFrame.test.tsx
# 视图结构回归（既有 capsule 系列）
npx vitest run src/components/common/capsule.test.tsx \
               src/components/common/listHeaderHeight.test.ts \
               src/components/common/sidebarRowFontSize.test.ts \
               src/components/common/overlayScrollCoverage.test.ts
```

### 7.2 启动冒烟（隔离实例，不影响正在运行的 Gateway）

```bash
# 独立 home 与端口；不要杀已有 gateway / runtime 进程
# CLI 顶层没有 --data-dir：隔离实例用 --home（别名 --work-dir）指定 config/ 与 data/ 的根目录
# 用仓库内相对路径，Windows / WSL 都可直接执行
./target/debug/acowork-gateway --addr 127.0.0.1:29976 \
    --mqtt-addr 127.0.0.1:29975 --home ./target/app-smoke-home
```

拿到登录后（ADR-114 账号系统恒开：先 `POST /api/auth/setup`，再 `POST /api/auth/login`）的 `$TOKEN`：

```bash
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:29976/api/apps
curl -H "Authorization: Bearer $TOKEN" -X POST \
     http://127.0.0.1:29976/api/apps/pm/session-ticket      # → ticket
curl http://127.0.0.1:29976/api/apps/pm/ui/                   # → PM 自带 HTML
curl -H "X-App-Session: $TICKET" \
     http://127.0.0.1:29976/api/apps/pm/api/app-context      # → 注入身份 + 平台 context
curl -H "X-App-Session: $TICKET" \
     http://127.0.0.1:29976/api/apps/doc/api/app-context     # → 401 invalid app session（跨应用重放被拒）
curl -H "Authorization: Bearer $TOKEN" \
     http://127.0.0.1:29976/api/platform/context
curl -H "Authorization: Bearer $TOKEN" \
     http://127.0.0.1:29976/api/pm/projects                   # 兼容别名不变
```

期望：`/api/apps` 返回 pm/doc 两个 AppDto；`ui` 返回 PM/DOC 自有页面；`app-context` 中 `app_id` / `actor` 正确且 `platform` 段非空；跨应用 ticket 401；禁用 app 后下一次 `acowork/global/mcps` 推送不再含该 server。

---

## 8. 边界与已知限制

- **打包与安装尚未实现**：契约已在 §6 定义（包格式、签名、安装/卸载/回滚、错误码、分阶段），代码未落地。当前「用户应用」的唯一形态是：手工放一份 `apps/*.toml`，且其二进制必须已经在 Gateway 可执行文件同级目录——没有包、没有版本、没有卸载。落地顺序见 §6.13。
- **前端并存**：PM / DOC 的业务主 UI 仍是 Desktop 内置 React 组件；`/ui/` 是应用自带的诊断页，用于验证完整通道。完整迁移见 ADR-116 §6 P2。
- **启停语义**：`enable` / `disable` 是平台侧准入（catalog / 导航 / 反代），进程生命周期在 Gateway 启动期决定；动态 spawn/kill 与安装联动属于 §6.10 的 P3 范围。
- **无 SSE**：平台接口的事件订阅（`/api/platform/events`）尚未实现，应用目前只能轮询 `/api/platform/*`。
- **legacy slot**：PM/DOC 通过 `legacy_slot` 映射到既有 `pm_process` / `doc_process` 状态字段，避免改动已有身份注入测试；这是有意的迁移桥，不是长期结构。
- **`navigation_guard.rs`**：桌面端 iframe 目标为 Gateway 反代路径（`http(s)` + 深层路径），需要在部署时确认导航防火墙对该前缀放行；当前 `UrlPreviewView` 的 iframe 例外是同一机制。
- `ExtensionsView` 仍无 `extensions` i18n 命名空间（沿用原有 TODO）。
- **用户画布 vs Agent 工作态**（ADR-116 未规定 Agent 改哪一份状态）：见 [ADR-118](../../adr/zh/ADR-118-hosted-app-dual-canvas.md) 与 [设计文档 35](./35-hosted-app-dual-canvas.md)。左侧导航是用户画布；MCP 默认不 attach；`visual` 应用的聊天预览是常驻栏 iframe，不是转录行。
