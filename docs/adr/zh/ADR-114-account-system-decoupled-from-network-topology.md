# ADR-114: 账号体系与网络拓扑解耦（账号系统恒开）

**状态**：已实施
**日期**：2026-11-13
**决策者**：大鱼

**取代 / 修订**：
- [ADR-076](./ADR-076-multi-user-account-system.md) §决策 12（`AUTH_MODE` = `local` / `multi_user` 的部署模式分流）
- ADR-076 §决策 12 v2/v3 中「首个密码不经网络、只能 TTY / CLI / 改 toml 设置」的约束
- [ADR-084](./ADR-084-user-standalone-process.md) 中「按模式决定是否启动 / 注入账号服务」的分支

**前置**：ADR-055（Node 拓扑）、ADR-076（账号模型）、ADR-084（user 独立进程 / user_proxy）

**后续**：[ADR-115](./ADR-115-desktop-single-server-address-model.md)（把 Desktop 侧「模式 + 地址」双旋钮合并为单一服务器地址，`local` / `remote` 降级为地址推论）

---

## 1. 问题

旧设计把两件正交的事绑在了一个开关上：

```
Gateway bind 地址 / --auth-mode
        ↓
AUTH_MODE = local | multi_user
        ↓
是否创建 accounts.json / 是否注册 /api/auth/* / 是否要求登录
```

后果是一台**单机**只要把 Gateway 从 `127.0.0.1` 改成 `0.0.0.0`（例如为了局域网调试、或启用 relay），登录模式就跟着翻转。反过来，显式 `--auth-mode local` 配一个公网可达的 bind，会把一个无鉴权的 Gateway 暴露在网卡上。

「网络拓扑」（谁能连到监听端口）和「账号体系」（这台机器上有没有账号、有几个、谁能登录）没有因果关系。把它们耦合，等于让一个网络配置去决定身份系统的存在性。

另外两个具体问题：

1. **旧的首启状态不可判定**。一个二值 `requires_setup: bool` 区分不了「账号系统压根不存在」和「账号存在但没有可登录的 admin」，Gate 只能靠猜。
2. **首启只能 SSH**。`ensure_bootstrap_admin()` 播种了一个 passwordless `admin`，却没有任何 HTTP 路径给它设密码；Desktop 的 `SetupRequiredView` 只是一张「去主机上跑命令」的说明页，引导流程是断的。

## 2. 决策

### 2.1 账号系统恒开，没有 mode

`AUTH_MODE` / `auth_mode` / `--auth-mode` **全部删除**。`acowork-user` 无条件构造 `AuthService`、无条件写 `auth/ed25519.key`、无条件注册 `/api/auth/*` 与 `/api/users`；Gateway 无条件加载 verifier、无条件反代用户域。

用户可选的只是**账号数量**：一个 admin，还是再建几个账号。这是运行时的账号管理动作，不是部署模式。

### 2.2 三态取代二值布尔

`acowork_core::account::AccountSystemState`（`acowork-core/src/account.rs`）：

| 状态 | 含义 | Gateway 提供 |
|---|---|---|
| `uninitialized` | 无账号存储 / 空存储 | setup 面 |
| `needs_setup` | 有账号但没有可登录的 admin（典型：已播种 passwordless `admin`） | setup 面 |
| `ready` | 至少一个 admin 有真实 Argon2 hash 且未禁用 | 全部 API |

`requires_setup()` = `state != Ready`。判据只来自 `accounts.json` 内容，不看配置、不看 bind。`GET /api/status` 用 `account_state` 字段下发；user 服务的 `/health` 也带该字段，Gateway supervisor 轮询后写入 `UserSnapshot`，`restricted_mode_middleware` 每请求 O(1) 读快照。

**「没有快照」与「快照内容不可信」必须分开处理**（code review 后的修正）：

- `user_snapshot == None`（`/health` 还没回来）：fail-open 为 `ready`。该窗口里 `user_process` 也未发布，`user_proxy` 对非公开路径回 503，谎报 `setup_required` 只会把 Desktop 错误地送进首启向导。
- `/health` 返回了 details 但 `account_state` 缺失 / 值未知：`parse_account_state()` **fail-closed** 为 `needs_setup` 并打 warn。旧的 fail-open `ready` 理由（"gate 还要 token，而 admin 存在之前不可能有 token"）在 ADR-114 下已经失效——setup 本身就是一条无 token 就能创建 admin **并签发首对 token** 的路径，静默放行等于把首启管理员交给任意能连到端点的人。
- `UserSnapshot::default()` 也改为 fail-closed（仅用于测试里的结构体更新语法，生产不会构造默认快照）。

判据统一：`AccountSystemState::from_accounts()` 复用 `UserAccount::is_login_capable()`（`disabled_at` + Argon2id PHC 形状校验），删除 user 服务里第二套 `requires_setup()` 自由函数，避免 `/health` 与 CLI `--check` 对同一个 store 得出相反结论。

### 2.3 bind 与 peer 是两件事（关键）

> **bind 地址回答「监听器能否被外部访问」；peer 地址回答「这个请求是否来自本机」。只有后者能用于信任判定。**

- 网关是 `127.0.0.1` 还是 `0.0.0.0`，不影响账号系统是否存在，也不影响首启状态。
- `POST /api/auth/setup` 只接受**真实 peer 为 loopback** 的请求：`restricted_mode_middleware` 从 `ConnectInfo<SocketAddr>` 取源地址，调用 `is_loopback_peer()`。**该判定在函数最前面无条件执行**，不能被 `account_state == Ready` 的早退短路——快照是轮询得来的、可能滞后或不可信，而 peer 不会。即使状态快照谎报 `ready`，远端也拿不到 setup 端点。
- 两个 TCP listener 都用 `into_make_service_with_connect_info` 挂载，因此生产环境每个请求都有 `ConnectInfo`；缺失即**fail-closed**（进程内调用没有可证明的网络来源，不得引导首个管理员）。
- `is_loopback_peer()` 除 `127.0.0.0/8` 与 `::1` 外还识别**严格 IPv4-mapped** 形式 `::ffff:127.0.0.0/8`：双栈监听器会把本机 IPv4 连接报成 IPv6 socket 地址，只调 `IpAddr::is_loopback()` 会把它误判为远端、把本机 Desktop 锁在 setup 之外。已废弃的 IPv4-compatible `::a.b.c.d` 以及映射后的可路由地址一律不信任。
- `X-Forwarded-For` 一类可伪造头一律不参与判定。
- `0.0.0.0` bind 上的本机连接，其 peer 仍是 `127.0.0.1`，因此本机 Desktop 依旧可以完成首启；远端客户端非 loopback，得到 403 `setup_local_only`。

### 2.4 首启引导与既有播种流程合并

`POST /api/auth/setup`（公开、无 token）请求体 `{username, password, display_name?}`，响应 `TokenPair`：

1. `AuthService::setup_first_admin()` 持 `setup_lock` 完成 load→check→mutate→save。
2. 先判 `AccountSystemState`：`Ready` 直接 409 `already initialized`，**服务自身就是权威**，即使 Gateway 的健康快照还滞后一轮也拦得住第二次 setup。
3. 若 store 里已有 `ensure_bootstrap_admin()` 播种的 passwordless `admin`，**原地激活它**（改名 / 改 display_name / 写 Argon2 hash / 清 invite 与 disabled 标记），不新建第二个 admin、不因用户名冲突 409。空 store 才走 `create_account(Role::Admin)`。
4. 走 blocking 池（Argon2id ~100ms），成功后 `sync_profiles()` 刷新派生视图，并 `mint()` 签发 token pair，Desktop 免二次登录。
5. 密码策略、用户名校验复用 `PasswordPolicy` / `is_username_byte`，失败 422 且不写盘。

CLI 路径（`admin-setup`、TTY prompt、`[bootstrap_admin]` 预置）**全部保留**为无头 / 脚本化部署的备选；配置了 `bootstrap_admin` 的部署启动即 `ready`，setup 端点返回 409。Desktop 的 `SetupRequiredView` 从「SSH 说明页」改为可提交的表单，同时保留轮询，以便主机上用 CLI 完成 setup 时自动继续。

### 2.5 relay 隧道：peer 判定不够，必须按 origin 拒绝

`relay/remote_listener.rs` 绑定 `127.0.0.1:{remote_http_port}`，是隧道客户端的唯一消费者 —— **隧道进来的远程请求，其 peer 同样是 loopback**。只靠 §2.3 的 peer 判定会误放行 setup。

因此 `remote_origin_policy` 的 deny list 显式加入 `/api/auth/setup`（所有 method → 404，不暴露路径存在）。这里信任锚是**监听器身份**，不是地址：远程面永远不允许创建第一个管理员。

### 2.6 中间件层序

`restricted_mode_middleware` 位于 CORS 内、auth middleware 外（axum 自底向上执行）：无 token 的首启请求应得到 403 `setup_required`（带 `account_state`），而不是 auth gate 先给出的 401。`SETUP_PATH` 同时在 Gateway 与 user 服务两侧的 `is_public_path` 白名单中。

函数内部顺序同样是安全契约：`SETUP_PATH` 的 peer 判定 → `Ready` 早退 → `ALWAYS_PUBLIC` → setup 放行 → 其余 403。把 peer 判定放到 `Ready` 之后会让一个 stale / fail-open 的快照直接决定「谁能创建第一个管理员」，这是本次 review 修掉的 P0。

## 3. 接口变更

| 旧 | 新 |
|---|---|
| `--auth-mode` / `auth_mode = "multi_user"` | 删除（无兼容分支） |
| `/api/status.requires_setup: bool` | `/api/status.account_state: "uninitialized" \| "needs_setup" \| "ready"` |
| `/api/status.auth_mode` | 删除 |
| Desktop `AuthMode = "local" \| "multi_user"` | `AccountSystemState` |
| `authStore.mode` / `AuthState."disabled"` | `authStore.accountState` / 删除 `"disabled"` |
| 首启无 HTTP 路径 | `POST /api/auth/setup` → `TokenPair` |
| `SetupRequiredView` 纯提示 | 用户名 / 显示名 / 密码表单 + `completeSetup()` |
| Mobile `GatewayStatus.auth_mode` + `requires_setup` | `GatewayStatus.account_state`（`needsSetup()` 派生）；设置页「模式」改为「账号系统」状态 |

`AuthService` 从 `Option<Arc<AuthService>>` 改为 `Arc<AuthService>`；Desktop 的 `selfProfile.ts`、`authFetch.ts`、PM/Doc 的 `localMode` 治理豁免，以及 Mobile `authStore` 的 `auth_mode === "local"` 直通分支等「账号系统不存在」分支全部删除。

## 4. 数据与迁移

项目未上线，**正式代码不含任何兼容 / 迁移分支**。

- `ensure_bootstrap_admin()` 的 passwordless `admin` 不是脏数据，它就是 `needs_setup`，由 setup 端点原地激活。
- 旧 `local` 安装若存在 `user_profiles.json` 而无 `accounts.json`，新代码会以空账号列表重建派生视图，不会崩；需要把旧单用户档案和 `user_id = "human"` / `None` 的 session owner 折算到首个 admin 时，使用一次性脚本 `scripts/migrate_legacy_local_data.py`（默认 dry-run，带备份，不参与任何构建）。

## 5. 验证

- `cargo check --workspace --all-targets`：通过、零 Rust warning（除环境层的 ONNX Runtime 提示与 Windows 增量编译锁 `os error 5` 提示）。
- `acowork-user` 单测：`setup_creates_the_first_admin_and_is_one_shot`、`setup_activates_the_seeded_admin_in_place`、`setup_enforces_username_and_password_policy` 等通过。
- `acowork-gateway` 单测：`restricted_mode::tests`（7 例，含 `127.0.0.1` / `0.0.0.0` 两种 bind 下 loopback 放行、远端拒绝）、`remote_listener::tests::policy_never_exposes_first_boot_setup`、`auth::mode::tests`（4 例，含 IPv4-mapped loopback 正例与映射可路由地址 / IPv4-compatible 形式的负例）通过。
- **进程级 e2e**（真实 spawn Gateway + user service，串行 `--test-threads=1`）：
  - `account_state_e2e.rs` 4/4：全新安装 → 403 `setup_required` → 本机 POST setup → token 可用 → 二次 setup 409；`[bootstrap_admin]` 预置 → 启动即 `ready` → setup 409；loopback-only bind 依然创建账号存储与签名密钥；`0.0.0.0` bind 不改变首启状态。
  - `chat_sessions_e2e.rs` 14/14：原「local mode 拒绝 chat 写入」用例重写为 `fresh_install_gates_chat_until_first_admin_setup` —— setup 前 chat 面 403 `setup_required`，本机 setup 后无 token 写入 401（身份 gate 生效）、会话面可达、二次 setup 409。
  - `user_proxy_e2e.rs` 3/3：原「local mode 下 login 返回 404」用例重写为 `fresh_install_gates_the_proxy_until_setup_then_serves_users` —— 路由恒注册，首启被 gate 403，setup 后 login 不再是 404，且 setup 签发的 token 可访问 `/api/users`。
  - `route_confirm_e2e.rs` 1/1；`route_confirm_live_e2e.rs` 1/1（首次批量运行时偶发 `timed out waiting for the create decision with a target session`，单独重跑 5s 通过，属 live Runtime + 脚本 provider 的时序抖动，与账号系统无关）。
- Desktop：`npx tsc --noEmit` 无本次改动引入的错误（余下 9 项为环境缺失的 `docx-preview` / `jszip` / `xlsx` / `pdfjs-dist` 依赖）；`npm run check:i18n` 通过（1962 处调用点）；受影响 4 个测试文件 57/57 通过；vitest 全量 3348 例中 3343 通过，5 例失败全部来自未被本次改动的 `GroupChatPanel.blockRef.test.tsx`（单独运行同样失败，属既有问题）。
- Mobile：`tsc --noEmit` 通过；vitest 432 passed / 2 skipped（`auth-poll.test.ts` 新增 `uninitialized → restricted` 与「未知状态 fail-open」用例）。
- 全仓源码扫描：`resolve_auth_mode` / `AuthMode::` / `is_local_mode` 等旧逻辑分支为零；保留的 `AUTH_MODE` 字样仅出现在 ADR-114 的废除说明、反兼容测试输入与「该字段不存在」的断言中。

### 5.1 实现后 code review 复核（本轮）

对整份 diff 逐条 double check。**确认为真问题并修复**：

| 级别 | 问题 | 修复 |
|---|---|---|
| P0 | `restricted_mode_middleware` 在 `Ready` 分支提前 return，短路了 `/api/auth/setup` 的 peer 判定；配合 fail-open 的 `/health` 解析，远端可在新装上抢注首个 admin | peer 判定提到函数最前、**无条件**执行；`ConnectInfo` 缺失改为 fail-closed；新增 3 例回归测试 |
| P0 | `parse_account_state()` 对缺失 / 未知 `account_state` fail-open `ready`，理由（"token 不可能早于 admin 存在"）被 setup 端点推翻 | 改为 fail-closed `needs_setup` + warn；`UserSnapshot::default()` 同步 fail-closed；新增 2 例测试 |
| P1 | user 服务私有 `requires_setup()` 与 `AccountSystemState::from_accounts()` 对 `disabled_at`、损坏 hash 的判据不一致 | 删除第二套判据；`from_accounts()` 复用 `UserAccount::is_login_capable()`（含 Argon2id PHC 形状校验）；`is_restricted()` / `store_requires_setup()` 统一走同一函数；新增 2 例测试 |
| P1 | `/health` 持 `RwLock` read guard 做同步读盘（Gateway 每 2s 轮询，会阻塞写者） | 锁内只复制小字段，读盘移到锁外 |
| P1 | `sync_profiles()` 重载 `accounts.json` 失败时静默 return | 改为 `tracing::error!` 记录并说明恢复路径；**不**升级为 HTTP 500（账号已写入，setup 重试会 409，反而更糟） |
| P2 | Desktop 硬编码密码策略 / 所有 403 都提示「仅限本机」/ `completeSetup()` 超时后留下 `logged_in + account=null` | 策略交回服务端 422 原文；按 error code 精确匹配 403；只有 `/me` 成功才置 `logged_in`，否则清会话落登录页 |
| P2 | Mobile 对未知 `account_state` fail-closed，与 Desktop 相反 | 统一为只认 `uninitialized` / `needs_setup`，其余 fail-open；新增测试 |
| P2 | 迁移脚本 `owner` 字段缺失时不回填；有普通账号但无 admin 时把旧会话错挂到该账号 | 缺失 / `null` 一并回填；无 admin 时拒绝猜测并提示先完成 setup |
| P2 | `dev/ci.sh` 仍强制「账号路由必须挂在 `is_multi_user()` 分支下」；`SECURITY.md` / runbook 仍把 `AUTH_MODE` 写成现行配置 | redline 改为「路由恒注册」；安全模型与 runbook §0–§2 按 ADR-114 重写 |
| P2 | 首启向导的 i18n key 只有 `defaultValue`、中文界面实际显示英文 | en / zh-CN 补齐 9 个 key；状态相关正文拆成两个 key |

**复核后判定不是问题、未改**（附理由，避免以后反复提出）：

- `GatewayConfig` 不能加 `deny_unknown_fields`：它带 `#[serde(flatten)] timeouts`，serde 不支持该组合；且 `config.rs::the_deployment_auth_mode_knob_is_gone` 已明确钉住「残留 `auth_mode` 只是未知键，不影响行为」。
- `remote_origin_policy` 把 setup deny 放在 `has_node_token` 之后是有意的：带 node token 的**任何**路径都应答 403，改成 404 会削弱「内部机器身份不得走远程通道」的不变量，也与 `/api/debug/*` 的处理一致。
- `setup_first_admin()` 只复用 sentinel admin 是刻意的：它要原地激活 `ensure_bootstrap_admin()` 播种的那一个；已有真 hash 但被禁用的 admin 另建一个账号是合理恢复，不是缺陷。

复核后验证：`cargo check --workspace --all-targets` 通过；`acowork-core` 14/14；gateway 单测 `restricted_mode` 10/10、`user_supervisor` 2/2、`auth::mode` 4/4、`remote_listener` 5/5；`acowork-user --lib` 239/239；进程级 e2e `account_state_e2e` 4/4、`user_proxy_e2e` 3/3、`chat_sessions_e2e` 14/14；Desktop `authStore.test.ts` 18/18、`check:i18n` OK、`tsc` 仅剩 9 项既有环境依赖错误；Mobile typecheck + `auth-poll.test.ts` 25/25；CI redline 语法与匹配条件验证通过；迁移脚本 dry-run / apply / 幂等 / 无 admin 场景冒烟通过。

### 5.2 实机首启复核（2026-11-14）：全新安装无法完成首次启动

删掉 `.acowork` 后由 Desktop spawn Gateway 的真机验证暴露三处叠加缺陷，任意一处单独都足以让首启卡死在 splash 超时页（表现为「start gateway 超时 → 恢复页 → Connect 仍失败」，永远看不到登录/引导界面）：

| 级别 | 问题 | 修复 |
|---|---|---|
| P0 | `cli.rs` 首启交互提示没有 daemon 豁免：Desktop 从 `npm run tauri dev` 的终端 spawn 子进程，stdin/stdout 继承真 TTY，`can_prompt_interactively()` 判真 → `rpassword::prompt_password` 永久等待一个无人可见、无人可输入的密码。Gateway 日志仅一行（`Gateway::new` 最后一条 INFO）、不 bind 端口、单线程零 CPU | daemon 恒不进交互提示（`self.daemon` → 直接 `warn_first_boot_restricted()` 后继续受限启动）；Desktop spawn 侧加 `stdin(Stdio::null())` 切断 TTY 继承，双保险 |
| P0 | `restricted_mode_middleware` 的 `ALWAYS_PUBLIC` 漏了 `/api/bootstrap`：首启态 boot 探针被 403 `setup_required` 拦截，`wait_for_bootstrap_ready` 死等 30s 超时——**即使 Gateway 正常启动**，Desktop 也到不了首启向导 | `/api/bootstrap` 加入 `ALWAYS_PUBLIC`（受限态各子系统照常就绪，实测 2s 内 phase=READY；信息暴露面与 `/api/status` 等价，无新增面） |
| P1 | Desktop `child_output_is_alive()` 以 `child.id() > 0` 判存活恒真：卡死/已退出的子进程永远不被重新 spawn，重试全部命中「already running, skipping spawn」，无自愈路径 | 改用 `try_wait()`（`Ok(None)` = 真存活）；handle 陈旧即清空、落回 probe→spawn 主链路 |

修复后实机验证：新二进制 daemon 模式 ~2s 内 `/health` 应答、`0.0.0.0:19876/19875` 监听、`/api/bootstrap` 返回 `phase=READY`（3/3 required）、`/api/status` 报 `account_state=needs_setup`、`/api/auth/setup` 可达；`restricted_mode` 单测 10/10（含新增的 bootstrap 公开断言）；desktop `cargo check` 干净。

### 5.3 实机首启复核二轮（2026-11-14）：boot 修复后落到登录页而非设置页

§5.2 修复后实机复测：boot 链路已通（Gateway 起来、`/api/bootstrap` READY、进到 UI），但首启落在**登录页**且登录报错原文 `setup_required`——无账号可登录的机器不该出现登录表单。

根因是**时序竞争**：bootstrap READY 的 required 子系统不含 user-supervisor 快照，Desktop 的 `init()` 在 READY 后立刻 probe `/api/status`，撞上快照未就绪窗口时该端点 fail-open 报 `ready` → 无 session → 落 `logged_out`；用户登录则被 restricted gate 以 403 `setup_required` 拒绝，Desktop 只把错误码原文显示出来。

| 级别 | 问题 | 修复 |
|---|---|---|
| P1 | `/api/status` 在 user-service 快照未就绪时 fail-open `ready`（routes.rs）。`uninitialized` 才是"还没从 user service 听到任何消息"的诚实表述；误报（极短暂的窗口）由 setup 向导的完成 poll 在一个轮询周期内自愈，而误报 `ready` 会把首启引进登录死胡同 | fail-closed：快照 `None` → `uninitialized`；测试同步更新 |
| P1 | Desktop `login()` / `firstLogin()` 对 403 `setup_required` 只显示错误原文。这是决定性兜底：无论 probe 竞争如何，登录尝试本身就该把用户送进首启向导 | 精确匹配 `status === 403 && error === "setup_required"` → 置 `needs_setup`/`setup_required` + 启动完成 poll，不再显示原文 |

Desktop 回归测试新增 2 例（403 转换 + 其余 403 仍显原文），`authStore.test.ts` 20/20；gateway `http::routes` 14/14。

### 5.4 code review 复核三轮（2026-11-14）：恒开 bearer gate 引入 34 例单测回归

全量 review 累积改动时发现：上轮（§5.1 前）实现把 `auth_middleware` 恒开化后，Gateway 单测从基线 5 例既有失败劣化到 39 例——**34 例是本次改动引入的回归**，全部集中在 6 个 proxy / preview 测试文件（memory_proxy 10、pm_proxy 7、doc_proxy、user_proxy、workspace_preview、permission）。共性根因：

- 这些测试用生产 `routes::build_router`（挂全套 middleware），state 里 `user_verifier = None` → 恒开 gate 应答 503（"service starting" 窗口）；§5.1 修法给 state 加了 fixture verifier，于是匿名请求又变成 401。
- 基线时这些测试靠「local mode 直通」绕过 gate，ADR-114 删除该旁路后语义悬空。

**修复模式**（与基线「匿名放行」语义完全一致，断言原样保留）：测试 state 加 `internal_service_token = Some("test-svc-token")`，请求加 `Bearer test-svc-token`——即 auth gate 的 machine-actor 分支（设计内：内部服务回环调用本就无用户身份）。

两例需要单独判断：

- `workspace_preview::the_mint_route_is_registered_under_the_path_the_desktop_calls`：同法修复；注释「Local mode: the bearer gate is a no-op」已过时，改为说明请求以 internal service（machine actor）身份认证。
- `permission::ownership_is_always_enforced`（原 `local_mode_noop`，§5 时已改名重写为期望 404）：无 AuthContext 请求在 permission 层直接放行——**这不是安全回归**。该放行分支是基线既有代码，语义是「machine actor（internal service token / node token）或已被 auth gate 应答的路径；ownership 是用户概念，逐路由凭证检查仍然生效」。生产中匿名请求到不了 permission 层（恒开 gate 先 401）。修法：测试挂完整生产栈（auth_middleware + permission_middleware）、以 `test_support::access_token` 签发的合法 token 认证为非 owner 用户 `bob`——「local mode 移除」的回归钉子由真实身份路径钉住，machine-actor 放行为设计语义（已在 ADR 及代码注释中写明）。

复核后 gateway 单测 **911 passed / 5 failed**，5 例失败与基线完全一致（embedding_api、lsp_admin_proxy、proxy×2、probe::runner，均为环境依赖类：需要真实 Runtime / 外部服务）。


## 6. 后果

**收益**：改 bind / 开 relay / 换网络拓扑不再影响登录；首启状态显式可判定且可被 UI 消费；新装机器不再需要 SSH 或 TTY 即可完成引导；攻击面收敛到「真实 loopback peer + 非 relay origin」。

**代价 / 注意**：
- 所有请求现在恒定带 bearer（有 session 时），`authFetch` 不再有「local 直通」短路；无 token 时 `acquire()` 返回 `null`，行为不变。
- Gateway 的 `account_state` 快照由 `/health` 轮询驱动，最多滞后一个轮询周期（2s）。因此 setup 成功后 Desktop 的 `completeSetup()` 会等 `/api/status` 变 `ready` 再发 `/api/auth/me`，避免撞上尚未撤销的 403 gate。只有 `/me` 真正成功后才置 `logged_in`；等不到或 `/me` 重试仍失败时丢弃本地会话、落到登录页并给出提示，不再留下 `logged_in` 但 `account === null` 的半成品状态（第二次 setup 会 409，向导不是恢复路径）。
- Desktop 首启表单不再复制一份服务端的 `PasswordPolicy`（服务端可配）；本地只校验非空与两次输入一致，策略违规直接展示后端 422 的原文。403 也只有 `error === "setup_local_only"` 才显示「仅限本机」提示，其余状态码展示真实原因。
- `/api/auth/setup` 在 user 服务自身也是公开的；其信任边界是「user 服务只绑 loopback + Gateway 层已判真实 peer」，与既有 `/internal/*` 一致。
- Mobile 从远端连入，peer 非 loopback，因此**不能**完成首启 setup（403 `setup_local_only`）；它的 `restricted` 页只做引导，首启仍须在 Gateway 本机通过 Desktop 表单或 CLI 完成。
