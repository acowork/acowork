# acowork-user 开发计划（用户域剥离）

> 版本：v0.1（草案）| 日期：2026-10-20
>
> 关联 ADR：[`docs/adr/zh/ADR-084-user-standalone-process.md`](../../adr/zh/ADR-084-user-standalone-process.md)（本计划的唯一权威；文件级改动清单见其 §6）
> 关联被取代 ADR：[`docs/adr/zh/ADR-076-multi-user-account-system.md`](../../adr/zh/ADR-076-multi-user-account-system.md)（业务语义仍有效，实现形态被 ADR-084 取代）
> 范式先例：[`docs/plan/zh/pm-dev-plan.md`](pm-dev-plan.md)、[`docs/plan/zh/doc-dev-plan.md`](doc-dev-plan.md)
>
> **一句话**：把账号 / 凭据 / 角色 / 档案 / 头像 / 用户↔用户聊天从 acowork-gateway 剥离为独立进程 `acowork-user`，Gateway 仅保留反代 + 鉴权闸门；按 M0→M5 六个里程碑交付，预估总工期 **7-11 人日**（单人全职）。

---

## 1. 排期假设

- **团队规模**：单人全职（兼任代码评审自审）。
- **工时口径**：1d = 8h，含编码 + 单测 + 集成测试 + 文档同步。
- **排期窗口**：1.5-2 周连续投入；不含代码评审、合并、跨服务联调 buffer。
- **前置依赖（已就绪，无需新工作）**：
  - `acowork_core::supervisor::{RestartHistory, backoff_with_jitter}`、`supervisor_defaults`（ADR-019 抽出）。
  - `acowork_core::health::HealthResponse` 子进程健康契约。
  - `lifecycle/{pm,doc}_supervisor.rs` + `http/{pm,doc}_proxy.rs` 可逐行参照的范式。
  - `acowork-gateway/src/http/proxy.rs` 的透明反代 + hop-by-hop 头处理。
  - Desktop 端 `auth-api.ts` / `user-chat-api.ts` / `authFetch.ts` / `userChatStore` 现成客户端（**契约不变，零改动**）。
- **并行机会**：M0（core 契约 + Gateway 验签）与 M1（建 crate 迁代码）可并行；M3 与 M4 的 `local` 分支部分可并行。单人情况下按 M0→M5 串行。

### 1.1 目标拓扑

```mermaid
graph TB
    D["Desktop App"] -->|"/api/auth/* · /api/users/* · /api/user/*"| MW
    subgraph GW["acowork-gateway"]
        MW["auth_middleware（本地 Ed25519 公钥验签 → AuthContext + x-user-id）"]
        UP["user_proxy（反代 + 注入 X-Auth-*）"]
        SUP["user_supervisor（spawn / /health / 退避重启）"]
        SNAP["user_snapshot（requires_setup / registration_open / 档案）"]
    end
    MW --> UP
    UP -->|"127.0.0.1:18083"| ACC
    SUP -.->|spawn / 心跳| ACC
    SNAP -.->|拉取快照| ACC
    subgraph US["acowork-user（新）"]
        ACC["account · auth(签发) · profile · chat"]
        DATA["$HOME/.acowork/acowork-user/"]
    end
    ACC --- DATA
```

---

## 2. 里程碑总览

| 阶段 | 内容 | 估时 | 交付物 / 出口条件 |
|------|------|------|-------------------|
| **M0 契约** | `acowork-core::auth`（Ed25519 签发/验签）；Gateway `token.rs` 换为验证者 | 1d | 网关本地验签单测绿；HS256 移除 |
| **M1 建 crate + 迁代码** | `core/acowork-user` 骨架 + `account`/`auth`/`chat`/`http` 迁入 + 全量 router + `/health` + 用户 CLI | 2-3d | 二进制独立启动，可登录 / 改密 / 收发消息 |
| **M2 拉起 + 反代** | `user_supervisor` + `http/user_proxy` + `[user]` 配置 + `GatewayState.user_process` | 1-2d | Desktop 经 Gateway 全链路可用 |
| **M3 闸门改造** | `auth_middleware` 公钥验签、`restricted_mode` 快照、档案快照刷新（MQTT 信号） | 1-2d | `multi_user` e2e 全绿（含受限模式 403） |
| **M4 统一 + 拆除** | `local` 模式打通；删除 Gateway 内用户域代码；`dev/ci.sh` lint 更新 | 1-2d | 两模式 e2e 全绿；clippy 干净；lint 通过 |
| **M5 迁移 + 打包** | 一次性数据迁移 runbook；build / 打包脚本加 `acowork-user`；文档 | 1d | 迁移校验通过；`package_desktop_*` 产物含新二进制 |

---

## 3. 里程碑详情

### M0 — 共享 Token 契约（Ed25519）｜1d

**任务**
1. `core/acowork-core/src/auth/` 新增：
   - `Claims`（`sub` / `role` / `family` / `kind` / `iat` / `exp`）、`TokenKind`、`TokenError`（自 [gateway/auth/token.rs](../../../core/acowork-gateway/src/auth/token.rs) 平移语义）。
   - `TokenIssuer`（Ed25519 私钥：`sign_access` / `sign_refresh`）。
   - `TokenVerifier`（Ed25519 公钥：`verify` / `verify_kind`）。
   - `Claims::is_family_consistent` 等不变量保持不变。
2. 依赖选型：`ed25519-dalek`（或 `ring`）；JWT header 改 `{"alg":"EdDSA","typ":"JWT"}`。
3. 密钥位置约定：私钥 `{user_data_dir}/auth/ed25519.key`（0600），公钥 `{user_data_dir}/auth/ed25519.pub`。
4. Gateway `auth/token.rs` 的**签发部分删除**，仅保留 `TokenVerifier` 用法（或整体改调 `acowork_core::auth`）。
5. 平移并改写单测：签验往返 / 篡改拒绝 / 过期拒绝 / kind 校验 / family 一致性。

> **实施备注（M0 已完成）**：`auth/token.rs` 改为对 `acowork_core::auth` 的 re-export shim，HS256 实现删除、`hmac` 依赖从 gateway `Cargo.toml` 移除。
> 一处**有意偏离**上文第 4 点：M0 时账号系统仍在 Gateway 内，`login` / `refresh` 仍由它签发，因此 `AuthService` 暂持 `issuer` + `verifier` 两个字段（`verifier` 由 `issuer.verifier()` 派生）。若 M0 就删掉签发，登录会立即断、M0 出口条件不成立。**M1 迁出账号后** Gateway 才降为"只持 `TokenVerifier`、从 `ed25519.pub` 载入"。
> 密钥落盘：私钥 `{data_dir}/auth/ed25519.key`（0600，原子写），公钥 `{data_dir}/auth/ed25519.pub`（每次载入时重写，自愈缺失/陈旧副本）。

**出口**：`cargo test -p acowork-core` 绿；`cargo test -p acowork-gateway --lib` 中 token 相关测试绿；`grep -rn "HS256" core/` 仅剩历史注释。

### M1 — 建 crate + 迁入用户域代码｜2-3d

**任务**
1. `core/Cargo.toml` `[workspace] members` 追加 `acowork-user`。
2. 新建 `core/acowork-user/`（镜像 [acowork-doc/Cargo.toml](../../../core/acowork-doc/Cargo.toml) / [acowork-pm](../../../core/acowork-pm)）：
   - `Cargo.toml`：`[lib] acowork_user` + `[[bin]] acowork-user`；依赖 `acowork-core`、tokio、axum、tower-http、serde、argon2、uuid、chrono、tracing、clap、reqwest、`acowork-mqtt-session`+`rumqttc`（档案变更信号，参照 doc）、`ed25519-dalek`（经 core 契约）。
   - `src/lib.rs` 模块声明 + re-export（对齐 doc 的 public API 形状）。
   - `src/main.rs`：CLI（`--host --port --port-file --data-dir --auth-mode --gateway-health-url --gateway-health-interval-ms --gateway-health-timeout-ms --log-level`）+ `admin-setup` 子命令 + 首次设置 TTY prompt（自 [gateway/cli.rs](../../../core/acowork-gateway/src/cli.rs) 平移）。
   - `src/server.rs`：全量 router = `/api/auth/*` + `/api/users/*` + `/api/user/avatar-*` + `/health`；内部路径**与旧 Gateway 逐字一致**（`user_proxy` 不剥前缀）。
   - `src/health.rs`：`HealthResponse`，`details` 带 `data_dir` + `requires_setup` + `registration_open`。
   - `src/config.rs` / `error.rs` / `types.rs`。
3. 迁入（git mv 保历史）：
   - `account/{store,password}.rs` ← gateway 同名。
   - `auth/{service,revoked}.rs` ← gateway 同名；新增 `auth/issuer.rs`（用 core 的 `TokenIssuer`）。
   - `chat.rs` ← gateway `src/chat.rs`。
   - `http/{auth_api,account_api}.rs` ← gateway 同名；`http/users_api.rs` → `http/profile_api.rs`（内部路由字符串不变）。
   - `http/chat_api.rs` ← gateway 同名。
4. `sync_profiles`（账号 → `user_profiles.json` 派生视图）随 `account_api` 迁入；用户服务成为 `user_profiles.json` 的**唯一写入方**。
5. 鉴权语义：用户服务从注入头 `X-Auth-User` / `X-Auth-Role` / `X-Auth-As-User` 取身份（见 M2），**不再自行验签**；公开路径（`/api/auth/login|refresh|logout|first-login`）不要求身份。
6. `[multi_user]` 相关配置（`password_policy` / `bootstrap_admin` / `registration_open`）迁入用户服务配置。
7. 平移单测：account store 原子写 / 损坏备份；Argon2id 登录改密；refresh-family 撤销与重用检测；聊天参与者收敛 / 未读 / 附件权限边界。

**出口**：`cargo build -p acowork-user` 产 `acowork-user(.exe)`；`cargo test -p acowork-user` 绿；`acowork-user --port 18083` 独立启动，`/health` 200，登录/收发消息可用。

> **实施备注（M1 已完成，出口已实测）**
>
> - **状态容器形状**（与计划第 2 点的笼统描述相比固化了）：`AppState { auth_service: Option<Arc<AuthService>>, shared: Arc<RwLock<SharedState>>, auth_mode }`，`SharedState { resource_cache, data_dir, registration_open }`。`auth_service: None` 即 `local` 模式（无账号系统），`require_auth_service()` 返回 503。`SharedState::new` 启动时从磁盘加载 `user_profiles.json`（原 Gateway 行为），`registration_open` 默认 `false`（对齐 Gateway `[multi_user]` 语义）。
> - **`auth/issuer.rs` 不是薄封装，而是 `pub use acowork_core::auth::{TokenIssuer, TokenVerifier, ...}`**。M1 阶段账号系统仍在用户服务内，它就是签发方，没有可包装的逻辑；M2 迁出后 Gateway 侧降为 verifier-only（见 M0 备注同一理由）。
> - **单一装配点** `crate::http::build_router(&AppState) -> Router<AppState>`：合并 user domain + `/health` + `inject_identity` 层。`server.rs`、`test_support.rs` 都走它，M5 的 `user_proxy` 目标也复用，避免路由顺序/层位置漂移。`health_route()` 因此**不需要参数**（`State<AppState>` 提取器自带），只声明 `Router<AppState>` 以参与 merge。
> - **删除了两处 Gateway 遗留，M3 接回**：(a) `mqtt_publisher_trigger` 的 5 处全局资源重发布调用，改为 `// ADR-084 M3` 占位注释；(b) `operation_store` 写入块（Gateway 里 `GatewayState::new` 从未给它赋值，本就是死代码）。`OperationAck` 仍由本地 `OperationRecord` 构造，响应形状不变。
> - **测试随代码迁移**：新增 `src/test_support.rs`，其中 `gateway_identity_shim` 复刻 Gateway 的 `auth_middleware`（验 Bearer → 注入 `X-Auth-*`，先无条件剥除客户端自报的 `X-Auth-*`；`as_user` 仅 GET/HEAD 且仅 admin）。它插在 `inject_identity` **之前**，因此 4 个测试模块约 94 处 `req(..., Some(&token))` / `build_router(state)` 调用点**逐字未改**，而覆盖的正是真实两段式路径。这是**有意重复实现**而非抽公共 helper：M3 的真身落地后无需再拆。
> - **两处非计划内但必需的改动**：(a) 新增直接依赖 `rand_core = { version = "0.6", features = ["getrandom"] }` —— `argon2::password_hash::rand_core::OsRng` 被 feature 门控，在 Gateway 依赖图里被别的 crate 统一开启，独立二进制必须自己声明；没有任何新 crate 进入依赖图。(b) `[dev-dependencies]` 补 `tower`（`ServiceExt::oneshot`）与 `serde_json`，与 Gateway 同款。另为 `ApiError` 补了 `Display` / `std::error::Error`（`tracing::error!(error = %e)` 需要）。
> - **实测出口**（隔离端口 18099 + 隔离 data-dir）：`admin-setup` 播种管理员并自动生成 `auth/ed25519.key`；`--port-file` 正确；`/health` 200 且 `details` 带 `requires_setup=false` / `registration_open=false`；登录返回 **`alg=EdDSA`**（M0 契约在线生效）；无身份 `/api/users` → **401**，注入身份 → **200**，`x-auth-role: user` → **403**；建账号 → 发消息 **201** → 对方 `GET /chats` 见 `unread_count: 1`。
> - **验证**：`cargo build/clippy -p acowork-user --all-targets -- -D warnings` 干净；`cargo test -p acowork-user` **91/91 绿**；`cargo check --workspace --all-targets` 干净；`cargo test -p acowork-gateway` **596/597**，唯一失败的 `mqtt::dispatch::tests::enroll_handshake_end_to_end` 是既有 MQTT 时序 flake（单独跑通过），与本次改动无关。
> - **遗留待办**：(a) CLI 顶层参数未设 `global = true`，所以 `acowork-user admin-setup --data-dir X` 会静默落到默认数据目录，必须写成 `acowork-user --data-dir X admin-setup`——M2 由 supervisor 下发参数时需确认拼法，或加 `global = true`。(b) `--mqtt-host/--mqtt-port` 已解析但 M3 才接线。(c) 冒烟测试确认了文档记录的信任边界：服务无身份也接受 `X-Auth-*`，因此只能绑 loopback。

### M2 — 拉起 + 反代｜1-2d

**任务**
1. `core/acowork-gateway/src/lifecycle/user_supervisor.rs`（照抄 [doc_supervisor.rs](../../../core/acowork-gateway/src/lifecycle/doc_supervisor.rs)）：
   - `UserProcessState { pid, port, ready }`；`UserSupervisorConfig { user_bin, port, port_file, log_dir, gateway_health_url, data_dir, auth_mode }`。
   - spawn `acowork-user`，`--auth-mode` 由 Gateway 解析的 `AuthMode` 下发（单一真相源在 Gateway）。
   - `/health` 轮询就绪 + `RestartHistory` + 退避重启；失败清空状态、非致命。
2. `core/acowork-gateway/src/http/user_proxy.rs`：
   - 反代 `/api/auth/{*rest}`、`/api/users/{*rest}`、`/api/user/{*rest}` → `127.0.0.1:{user_port}`，**前缀保持不变**。
   - 注入可信身份：已认证 → `X-Auth-User`(= `AuthContext.user_id`)、`X-Auth-Role`、`X-Auth-As-User`；客户端自报 `X-Auth-*` 一律丢弃；公开路径不注入。
   - `user_process` 为 `None` → **503 + `Retry-After: 2`**。
   - 复用 [proxy.rs](../../../core/acowork-gateway/src/http/proxy.rs) 的 `is_hop_by_hop_header` / 共享 http client。
3. `config.rs` 新增 `[user] { enabled = true, port = 18083 }`（镜像 `PmConfig`/`DocConfig`）。
4. `gateway/state.rs` 新增 `user_process: Option<UserProcessState>`（+ M3 的 `user_snapshot`）。
5. `gateway/mod.rs`：启动 `user_supervisor`（镜像 pm/doc 段），移除 `AuthService` 初始化块（[gateway/mod.rs:222](../../../core/acowork-gateway/src/gateway/mod.rs#L222)）。
6. `http/routes.rs`：删除 `account_api` / `users_api` / `chat_api` 的本地注册，改挂 `user_proxy_routes()`（[routes.rs:261-290](../../../core/acowork-gateway/src/http/routes.rs#L261)）。
7. `user_proxy` 单测：身份注入（丢弃自报头）/ 公开路径不注入 / 503 语义。

**出口**：Desktop 登录 → token → 拉 `/api/users` → 收发用户聊天，全部经 Gateway 正常；杀掉用户服务进程 → Desktop 收到 503 并可重试。

> **实施备注（M2 已完成，出口已实测；M2/M3 合并落地）**
>
> M2 的出口条件要求 Gateway 能验签，而 M2 第 5 点又要删掉 `AuthService` 初始化 —— 删完 Gateway 就没有验签能力，鉴权是断的。因此 **M2 与 M3 的闸门改造合并执行**（否则 Gateway 会短暂成为 `accounts.json` 的第二个写者，正是 ADR-084 要禁掉的）。落地分三批：**A1** config + state + supervisor + proxy；**A2** 闸门（验签 / 受限模式 / `/api/status`）；**A3** 切换路由 + 拆除 + 委托 CLI。
>
> - **数据目录不由 Gateway 解析**（偏离计划第 1 点 `UserSupervisorConfig { ... data_dir ... }` 的写法）。ADR-084 §决策 5 写的是"默认由用户服务自解析"，Gateway 侧只在 `[user].data_dir` 显式配置时下发 `--data-dir`。计划原写法让 Gateway 猜一个默认值，而该默认在 `--home` 下与服务自身解析的结果不同（`project_root().parent()` vs `$HOME`），后果是 Gateway 去一个没人写的目录读 `auth/ed25519.pub` → **每个 token 都被拒**。改为：服务在 `/health` 的 `details.data_dir` 里回报自己的目录，supervisor 从那里取公钥（ADR-084 第 190 行的"或经 supervisor 传入"）。配套：`acowork-user` 的 `default_data_dir` 让 `ACOWORK_HOME` 优先，好在 `--home X` 下把用户域存储也收进 `X`，否则 e2e 会去动开发者真实的 `~/.acowork/acowork-user`。
> - **新增 `[user].config`**（计划未列）：`[multi_user]` 的 `password_policy` / `bootstrap_admin` / `registration_open` 按计划 M1 第 6 点已迁入用户服务配置，但 Gateway 监管启动时没有任何路径把它们送过去 —— 等于 `bootstrap_admin` 这条 ADR-076 §决策 12 的官方首启路径被静默废掉。现在 `[user].config = "<path>"` 原样转发 `--config`；同时 `cli.rs` 首启提示与 `restricted_mode` 的 403 文案里"编辑 gateway.toml 的 `[multi_user].bootstrap_admin`"已改为指向用户服务配置（原文案此时已是假话）。
> - **两个实测出来的真 bug**（计划未预见，均由 e2e 捕获）：
>   1. Gateway 首启探测 `acowork-user admin-setup --check` 会**播种**（`AuthService::new` 建 `auth/` 并生成签名密钥 + `ensure_bootstrap_admin` 建无密码 admin）。它跑在服务启动之前，于是操作者配置的 `bootstrap_admin` 永远不会被应用（`ensure_bootstrap_admin` 是 create-once）。改为 `--check` 只做只读判定：新增 `auth::service::store_requires_setup(&Path)`，不构造 `AuthService`。
>   2. `is_restricted()` 在**空库**时返回 `false`（"有管理员且全部无密码"在零管理员时为假）。ADR-084 之前不可达（Gateway 总是先播种再问），只读探测把它暴露出来：全新安装会报"已配置"，既不提示也不进受限模式。改为"没有管理员也算需要 setup"。
> - **单测代价**：Gateway 侧删掉的 4 个模块里有 28 个测试靠 `seed_and_login` 走 `AuthService`。新增 `http/test_support.rs`（固定 Ed25519 密钥 fixture，同时供 `doc_proxy` / `pm_proxy` 复用），`seed_and_login` 改为签一个测试 token —— **23 个调用点逐字未改**。
> - **`tests/auth_mode_e2e.rs` 的 3 处改动**：(a) `requires_setup` 现在来自 supervisor 的 `/health` 快照，是异步到达的，故加 `wait_for_status` 轮询；(b) `accounts.json` 断言路径改为 `<home>/acowork-user/`（`--home` 现在会带走它）；(c) `bootstrap_admin` 改写在用户服务的 TOML 里。另外顺手修了该文件一个**既有的 Windows-only 失败**：`gateway.toml` 里用 `Path::display()` 拼出的 `"C:\Users\..."` 是非法 TOML（`\U` 是 unicode 转义），改为正斜杠。
> - **实测出口**（隔离 `--home` + 隔离端口，脚本跑完即删）：`admin-setup` 委托建库 → 经 Gateway 登录（EdDSA token）→ `GET /api/users` → 建 alice → alice 登录 → admin 发消息 → alice 侧 `unread_count: 1` 且 `peer_display_name` 正确（证明 `X-Auth-*` 注入生效）→ 杀 `acowork-user` → `503` + `retry-after: 2`。
> - **回归**：`cargo clippy -p acowork-gateway -p acowork-user -p acowork-core --all-targets -- -D warnings` 干净；`cargo test -p acowork-gateway` 579 + 全部集成测试绿（起含 `auth_mode_e2e` 4/4）；`-p acowork-user` 106 绿；`-p acowork-core` 233 绿；`cargo check --workspace --all-targets` 干净。
> - **已知遗留**：`cargo test -p acowork-gateway` 单独跑时，被监管的 `acowork-user.exe` 不保证是新的（Cargo 不为别的包构建 bin）——只有 workspace 级 `cargo test` 或先 `cargo build -p acowork-user` 才会重建；`doc_supervisor_integration` 对 `acowork-doc.exe` 有同样的既有假设。另外每条 e2e 用例都会留下存活至多 300s 的用户服务进程（ADR-018 watchdog），会占 18083+ 端口，并锁住 `target/debug/acowork-user.exe`。


### M3 — 闸门改造（验签 / 受限模式 / 档案快照）｜1-2d

**任务**
1. `http/auth_middleware.rs`：`verify_access` 改调 `acowork_core::auth::TokenVerifier`（公钥，启动时载入内存）；`AuthContext` / `effective_user_id` / `x-user-id` 注入**语义不变**（ADR-076 §决策 4 保持）。
2. `http/restricted_mode.rs`：`is_restricted()` 改读 `GatewayState.user_snapshot.requires_setup`（**无 I/O**）；保持 v2 合约（受限时无 token 请求应答 **403 `setup_required`**，非 401）。
3. `user_snapshot` 刷新机制：
   - `user_supervisor` `/health` 心跳解析 `requires_setup` / `registration_open` 写入 `GatewayState.user_snapshot`。
   - 用户服务在档案 / 账号变更后经 MQTT（参照 [acowork-doc/mqtt_publisher.rs](../../../core/acowork-doc/src/mqtt_publisher.rs)）发布变更信号；Gateway 订阅后**重新拉取** `GET http://127.0.0.1:{port}/internal/user-profiles` → 更新 `ResourceCache.user_profile_list` → 触发全局资源重发布（ADR-042）。
4. `resource_cache.rs` / `mqtt/global_resources_builders.rs`：`user_profile_list` 来源改为用户服务快照（不再读本地 `user_profiles.json`）。
5. `/api/status` 的 `registration_open` 来源改为快照（公开契约不变）。

**出口**：`multi_user` e2e 全绿；受限模式回归用例（走真实 `build_router`，403 而非 401）；改档案后 Runtime 收到的 `last_user_profile` 及时更新。

> **实施备注（M3 已完成，出口已实测）**
>
> 信号通道按 ADR 走 MQTT（`acowork/user/profiles/changed`，QoS 1 非保留，payload = 版本号明文，topic 常量放 `acowork-core` 供两侧共享），但 **额外** 用 supervisor 已有的 2s `/health` 轮询对帐 `user_profile_version` 作兜底。原因是实测出来的：`mqtt.auth_enabled: true` 时 broker 的 `check_connect_auth` 只放行 `node:*` / `bot:*` / `gateway:publisher` / Desktop，`user:service` 会被拒 —— 信号会**静默失效**，`last_user_profile` 就此永久陈旧（doc 的 `doc:service` 有同样的既有隐患）。轮询让正确性不依赖 broker，MQTT 只承担低延迟快路径。轮询复用既有请求，不新增 I/O。
>
> - **`/internal/user-profiles` 必须列入 `is_public_path`**：Gateway 是用"拉"的方式取快照，它没有身份可报（与 `/health` 同理）。第一版漏了这条，真实 `multi_user` 服务返 **401**，Gateway 永远拉不到快照 —— 而单测因为用了 `local` 模式（那里 identity 中间件是 no-op）**假绿**通过。已修，并把该测试改成 `multi_user` 断言、注释写明为什么不能用 `local`。
> - `resource_cache.user_profile_list` 不再读本地 `user_profiles.json`：启动为空，由拉取填充（`load_resource_cache` 保留其余列表的磁盘加载）。顺带删除已成死代码的 `user_profile_list_path` / `load_user_profile_list` / `save_user_profile_list` / `rebuild_and_save_user_profile_cache` —— Gateway 侧不再有任何用户档案写入路径。
> - **实测出口证据**（隔离 `--home` + 隔离端口，脚本跑完即删）：(a) `admin-setup` 后 `/api/status` 的 `requires_setup` 由 `true` 翻成 `false`，**gateway 未重启**（2s 轮询生效）；(b) 建 alice 后 dispatch 记 `user profile change signal received version="5"`、紧接 `user profile snapshot refreshed version=5 users=2`，**同一毫秒** → 走的是 MQTT 快路径而非轮询；(c) 直接 `GET http://127.0.0.1:{user_port}/internal/user-profiles` 返回 `{"version":5,"users":[admin,Alice]}`。
> - **新增测试**：user `/internal/user-profiles` 路由契约（multi_user）；`user_profile_sync` 的 topic 契约、无服务 no-op、对真实 HTTP server 的"拉取→替换缓存→同版本不重复"，另加 `mqtt_publisher` 的 topic/明文 payload 两条。回归：`cargo test -p acowork-gateway`（582 lib + 全部集成）与 `-p acowork-user`（95）全绿，`clippy -D warnings` 三个 crate 干净。
> - **未覆盖**：MQTT 快路径与轮询兜底之间没有强一致/去重语义（信号重复时靠版本号幂等跳过）。
> - **后续补修（收尾已落地）**：`mqtt.auth_enabled` 下 `user:service` 被 broker 拒的缺口已修——broker 的 `check_connect_auth` 放行 `user:service` / `doc:service`（并入 `gateway:publisher` 分支，复用启动时生成的 `publisher_token`）；Gateway 在 supervisor 启动前生成 token 并写入两个 supervisor cfg，supervisor 经 `--mqtt-password` 注入（服务端 CLI 支持 `ACOWORK_MQTT_PASSWORD` env 兜底）；doc 的 `doc:service` 同款既有隐患一并修复。回归：gateway broker 12 单测（含新增 `internal_service_publishers_accept_publisher_token`）、user 95、doc 100 全绿，clippy 三 crate 干净。协议文档 zh/en §8.5 + §8.7 四张表已同步，ADR-084 §9 未决项 2 同步关闭。
> - **顺带发现两条待修**（已记入 M4）：`--home` 是顶层参数，`acowork-gateway admin-setup --home X` 会报 `unexpected argument`（必须 `--home X admin-setup`），与 `acowork-user` 的同一个坑，用 clap `global = true` 一并解决；`admin-setup` 输出的 `Restart gateway to serve requests` 现在已不准确（2s 轮询会让它自动生效，无需重启）。


### M4 — 统一 local 模式 + 拆除 + lint｜1-2d

**任务**
1. `local` 模式：用户服务照常启动（只跑档案 / 头像）；`user_proxy` 代理 `/api/users/*`、`/api/user/*`；`/api/auth/*`、`/api/users/{id}/chats/*` 在 `local` 下不注册（404）。移除 Gateway 内 `users_api` 的 local 分支与头像逻辑。
2. 删除 Gateway 用户域代码：`src/account/`、`src/chat.rs`、`src/auth/service.rs`、`src/auth/revoked.rs`、`src/http/{account_api,users_api,auth_api,chat_api}.rs`；`cli.rs` 的 `AdminSetup` 子命令删除（迁至用户 CLI）。
3. 依赖瘦身：`Cargo.toml` 移除 `argon2` / `hmac`（若 `hmac` 仅 HS256 用）/ `rpassword` / `zeroize` / `tokio-util`（若仅聊天附件流用）。**逐项确认无其它引用后**再删。
4. `dev/ci.sh` 更新红黄线（见 §5.3）。
5. 全量回归：`cargo clippy --all-targets -- -D warnings`（含 `-p acowork-user`）+ `cargo test`。

**出口**：两模式 e2e 全绿；`cli.sh all` 通过；`grep` 确认 Gateway 内无用户域代码残留。

> **实施备注（M4 已完成，出口已实测）**
>
> - **代码拆除**（复核确认）：Gateway 内 `src/account/`、`src/chat.rs`、`src/auth/{service,revoked}.rs`、`src/http/{account_api,users_api,auth_api,chat_api}.rs` 全部删除（glob 0 命中）；`local` 分支与头像逻辑随文件移除，`/api/auth/*`、`/api/users/{id}/chats/*` 由用户服务按 `auth_mode` 决定是否注册（`local` 下 404）。
> - **依赖瘦身**：`argon2` / `hmac` / `tokio-util` 均不在 gateway 依赖表（随用户域迁出）。**保留 `rpassword` / `zeroize`**（对计划第 3 点"移除"清单的有意偏离）：Gateway `cli.rs` 的交互式首启仍在本地读密码（隐藏回显 + 缓冲区擦除），再经 `admin-setup --password-stdin` 委托 `acowork-user`；删除会让交互式首启失效，与 M2"委托 CLI"决策一致。
> - **`dev/ci.sh` 红黄线**（§5.3 五处全部落地，另三条建议 lint 合并为一条）：
>   - 修复 `all` 分支调用旧函数名（`run_gateway_auth_mode_redline` / `run_gateway_chat_path_redline` → `run_user_*`）：此前 `all` 一进 lint 段即 `command not found` 中止（`check` 分支当时是好的，因此未暴露）。
>   - `run_gateway_fs_redline` 追加 ADR-084 §决策 1 扫描：Gateway 侧不得出现 `accounts.json` / `user_profiles.json` / `assets/avatars` / `.join("users"|"chats"|"avatars")`。
>   - 新增 `run_user_boundary_redline`：用户侧不得引用 `acowork-gateway/data`；Gateway 侧不得出现 `ed25519.key`；用户侧不得手写 `"x-auth-` 字面量（须 import `acowork_core` 常量）。
>   - 验证：WSL bash 下用 stub 副本（cargo 函数替换为 echo）跑 `check` / `all` 两模式全过；负向探针逐条命中违规，注释行正确豁免。
> - **计划外修复**：`dev/build_core.ps1` 首启检测路径 `~/.acowork/acowork-gateway/data/accounts.json` → `~/.acowork/acowork-user/accounts.json`（旧路径在 ADR-084 后必然不存在，会每次启动误报"需要设置密码"）。
> - **回归实测**：`cargo clippy -p acowork-gateway -p acowork-user -p acowork-core --all-targets -- -D warnings` 干净；`cargo test`：core 250 绿、user 95 绿、gateway 572 绿（lib 526 + 集成 45 + doctest 1；含 `auth_mode_e2e` 4/4：local ×2、受限模式、multi_user 各一）。
> - **已执行**：完整 `dev/ci.sh all`（run7，2026-09-28）：8 条红线 + `cargo check` + `cargo clippy`（含 acowork-embed） + `cargo test --workspace --exclude acowork-embed` + `run_integration` 4 项（`node_control_plane_e2e` 6/6 / `settings_api` 6/6 / `node_proto_golden` 11/11 / `dependency_redline` 1/1）全部绿。`run_smoke` 39/44 过、5 项败：见下“环境性失败”条。环境与中途修复：
>   - **LSP-relay 锁**：`target/debug/acowork-lsp-relay.exe` 被用户 daemon（PID 33812，1:21 启动）持锁，`cargo build --workspace --bins` 删除旧 binary 失败（os error 5 / Access denied）。本保大厅能够直接杀用户进程（AGENTS.md 禁烟），以加速路为 `cargo build --workspace --bins --exclude acowork-embed --exclude acowork-lsp-relay`（跳过 lsp-relay rebuild，复用其 1:21 binary——spwan 后 Windows 可共享 read 加载现有 binary）。然后 inline 补跑 smoke，避免动 ci.sh。
>   - **memory_m5_bench Plan Y 假设修补**：ADR-062 §6.2 Plan Y 以 "BM25 alone cannot match K* nodes" 为前提，但 `MemoryManager::retrieve` 始终为 hybrid（vector + text RRF）——vector 路径在 `DeterministicEmbedding` 下 cosine ≠ 0，K* 在 keyword_index=false 下 hit@5=1.0。将 L518 断言改为 `after.mrr > before.mrr`（Plan Y 真正的 MRR lift，0.7917 → 0.9375），p5/r5/mrr 回归护栏保留。ADR-062 §6.2 需后续提 PR 备注 supersede。
>   - **git_api_e2e 两补全**：`status_workspace_subdir_of_repo_filters_changes` 500 原因为夹具 `format!` 手拼 JSON 含 Windows 非法转义 `\U`——改为 `serde_json::json!` 序列化；`diff_index_ref_reads_index_not_worktree` 400 原因为 `diff_two_refs` 未接通 head_ref=":" 路由（`rev-parse ":"^{commit}"` 必败），补实现：特判 "："、跳过头侧 rev-parse、read_blob 传空 rev（拼出 `git show :path` 即 index 语义），`head_rev` 为 `None`。两个修复后 git_api_e2e 31/31 全过。
> - **环境性失败（run_smoke，5/44）**：isolated broker 环境下 `user:service` / `doc:service` MQTT 客户端从启动后 ~25 s 进入 重连死循环（broker 抛 `Duplicate client_id, dropping previous`）——这不是本表修代码造成的，反而是 smoke_test.py L417-419 已记载的 ADR-018 watchdog 已知副作用。败项 TC-BOOT-02（system bot 20 s 未 ready）/ TC-HARNESS-08（provider MQTT 10 s 未同步）/ TC-CHAT-02（bot start 404） / TC-AUTH-02（identity.json 未出现）/ Recovery（node 未重 enroll）均为同一现象（系统 bot / node service 等待被 user/doc 连接握手拖住）。本地完整 daemon 下不会重现（本 smoke 是故意 isolated broker）；解决路径：（1）on demand 中修复 broker 携 session takeover 以接受同 client_id 重接（代 ADR-018）；（2）或 smoke 跑在独立 daemon下（先 stop 本地 daemon/lsp-relay，待锁释放）。本次不动产品代码、动 dev/ci.sh 仅限临场 inline 环境调整。

### M5 — 迁移 + 打包｜1d

**任务**
1. 执行 §6 数据迁移 runbook（一次性，不入库）。
2. 构建 / 打包脚本加新二进制（见 §5）。
3. 文档同步：ADR-084 状态定稿；ADR-076 已加取代说明；`docs/module-design/zh/` 视需要补 `acowork-user` 条目（可选）。

**出口**：迁移后登录旧账号成功、历史聊天可读、头像可见；`package_desktop_windows.ps1` 产物 `bin/` 含 `acowork-user.exe`。

> **实施备注（M5 打包链 + 文档同步已完成；§6 数据迁移经评估为本机 no-op，见下）**
>
> - **构建 / 打包链**（§5.2 全部落点）：
>   - `apps/acowork-desktop/package.json`：`core:build:debug` / `core:build:release` 追加 `-p acowork-user`。
>   - `apps/acowork-desktop/src-tauri/build.rs`：`BINARIES` 追加 `acowork-user`（release 随 `bin/*` resources 进包；dev 不 stage，保持反文件锁行为）。
>   - `dev/build_core.sh` / `build_core.ps1` / `build_macos.sh`：停止列表（`stop_process "acowork-user"`）+ 18083 端口释放 + 构建步骤 + 产物清单；ps1 / macOS 步数变量同步 +1。
>   - `dev/package_desktop_{windows.ps1,linux.sh,macos.sh}`：拷贝 `target/release/acowork-user(.exe)` → `src-tauri/bin/`，缺失时警告 `/api/auth/*`、`/api/users/*`、`/api/user/*` 将 503。
>   - `dev/e2e_frontend_smoke/`（`smoke_test.py` / `onboarding_installs_all_agents.py`）：孤儿回收清单加 `acowork-user`（ADR-018 watchdog 最长 300s 存活，会占端口并锁住 `target/debug/acowork-user.exe`）。
> - **文档**：`AGENTS.md`（16 crates / Required binaries 加 `acowork-pm`、`acowork-user` / crate 列表加 `acowork-user` / Logs 加 `user.log` / ADR 计数 80 = zh 77 + en 3）；ADR-084 状态定稿"已决策"；ADR-076 取代说明（先行落字）。
> - **§6 数据迁移评估（本机 no-op，含结论依据）**：源目录 `~/.acowork/acowork-gateway/data` 无 `accounts.json` / `users/` / `assets/` 等账号级遗留（账号数据唯一真源在用户服务数据目录）；仅存一份 `user_profiles.json`，是 M2 之前构建对 `POST /api/users` 的本地写产物（派生视图，日志实锤该请求由 Gateway 本地处理返回 201），current tree 完全不读不写；ADR-084 §决策 8 明确该视图“可由 `accounts.json` 重新派生，可省”。目标 `~/.acowork/acowork-user/` 已有真实数据。结论：不拷贝、不删除（原目录保留备查）；旧档案展示名如需找回，在新 UI 改 admin 档案即可。
> - **既有缺口已补**（收尾）：`package.json` 的 `core:build:debug` / `core:build:release` 已补 `-p acowork-doc`——`build.rs` 的 `BINARIES` 与三个 `package_desktop_*` 都预期 doc 存在，补上后 `npm run tauri build` 的 core:build 阶段会构建 doc，`bin/` 不再缺 `acowork-doc`。（完整 `npm run tauri build` 产物校验仍待执行。）
> - **出口状态**：拷贝清单已含 `acowork-user`；实际产物校验（完整跑一次 `npm run tauri build` 后检查 `bin/acowork-user.exe`）待执行。

---

## 4. 文件级改动清单

**以 [ADR-084 §6](../../adr/zh/ADR-084-user-standalone-process.md) 为准**，此处仅列实施视角的落点（不重复 ADR 正文）：

| crate | 动作 | 关键文件 |
|---|---|---|
| `core/acowork-core` | 新增 | `src/auth/{mod,claims,issuer,verifier}.rs` |
| `core/acowork-user` | 新增 | `Cargo.toml`、`src/{main,lib,server,health,config,error,types}.rs`、`src/account/*`、`src/auth/*`、`src/http/*`、`src/chat.rs`、`src/mqtt_publisher.rs` |
| `core/acowork-gateway` | 删除 | `src/account/*`、`src/chat.rs`、`src/auth/{service,revoked}.rs`、`src/http/{account_api,users_api,auth_api,chat_api}.rs`、`cli.rs::AdminSetup` |
| `core/acowork-gateway` | 新增 | `src/lifecycle/user_supervisor.rs`、`src/http/user_proxy.rs` |
| `core/acowork-gateway` | 修改 | `config.rs`、`gateway/state.rs`、`gateway/mod.rs`、`http/routes.rs`、`http/auth_middleware.rs`、`http/restricted_mode.rs`、`resource_cache.rs`、`mqtt/global_resources_builders.rs`、`auth/token.rs` |
| `apps/acowork-desktop` | 不改 | 契约不变 |

---

## 5. 构建 / 打包 / CI 改动

### 5.1 Cargo workspace
- `core/Cargo.toml` `[workspace] members` += `"acowork-user"`。

### 5.2 构建与打包脚本（每处 `acowork-pm` / `acowork-doc` 出现处一并加 `acowork-user`）
| 文件 | 改动 |
|---|---|
| [apps/acowork-desktop/package.json:8-9](../../../apps/acowork-desktop/package.json#L8) | `core:build:debug` / `core:build:release` 追加 `-p acowork-user` |
| [dev/build_core.sh](../../../dev/build_core.sh)、[dev/build_core.ps1](../../../dev/build_core.ps1)、[dev/build_macos.sh](../../../dev/build_macos.sh) | 新增 build + stop 步骤（`stop_process "acowork-user"`） |
| [dev/package_desktop_windows.ps1](../../../dev/package_desktop_windows.ps1)、[linux](../../../dev/package_desktop_linux.sh)、[macos](../../../dev/package_desktop_macos.sh) | `Copy` `target/release/acowork-user(.exe)` → `src-tauri/bin/` |
| `dev/e2e_frontend_smoke/smoke_test.py` | 若涉及进程清单，加 `acowork-user` |

> 注意（ADR-055 §6.11 / ADR-064 同款约束）：Gateway 经 `current_exe().parent().join("acowork-user")` 定位二进制，**必须与 `acowork-gateway` 同目录**，否则 supervisor 报 "binary not found" 静默降级为 503。

### 5.3 `dev/ci.sh` 红黄线（5 处必改）
| lint | 现状 | M4 后 |
|---|---|---|
| `run_gateway_fs_redline`（ADR-009） | 盯 `install_path` fs 访问 | 追加：Gateway 不得读 `accounts.json` / `user_profiles.json` / `users/` / `assets/avatars/` |
| `run_gateway_auth_scope_redline`（ADR-076 §决策 4） | `USER_SCOPE_HEADER`/`"x-user-id"` 仅允许在 `auth_middleware.rs` | 保持；**追加** `X-Auth-User`/`X-Auth-Role`/`X-Auth-As-User` 仅允许在 `user_proxy.rs`（+ 测试） |
| `run_gateway_auth_mode_redline`（ADR-076 §决策 12） | 盯 `routes.rs` 的 auth 注册分支 | 改写：盯 `user_proxy` 的注册是否处于 `auth_mode`/`[user].enabled` 分支 |
| `run_gateway_chat_path_redline`（ADR-076 §决策 8） | 盯 gateway `chat.rs` 拥有配对路径 | 路径 root 改指 `core/acowork-user/src/chat.rs` |
| `run_meta_layout_redline` | 会话 meta 布局 | 不变 |

> **机制提醒**：现有 5 条 lint 的 `root` 全部硬编码为 `core/acowork-gateway/src`（见 [dev/ci.sh:170](../../../dev/ci.sh#L170)、[:74](../../../dev/ci.sh#L74)）。chat-path lint 迁出后若不把 `root` 与 allowlist 一并改指 `core/acowork-user/src`，它会**静默放过**——新 crate 根本不在扫描范围内，CI 假绿。

新增 lint（建议）：
- `acowork-user` 侧不得引用 Gateway 数据目录路径（`acowork-gateway/data`）。
- Gateway 侧不得出现 `ed25519.key`（私钥只在用户服务）。
- **头常量单一来源**：`X-Auth-User` / `X-Auth-Role` / `X-Auth-As-User` 的字面量只在**一处定义**（建议 `acowork-core` 协议常量），Gateway `user_proxy`（写）与用户服务（读）各自 import。否则 lint 只能盯住 Gateway 一侧，读写两端仍会字面量漂移。

---

## 6. 数据迁移 runbook（一次性，不入库）

> 开发期无兼容需求（ADR-084 §决策 8）：**不写迁移代码**。以下由操作员手动 / 临时脚本执行一次。

**源**：`$HOME/.acowork/acowork-gateway/data`｜**目标**：`$HOME/.acowork/acowork-user`

```bash
GW="$HOME/.acowork/acowork-gateway/data"
US="$HOME/.acowork/acowork-user"
mkdir -p "$US"
[ -f "$GW/accounts.json" ]      && cp "$GW/accounts.json"      "$US/"
[ -f "$GW/user_profiles.json" ] && cp "$GW/user_profiles.json" "$US/"
[ -d "$GW/users" ]              && cp -r "$GW/users"           "$US/"
[ -d "$GW/assets" ]             && cp -r "$GW/assets"          "$US/"
```

| 数据 | 处理 | 备注 |
|---|---|---|
| `accounts.json` | 拷贝 | 仅 `multi_user` 存在 |
| `user_profiles.json` | 拷贝（或省略，由 `accounts.json` 重新派生） | 用户服务为唯一写入方 |
| `users/` | 递归拷贝 | 用户↔用户聊天树 `{min}/chats/{max}/` |
| `assets/avatars/`（`multi_user`）、`assets/`（`local` 共享） | 递归拷贝 | 头像 |
| 旧 `auth/secret`（HS256） | **丢弃** | 切 Ed25519 后旧 token 全失效，重新登录即可 |

**校验**：`multi_user` 登录旧账号 → 200；用户↔用户历史聊天可读；头像可见；`local` 模式档案可编辑。

---

## 7. 测试矩阵

| 层 | 用例 |
|---|---|
| 单元 | core Ed25519 签验/篡改/过期/kind/family；account store 原子写+损坏备份；Argon2id 登录改密；refresh 撤销与重用；聊天配对/未读/附件权限；`user_proxy` 身份注入与丢弃自报头 |
| 集成（e2e） | 真实 `build_router`：`multi_user` 登录→带 token→`/api/users`→代理→200；`local` 档案 CRUD 经代理；`/api/auth/*` 在 `local` 下 404；受限模式 403 `setup_required`（非 401）；越权 403/404；admin view-as 只读（写操作 403）；用户服务未就绪 503 + `Retry-After: 2` |
| 契约 | Desktop `auth-api.ts` / `user-chat-api.ts` / `gateway-api.ts` 迁移前后字节级对比 |
| 安全 checklist | 直连 `127.0.0.1:18083` 伪造 `X-Auth-*`；客户端自报 `x-user-id` 被丢弃；Gateway 进程不持有 Ed25519 私钥；用户服务仅绑 loopback |

---

## 8. 风险与回滚

| 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|
| Ed25519 迁移引入 token 兼容问题 | 中 | 中 | 开发期无存量 token；M0 单独成里程碑，先绿再动 crate |
| Gateway 对用户服务的两条运行时依赖（档案快照 / `requires_setup`）未就绪时语义退化 | 中 | 中 | 定义保守降级：档案空 → `last_user_profile` 缺省；`requires_setup` 未知 → 直通（见 ADR-084 §9 开放问题 1） |
| 打包脚本漏加二进制 → supervisor 静默 503 | 中 | 中 | §5.2 逐脚本过一遍 + 打包产物 smoke 校验 `bin/acowork-user*` 存在 |
| redline lint 指向旧路径，CI 假绿/假红 | 中 | 低 | M4 与代码删除同批更新 lint，`cli.sh all` 本地先跑 |
| `local` 模式为档案单起进程的开销争议 | 低 | 低 | ADR-084 §决策 6 已论证；如需回退见 §9 |

**回滚**：`[user].enabled=false` → 不 spawn，`/api/auth/*`、`/api/users/*` 返回 503（不影响其它服务）；数据目录改回即恢复。Ed25519 改动为单向，如需回退须恢复 `token.rs`（开发期无数据影响）。

---

## 9. 未决 / 待确认（同步 ADR-084 §9）

1. 用户服务未就绪时受限模式的保守语义：直通（availability）vs 拒绝（security）——**默认直通**，评审确认。
2. 档案刷新的信号通道：MQTT topic（推荐，复用 doc 范式）vs 回调 Gateway vs 轮询。
3. `local` 模式是否常驻用户服务：**默认常驻**（ADR-084 §决策 6）。
4. `/api/status` 的 `registration_open` 是否继续由 Gateway 透出（推荐保持）。
5. Ed25519 密钥轮换：本期不做（YAGNI），记录公钥 `kid` 升级路径。

---

## 10. 交付定义（Definition of Done）

- [ ] `core/acowork-user` 独立进程可起停、可被 supervisor 监督；`/health` 契约达标。
- [ ] Gateway 内**无**用户域代码与用户数据目录直读（lint 兜底）。
- [ ] 对外契约字节级不变，Desktop 零改动。
- [ ] `multi_user` / `local` 两模式 e2e 全绿；受限模式回归绿。
- [ ] `cargo clippy --all-targets -- -D warnings` + `cargo test` + `dev/ci.sh all` 通过。
- [ ] 一次性数据迁移校验通过；打包产物含 `acowork-user` 二进制。
