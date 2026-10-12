# ADR-076: 多用户账号系统

**状态**：草案（业务语义有效；**实现形态已由 [ADR-084](./ADR-084-user-standalone-process.md) 取代**）
**日期**：2026-10-15
**决策者**：大鱼

**前置**：
- [ADR-009](./ADR-009-gateway-workspace-isolation.md)（§5.4 Gateway 边界规则 — 用户聊天数据归属 Gateway 自身，不在该边界禁止范围内，但要在本文档显式落字）
- [ADR-024](./ADR-024-merge-metadata-into-index.md)（conversation 持久化范式 — meta.json + jsonl 双文件 — 是用户聊天的复用模板）
- [ADR-055](./ADR-055-remote-runtime-node-topology.md)（Node 拓扑 — Node 拥有 `install_path`，session 数据物理上落在 Node；本 ADR 不打破这点，但 session 过滤维度从 agent_id 扩到 `(instance_id, user_id)`）
- [ADR-073](./ADR-073-agent-instance-identity-decomposition.md)（agent instance / node / user 三层身份范式 — 本文把 user 提升为与 instance、node 同级别的身份维度）
- [ADR-059](./ADR-059-parallel-onboarding-handshake.md)（Vault Argon2id + ChaCha20-Poly1305 KDF 链 — 本 ADR 复用同一 vault 的 master key 派生 per-user 加密条目，而非新建密码体系）

> **取代说明（2026-10-20）**：本 ADR 的**业务语义与数据模型仍然有效**（账号模型、session 隔离、Desktop UI、部署模式分流等决策保留）。但本 ADR 落地的**实现形态——把账号 / 用户聊天内嵌在 Gateway 进程内**——已由 [ADR-084](./ADR-084-user-standalone-process.md) **取代**：用户域（账号、凭据、角色、档案 / 头像、用户↔用户聊天）迁出为独立进程 `acowork-user`，Gateway 仅保留反代 + 鉴权闸门。
>
> 受影响的是**代码归属**，逐条对照：
>
> - **§决策 8（用户-用户聊天，Gateway 侧 conversation.json / jsonl）** → 迁入 `acowork-user`（数据与语义不变）。
> - **§决策 1 / 2（账号模型与凭据存储）** → 数据与语义不变，存储位置迁入 `acowork-user` 数据目录。
> - **§决策 3（HTTP 认证）** → **签发**（login / refresh）迁入 `acowork-user`；**每请求校验**留在 Gateway，并改为 Ed25519 公钥本地验签（HS256 → EdDSA）。
> - **§决策 5 / 6（管理员角色 / 账号生命周期）** → 迁入 `acowork-user`。
> - **§决策 12（部署模式分流）** → 语义不变；`[multi_user]` 配置归属迁入 `acowork-user`。
> - **§6.4（core/acowork-gateway 改动清单）** → 以 ADR-084 §6 为准（用户域代码迁出，Gateway 改为 supervisor + user_proxy）。
>
> **不受影响、仍然有效**：§决策 4（session 隔离）、§决策 7（Desktop UI）、§决策 10（PM / Doc 反代身份注入）、§决策 11（PM 成员模型），以及 [ADR-042](./ADR-042-mqtt-user-identity-delivery.md) 身份下发。
>
> 实施计划见 [docs/plan/zh/user-dev-plan.md](../../plan/zh/user-dev-plan.md)。

---

## 1. 决策摘要

### 1.1 一句话

**把"用户"从纯展示偏好提升为一等身份维度**：`UserProfile` 升级为 `UserAccount`（带账号/密码/角色/admin flag），账号凭据通过现有 Vault 的 master key 加密落盘；session 元数据增加 `user_id` 字段 + `visibility` 可见性开关，Runtime 从 Gateway 注入的 `x-user-id` 头解析调用者 scope，在**分页前**过滤 `GET /sessions` 并校验每个 session 维度的读写；会话控制面（create / open / close / delete）从 MQTT 迁到 HTTP，因为 MQTT 控制消息不携带身份；新增"系统管理员"角色绕过所有 session 隔离；Agent 列表侧栏并排新增"User List"折叠分组（复用 `partitionAgentsByNode` 的分组范式）；Gateway 侧新增用户-用户聊天持久化（conversion.json / jsonl 双文件，参考 ADR-024 拆分）。

### 1.2 关键决策表（详细理由见 §4）

| # | 决策 | 结论 |
|---|---|---|
| 1 | 账号数据模型 | 升级 `UserProfile` → `UserAccount`，新增 `password_hash`(Argon2id)、`password_salt`、`role`(`user`/`admin`)、`created_at`、`disabled_at?`；display_name / language / avatar 等展示字段保留 |
| 2 | 凭据存储 | **复用现有 Vault 的 master key**，account 文件以 `vault://accounts/{user_id}.enc` 形式加密落盘；账号创建/改密均要求 Vault unlocked；**不引入第二套密码** |
| 3 | HTTP 认证 | 单一 bearer token 改为 **登录令牌**（短期 access_token + 长效 refresh_token），token payload 含 `user_id` + `role`；middleware 解析后注入 `AuthContext` 到 `AppState`；admin token 通过额外 flag 区分 |
| 4 | Session 隔离 | `SessionMeta` 新增 `user_id: Option<String>` + `visibility: Option<SessionVisibility>`（`None` = 公开，缺省不改变旧数据行为）；身份由 Gateway 注入 `x-user-id` 头（`*` = admin 不过滤），Runtime 解析为 `SessionScope` 并在**分页前**过滤；读走 `is_readable_by`（不可读 → 404），写走 `is_writable_by`（仅 owner / admin）；**会话控制面从 MQTT 迁到 HTTP**（见本节实施记录） |
| 5 | 管理员角色 | `role = "admin"` 用户绕过 `user_id` 过滤，且 `GET /api/users` 看到全部账号（含密码哈希元数据但不含明文）；普通用户只能 `GET /api/users/{self}` |
| 6 | Desktop 账号切换 | 顶栏新增"当前用户"菜单，下拉含"切换账号 / 修改密码 / 注销 / 退出登录 / 注册新账号(若允许)";账号切换等价于"清空本地缓存 + 重连 Gateway + 重新拉取 agent列表 + 重连 MQTT" |
| 7 | Sidebar User 折叠分组 | 在 AgentList 同级渲染 `partitionAccountsByAccountType` 折叠项——单独 item "Users (N)" 默认折叠，点击展开列出全部账号；admin 视图下点击账号名进入该 user 的 session 列表过滤模式 |
| 8 | 用户-用户聊天 | Gateway 侧新增 `data_dir/users/{user_a_id}/chats/{user_b_id}/conversation.json` + 同名 `.jsonl`（按字典序排 `(min(a,b), max(a,b))` 避免重复）；参考 ADR-024 meta/jsonl 拆分；不支持 group chat |
| 9 | 存储归属 | 用户聊天数据完全归属 Gateway 所在机器（`data_dir/users/...`），**不**走 Runtime HTTP 反代；明确写入 ADR-009 §5.4 例外条款 |
| 10 | 反代身份注入（PM/Doc） | REST 反代 `X-Actor` 从硬编码 `"human"` 改为 `AuthContext.effective_user_id`（决策 3 的 token 身份）；MCP 路径 `X-MCP-Actor` 校验不变（agent 身份与 user 正交，见 §决策 10） |
| 11 | PM 成员模型多用户化 | `ProjectMember` 新增 `kind`（`Agent`/`User`），人类操作者与 agent 成员对称；移除 `assignee = "human"` 特例，不变式收紧为 `assignee ∈ ∅ ∪ members`（见 §决策 11） |
| 12 | 部署模式分流 | 新增 `AUTH_MODE ∈ {local, multi_user}`，由 bind 地址自动推断（`127.0.0.1` → `local`；`0.0.0.0` → `multi_user`），可显式覆盖；local 模式下 §1-§11 退化为 no-op（不引入登录页 / Argon2id / admin / session 过滤 / 用户聊天）。**multi_user 模式下空账号库的处理（v2 修订）**：不再"缺则拒启动"，而是 seed 一个 `username=admin` 的无密码账号（`password_hash=DISABLED_PASSWORD_HASH`），Gateway 进入**受限模式**——只有 `/health` 和 `/api/status` 可达，其他 `/api/*` 一律返回 403 `{error: setup_required}`。操作员在 Gateway 主机上完成首次设置（TTY prompt / `admin-setup` 子命令 / `[multi_user].bootstrap_admin` toml 段）后，受���模式关闭。**没有任何 HTTP 端点接受首次密码**——密码只走 stdin / 文件 / TTY / toml，**永远不上网络**。**（v3 修订：daemon 永远先起 HTTP，受限模式真的对外服务；prompt 只是 best-effort，非 TTY 不阻断启动——见 §决策 12 v3）** 详见 §决策 12 v2 / v3。 |

### 1.3 不变量（必须满足）

1. **multi_user 模式下 session 隔离是强制默认**：除 admin 外，所有 session 维度的读写（list / messages / state / files）必须经过 user_id 过滤；漏掉任何一处 = 数据泄漏。local 模式下不启用 session 过滤（见 §决策 12）。**（当前状态：✅ 已满足——Runtime 读路径过滤 + 写路径 owner 校验 + 控制面 HTTP 化均已落地，见 §决策 4 Phase D 实施记录。）**
2. **admin 不能伪造 user_id**：admin 视图下"以 user A 身份看 session"通过 `?as_user=<user_id>` query 实现，但 `as_user` 不会被普通 user 使用；token 中 `role` 字段在签发时定死，不接受请求内覆盖。
3. **账号凭据加密不依赖 Vault unlocked**：账号读路径在 Vault locked 状态下退化为 401（无法解密 → 无法登录），但**账号列表（不含密码）的元数据允许在 Vault locked 时展示**（仅元数据，如 username/role/created_at），便于锁屏场景下仍能选账号。
4. **session.user_id 写入是 immutable**：一个 session 一旦创建绑定 user_id 后**不再修改**（迁移/导入等场景除外，且必须 admin 操作）；这保证会话历史"主人"的不可篡改性。
5. **聊天双方对等**：用户 A → 用户 B 的消息存在 `min(a,b)/chats/max(a,b)/` 目录下，双方 GET / POST 对称，无需在 Gateway 内维护 per-user 状态机。
6. **密码修改强制旧密码**：改密 API 接受 `old_password + new_password`，避免 token 泄漏后任意改密；admin 不能改他人密码（必须先 reset 再走首次登录改密流程）。

### 1.4 部署模式行为对照表（§决策 12 速查）

| 维度 | `AUTH_MODE=local`（bind `127.0.0.1`，默认） | `AUTH_MODE=multi_user`（bind `0.0.0.0`） |
|---|---|---|
| HTTP 认证 | 现有 bearer token（`data_dir/http_token`） | access_token(HS256, 15min) + refresh_token(30d) |
| 登录流程 | 无（Desktop 直接用 token） | `/api/auth/login` + LoginView |
| `UserAccount` schema | `user_profiles.json` 沿用现有 `UserProfile` 字段 | `UserAccount`（含 `password_hash` / `role` / `disabled_at`） |
| `accounts.json`（账号权威表） | 不创建 | 创建（明文）；`vault/accounts/*.enc` 扩展可选 |
| 首位账号 | 沿用现状（无账号概念，`user_profiles.json` 不变） | seed 无密码 `admin` + 受限模式（**v2 修订**，见 §决策 12 v2 / v3；原为"缺配拒启动"） |
| admin 角色 | 无（OS 用户即 admin） | `role = Admin`，`GET /api/users` 全量 |
| session 隔离 | `SessionMeta.user_id` 写入但 **read 不过滤**（无 `x-user-id` 头 → `Unfiltered`） | read 路径强制过滤（admin 除外）；`visibility = Private` 时非 owner 视为不存在（404） |
| session 控制面 | 同一条 HTTP 路由（Runtime 视为 `Unfiltered`） | HTTP + token 鉴权；create 记录 owner |
| session `visibility` 默认 | `None`（公开，不过滤） | **有主会话 = `Private`（创建时落盘）；无主会话 = `None`（公开）**——见 §决策 4「默认值的两种含义」 |
| `?as_user=` | 路由不注册 | admin-only 只读视图 |
| `/api/auth/*` 路由 | **不注册** | 全部注册 |
| 用户-用户聊天 | **不注册** `data_dir/users/` 不创建 | `/api/users/{self}/chats/*` 全量 |
| PM/Doc 反代 `X-Actor` | 常量 `"human"` | `auth.effective_user_id` |
| PM `assignee == "human"` 特例 | **保留**（无 token 身份可注入） | 移除，`assignee ∈ ∅ ∪ members` |
| Desktop 顶栏 | "用户偏好"（现状不变） | "账号菜单"（切换 / 改密 / 注销 / 退出） |
| Desktop 侧栏 User 分组 | 不显示 | `partitionAccounts` 折叠分组 |
| 升级路径 | → multi_user：补 `password_hash` + `invite_token` 激活（无需数据迁移） | — |
| 降级路径 | — | → local：`--auth-mode local`，账号文件保留但不可登录 |

> **配置通道（实施期落字）**：本文档用 `AUTH_MODE` 作为部署模式的**概念名**，不映射到某个具体环境变量。实际配置三通道（优先级 CLI > TOML > bind 推断 > 默认 `local`）：
> - CLI：`--auth-mode <local|multi_user>`（环境变量 `ACOWORK_GATEWAY_AUTH_MODE`，见 [cli.rs](core/acowork-gateway/src/cli.rs)）
> - TOML：顶层 `auth_mode = "local" | "multi_user"`（见 [config.rs](core/acowork-gateway/src/config.rs)）
> - 推断：HTTP bind 地址（loopback → `local`，其余 → `multi_user`）
>
> 推断只看 HTTP bind 地址，不看任何 `AUTH_MODE` 字样；文档正文凡单独写 `AUTH_MODE=local/multi_user` 均指概念模式。

---

## 2. 背景与动机

### 2.1 现状：一个用户，一份配置，全局共享

```text
                        ┌────────────────────────┐
                        │       Desktop App      │
                        │   (单一 localStorage)  │
                        └──────────┬─────────────┘
                                   │ HTTP + Bearer Token (单一共享)
                                   ▼
                        ┌────────────────────────┐
                        │       Gateway          │
                        │  HttpAuth (1 token)    │
                        │  user_profiles.json    │  ← 所有 user 平铺，无认证
                        └──────────┬─────────────┘
                                   │ MQTT
                                   ▼
                        ┌────────────────────────┐
                        │   Runtime (per inst)   │
                        │  conversations/meta/   │  ← 无 user_id 字段
                        │  conversations/*.jsonl │
                        └────────────────────────┘
```

**关键缺陷**：
- `UserProfile` 是"显示偏好"，不是"账号"——任何前端都能 `POST /api/users` 注册新 user，也能在 `is_active=true` 时把自己的 profile 推到 Runtime `last_user_profile`。
- `HttpAuth` 的 bearer token 是 Gateway 启动时随机生成的 32-byte hex，所有 Desktop 实例**共享同一个 token**；`http_token` 文件一发则所有机器同权。换言之：登录的是"这台 Desktop"，不是"这个用户"。
- `SessionMeta` 不携带 user_id，`GET /api/agents/{id}/sessions` 返回**所有**会话；Desktop 上"我创建的会话 vs 别人创建的"无法区分。
- 没有"管理员"概念——所有 user 平权，谁也看不到全局。

### 2.2 已有可复用的积木

| 积木 | 现状 | 本文复用点 |
|---|---|---|
| `Vault`（Argon2id + ChaCha20-Poly1305）| [core/acowork-vault/src/vault.rs](core/acowork-vault/src/vault.rs) 一个 master key 派生所有 `.enc` 条目 | 用户账号文件以 `vault://accounts/{user_id}.enc` 复用同一 master key |
| `partitionAgentsByNode`（Node 折叠范式）| [apps/acowork-desktop/src/components/agent-list/partitionAgentsByNode.ts](apps/acowork-desktop/src/components/agent-list/partitionAgentsByNode.ts) | 新增 `partitionAccounts` 完全照搬——单一折叠分组 vs 多 node 分组是同构问题 |
| `SessionMeta` + jsonl（ADR-024）| meta.json 400 bytes + jsonl 流式 append | 用户聊天 = `conversation.json` (meta) + `conversation.jsonl` (流)；schema 字段不同 |
| `HttpAuth::validate_token`（常量时间比较）| [core/acowork-gateway/src/http/auth.rs:60](core/acowork-gateway/src/http/auth.rs#L60) | token 改为签名的 JWT-ish，验证代码替换为 signature 检查 |
| `OperationAck` + `expected_version`（ADR-059 §7.3）| [core/acowork-gateway/src/http/users_api.rs:147](core/acowork-gateway/src/http/users_api.rs#L147) 乐观并发 | 账号创建/改密/角色变更都走同一乐观并发协议 |

### 2.3 已尝试 / 已拒绝的方案（避免重蹈覆辙）

- **新建第二套密码系统（与 Vault 解耦）**：❌ 用户被迫记两个密码；salt/迭代参数分裂两份；运维成本翻倍。
- **账号信息塞进 `UserProfileListFile`**（现有 json）：❌ 该文件路径 `data_dir/user_profiles.json` 当前明文落盘，把账号凭据混进来会破坏 ADR-059 §7.3 的"非敏感展示元数据"语义。
- **让 Runtime 内置 user store**：❌ Runtime 是无主进程，跨 Node 上同一个 agent instance 的 user 视图无法合并；且 ADR-009 §5.4 禁止 Gateway 直读 Runtime 私有数据，方向反了。
- **session 过滤在 Gateway 侧用 `instance_id` 索引文件实现**：❌ 引入第二索引源（与 Runtime scan_sessions 并存），维护两套真相；session 持久化路径全在 Runtime，重复造轮子。

---

## 3. 目标

1. **账号层**：注册 / 登录 / 改密 / 注销账号全流程；密码 Argon2id 哈希 + Vault 加密存储；不引入第二套密码体系。
2. **session 隔离**：每个 session 持久化时绑定创建者 user_id；普通 user `GET /api/agents/{id}/sessions` 默认只看自己；admin 看全部。
3. **管理员**：内置 admin 角色；安装 Gateway 时通过环境变量 / 首次启动交互创建首位 admin；admin 可看全部数据但不能伪造 user_id 操作（"以 user X 身份查看"通过 `?as_user=` query 而非身份冒用）。
4. **Desktop UI**：顶栏账号切换 / 修改密码 / 注销 / 注册入口；侧栏 Agent 列表旁增加 User 折叠分组（admin 视图下点击账号可进入"以该 user 视角看 session"模式）。
5. **用户-用户聊天**：文字 + 图片 + 文档；conversion.json (meta) + conversion.jsonl (流) 持久化在 Gateway 本地 `data_dir/users/{a}/chats/{b}/`；不走 Runtime HTTP。
6. **存储**：账号 + 用户聊天全部位于 Gateway 所在机器（`data_dir/` 下），不依赖 Node agent 文件系统；session.user_id 维度不改变 Runtime 物理数据布局（仍按 `{install_path}/workspace/conversations/` 落地）。

---

## 4. 决策

### 决策 1：账号数据模型 — `UserProfile` → `UserAccount`（in-place 升级，迁移脚本同表转换）

```rust
// core/acowork-core/src/account.rs (新文件)
pub struct UserAccount {
    // ── ADR-076: 身份字段 ──
    pub user_id: String,                  // UUID v4，复用现有 user_id 语义
    pub username: String,                 // 新增：登录用唯一 handle（小写 + 数字 + -_）
    pub display_name: String,             // 旧 UserProfile.display_name
    pub role: Role,                       // User / Admin

    // ── ADR-076: 凭据字段 ──
    /// Argon2id PHC string: "$argon2id$v=19$m=...,t=...,p=...$<salt>$<hash>"
    /// 单独存 —— 独立于 Vault —— 目的是让登录校验在 Vault locked 时也能完成
    /// （Vault 是用来加密敏感扩展字段，不是密码哈希本身）
    pub password_hash: String,
    pub password_changed_at: String,      // ISO8601，强制改密策略用
    pub password_expires_at: Option<String>,

    // ── 旧 UserProfile 平移 ──
    pub language: String,
    pub timezone: String,
    pub city: Option<String>,
    pub country: Option<String>,
    pub occupation: Option<String>,
    pub avatar: Option<String>,
    pub builtin_avatar: Option<String>,
    pub communication_style: Option<String>,
    pub custom: HashMap<String, String>,

    // ── 生命周期 ──
    pub created_at: String,
    pub updated_at: String,
    pub last_login_at: Option<String>,
    pub disabled_at: Option<String>,      // 软删除：保留历史 session 归属
}

pub enum Role { User, Admin }
```

**与现有 `UserProfileListFile` 的关系**——三个文件，各司其职：

| 文件 | 内容 | 加密 | 权威性 |
|---|---|---|---|
| `data_dir/accounts.json`（新增，`AccountListFile`） | **账号权威表**：`user_id` / `username` / `role` / `password_hash` / 生命周期 / 展示字段 | 明文 | **源**（multi_user 模式） |
| `data_dir/user_profiles.json`（保留，`UserProfileListFile`） | `UserProfile` 公开视图（展示字段，无 username / role / 密码） | 明文 | **派生**（Runtime `last_user_profile` 推送源） |
| `data_dir/vault/accounts/{user_id}.enc`（新增，可选） | 敏感扩展：`api_secrets` / `recovery_codes` / `encrypted_notes` | Vault master key | 扩展 |

**关键点**：`password_hash` 落在**明文**的 `accounts.json`——它是一向的 Argon2id PHC 串，本身不含明文密码，可安全落盘；放明文正是为了让登录校验在 Vault locked 时也能完成（见下）。`vault/accounts/*.enc` **不含** password_hash，只装真·机密扩展字段。`user_profiles.json` 由 `accounts.json` 派生，供 Runtime `last_user_profile` 推送（脱敏副本）。

**派生视图的残余 ceiling**（`account_api::sync_profiles`）：multi_user 下每个会话都自带 owner（`x-user-id`），"全局 active user" 这个概念本身已经没有意义了；但那条遗留的 `last_user_profile` 推送主题还在按**单一**账号推送，所以每次账号变更仍要从 `accounts.json` 重建这份派生视图，并用"最近登录的账号，没有就取第一个启用的 admin"来挑一个（`is_active`）。它现在**只喂**那条遗留主题，不影响任何鉴权判定（鉴权读 `accounts.json` / token claim）。升级路径 = Runtime 学会按请求携带的 owner 读 profile，这份派生视图和整个 `sync_profiles` 一起消失。触发条件：Runtime 侧要做"多账号各自的 profile 推送"（当前没有任何消费者需要它）。

**为什么 Argon2id 不走 Vault**：Vault 是对称加密（加密/解密需要同一把 master key），用于"短期可解密的机密"。密码哈希是**单向**的（无法反推明文），且需要在 Vault locked 状态下也能校验（典型场景：开机后用户第一次登录解锁 Vault 之前）。两者密码学属性不同，强行塞进 Vault 反而要让登录路径依赖 Vault unlock 状态（违反 §1.3 不变量 3）。

**为什么不直接用 bcrypt/scrypt**：项目 Vault 已选 Argon2id（ADR-059），保持单一 KDF 算法，减少密码学 surface。

### 决策 2：Vault 复用 — 加密"扩展敏感字段"

**布局**——账号权威表 `accounts.json`（明文）、公开视图 `user_profiles.json`（派生）、加密扩展 `vault/accounts/{user_id}.enc`（可选）：

```text
data_dir/
├── accounts.json                       # 账号权威表（明文；password_hash 为单向 PHC 串）
│   └── accounts[] = [{user_id, username, role, password_hash, display_name, ...}]
├── user_profiles.json                  # 公开视图（派生；Runtime last_user_profile 源）
│   └── users[] = [{user_id, display_name, avatar, ...}]   # 无 username / role / 密码
│
└── vault/                              # 现有 Vault 目录
    ├── salt                            # Argon2id master salt（不变）
    ├── openai.enc                      # 现有 LLM key
    └── accounts/                       # 新增子目录（可选——无扩展字段时不存在）
        ├── {user_id_1}.enc             # 加密敏感扩展字段（api_secrets / recovery_codes）
        └── ...
```

**`accounts/{user_id}.enc` 内嵌 JSON 结构**：

```json
{
  "schema_version": 1,
  "user_id": "...",
  "encrypted_notes": "...", // ChaCha20-Poly1305 加密的 note-to-self（类似 memo）
  "api_secrets": {         // 用户自己的 API key（如果有）
    "openai": "sk-...",
    "anthropic": "sk-..."
  },
  "recovery_codes": [...]  // 二次验证恢复码
}
```

**关键设计**：
- 账号创建/改密/登录**不要求** Vault unlocked——只需校验 `password_hash`。
- Vault locked 时仍可登录、可改密、可看公开 user 列表；只有"读账号加密扩展字段"才要求 unlock。
- 这恰好符合用户预期："我的密码 = 我的账号"是首要凭据，Vault 是次要保护层。

### 决策 3：HTTP 认证 — 短期 access_token + 长效 refresh_token

```text
                    ┌───────────────────────────────────────┐
                    │            Gateway                    │
                    │  POST /api/auth/login                 │
                    │   → 校验 password_hash                │
                    │   → 签发 access_token (15 min, HS256) │
                    │   → 签发 refresh_token (30 day)        │
                    │                                       │
                    │  POST /api/auth/refresh               │
                    │   → 校验 refresh_token                │
                    │   → 续签 access_token                 │
                    │                                       │
                    │  POST /api/auth/logout                │
                    │   → 撤销当前 refresh_token            │
                    │                                       │
                    │  middleware: extract AuthContext     │
                    │   → {user_id, role, as_user?}         │
                    │   → 注入到 AppState                   │
                    └───────────────────────────────────────┘
```

**Token payload（HS256 + 独立持久化签名密钥）**：

```json
// access_token
{
  "sub": "user_id_xxx",
  "role": "user" | "admin",
  "iat": 1700000000,
  "exp": 1700000900,
  "jti": "..." // 用于黑名单
}

// refresh_token
{
  "sub": "user_id_xxx",
  "token_family": "...", // 旋转检测：每次 refresh 生成新 family，旧 family 全部撤销
  "iat": ...,
  "exp": ...
}
```

**为什么 HS256 + 独立持久化签名密钥（非 Vault 派生）**：避免引入非对称密钥管理负担；签名密钥是首次启动生成的 32 字节随机 secret，持久化在 `data_dir/auth/secret`（Unix `0600`），**独立于 Vault master key**。

> **评审修订（实施期）**：原设计写的是"HMAC key 复用 Vault master key 的 SHA256 摘要"，实测发现它会让 token 签发 / 校验依赖 Vault unlocked 状态——与 §决策 1 的"登录不要求 Vault unlocked"（§1.3 不变量 3）直接冲突：Vault 一锁，所有已签发 token 无法校验、新 token 无法签发。改为独立 secret 后，Vault relock 不会踢掉在线会话，登录路径与 Vault 状态彻底解耦。签名密钥仍是 Gateway 本机机密，token 无该 secret 无法伪造，"认证锚定在 Gateway"语义不变。

**middleware 路径**：

```rust
// core/acowork-gateway/src/http/auth_middleware.rs (新文件)
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    // 1. 跳过白名单: /api/health, /api/auth/login, /api/auth/refresh
    // 2. 从 Authorization: Bearer 抽 token
    // 3. HS256 验签 + exp 检查
    // 4. payload 注入 req.extensions_mut::<AuthContext>()
    // 5. next.run(req)
}
```

**实施记录（Phase C-2 已完成）**：

- **中间件位置**：`auth_middleware` 作为**全局层**挂在 `build_router` 的 CORS 内层、路由外层——不在 CORS 内层则 401 响应缺 `Access-Control-Allow-Origin`（浏览器读不到错误体）；不在路由外层则新增 handler 可能漏过鉴权。`state.auth_service == None` 时直接 `next.run(req)`（local 模式 no-op）。
- **白名单**（实测路径，注意实际 liveness 端点是 `/health` 而非 ADR 早期写的 `/api/health`）：`/health`、`/api/status`、`/api/bootstrap`、`/api/auth/login`、`/api/auth/refresh`、`/api/auth/logout`、`/api/auth/first-login`。`OPTIONS`（CORS preflight）无条件放行——浏览器不会带 `Authorization`。
- **`AuthContext`**：`{ user_id, role: Role, as_user: Option<String> }`，`effective_user_id()` 仅在 `is_admin()` 为真时才吃 `as_user`。`as_user` 的校验前移到**中间件**：非 admin 携带 `as_user` 直接 403（而非静默忽略），杜绝"看起来生效了"的错觉。
- **access token 校验是无状态的**（仅验签 + `exp`，不读 `accounts.json`）：这个选择把"账号被禁用后 token 还能用"的窗口上界钉死在 `ACCESS_TTL_SECS`（15 分钟），换掉每个反代请求一次磁盘解析。**refresh 是强一致执行点**：`AuthService::refresh` 每次都重读 `accounts.json` 并校验 `is_login_capable()`，所以禁用账号最多苟活 15 分钟。
- **refresh 轮换 + 复用检测（RFC 9700 §4.14.2）**：refresh token **单次使用**——每次刷新把"呈现的 family"标记为 `rotated`（`revoked_families.txt` 里 `r:{family}` 前缀）并铸造新 family。若再收到一个已 `rotated` 的 family，即判定为**泄漏或客户端重放**，**撤销该 user 的全部 family**（`{user_id}.*`），让"先刷新的窃贼"也会在受害者下一次刷新时被踢下线。`x:{family}` 前缀表示**显式撤销**（登出）——重放它只返回 401，**不**连坐该 user 的其他设备（登出手机不该杀死桌面端）。两种前缀必须区分，这是"登出"与"轮换"语义的分水岭。
- **宽限窗口 + 时间基准撤销（实施记录，修"登录后约 13 分钟必被踢下线"）**：上述严格模式有两个已在本轮收口的缺陷——① 丢包重试被判复用（原 `ponytail:` 项），② `{user_id}.*` 是**无界**条目，一旦写入就永久生效，而 `login()` 只铸造不清除登记簿，于是误判一次就**循环锁死**：登录 → 13 分钟后 refresh 401 → 弹登录 → 再登录 → 再 401。实测触发者是同一 webview 内相隔 3 ms 的两个 refresh（定时器与 401 兜底路径撞车），不是多设备。修法：
  - `revoked_families.txt` 每条条目改为 `entry\t{unix秒}`，无时间戳的 legacy 行按文件 mtime 继承（**fail closed**，存量撤销不失效），旧文件无需迁移。
  - `{user_id}.*` 的语义从"永远"改为"**撤销 `iat <= cutoff` 的全部 family**"。新登录的 `iat` 必然晚于 cutoff，因此通配条目对"下一次登录"天然免疫——误判不再需要人工清理，登录本身就是恢复路径（回归测试 `a_misjudged_replay_recovers_on_the_next_login`）。
  - `r:` 条目带时间戳，`REFRESH_REUSE_GRACE_SECS = 10` 内的重复呈现只拒绝**这一次**请求，不连坐整条链；超窗才 `revoke_user`。宽限不能无限延长：合法轮换间隔约 13 分钟 ≫ 10 秒，攻击者重放旧 token 仍会落在窗外（`refresh_rotates_and_replay_inside_grace_spares_the_chain` / `refresh_replay_outside_grace_kills_the_whole_chain`）。
  - `r:` 标记在正常流程中**每个 family 只写一次**：每次轮换铸造的是**全新** family（而非同一 family 的后继），重放在 `rotated_at` 处即被拦截，走不到第二次标记。`mark_rotated` 仍保留单调守卫（时间戳只前进不回退，last write wins），但这只是针对重试落盘 / 时钟抖动的防御——回退会缩短攻击者重放该 token 需要跨过的复用窗口。宽限窗口本身不依赖标记前进：两次相隔几毫秒的刷新共用同一个已写入的时间戳。回归测试 `rotation_stamp_tracks_the_latest_spend`。
  - `revoke_user` / `revoke_family` 签名新增 `now`；四处调用点（改密 / 禁用 / 重置 / 复用连坐）全部显式传 `now`。

**`AuthContext` 注入后下游 handler 的写法**：

```rust
pub async fn list_sessions(
    State(state): State<AppState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
    Query(params): Query<ListSessionsQuery>,
    headers: HeaderMap,
) -> Response {
    let effective_user = auth.effective_user_id(); // 普通 user = 自己; admin + as_user query = as_user
    let mut q = params;
    q.user_id = Some(effective_user);
    proxy_to_runtime_with_query(...)
}
```

### 决策 4：Session 隔离 — `SessionMeta.user_id` + `visibility` 开关 + Runtime 侧 owner 校验

**职责划分**：归属与可见性由 **Runtime** 判定（`meta.json` 在它手里），身份由 **Gateway** 提供（token 在它手里）。Gateway 只做一件事——反代前把调用者 scope 写进 `x-user-id` 头；Runtime 读该头做过滤与鉴权。**Runtime 是唯一的授权决策点**，因为它是决策所需数据（`user_id` / `visibility`）的唯一持有方；Gateway 侧再加一层 owner 校验只会制造第二份真相（要读同一份 meta，还要处理"meta 刚被删"的竞态）。这取代了原稿"Gateway 反代时 verify owner"的设计。

**schema**（`core/acowork-runtime/src/conversation.rs`）：

```rust
pub struct SessionMeta {
    // ... 现有字段 ...
    /// ADR-076: 创建该 session 的 user_id。None = 旧数据 / 无账号模式。
    /// 创建后不改（write-once）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// ADR-076: 可见性开关。None 与 `Public` 等价 —— 但只对**无主**会话如此；
    /// 创建路径给有主会话显式落 `Private`（见 §决策 4「默认值的两种含义」）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<SessionVisibility>,
}

pub enum SessionVisibility { Public, Private }
```

**两个判定谓词**（`SessionMeta::is_readable_by` / `is_writable_by`）：

| scope \ session | public（含 `visibility = None`） | private |
|---|---|---|
| admin（`x-user-id: *`） | 可读可写 | 可读可写 |
| owner | 可读可写 | 可读可写 |
| 其他 user | 可读 | **404**（不可读、不可写） |
| local（无头） | 可读可写 | 可读可写 |

**默认值的两种含义**（本次修正）：

`visibility = None` 在谓词里读作 public，而它原先同时背了两种含义：①「ADR-076 之前的遗留会话」②「新建时没表态」。第二含义在多用户下是个实打实的洞——绑 `0.0.0.0` 的部署里，alice 新建的每一段对话（含 system agent）**默认 bob 可读**，要等到有人发现锁图标没亮。

现在拆开：

- **有主会话**：创建路径（`create_frontend_session`）在写入 `user_id` 的同时落 `Some(Private)`。**写死在盘上**，不靠读路径推断——因为 `SessionListView.visibility` 会把该值下发给 Desktop 拼 🌐/🔒 图标，如果"实际私有而字段为空"，那个开关就会显示"公开"给一个谁都读不到的会话，正好把唯一的手动逃生口变成谎话。
- **无主会话**：仍写 `None`（= 公开）。这既保住 local 模式零改动（local 下 `user_id` 恒为 `None`），也保住升级前的历史数据不被追溯隐藏——`is_readable_by` 对无主会话本来就忽略该标志（没有主人可以限制给谁，遵守它只会把会话藏给所有人）。

**无主会话也分两种**（同一个洞的另一半）：

上面只堵了"有主但不表态"。真正让多用户串号的是**无主会话**——`None` 对它读作公开，而 `is_writable_by` 对无主会话同样一律放行（那是为升级前的历史数据准备的：不能因为"没有主人"就把用户锁在自己的历史之外）。于是：

- **agent 冷启动会话**：Runtime 启动时发现磁盘上没有会话，会**自动建一个**（`session_init`，为的是 `/latest-session` 立刻有东西可返、ChatPanel 不空白）。它诞生在任何账号开口之前，**没有任何人可以是它的 owner**。此前它落在"无主 = 公开"那一列 → 在多用户下**每个账号都能读、且都能写**，而 Desktop 的 `selectAgent → /latest-session` 正好会把它返回给每个账号：**第一个用户和第二个用户在同一段对话里打字**。
- **修法**：把它标成 `Some(Private)`，并让谓词承认"**无主 + Private = 无人认领**"——除 admin 外谁都读不到、谁也都写不了。语义是诚实的：没有主人的私有会话 = 不属于任何人，那就不该交给任何账号。无主 + `None` / `Public` 仍是"公开"，升级前的历史数据与 local 模式因此零改动（local 下调用者 scope 恒为 `Unfiltered`，谓词第一分支直接短路）。
- **配套**：认领权收窄。`authorize_write` 对无主历史数据是刻意宽松的（否则用户写不了自己的旧会话），但"**谁能重新共享**它"是更窄的问题——否则任一账号都能把一个共享的无主会话一键设为私有藏起来，或把无人认领的私有会话翻回公开再共享。现在 `PUT .../visibility` 对无主会话只认 admin（403），规则落在 `may_change_visibility` 一处。
- **客户端配套**：`/latest-session` 从 agent 级缓存作答，多用户下只要有两个账号用过这个 agent，它就会返回 404（不接受泄漏 id，见 §决策 4 的 404 语义）。这不是"服务还没起来"，重试治不了——ADR 原文写的行为就是"**让客户端回退到过滤后的列表**"。Desktop 现在照做：`/latest-session` 拿不到时先看自己 scope 过滤后的列表（有行就开最新那条，**不再空转 10 秒**），列表也是空的才建一条自己的（有主 + 私有）。

逐条判定不受影响（表三列语义不变），变的只是"新建的有主会话落在哪一列"与"无主会话按标志分两列"。显式 `visibility` 仍可在 `POST /sessions` 的 body 里覆盖创建默认值，per-session 开关照旧是逃生口。

- **读**（`GET /sessions`、`/sessions/{sid}`、`/sessions/{sid}/messages`、`/sessions/latest`、`/sessions/{sid}/config`）：走 `is_readable_by`。不可读一律 **404**，不用 403——403 会把这个端点变成"某 session 是否存在"的探测器，正是列表过滤要藏起来的信息。
- **写**（`open` / `close` / `DELETE` / `visibility` / `workspace` / `config` / 全部会话动作）：走 `is_writable_by`，**仅 owner 或 admin**。公开 ≠ 可改：把 session 设为 public 是"让别人能读"，不是"让别人能删"。旧数据 `user_id = None` 的会话（ADR-076 之前创建）**仅** admin / local 可写——不能因为"没有主人"就人人可删。
- **观众读公开会话时「不激活」后端会话**（见 §决策 4「观众不激活」与本节末「会话内存回收」）：`POST .../open` 是**写**操作，前端在 `can_write === false` 时**根本不发**。原因不是省一次请求，而是 `Active` / `Closed` 是 **per-session 全局状态**（不是 per-connection）：观众若能激活，它就制造了一个"自己无权关闭（close 是写授权，刻意不让旁观者拆掉 owner 的会话）、owner 也不知道被谁占着"的常驻会话——生命周期失去责任人。只读浏览**不需要**激活：历史走 `GET /messages`（读授权），事件流走 Desktop 的通配 MQTT 订阅，owner 在用（=Active）时天然能收到，owner 关了（=Closed）本来也没有东西在跑。


**`visibility` 为什么是 opt-out 而不是 opt-in**（用户决策）：`None` = 公开是**旧数据的自然语义**——ADR-076 之前创建的 session 没有 `visibility` 字段，它们本来就对所有人可见。默认 Private 会在升级瞬间把全部历史会话变成私有，那是一次**静默的数据丢失**。opt-out 让升级前后行为一致，且"设为私有"是一个明确的用户动作。若某个部署真要从"全公开"迁到"全私有"，那是一次数据迁移，不该由字段默认值顺手完成。

**过滤必须在分页之前**：`scan_sessions_async` 先按 scope 过滤，再分页——否则 `total_count` / `total_pages` 会把调用者看不见的行算进去，等于用分页元数据泄漏"你还有 N 个别人的 session"。

```rust
// 调用者的身份范围，由 Gateway 注入的 x-user-id 头解析
pub enum SessionScope { Unfiltered, User(String) }
// "*" → Unfiltered（admin）; 具体 id → User(id); 头缺失 → Unfiltered（local 模式）
```

`Unfiltered` 同时覆盖"admin 看全部"与"local 模式无账号系统"——两者在数据面完全同构，不必分成两个变体（多一个变体就多一处忘了处理的分支）。

**会话控制面从 MQTT 迁到 HTTP**（对 ADR-034 §11.2 的反转）：

原稿假设的链路 `POST /api/agents/{id}/sessions` → Gateway 反代 → Runtime 读 `X-User-Id` **当时并不存在**：Desktop 建 session 走的是 MQTT 控制面——直接向 broker 发一条 `CreateSession`（消息体为空），不经过 Gateway HTTP，没有反代 hop 可以挂头；而 MQTT ACL 是空壳（`can_publish` 丢弃 topic 参数），broker 无法给消息打身份标记。**身份进不了 MQTT 控制面，所以把控制面搬到能带身份的地方。**

create / open / close / delete 四条操作迁到 HTTP（usecase 全部现成，只换接口层）：

| 操作 | HTTP 路由（Gateway 反代 → Runtime） | Runtime usecase |
|---|---|---|
| create | `POST /api/agents/{id}/sessions` | `create_frontend_session` + `set_user_id` + `set_visibility` |
| open | `POST /api/agents/{id}/sessions/{sid}/open` | `resume_session`（ADR-038 激活状态机） |
| close | `POST /api/agents/{id}/sessions/{sid}/close` | `close_session` |
| delete | `DELETE /api/agents/{id}/sessions/{sid}` | `delete_session` |
| switch workspace | `PUT /api/agents/{id}/sessions/{sid}/workspace` | `route_workspace_switch` |
| share / unshare | `PUT /api/agents/{id}/sessions/{sid}/visibility` | `set_visibility` + `write_meta` |
| switch model | `PUT /api/agents/{id}/sessions/{sid}/config`（`{model, provider}`） | `apply_config` |
| reasoning effort | `PUT /api/agents/{id}/sessions/{sid}/config`（`{reasoning_effort}`） | `apply_config` |
| rename title | `PUT /api/agents/{id}/sessions/{sid}/config`（`{title}`） | `apply_config` 的 `title` 分支 |

**为什么只有 workspace 需要新端点，另外三条复用 `PUT .../config`**：MQTT 那 4 条写命令在 Runtime 里本来就落在这两条路径上（`gateway_loop` 的 `ModelSwitchAction` / `ReasoningEffortAction` 先试 `svc.apply_config`，`WorkspaceSwitchAction` 直接调 `route_workspace_switch`）。`apply_config` 的 `title` 分支与 `update_title_force` 行为等价（截断 + `title_set` + `write_meta` + `notify_config_change` + `config_version++`）。但 **`apply_config` 的 `workspace_id` 分支只做内存赋值 + `write_meta`**，不更新 `current_work_dir`、不重推 per-session workspace context / prompt 文件——把它当等价物用会让工具在旧目录里干活而 meta 声称已切换，所以 workspace 单独走 `route_workspace_switch`。

**关键：MQTT 侧的用户操作命令已全部"删除"，不是 deprecated、也不是拒收。** 分两批：
**第一批** 8 条会话作用域写命令（`create_session` / `delete_session` / `close_session` /
`open_session` / `update_session_title` / `model_switch` / `reasoning_effort` /
`workspace_switch`）；**第二批** 8 条会话动作（`chat_message` / `stop` /
`continue_execution` / `approval_decision` / `question_answer` / `cancel_tool` /
`compress_action`，以及 `compress_action` 的重复命令 `compact_context`）。
`ControlCommand` 的对应 proto 字段已移除并**整体重排为连续**（开发期无兼容需求，不留空号），
`ControlAction` / `InboundMessage` 的对应变体、`control_action_to_inbound` 的映射臂、
Gateway `mqtt/client.rs` 与 Tauri `chat_mqtt.rs` / `mqtt_client.rs` 的命令名映射表全部删除。
项目仍在开发期、无兼容需求，所以连"发出去被拒收"这一步都不需要——**能力在类型层面不可表达**：

| 已删除的 MQTT 命令 | 为什么不能留在 MQTT |
|---|---|
| `create_session` | 消息不带身份 → 建出来的 session 无主（`user_id: None`），而 ownerless session 是"任何登录账号可写"（见 §决策 4 的 `is_writable_by`），等于公开可删 |
| `delete_session` / `close_session` | 无身份 → 无法校验 owner，任何 broker 客户端可删掉别人的会话，且 MQTT 是 fire-and-forget，受害者连错误都看不到 |
| `open_session` | 同上（可激活他人会话） |
| `update_session_title` / `model_switch` / `reasoning_effort` / `workspace_switch` | 同上：可静默改写他人会话的标题 / 模型 / 思考深度 / 工作区（工作区还包括让工具在攻击者指定目录里读写） |

**第二批（会话动作）为什么也搬**：第一版曾判断这些命令"作用在已经打开的那个会话上，不承载归属决策，留在 MQTT 延迟更低，换不到授权收益"——但**身份缺失的代价被低估了**：它们作用在"哪个会话"完全由消息自己声明，broker 上任何客户端都能把 `approval_decision{approved:true}` 投进别人的会话（= 在他人工作区执行任意命令），或把 `chat_message` 投进别人的会话。搬走之后分工彻底清晰：**HTTP = 用户主动触发（必然带身份），MQTT = 后端主动上报**。延迟上并不吃亏——HTTP handler 只做"鉴权 + 入队"立即返回 `202`，真正的 token 流仍走 MQTT 事件。对应的 HTTP 端点：
`messages` / `stop` / `continue` / `approval` / `answer` / `cancel-tool` / `compress`
（见 [http.md §5.6](../../protocols/zh/http.md)）。

留在 MQTT 的只剩**非用户动作**：`intent`（Runtime → Runtime，cron / 跨 agent）与
`active_heartbeat`（Desktop → Runtime 存在性心跳）——它们不携带任何归属决策，也没有
"投进别人会话"的语义。第一版那道"反过度迁移"护栏单测
`gateway_loop.rs::chat_traffic_still_maps_over_mqtt` 因此**已删除**：它守护的正是这批被搬走的
命令；留下的两条不需要护栏——`ControlAction` 只剩 `IntentReceived` + `ActiveHeartbeat`，
编译器就是边界。

**MQTT 侧的身份传递**本期暂缓（原计划 ADR-077 的课题）——控制面搬走之后，MQTT 上已没有任何**需要按账号授权的命令**（只剩 `intent` / `active_heartbeat`），所以它不再是**写侧**阻塞项；但**读侧的 per-user 订阅授权**（事件面保密性）仍是已知缺口，见 §5.5 与 [mqtt.md §10](../../protocols/zh/mqtt.md)。

**`can_write` 下发（前端 UI 禁用的唯一依据）**：`SessionSummary`（Runtime）与 `SessionInfo`（TS）新增 `can_write = SessionMeta::is_writable_by(&scope)`。前端**不从 `visibility` 反推**权限：public 是"别人能读"，admin / local 模式还能写自己没有的会话。字段缺省时前端按 `true` 处理（老 Runtime 退化为"先试，后端 403"，而不是把全部控件锁死）。

`create` 顺带解决了另一个老问题：MQTT 命令无法返回新 session id，HTTP 响应可以。Desktop 仍等 `session_created` MQTT 事件取 sid，所以下游逻辑零改动——`ponytail:` 这次迁移**刻意**没顺手改这个（改动面越大越难回滚）；想省掉那一次事件往返时，改为直接用 POST 响应体即可。

**关键安全检查点**（grep 必须覆盖的清单）——已全部落地：

- `Runtime::GET /sessions`：`scope` 入参 + **分页前**过滤
- `Runtime::GET /sessions/{sid}` / `.../messages` / `.../latest` / `.../config`：`authorize_read`（**受 `visibility` 控制** —— private 且非 owner → 404）
- `Runtime::POST .../open`：`authorize_write`（**不受 `visibility` 控制** —— `open` 是写操作：它把会话激活进内存。公开会话的非 owner **不激活**，前端在 `can_write === false` 时根本不发这个请求，因此这里刻意不接受"只读观众"。见 §决策 4 的「观众不激活」）
- `Runtime::POST .../close` / `DELETE /sessions/{sid}` / `PUT .../visibility` / `PUT .../workspace` / `PUT .../config` / `POST .../files` / 第二批 7 条会话动作：`authorize_write`（**不受 `visibility` 控制** —— 见下）
- **`visibility` 只影响读，不影响写**：`is_writable_by` 完全不看该字段。public 的语义是"让别人能读"，不是"让别人能改"——否则把 session 设为公开就等于交出 close / delete / 改 config 的按钮。所有写路径恒定为 owner 或 admin。
- `GET /files/{document_id}` **是唯一的例外，且目前无校验**：blob 按 `document_id` 全局存储（`<work_dir>/files/`），读路径拿不到 sid，无法反查归属。**这是已知 ceiling，见 §5.5。**
- `Runtime::POST /sessions`（create）：owner 从 `x-user-id` 头写入，**不接受 body 指定 owner**（body 只接受 `workspace_id` / `model` / `provider` / `visibility`）
- `Runtime::GET /sessions/latest`：缓存是 agent 级的（启动时写入），可能指向调用者读不到的 session → 走 `authorize_read`，失败 404 让前端回落列表（`ponytail:` 非 owner 多一次往返，换来的是"不泄漏 id"；替代方案是每次启动调用做一次全量扫描）
- `Gateway → Runtime` 的所有 `/api/agents/{id}/sessions/*` 反代：必须经过 `auth_middleware`（全局层）

**Admin `as_user` 机制**（"以 user X 视角看"，但**不是身份冒用**）：

```text
GET /api/agents/{id}/sessions?as_user=<user_id>
# admin token + as_user query → 返回该 user 的 session 列表
# 普通 user token + as_user query → 403
# 普通 user token + 不传 as_user → 仅自己的 session
```

**为什么 `as_user` 不做成"临时切换身份"**：避免 XSS / CSRF 攻击链——若前端可任意切换身份执行写操作，cookie/header 注入就能横着走。`as_user` 只用于**只读视图**（list / get_messages / get_state），写操作（POST / DELETE）永远用 token 实际身份。

#### Phase D 实施记录（已落地）

**已落地**：

1. **Runtime schema（向后兼容）**：`SessionMeta.user_id: Option<String>` + `SessionMeta.visibility: Option<SessionVisibility>`（均 `#[serde(default, skip_serializing_if = "Option::is_none")]`），无该字段的旧 `meta.json` 加载为 `None`（= 公开、无主人）；`ConversationSession::set_user_id`（**write-once**：首次写入生效，之后试图**改成别的值**会被拒绝并打 warn，幂等重写同值不报错——meta 的 `user_id` 是不可变事实）+ `set_visibility` / `is_private`。测试：`session_meta_user_id_is_backward_compatible`、`set_user_id_is_write_once`。
2. **Gateway 头部卫生（安全不变量）**：反代**逐字转发**入站头（`proxy_to_runtime_with_method` 只过滤 hop-by-hop），所以客户端自带的 `x-user-id` 会先于 Gateway 抵达 Runtime 并冒充他人 session。中间件因此在**每个**请求上先 `remove(x-user-id)`，鉴权通过后再按 token 重新注入：

   | 身份 | 注入的 `x-user-id` |
   |---|---|
   | 普通 user | 自己的 `user_id` |
   | admin，无 `as_user` | `*`（不过滤） |
   | admin + `as_user=<id>` | 该 `id`（非 admin 携带 `as_user` → 403；非法形态的 id 也 403，绝不退化成"不过滤"） |
   | local 模式 | 只删不注（账号系统整体 no-op，Runtime 侧 = `Unfiltered`） |

   `x-user-id` 因此**只有 Gateway 一个可信写入方**。测试：`middleware_strips_client_scope_and_injects_the_token_scope`（走真实 `build_router` 层：伪造头被剥、无 token 401、admin 拿到 `*`、`as_user` 收窄、非法 `as_user` 403）。
3. **Runtime scope-aware 读路径**：`SessionScope::from_header_value`（`*` → `Unfiltered`，具体 id → `User`，头缺失 → `Unfiltered`）；`scan_sessions_async` 加 `scope` 入参并在**分页前**过滤（`total_count` / `total_pages` 反映调用者可见行数）；`SessionInfo` 加 `visibility` 字段；`SessionMetadataService::list_sessions` 加 `scope` 参数；`GET /sessions/{sid}` / `/messages` / `/latest` 走 `authorize_read`（不可读 → 404）。测试：scope 判定 + visibility 三态 + 分页计数。
4. **HTTP 会话控制面**（取代 MQTT，第一批）：新建 `core/acowork-runtime/src/http/session_control.rs`（第一批 7 个 handler + `authorize_read` / `authorize_write` 辅助 + `PUT .../visibility` + `PUT .../workspace`），`SessionManager::create_frontend_session` 加 `user_id` / `visibility` 入参，新增 `SessionManager::resume_session`（封装 ADR-038 激活状态机，`gateway_loop` 的 MQTT `open_session` 改为调它）；Gateway 侧 `proxy.rs` 加 6 条反代路由（含 body / method 透传），Desktop 侧新建 `src/lib/session-control.ts` 封装 HTTP 调用替换 `invoke("mqtt_publish_control")`——`create` / `open` / `close` / `delete` / `visibility` / `workspace` / `patchSessionConfig`（model / reasoning / title 三合一），共替换 5 个调用点，并删掉 `setSessionWorkspaceMqtt` 这个已经名不副实的旧名。测试：`conversation.rs` 的 `visibility_and_ownership_gate_read_and_write` / `session_scope_from_header_value` / `scan_filters_by_scope_before_paginating`（含 `can_write` 下发断言：owner 可写、他人 public 会话可读不可写、ownerless 依旧可写、admin 全可写）；handler 接线由既有 HTTP server 测试证明（`test_session_config_get_unknown_session` 现在返回 404、`test_http_upload_file_docx_lands_with_real_extension` 要求 session 先存在）+ Desktop `chatStore.test.ts`。

5. **MQTT 写路径删除（两批）**：第一批 8 条生命周期写命令 + 第二批 8 条会话动作（`chat_message` / `stop` / `continue_execution` / `approval_decision` / `question_answer` / `cancel_tool` / `compress_action` / `compact_context`）的 proto 字段、`ControlAction` / `InboundMessage` 变体、命令名映射表全部删除；`ControlCommand` 字段号整体重排为连续，现只余 `Intent` + `ActiveHeartbeat` 两条非用户动作。原护栏单测 `gateway_loop.rs::chat_traffic_still_maps_over_mqtt` 随第二批迁移一并删除（它守护的正是被搬走的命令）；边界改由类型系统承担——`ControlAction` 只剩 `IntentReceived` + `ActiveHeartbeat`。

6. **前端读权限并禁用写控件**：`SessionInfo.can_write` → `ChatPanel` 派生 `readOnlySession` → `ModelMenu` / `ReasoningEffortMenu` / `WorkspaceSelector` 传入 `readOnly`；`ToolbarDropdownTrigger` 新增 `disabled`（`disabled` + `aria-disabled` + `cursor-not-allowed opacity-50`），一处改动覆盖三个控件。控件是**禁用而非隐藏**——共享的只读会话仍需显示"当前用的是哪个模型 / 哪个工作区"。i18n 键 `chatPanel.readOnlySession` 五种语言齐全。

7. **第二批会话动作迁移（ADR-076 §决策 4 收官）**：`core/acowork-runtime/src/http/session_control.rs` 追加 7 个 action handler（`messages` / `stop` / `continue` / `approval` / `answer` / `cancel-tool` / `compress`）+ 共享的 `dispatch_session_action` 辅助——同步只做"鉴权 + 入队"（`202` / `403` / `404`），执行结果仍走 MQTT 事件回流；Gateway `proxy.rs` 加 7 条反代路由；Desktop `session-control.ts` 加 7 个函数，`chatStore.ts` / `ChatPanel.tsx` / `ContextUsageIcon.tsx` 共 8 个调用点从 `invoke("mqtt_publish_control")` 改为 `fetch`。`idle_watcher.record_inbound()` 从 MQTT 控制回环挪到 HTTP→MQTT 共享转发点，修复 HTTP 发起的动作不刷新 session 活跃度导致的"自睡"。`compact_context` 作为 `compress_action` 的重复命令（SessionTask 分支逐字节同构、无调用者）一并删除。测试：`session_actions_are_owner_gated_and_keep_their_payload`（owner 门禁 + payload 保留）+ 3 个 e2e 改 HTTP 驱动；协议字段号重排后 `node_proto_golden.rs` 的 6 组 golden hex 重算。

8. **公开会话的只读浏览（观众不激活）**：`ChatPanel` 输入框在 `readOnlySession` 时禁用 + 专用 placeholder；`SessionVisibilityToggle`（composer 工具行 🌐/🔒）接线 `PUT .../visibility`；`chatStore.closeTab` / `openSession` 在 `can_write === false` 时**分别跳过** `POST /close` 与 `POST /open`。第 8 条的关键决定是"观众不激活"——理由见上文「观众读公开会话时『不激活』后端会话」：`Active`/`Closed` 是 per-session **全局**状态，观众激活会造出一个"自己无权关、owner 也不知道被谁占着"的常驻会话，而要正确回收它就必须引入观察者引用计数。只读浏览不需要激活（历史走 `GET /messages`，事件走通配 MQTT 订阅，owner 在用即 Active 即实时）。测试：Desktop `src/stores/sessionSharing.test.ts`（可见性乐观翻转 + 回滚；`can_write === false` 时 open / close 零请求）。

**`ponytail:` 有意留下的 ceilings**：

- **MQTT 控制命令已删（此项已结清）**：全部用户操作命令（生命周期 8 条 + 会话动作 8 条）的 proto 字段、Runtime 变体与映射表已全部删除，字段号随后**整体重排为连续**（开发期无兼容需求，不留空号）。`ControlCommand` 现只余 `Intent` + `ActiveHeartbeat` 两条非用户动作。
- **`PUT .../config` 不做 `route_*` 回退**：MQTT 时代的 `ModelSwitchAction` / `ReasoningEffortAction` 在 `apply_config` 报错时会回退到 `SessionManager::route_model_switch` / `route_reasoning_effort`（覆盖"会话不在 config service 内存表里"这种场景）。HTTP `PUT .../config` 没有这条回退，直接 500。实际不可达：这三个控件都绑定 `activeSessionId`，而活跃会话必在表里；且迁移前 `setSessionContextWindow` 就已经是无回退的裸 `PUT .../config`，与邻居保持一致优于与死掉的 MQTT 路径保持一致。
- **`GET /sessions/latest` 对非 owner 多一次往返**：见上文安全检查点。缓存值不解 scope。曾经的理由是"解 scope 需要一次全量扫描"——该前提已被 `meta/` 内存索引推翻（见 §5.5「已结清：`meta/` 目录的全量扫」），现在解 scope 只是对已排序行的内存过滤，不再需要新缓存。**行为暂不改**：本轮是纯性能改动，改 `/latest` 的可见性语义要有自己的测试与文档；升级路径已开放——用 `with_meta_index` 取调用者可见的最新行即可。
- **`visibility` 开关已有 UI（此项已结清）**：`SessionVisibilityToggle` 挂在输入框工具行，仅 owner 可点（`can_write === false` 渲染为 disabled）。**新会话默认值已结清（本次）**：有主会话创建即 `Private`，无主（local / 升级前数据）维持 `None` = 公开——见 §决策 4「默认值的两种含义」。原先那句「per-agent 默认可见性仍未做且刻意留白」**问题已消解**：被留白的是「admin 能否强制某个 agent 的会话可见性」，那是一个 admin 级策略（"这个 agent 的对话是团队共享日志"），与"新会话默认私有"不是同一层，也不再有需求把它当成默认值的替代品；真要做得是 `accounts.json`/配置里的 admin 字段，届时另立决策。
- **会话内存回收与会话生命周期解耦（未解决，独立议题）**：`SessionManager::evict_idle_sessions` 定义了但**全仓无调用者**（死代码）；实际回收只有 agent 级自动休眠（`process::exit`），而它的续期含**全局** `ActiveHeartbeat`（任何 Desktop 选中该 agent 都续期，不带 user 身份）。因此"观众不该激活会话"是与该缺口直接相关的设计约束：不再给这个没有 per-session GC 的系统增加参与者。详见 §5.5。

### 决策 5：管理员角色 — `role = "admin"` 绕过过滤 + 特殊权限

**admin 创建流程**：
1. Gateway 首次启动时，配置文件 `gateway.toml` 含 `bootstrap_admin = { username, password }`
2. 若 `accounts.json` 为空 → 启动时强制创建该 admin 账号
3. 后续 admin 通过 admin token 创建其他 admin（需 `username` + `display_name`，密码由被创建者首次登录时设置——首次登录流程：`POST /api/auth/login?invite_token=<xxx>`）

> **评审修订（实施期）**：`bootstrap_admin` 是**仅首次启动有效**（first-boot-only）的引导凭据，不是"永远必须配置"的常驻项。实施时明确为：
> - `accounts.json` **为空** + `AUTH_MODE=multi_user` → ~~必须配 `bootstrap_admin`，否则拒绝启动（fail-fast）~~（**v2 修订**：空库改为 seed 无密码 admin + 受限模式，见 §决策 12 v2 / v3；原为"拒启动"）；
> - `accounts.json` **非空** → `bootstrap_admin` **被忽略**（仍配置则打 warn 日志）。
>
> 理由：若每次启动都强制要求配置，等于把引导密码变成**常驻的第二组 admin 凭据**——它躺在明文 `gateway.toml` 里、绕过改密流程、且无法吊销，是一个长期敞口。Gitea / Jenkins / GitLab 的引导凭据同样是首启专用。创建之后该账号完全由正常的改密 / 禁用流程管辖。
> 检查点落在 `Gateway::new`（构造期 `Result`），所以是真正的"拒绝启动"，而非"启动后打日志"。

**admin 能力清单**：
- ✅ `GET /api/users` 看到全部账号（含 `last_login_at`、`disabled_at`、但不含 `password_hash`）
- ✅ `GET /api/users/{any_id}` 看到任意账号元数据
- ✅ `GET /api/agents/{id}/sessions?as_user=<any>` 看到任意 user 的 session 列表
- ✅ `GET /api/agents/{id}/sessions/{sid}/messages?as_user=<any>` 看到任意 user 的 session 消息
- ✅ `POST /api/users/{id}/disable` 软删除账号（`disabled_at` = now）
- ✅ `POST /api/users/{id}/reset-password` 生成一次性 invite_token（24h 过期）
- ❌ 不能改他人密码（必须走 reset → 首次登录改密流程）
- ❌ 不能 `as_user` 执行写操作（POST / DELETE 仍按 token 实际身份校验）

**admin 软删除**（`disabled_at`）：保留 `user_id` 和 `session.user_id` 不变；disabled 账号的 session 仍可读（admin 视角），但 disabled 账号无法登录。注销 vs 软删除区分见决策 6。

### 决策 6：账号生命周期 — 注册 / 登录 / 改密 / 注销

**API 表**：

```text
POST   /api/auth/login              {username, password} → {access_token, refresh_token}
POST   /api/auth/refresh            {refresh_token}      → {access_token, refresh_token}
POST   /api/auth/logout             {refresh_token}      → 204
POST   /api/auth/change-password    {old_password, new_password} → 204  # 需 access_token
GET    /api/auth/me                 → UserAccount（脱敏）
POST   /api/users                   {username, display_name, password} → UserAccount
                                       # admin 创建；或开放注册模式（见下）
POST   /api/users/{id}/disable      → 204  # admin only
POST   /api/users/{id}/reset-password → {invite_token}  # admin only
POST   /api/auth/first-login        {invite_token, new_password} → {access_token, refresh_token}
DELETE /api/users/{self}            → 204  # 自己注销（软删除）
```

**注册模式开关**（gateway.toml）：

```toml
[multi_user]
registration_open = false   # 默认 false：只有 admin 可创建账号
allow_public_signup = false # 极端开放模式（仅 demo 用）
```

**谁可以建号 + Desktop 入口**（本轮结清）：
- `POST /api/users` 永远需要**已认证**的调用者——`allow_public_signup`（匿名注册）**未接线**，它要求把 `/api/users` 移出认证中间件的白名单之外，是无认证攻击面的净扩张（无凭据者可刷号、猜 invite_token），在本 ADR 的目标部署（本机 / 小团队）没有对应场景，YAGNI。
- 因此 `registration_open = true` 的语义是"**任何已登录账号都可以邀请新账号**"，而不是"任何人都能注册"。创建出的账号 role **硬编码为 `user`**（`Role::Admin` 只能由 admin 显式指定），所以非 admin 无法借此提权。
- 这个开关此前只有后端在跑（handler 已按 `!ctx.is_admin() && !registration_open()` 判 403 并有测试），**Desktop 无入口**：`Users (N)` 分组的 "+" 按钮写死 `isAdmin`。现补齐——`GET /api/status` 新增 `registration_open: bool`（无认证可读，与 `auth_mode` 同一条部署策略面；账号系统未运行时恒为 `false`，避免前端给出一个必然 403 的按钮），Desktop `fetchAuthPolicy()` 一次探测同时取回 `auth_mode` + `registration_open`，非 admin 在开关打开时才看到 "+"。回归测试：Desktop `UserList.registration.test.tsx`（非 admin + 开 → 有按钮；非 admin + 关 → 无按钮；admin → 恒有）。
- 非 admin 建号后**看不到自己创建的账号**（他们的 `Users (N)` 只列自己，`GET /api/users` 是 admin-only）——这是刻意的：该流程是"建号 + 交出 `invite_token`"（`InviteTokenModal` 会展示），不是账号管理。非 admin 的 `onCreated` 因此**不触发** `reload()`（那会打 admin-only 端点拿到 403 并亮出加载失败横幅）。

**注销 vs 软删除**：
- 自己 `DELETE /api/users/{self}` → `disabled_at = now`；保留所有 session、聊天记录、avatar 等（数据可被 admin 恢复）。
- admin `POST /api/users/{id}/disable` → 同上，但 admin 恢复需要二次操作。
- **不提供硬删除**：账号相关的 session / 聊天是历史数据，硬删会破坏引用完整性。

**改密强制流程**：
1. 改密：必须提供 `old_password`，新密码 Argon2id 重新哈希。
2. 改密成功后：撤销该 user 的**所有 refresh_token**（`token_family` 全杀），强制重新登录。
3. 首次登录改密：`invite_token` 单次有效（用后即焚），绑定到 `user_id`。
4. 密码策略（gateway.toml）：`min_length = 8`，`require_digit = true`，`require_mixed_case = false`。

### 决策 7：Desktop UI — 账号切换 + 侧栏 User 折叠分组

**顶栏账号菜单**（替换当前"用户偏好"入口）：

```text
[头像] 大鱼 ▾
       ├─ 切换账号...    → 弹登录 modal（清空 chatStore + 重连 MQTT）
       ├─ 修改密码      → 弹改密 modal
       ├─ 注销当前账号  → 二次确认 → DELETE /api/users/{self}
       ├─ ─────────
       ├─ 用户偏好      → 旧 UserProfile 编辑（language/timezone/avatar）
       └─ 退出登录      → 清 token + 回登录页
```

**账号切换实现**：

```ts
// apps/acowork-desktop/src/stores/authStore.ts (新)
async function switchAccount(username: string, password: string) {
  // 1. POST /api/auth/login → tokens
  // 2. localStorage["acowork.auth.tokens"] = tokens
  // 3. reset(): chatStore, agentStore, sessionStore, userProfileStore
  // 4. mqttClient.disconnect() + reconnect()（携带新 token）
  // 5. fetchAgents() / fetchUsers() / fetchSessions()
}
```

**侧栏 User 折叠分组**（[AgentList.tsx](apps/acowork-desktop/src/components/agent-list/AgentList.tsx) 同级渲染）：

```text
┌─ Agent (12) ──────────┐
│  ▶ Node A (5)         │  ← 现有 partitionAgentsByNode
│  ▶ Node B (7)         │
├─ Users (3) ───────────┤  ← 新增 partitionAccounts
│  ▶ 大鱼 (admin)       │     admin 视图下点击进入"以该 user 视角看"
│  ▶ Alice              │
│  ▶ Bob                │
└───────────────────────┘
```

**`partitionAccounts` 复用 partition 范式**（[partitionAgentsByNode.ts](apps/acowork-desktop/src/components/agent-list/partitionAgentsByNode.ts) 同结构）：

```ts
// apps/acowork-desktop/src/components/user-list/partitionAccounts.ts (新)
export function partitionAccounts(accounts: UserAccount[]): AccountGroup[] {
  // 与 partitionAgentsByNode 同形：单一折叠分组 "Users (N)"，
  // 默认折叠，点击展开；admin 视图下每行可点击触发 "以该 user 视角看 session"
}
```

**为什么 User 列表不像 Agent 那样按 Node 分组**：用户账号天然是 Gateway 维度，不存在"用户的 Node"概念——用户与 node 是正交维度（一个 admin 可以在多个 Node 上管理 agent）。强行按 Node 折叠反而制造噪音。

### 决策 8：用户-用户聊天 — Gateway 侧 conversion.json / jsonl

**存储布局**（**全部在 Gateway 机器**）：

```text
data_dir/
└── users/
    ├── {user_a_id}/
    │   ├── account.enc                    # 账号加密扩展字段（决策 2）
    │   └── chats/
    │       ├── {user_b_id}/               # min(a,b) 字典序
    │       │   ├── conversation.json      # meta（类比 SessionMeta）
    │       │   └── conversation.jsonl     # 消息流（类比 jsonl）
    │       └── {user_c_id}/
    │           └── ...
    └── {user_b_id}/
        └── chats/
            └── {user_a_id}/               # 同 min(a,b) 路径，无重复
                ├── conversation.json
                └── conversation.jsonl
```

**为什么按 `(min, max)` 字典序**：避免双向重复（A→B 和 B→A 写到同一目录），简化同步逻辑。**不**支持 group chat（超出本期范围；如需后续可扩为 `groups/{group_id}/`）。

**conversation.json schema**：

```json
{
  "schema_version": 1,
  "chat_id": "min_user_a__max_user_b",
  "participants": ["user_a_id", "user_b_id"],
  "created_at": "2026-10-15T...",
  "last_active_at": "2026-10-15T...",
  "last_message_preview": "...",
  "unread_count_a": 0,
  "unread_count_b": 3,
  "version": 42
}
```

**conversation.jsonl 一行一条**：

```json
{"ts":"2026-10-15T...","from":"user_a_id","kind":"text","body":"..."}
{"ts":"...","from":"user_b_id","kind":"image","body":"...","attachments":[{"id":"...","filename":"...","mime":"image/png","size":12345}]}
{"ts":"...","from":"user_a_id","kind":"document","body":"...","attachments":[{"id":"...","filename":"spec.pdf","mime":"application/pdf","size":67890}]}
```

**kind 集合**：`text` / `image` / `document`（**本期不支持** voice / video / reaction / edit / delete，遵循 YAGNI）。

**附件的传输细节（已实现）**：

- **限额**：`image/*` 25 MiB，其余 100 MiB，按**客户端声明的 mime** 选档——因此 mime 在入库前先被规整（裸 `type/subtype` token 对，否则回落 `application/octet-stream`），这也是它后来被回显成响应头的前提。
- **body 上限**：Gateway 根路由的 `GLOBAL_BODY_LIMIT` 是 64 MiB，低于 100 MiB 的文档额度，所以上传路由**给自己单独抬到 101 MiB**（`DefaultBodyLimit` 挂在 `chat_routes()` 上，而不是抬全局）。回归测试 `the_upload_route_raises_the_global_body_limit`。
- **下载**：响应带 `Content-Disposition: attachment` + `X-Content-Type-Options: nosniff`。存储的 mime 是客户端声明的，若允许 inline 渲染，`text/html` 就是一段跑在 Gateway 源上的脚本。图片仍按原 mime 返回（`<img>` 加载不受 `attachment` 影响，能正常显示）。
- **文件名**：`file_name` 取最后一段路径、剥掉控制字符与引号、截 200 字符；落盘名不受它影响，只因它会被插进 `Content-Disposition`。同时下发 `filename*=`（RFC 5987，UTF-8 百分号编码），CJK 文件名才不会退化成下划线。
- **读**：附件下载与消息读取同权限（self-or-admin + 必须是参与者），既不是参与者也不是 admin 一律 404——不泄露某个附件是否存在。id 在拼路径前校验为 UUID。

**附件存储（已实现，与原稿的偏差见下）**：

```text
data_dir/users/{min(a,b)}/chats/{max(a,b)}/files/{id}       附件本体（{id} = UUIDv4）
data_dir/users/{min(a,b)}/chats/{max(a,b)}/files/{id}.json  附件元数据（filename / mime / size）
```

**为什么不用原稿的 `{message_id}_{filename}`**：① 上传发生在**发送之前**（客户端先传文件拿到 `id`，再发一条引用 `id` 的消息），此时 `message_id` 尚不存在；② 把用户提供的字符串拼进路径同时引入穿越与重名两个问题。落盘名改用不透明 UUID，元数据放 sidecar，**blob 先写、元数据后写**——中间崩溃只会留下一个没人能引用的孤儿 blob，不会留下指向空文件的元数据。

**消息里的附件只有 id**：`POST .../messages` 的 `attachments` 是 **id 数组**，不是对象数组。名字 / mime / size 全部由 `files/{id}.json` 解析后回填，客户端无法声明它没上传过的东西；`kind` 同理由服务端从 mime 推导（`image/*` → `image`，其余 → `document`），不接受客户端传 `kind`——否则一个 PDF 可以被标成图片。

**为什么放 Gateway 而非 Runtime**：用户聊天与 agent 无关，是 Gateway 维度的横向数据；放到 Runtime 会触发 ADR-009 §5.4 边界问题——需要为它新造 Runtime 入口，复杂且无收益。**这是 ADR-009 §5.4 的显式例外**，在本文 §5.4 显式落字。

**API**：

```text
GET    /api/users/{self}/chats                              → 聊天列表（含每个 chat 的 last_message_preview + unread_count）
GET    /api/users/{self}/chats/{other_user_id}/messages    → 该对话消息分页（offset/limit 同 ADR-050）
POST   /api/users/{self}/chats/{other_user_id}/messages    {body, attachments: [id]} → 201 + 消息全文（含服务端回填的 attachments）
POST   /api/users/{self}/chats/{other_user_id}/read        → 清空自己的 unread_count
POST   /api/users/{self}/chats/{other_user_id}/files       multipart → 上传图片/文档，返回 attachment_id
GET    /api/users/{self}/chats/{other_user_id}/files/{aid} → 下载附件
```

**Admin 权限**：
- ✅ admin 可读任意 `chats/`（应急调查需要）
- ❌ admin 不能 POST 消息冒充他人（消息 `from` 字段强制 = token.sub）
- ❌ admin 不能修改 unread_count（只能读）

### 决策 10：PM / Doc 反代身份注入 — `X-Actor` 从硬编码 `human` 改为真实 `user_id`

**现状**（单一用户假设下引入的常量，[pm_proxy.rs](core/acowork-gateway/src/http/pm_proxy.rs#L145)，[doc_proxy.rs](core/acowork-gateway/src/http/doc_proxy.rs#L145) 同构）：

| 路径 | 策略 | 注入值 |
|---|---|---|
| `/api/pm/*`、`/api/doc/*`（REST，Desktop） | 丢弃客户端自报 `X-Actor`，注入可信值 | 恒为 `"human"` |
| `/api/pm/mcp`、`/api/doc/mcp`（MCP，Agent） | 校验 `X-MCP-Actor` ∈ Gateway `installed_agents` 后透传；否则剥离（→ 匿名，仅只读工具） | agent instance_id |

**决策**："REST 面 = 人类操作面"这一安全语义**保留**，但"人类"的表示从全局单例常量升级为账号身份——反代注入值从 `"human"` 改为 `AuthContext.effective_user_id`（决策 3 的 token 身份，经 auth_middleware 注入）：

```text
POST /api/pm/projects  →  auth_middleware 解析 token → AuthContext
                       →  反代注入 X-Actor: <effective_user_id>   （不再硬编码 "human"）
```

**PM 消费端语义联动**（acowork-pm，值来自 header）：

| 消费点 | 现状（`"human"` 常量） | 多用户后（user_id） |
|---|---|---|
| `create_project.created_by` | `"human"` | 真实 user_id |
| `create_project` 自举（[tree.rs:505](core/acowork-pm/src/store/tree.rs#L505)） | `created_by != "human"` → 自动加入 members | User / Agent 创建者都自动加入 members（见决策 11） |
| `create_task` review_status（[tree.rs:762](core/acowork-pm/src/store/tree.rs#L762)） | `created_by == "human"` → NotRequired | 已登录用户创建 → NotRequired（判定按 `kind`，见决策 11） |
| `ensure_assignee_is_member`（联动指派） | `assignee ∈ ∅ ∪ members ∪ {"human"}` | `assignee ∈ ∅ ∪ members`，无 `"human"` 特例（见决策 11） |

**MCP 路径不变**：`X-MCP-Actor` 校验对象是 agent instance_id（ADR-073 身份），与用户账号正交，multi-user 不改变该校验语义。

**安全检查点**：
- `build_trusted_headers` 的 REST 分支必须从 `Extension(auth).effective_user_id` 取值，**不得**回退到常量、也不得接受客户端自报值；auth_middleware 未生效时 PM/Doc REST 反代应 401（与决策 3 的全局强制一致）。
- `X-Actor` 值域从 `"human" \| instance_id` 变为 `user_id \| instance_id`——PM 侧按值域区分，不应再出现 `"human"` 字面量；迁移期可保留兼容解析（收到 `"human"` 视为旧版 Gateway）。

### 决策 11：PM 成员模型多用户化 — 人类操作者成员化（§9 开放问题 8 决议：选 B）

**决策**：`ProjectMember` 扩展为可承载两类身份，人类操作者与 agent 成员**对称**管理，移除 `assignee = "human"` 任意人类特例。

**数据模型**（[types.rs:210](core/acowork-pm/src/types.rs#L210)）：

```rust
pub enum MemberKind { Agent, User }

pub struct ProjectMember {
    pub instance_id: String,  // 字段名兼容保留：Agent → instance_id; User → user_id
    pub kind: MemberKind,     // 新增；#[serde(default)] = Agent → 旧 project.json 零迁移
    pub added_at: DateTime<Utc>,
}
```

- **保留 `instance_id` 字段名 + 新增 `kind`**：JSON 契约不变（前端 `pm-types.ts` / `normalizeProject` 无需改字段名），`kind` 默认 `Agent` 使旧数据读出即 Agent，零迁移；语义扩展写入注释与本文档。
- **为什么显式 `kind` 而非前缀编码（`user:` / `agent:`）**：避免 user_id 与 instance_id 语义混淆；schema 迁移意图明确；与 ADR-073 三层身份 / ADR-076 user_id 维度对齐。前缀编码把类型塞进值域，破坏 UUID 可读性且无法用 serde 默认表达。

**联动指派不变式更新**：

```text
旧: task.assignee ∈ ∅ ∪ project.members ∪ {"human"}     （任意人类特例）
新: task.assignee ∈ ∅ ∪ project.members                  （人类与 agent 同构，无特例）
```

- `claim` / `submit` / `review`：actor 值域 = user_id（REST `X-Actor`）∪ instance_id（MCP `X-MCP-Actor`），必须 ∈ members——校验逻辑同一，无分支。
- `create_project` 自举（[tree.rs:505](core/acowork-pm/src/store/tree.rs#L505)）：`created_by` 无论 User 还是 Agent **都**自动加入 members。人类创建者入 members 是"人类成员化"的自洽前提（否则创建者自己无法被指派 / 认领）。
- `review_status`（[tree.rs:762](core/acowork-pm/src/store/tree.rs#L762)）：判定从 `created_by == "human"` 改为 `kind(created_by) == User` → NotRequired；Agent → Pending。

**迁移**（multi-user 上线时一次性执行）：
- 现有 `members[]` 全为 agent instance_id → `kind: Agent`（serde default 自动成立，无需数据改写）。
- `task.assignee == "human"`（旧"任意人类"指派）→ 迁移为创建者自己的 user_id；迁移期 PM 侧保留 `"human"` 兼容解析（视为旧版 Gateway，见决策 10 安全检查点）。

**与决策 10 的关系**：决策 10 解决"Gateway 注入真实身份"（`X-Actor` = user_id）；本决策解决"PM 侧消费端对称化"。配套实施、缺一不可——只做 10 不做 11，"任意人类"仍靠 `"human"` 特例兜底；只做 11 不做 10，人类成员身份无法从 header 区分。

### 决策 12：部署模式分流 — `AUTH_MODE` 由 bind 地址自动推断

**核心**：避免在单机 self-hosted 场景下强制走 multi-user 完整链路。`AUTH_MODE`（概念名，配置通道见 §1.4 注）由 bind 地址自动推断（也可显式覆盖），local 模式下完整 §1-§11 决策退化为 no-op。**这不是 §5.4 的"回滚开关"——是第一类配置**。

**心智模型**（对应 [runbook §0](../runbooks/single-machine-remote-topology.md) "没有 local/remote 两套拓扑"）：架构始终一套，差异在"外部可达性" → "认证强度"。loopback-only = 物理 OS 用户管理兜底 = 信任域；LAN 暴露 = 不可信域 = 完整账号体系。

**推断规则**：

| bind 配置 | 推断 AUTH_MODE | 触发理由 |
|---|---|---|
| `127.0.0.1` / `::1`（默认） | `local` | 仅 loopback 可连 = 物理 OS 用户管理兜底 = 单机信任域 |
| `0.0.0.0` / LAN IP / 域名 | `multi_user` | 跨机/跨用户可连 = 必须完整账号体系 |
| `--auth-mode local` / `multi_user` 显式 | 覆盖推断 | 异常场景（reverse proxy 后 + 仅内网访问时强制 local；loopback 但想演示 multi_user 时强制 multi_user） |

**优先级**：CLI `--auth-mode` > TOML 顶层 `auth_mode` > bind 自动推断 > default `local`。（`auth_mode` 键在 `GatewayConfig` **顶层**，不在 `[multi_user]` 段内；`[multi_user]` 段承载 `bootstrap_admin` / `password_policy` / `registration_open`，**已实现**，见 §6.4。）

**multi_user 最小配置**（首次启动，`accounts.json` 为空时必需）：

```toml
auth_mode = "multi_user"          # 或 bind 到非 loopback 让系统自动推断

[multi_user]
bootstrap_admin = { username = "root", password = "change-me-1", display_name = "管理员" }

[multi_user.password_policy]      # 缺省即下面三个值
min_length = 8
require_digit = true
require_mixed_case = false
```

首启后 `bootstrap_admin` 即可从配置里删掉——账号已经建好，它只在 `accounts.json` 为空时有意义（见决策 5 实施修订）。

**local 模式行为**（`AUTH_MODE=local`，**所有 §1-§11 决策退化为 no-op**）：
- `HttpAuth` 维持现有 bearer token（`data_dir/http_token` 文件，决策 3 的 access/refresh 不引入）
- `user_profiles.json` 维持现状（明文展示偏好），不升级为 `UserAccount` schema（user_id 字段沿用现有逻辑，不引入 password_hash / role / disabled_at）
- `SessionMeta.user_id` 字段在 write 路径**仍写入**（保持 schema 统一），但 read 路径**不过滤**——`?user_id=` query 接受但不生效
- 无 admin 角色、无 `bootstrap_admin`、无 `as_user`、`/api/auth/*` 路由不注册
- Desktop 顶栏维持现有"用户偏好"入口，不显示账号切换菜单
- 用户聊天（决策 8）不创建 `data_dir/users/` 目录；`/api/users/{self}/chats/*` 路由不注册
- PM/Doc 反代（决策 10）`build_trusted_headers` REST 分支仍注入 `X-Actor: human`（常量路径，**不**走 token 身份）
- PM 成员模型（决策 11）`assignee == "human"` 特例**保留**（与决策 11 移除特例的方向相反——但因 local 模式无 token 身份可注入，回退到原常量是唯一合理路径）

**multi_user 模式行为**（`AUTH_MODE=multi_user`）：
- 完整启用 §1-§11 所有决策
- 启动检查：~~`bootstrap_admin` 必须配置，否则拒绝启动（fail-fast；与决策 5 的"漏配告警"对冲——告警可被忽略，拒启动不可绕过）~~（**v2 修订**：空库改为 seed 无密码 admin + 受限模式，见 §决策 12 v2 / v3；原为"拒启动"）
- bind 自动调整为 `0.0.0.0`（若仍 loopback → 仅 warn，**不强制改**——便于本地演示 multi_user）

**UserAccount 在 local 模式下的存在性**（YAGNI：local 模式零改动）：
- `user_profiles.json` **维持现状**（`UserProfile` schema 不变，无 `password_hash` / `role` / `disabled_at` 字段）；凭据表 `accounts.json` **不创建**
- `AUTH_MODE=local` = 现状不变；`DISABLED_PASSWORD_HASH` sentinel **不用于 local**——它专用于 multi_user 下"admin 已创建、owner 尚未首次登录激活"的账号
- 升级 multi_user 时**一次性迁移**：`user_profiles.json` 的每个 `UserProfile` → `accounts.json` 的 `UserAccount`（`password_hash = DISABLED_PASSWORD_HASH`），owner 走 `invite_token` 首次登录设密码激活

**回滚/降级**（与 §5.4 对齐，但层级提升）：
- multi_user → local：设 `AUTH_MODE=local` 或 `--bind 127.0.0.1`；前端跳过 LoginView，token 验证回落 bearer，admin 路由不注册；账号文件保留但不可登录（`password_hash` 仍在但无 login UI 触发校验）
- local → multi_user：复用决策 6 的 `invite_token` 流程——现有 user 走"首次登录设置密码"激活；admin 创建的首 user 走同样的 invite 流程

**主流参照**：GitLab / Gitea / Jenkins / Outline / Wiki.js / Plausible 等自托管产品均按 bind/外部可访问性分流认证强度——本地模式信任物理访问，多用户模式要求完整账号体系。

**与之前决策的联动**：
- **决策 1**：`UserAccount` 字段全保留，local 模式下 `password_hash` 填 sentinel（schema 不分裂）
- **决策 5**：local 模式下 `bootstrap_admin` 不强制（首位 admin = 物理 OS 用户）；multi_user 模式下从"漏配告警"升级为"漏配拒启动"，再修订为"seed 无密码 admin + 受限模式"（见 §决策 12 v2 / v3）
- **§5.4 回滚段**：`AUTH_MODE` 不再只是回滚段的环境变量，而是第一类配置——回滚段降级为"mode 内降级"
- **§9 开放问题 1**：local 模式下"首位 admin"概念不存在（物理 OS 用户就是 admin），multi_user 模式下保留 bootstrap_admin；问题按模式分流消解

**ponytail 标记**：bind 推断逻辑只覆盖 `127.0.0.1` vs `0.0.0.0` 二分；IPv6 link-local（`fe80::/10`）视为 multi_user（默认安全侧）。超规模场景（reverse proxy 后 + 仅内网访问）需手动 `--auth-mode local` 覆盖；bind + reverse proxy 的混合可信域判断留后续 ADR。

#### §决策 12 v2：空账号库不再拒启动——seed 无密码 admin + 受限模式

**修订动机**（实施期记录）：原 §决策 12 的 "multi_user + 空账号库 → fail-fast" 把"创建首位 admin"的责任推给 toml 的 `[multi_user].bootstrap_admin` 段。新用户跑 `build_macos.sh --start --remote`（bind 0.0.0.0 → 自动推断 multi_user）→ 无 toml → gateway 拒启动 → **用户看不到 stderr**（脚本 `> /dev/null`）→ desktop 连不上 → 黑洞。��是糟糕的首次使用体验，跟"产品还没人能用"等价。

**新合约**（v2，**已实现**，见 §6.4 `Gateway::new` 的 passwordless seed 路径 + `http/restricted_mode.rs` middleware）：

1. **空账号库 + 无 toml `bootstrap_admin`**：seed `username=admin` / `role=Admin` / `password_hash=DISABLED_PASSWORD_HASH` 的无密码账号，进入**受限模式**（`is_restricted()` 返回 `true`）。
2. **受限模式下的 HTTP 行为**：
   - `/health` → 200
   - `/api/status` → 200 且新增字段 `requires_setup: true`
   - 其他全部 `/api/*` → **403 `{error: "setup_required"}`**（不是 401）
3. **受限模式解除**：操作员在 Gateway 主机上完成首次设置，受限模式关闭，恢复正常服务。**没有 HTTP 端点接受首次密码**——密码只走 stdin / 文件 / TTY / toml，**永远不上网络**。
4. **空账号库 + toml `bootstrap_admin` 已配**：行为不变（直接用 toml 密码建 admin，跳过受限模式），保留为非交互/容器部署的"零交互启动"路径。
5. **非空账号库**：行为不变（`bootstrap_admin` toml 被忽略 + warn）。

**三种首次设置入口**（按使用频率排序）：

| 入口 | 适用场景 | 实现 |
|---|---|---|
| **TTY prompt**（无子命令调用时自动） | 本地开发者、ssh 远程 | `cli.rs`：`Gateway::new` 后检测 restricted + `stdin`/`stdout` **双** TTY → `rpassword::prompt_password` 两次确认 → 写 accounts.json。非 TTY 不阻断启动（v3，见 §决策 12 v3） |
| **`admin-setup` CLI 子命令** | systemd / Docker / 非 TTY | `acowork-gateway admin-setup [--password-file PATH] [--password-stdin]`：从文件/stdin/rprompt 读密码 → `AuthService::set_admin_password` → **不启动 Gateway**，退出 0 |
| **`[multi_user].bootstrap_admin` toml 段** | 纯配置文件驱动（k8s ConfigMap / 镜像打包） | 重新启动 daemon → `ensure_bootstrap_admin` 走 path A → 受限模式从一开始就不进入 |

**安全不变量**（v2 不破坏任何原有安全保证）：

- **首次密码永远不上网络**。HTTP 上没有任何端点接受它。即使 LAN 攻击者抢先连到 Gateway，端口开放的 `/health` 和 `/api/status` 不接受密码。SSH/物理访问 = 已经是 admin。
- **受限模式 + LAN 攻击者 + 远程 desktop**：desktop 拿到 `requires_setup=true` → 显示"去 Gateway 主机设密码"提示页，**不会**尝试登录。攻击者不能代替操作员设密。
- **race window**：从"daemon 完成 `set_admin_password` 写盘"到"HTTP 中间件下一次看到 `is_restricted()==false`"是 micro-秒级（同一进程内的 accounts.json 原子写 + middleware 直接调 `is_restricted()` 读盘）���
- **policy 一致性**：`set_admin_password` 走 `PasswordPolicy::validate`，跟普通 `change-password` 同一条代码路径。

**与原 §决策 12 的关系**：

- 原"fail-fast"是 v1 的"安全优先"极端选择，假设操作员一定会读 toml。**经验证**这假设不成立（build_macos.sh 把 stderr 吞了）。v2 保持"操作员必须设置密码"这个安全不变量的同时，**把"如何设置"从"必须读文档改 toml"降到"gateway 启动时一个 TTY 提示"或"一行 CLI"**。
- `bootstrap_admin` toml 段**保留**——它是"纯配置文件驱动"场景的合规出口，CI/k8s/CI 镜像打包用它。
- v2 是 v1 的**降级**，不是替代：bind 推断规则、local 模式所有 no-op、UserAccount 在 local 模式下的存在性、回滚/降级路径、主流参照、ponytail ceiling 全部继承。

**实施映射**：

| 文件 | 变更 |
|---|---|
| `auth/service.rs::ensure_bootstrap_admin` | 拆分为 Path A（toml bootstrap_admin → 记账码 admin）与 Path B（空 store → seed 无密码 admin）；保留"非空 store → bootstrap_admin 被忽略"分支 |
| `auth/service.rs::set_admin_password` + `is_restricted` | 新方法，set 校验 policy、emit 一次性 fail；is_restricted 是 middleware / status 字段的唯一判据 |
| `gateway/mod.rs::is_first_boot_restricted` + `set_admin_password` | 公开 façade，daemon arm 用 |
| `cli.rs::Commands::AdminSetup` | 新子命令，三种密码源（file/stdin/rprompt），**不启动 Gateway** |
| `cli.rs` daemon arm | `is_first_boot_restricted()` → `stdin`+`stdout` 双 TTY 才 prompt（写盘后未带 `--daemon` 则退出）；**非 TTY 或 prompt 失败只 warn（stderr + 日志文件）不中断启动**，daemon 照常起 HTTP 进受限模式。见 §决策 12 v3 |
| `http/restricted_mode.rs` | 新 middleware（已实现），装在 `auth_middleware` 之前，403 `setup_required` |
| `http/routes.rs::SystemStatusResponse` | 新增 `requires_setup: bool` 字段，`/api/status` 序列化自动带上 |
| Desktop `authStore` + `SetupRequiredView` | 探测 `requires_setup=true` → 切到 setup_required 状态 + 5 s 轮询 `/api/status` → flip 后自动 `init()` 继续 |

**ceiling / 未做**（有意留白）：

- **远程 desktop 不能自己**——first password 不能从 desktop UI 设置。要么 SSH 上去，要么 `admin-setup` 子命令，要么 toml。这是设计，不是 bug（见 §决策 12 v2 安全不变量第 1 条）。
- **轮询间隔 5 s 是硬编码**。Setup 完成后用户最多感知 5 s 延迟。需要 sub-second 时切 WebSocket / SSE。
- **受限模式不限制 MQTT**：依赖现有 broker CONNECT 鉴权（`mqtt.auth_enabled`）。如果运营需要"受限模式 = broker 也拒所有 user:*"，加 broker ACL 即可，**当前不做**——理由：MQTT credential 本来就跟 HTTP bearer 复用同一个 secret，没密码的 admin 拿不到 token，连不上 broker，无需额外限制。

#### §决策 12 v3：首次设置与 daemon 启动解耦——daemon 永远先起 HTTP

**修订动机**（实施期记录；代码注释 `cli.rs` 里已引用 v3）：v2 把首次设置的实现写成「`cli.rs` **daemon arm**：TTY 检测 + prompt + 写盘；非 TTY **退出 1**」。实测这条实现**没有修好它自己的动机场景**——受限模式在首次启动路径上根本不可达：

1. `build_macos.sh --start` 用 `"$GATEWAY_EXE" ... &` 起进程。非交互 shell 里后台作业的 stdin 被赋为 `/dev/null`（POSIX 行为，pty 下实测确认），于是 `stdin.is_terminal() == false` → 走「非 TTY」分支 → **`return Err` → exit 1，HTTP 从没 listen**。Desktop 拿到的仍是 connection refused，和 v1 的黑洞等价；只是这次 accounts.json 里多了一个谁也看不到的 seed。
2. 就算 stdin 是 TTY（前台交互启动），prompt 也排在 `if self.daemon { async_main(...) }` **之前**且阻塞：密码写完 `is_restricted()` 已经是 `false`，HTTP 才开始服务。也就是说**受限模式从来没有真正对外服务过**——`requires_setup` / `SetupRequiredView` / 5 s 轮询这套机器只会在运行期被 `reset_password` 打中时才出现（那是另一种故障，见 §决策 12 v2 的 `is_restricted()` 定义）。

**v3 合约**：首次设置是 **best-effort**，永远不允许打断启动。

1. `is_first_boot_restricted()` 为真时，只有 `stdin` 与 `stdout` **都是** TTY 才弹 prompt。`rpassword` 直接读写 `/dev/tty`（不看 fd 0/1），所以「stdio 两端都是终端」是「有人坐在前面」的保守代理；被重定向的启动（构建脚本 / systemd / Tauri 子进程）一律不弹。
2. prompt 成功 → 写盘；未带 `--daemon` 时退出，让操作员自己决定何时起服务（v2 语义保留，见 `cli.rs` 的 v3 注释）。
3. **prompt 不可用（非 TTY）或失败（两次不符 / 不满足 policy）→ 只 warn，不中断**：`eprintln!` + `tracing::warn!` 点名三条解法，然后照常继续。`--daemon` 会起 HTTP，受限模式开始**对外服务**。
4. 因此受限模式是真的可达：`/health` 与 `/api/status`（带 `requires_setup: true`）返回 200，其余 `/api/*` 一律 403 `setup_required`；Desktop 渲染提示页并 5 s 轮询，操作员在主机上 `admin-setup` 写盘后，下一次中间件读盘即解除，**无需重启**。

**为什么解法要写进日志文件**：动机场景的 stderr 被脚本丢进 `/dev/null`，所以 `warn_first_boot_restricted()` 同时发 `tracing::warn!`，落到 `data_dir/logs/*.log`——这是「用户什么都看不到」的正解。

**实测**（`--home <tmp> --auth-mode multi_user --daemon --addr 127.0.0.1:21999`，stdio 全部重定向）：

| 请求 | 结果 |
|---|---|
| 进程 | 存活（v2 下此处是 exit 1） |
| `GET /health` | 200 |
| `GET /api/status` | 200 + `requires_setup: true` |
| `GET /api/users`（无 token） | **403 `setup_required`**（不是 401，见下方已修 bug） |
| `POST /api/auth/login` | 403 `setup_required` |
| `admin-setup --password-stdin` 之后 | `requires_setup: false`，login 拿到 token 对，`/api/users` 回到 401 |

> **已修 bug（本轮）**：`restricted_mode` 中间件原先 `.layer()` 在 `auth_middleware` **之前**，而 axum 的 layer 是「后加的更外层、请求从下往上跑」——于是它实际在 auth **内层**，受限模式下的无 token 请求先被 auth 答成 401，与 v2 合约「403 不是 401」相反。现在改为挂在 auth 之外，并补了一个走真实 `build_router` 的回归用例（`real_router_answers_403_not_401_without_a_token`）。

---

## 5. 后果

### 5.1 正面

1. **账号体系最小入侵**：复用现有 Vault master key、partition 范式、SessionMeta 持久化范式，不引入新密码学、不引入新存储层、不引入新折叠组件。
2. **session 隔离是单字段改动**：Runtime 侧只新增一个 `SessionMeta.user_id` 字段 + 一个 `?user_id=` query 参数；其它代码路径不动。
3. **admin 不破坏权限模型**：admin 是 token 内的 `role` 字段，不是身份冒用；`as_user` 严格只读，写操作按 token 真实身份校验。
4. **用户聊天是 Gateway 自有数据**：不与 Node agent 拓扑耦合；后续加 group chat / 表情反应 / 已读回执都是 schema 升级，不涉及 Runtime。
5. **token rotation**（refresh token family 旋转）：泄漏一个 refresh token 不会让攻击者永远续命——新一次 refresh 会让旧 family 全部失效。

### 5.2 负面 / 成本

1. **登录态中间件全栈改造**：所有现有 handler 必须经过 auth_middleware，handler 函数签名要加 `Extension(auth): Extension<AuthContext>`。约 30+ handler 要 touch。
   > **实施修订（Phase C-2）**：中间件做成**全局层**后，handler **不必**逐个改签名——`auth_middleware` 已经挡住所有未带 token 的请求，只有**需要用到身份**的 handler（反代注入、`/me`、`/change-password`）才按需加 `Extension<AuthContext>`。实际改造成本从"30+ handler"降到"少数几个 handler + 一条 layer"，这是挂全局层而非逐路由 `route_layer` 的直接收益。
2. **session 隔离散布在 Runtime 的多个 handler**：过滤与 owner 校验必须覆盖 list / get / messages / latest / open / close / delete / visibility 全部入口；漏一处 = 数据泄漏。已收敛到两个共享谓词（`is_readable_by` / `is_writable_by`）+ 两个共享辅助（`authorize_read` / `authorize_write`），新增 handler 只需调辅助即可，但仍需 grep ceiling lint 兜底（见 §6.6）。
3. **Desktop 双 store 并存过渡期**：`userProfileStore`（旧） + `authStore`（新）共存一段时间，迁移期两者数据可能不一致；需要清晰 deprecation 路径。
4. **token 撤销的存储成本**：refresh token family 需要持久化以支持"该 family 全部撤销"语义，单独文件或 Redis；本期选文件（`data_dir/auth/revoked_families.txt`），量小可接受。
5. **首次启动门槛（multi_user 模式）**：~~必须通过 `bootstrap_admin` 配置创建首位 admin；漏配导致系统空跑无人能登录——fail-fast 拒启动~~（决策 12 v2 修订为 seed 无密码 admin + 受限模式，见 §决策 12 v2 / v3），而非仅告警。local 模式无此门槛（首位 user 由 `bootstrap/orchestrator.rs` 自动创建，见决策 12 "UserAccount 在 local 模式下的存在性"）。

### 5.3 边界 / 例外

**ADR-009 §5.4 的扩展条款**（在 ADR-009 v3 修订时追加）：

> **例外 — 用户账号与聊天数据**（ADR-076 §决策 8）：账号凭据（`data_dir/vault/accounts/`）、账号元数据（`data_dir/user_profiles.json`）、用户-用户聊天记录（`data_dir/users/*/chats/`）归属 Gateway 进程，由 Gateway 直接读写 fs。这些数据与任何 agent instance 无对应，不属于"Runtime 私有数据"。运行时 session 数据仍只通过 Runtime HTTP 反代访问，本例外仅限上述三类数据。

### 5.4 回滚

**模式级回滚**（第一类，由 §决策 12 的 `AUTH_MODE` 控制）：
- multi_user → local：设 `AUTH_MODE=local` 或 `--bind 127.0.0.1`；前端跳过 LoginView，token 验证回落 bearer，admin 路由不注册；账号文件保留但不可登录（密码哈希仍在但无 login UI 触发校验）；`SessionMeta.user_id` 写入但不过滤；用户聊天路由不响应；PM/Doc REST 反代回退到 `X-Actor: human` 常量注入
- local → multi_user：复用决策 6 的 `invite_token` 流程——现有 user 走"首次登录设置密码"激活；admin 创建的首 user 走同样的 invite 流程

**字段级回滚**（第二类，mode 内的细粒度退化）：
- `UserProfile` → `UserAccount` 是 in-place 升级（迁移脚本同表字段），回滚 = 删 `user_id` / `password_hash` 字段
- `session.user_id` 字段可选，删除后过滤失效（回到所有人共享）——**仅 multi_user 模式相关**，local 模式本就不过滤
- auth middleware 可选：保留 `HttpAuth` bearer token 兜底路径
- 聊天数据独立目录，删除 `data_dir/users/*/chats/` 即视为"未启用用户聊天"
- PM/Doc 反代身份（决策 10）：multi_user 模式下 `build_trusted_headers` 注入 `auth.effective_user_id`；local 模式注入常量 `human`（决策 12 联动）
- PM 成员模型（决策 11）：`ProjectMember.kind` 带 `#[serde(default)]`，回滚仅丢弃 kind 字段、数据文件 schema 兼容；`"human"` 兼容解析保留到迁移完成后再移除（multi_user 模式路径，local 模式保留 `"human"` 常量路径）

### 5.5 已知技术债

> 本节混放**变更记录**（标 已结清 / 已实现）与**未结清的 ceiling**。只看"还剩什么没做"请直接跳 [§10 遗留清单](#10-遗留清单仍未做全集)——那一节是本节加上 §7.2 / §9 的**索引**，按"触发条件"排序，并单列了**已否决**的方案。

- **ponytail: access token 校验无状态**（只验签 + `exp`，不读 `accounts.json`），所以"账号被禁用后 token 仍可用"的窗口 = `ACCESS_TTL_SECS`（15 分钟）。这是**有意的上界**，不是遗漏：代价是每请求一次磁盘解析，收益只有 15 分钟。真正的强一致点在 `refresh`（每次重读 store）。要即时生效 → 加一个内存 `user_id → revoked_at` 集合在 `verify_access` 里查（无磁盘 I/O）。用户量 > 100 或需要即时踢人时再上 RS256 + 撤销名单。
- **✅ 已结清：refresh token 单次使用的复用检测会误伤重试**（原 `ponytail:` 项）。原缺陷：客户端发出 refresh、服务端处理成功、但响应在网络上丢失，客户端重试同一 token 被判"复用"→ 连坐撤销该用户全部 family；且 `{user_id}.*` 无界，`login()` 不清除登记簿，于是**循环锁死**（登录 → 约 13 分钟必 401 → 弹登录 → 再登录 → 再 401）。实测触发者是同一 webview 内相隔 3 ms 的两个 refresh，不是多设备——见 §决策 3「Desktop token 单一所有者」。实现 = 原升级路径：`r:` 条目加时间戳 + 10 秒宽限判定，通配条目改为 `iat <= cutoff` 时间基准（新登录天然免疫，登录即恢复路径）。详见 §决策 3 实施记录与 `core/acowork-user/src/auth/revoked.rs` 模块文档。
- **ponytail: `revoked_families.txt` 是扁平文件、无 GC**，每次 refresh 追加一行。目标规模（< 100 用户）无问题；超过需要 SQLite 表 + 过期列。
- **ponytail: 用户聊天列表是 fs 扫描**，O(n) on `data_dir/users/`。n < 1000 时可接受；超过需要 SQLite 索引。
- **ponytail: `as_user` query 在反代链路上是字符串透传**，未来如果引入 proto 升级需要结构化字段。
- **未实现**：多设备登录并发 session 限制、密码过期强制改密（本期仅记录 `password_expires_at` 不强制）、账号 lockout（5 次失败 → 15 分钟锁定）、**登录端点速率限制**（`POST /api/auth/login` 既无限流也无 lockout，[§7.4](#74-安全测试手动-checklist) 与 §10.2 第 14 项已按"信任边界缺口"记录——唯一的缓解是默认 bind `127.0.0.1`）——前三项放后续 ADR，第四项在"暴露到非信任网络"之前必须补。
- **已实现（Phase E）**：`/api/auth/first-login`（`invite_token` 以 SHA-256 落 `accounts.json`，24h 过期、用后即焚）、账号 CRUD（`account_api.rs`）、`registration_open` 接线（`[multi_user].registration_open = true` 时非 admin 亦可创建普通账号，永不创建 admin）。`allow_public_signup`（匿名注册，ADR 里的 demo-only 极端模式）**仍未接线**——它需要把 `/api/users` 加入中间件白名单，属安全面扩大，未做。
- **已结清：multi_user 下展示字段（language / timezone / avatar …）的单一写入权威**。原先两条写路径各自为政：`PUT /api/users/{id}` 只接 `display_name` + `role`（Desktop 的其余偏好字段被 serde 静默忽略，no-op 不报错），而 `/api/user/avatar-*` 直接改 `user_profiles.json` 里共享的 "active user"——`account_api::sync_profiles` 每次账号变更都从 `accounts.json` **整体重建**该视图，头像变更会被下一次账号变更冲掉，且 multi_user 下 "active user" 本身无意义。现收敛到 `accounts.json` 单权威：`AuthService::update_account` 改收 `ProfilePatch`（全部展示字段；`None` = 不动，空串 avatar = 清除，`display_name` 沿用 trim + 空白忽略契约），`UpdateAccountRequest` 以 `#[serde(flatten)]` 透传；avatar 路由在 multi_user 分支按 `AuthContext.user_id` 写穿 `accounts.json` 后走同一个 `sync_profiles`（local 模式保留 active-user 路径不动）。回归测试：`update_account_applies_the_display_patch`（service 层：patch 语义 + 落盘）、`update_account_persists_display_fields`（HTTP：PUT 展示字段 → 权威读回一致）、`avatar_config_writes_through_accounts_under_multi_user`（先设头像 → 再改 display_name 触发重建 → 头像仍在，旧实现必失败）。**资产归属（已结清）**：头像文件按账号命名空间落 `assets/avatars/{user_id}/`——upload / list 只见自己的命名空间，delete 在 unlink **之前**做归属守卫（路径前缀非本人命名空间 → 403，admin 无 bypass：先经 `PUT /api/users/{id}` 解引用再删；守卫放在 unlink 前是因为"删完再拒"文件已经没了），GET 保持全池可读（头像本就要展示给他人，读隔离不是需求）；local 模式保留 `assets/` 根（单用户无跨用户向量）。user_id 进路径前在边界校验字符集（alnum / `-` / `_`），不信任 token claim 的形状。不做存量迁移——multi_user 无已发布数据（ADR 未评审、Desktop authStore 未接线），legacy `assets/avatar-XX` 路径 GET 仍可读，用户重新上传即迁移。回归测试：`avatar_file_deletes_are_confined_to_the_owner_namespace`（他人 / admin 删 → 403 且文件存活；owner 删 → 200 + 字段经权威清掉）、`avatar_target_dir_is_per_user_under_multi_user_and_shared_under_local`（目录解析 + 越界 user_id 拒绝）。**仍开放：上传配额**。Phase 6 已评估：附件上传落地时**没有**顺手补它，而本轮进一步**否决了 per-user 配额这个框架**——磁盘是共享资源，按账号限额只在"配额本身是公平契约"的多租户下才有意义（否则一个用户 100 MiB 上限 × 50 个用户 = 5 GiB，保护不了任何东西），而本期唯一的写者 `store_attachment` 已按 mime 分档限制单文件体积，"单文件大小"与"账号总量"也不是同一件事。**真要做就换全局水位**：在 `store_attachment` 前统计 `data_dir/users/**/files/` 总量，超过 `data_dir` 的水位阈值就拒绝（一个数字、一处检查、保护真正被争用的那个资源）。触发条件：出现真实的磁盘压力，或引入了互不信任的多租户。
- **ponytail: 公开 session 的分页计数是"可见行数"而非"总行数"**：过滤发生在分页前，所以 `total_count` / `total_pages` 只数调用者能看见的 session。这是有意的——按总数分页会泄漏"别人还有 N 个 session"——但代价是不同用户看到的同一页边界不同，前端**不能**缓存跨用户的分页结果。
- **已结清：会话控制面的 MQTT 命令已全部删除**（proto 字段 + Runtime 变体 + Gateway/Tauri 映射表），字段号整体重排为连续。协议文档（`mqtt.md` / `http.md` / ADR-034 §11.2.B）已同步为"已删除 + 迁 HTTP"。**仍未结清**：MQTT 事件面的读侧保密性（无 per-user topic ACL，`mqtt.md` §10 已标注为暂缓 / 已知缺口）。
- **已结清：`meta/` 目录的全量扫已改为进程内内存索引**。原先每次列表都是 `scan_sessions_from_meta` = `read_dir` + 逐文件 `read + serde_json`。实测（2000 个 meta / 1.5 MB，本机 APFS 热缓存）：release **23 ms**、debug **42 ms**，而 `read_dir().count()` 只要 **0.9 ms**——release 仅比 debug 快 1.8×，说明瓶颈是 **2000 次 `open`/`read`/`close` 系统调用（~11 µs/文件），不是 JSON 解析**。真实规模（本机各 agent 1–17 个会话）单次约 0.2 ms，所以当时判断这是**形状**问题（随历史线性增长）而非当下问题。现已按当初写下的升级路径落地：`META_INDEX`（进程内 `HashMap<会话目录, MetaIndex>`，Runtime 一进程一 agent → 生产只有 1 条）持有已排序的 `Vec<(String, SessionMeta)>`，`scan_sessions_async` / `find_latest_session` / `prune_excess_sessions` 读缓存；**写口唯一**是 `write_session_meta`（写完顺手 upsert，不存在"记得失效"这一步），**删口唯一**是 `remove_session_meta`（`delete_session` 与 prune 都收敛到这里）。有效性靠 `read_dir().count()` 探针（不等则重建，自愈）。为正确性加的两处：排序补 `session_id` tie-break（否则缓存索引与重新扫描在毫秒级平局会话上分页边界不一致）、`dev/ci.sh` 的 `run_meta_layout_redline` 把 meta 路径构造限制在 `conversation.rs`（其余仅测试夹具、固定上限、只降不升）。**未落盘成索引文件**——那是第二真相源，已在本 ADR §决策 2 否决。回归测试：`session_index_reflects_funnel_writes_without_rescanning`（同一 meta 被改写时条目数不变，只有写侧 upsert 能让列表看见新值）、`session_index_self_heals_on_out_of_band_meta_change`、`remove_session_meta_drops_the_session_from_the_listing`。
- **已结清：会话数量上限按 owner 分桶**（`prune_excess_sessions`）：原先按 `last_active_at` **全局**排序裁剪，multi_user 下一个账号新建会话会把**另一个账号**最老的会话归档（`.jsonl` → `.jsonl.archive` + 删 meta → 在该账号列表里消失且无恢复入口）。现按 `SessionMeta::user_id` 分桶、每桶**各自**应用 `max_sessions`；`user_id = None`（ADR-076 之前的数据 / local 模式）自成一组，保持旧语义。回归测试 `prune_excess_sessions_is_per_owner_not_per_agent`：alice 2 / bob 3 / 无主 3、上限 2 → 只裁两个超限桶各 1 个；**alice 的时间戳故意最老**，所以旧实现必然失败。
- **已结清：`visibility` 开关已有 UI**。per-session 开关在输入框（composer）工具行：🌐 / 🔒 图标调 `PUT /api/agents/{id}/sessions/{sid}/visibility`，仅 owner（`can_write === true`）可点，非 owner 渲染为 disabled（而不是隐藏，让旁观者知道自己在看什么）；乐观更新 + 失败回滚。**新会话默认值也已结清**（有主 → `Private`，无主 → 保持公开；完整语义见 §决策 4「默认值的两种含义」，回归测试见 §7.1 的 `owned_sessions_start_private_and_ownerless_stay_public` 与 `only_an_admin_may_re_share_an_unowned_session`）。原先这条写的「per-agent 默认可见性刻意留白」是**同一个洞的第二层**，现已决议：被留白的是「admin 能否强制某个 agent 的会话对所有账号可见」——那是 admin 级策略（"这个 agent 的对话是团队共享日志"），与"我的新会话默认私有"不是同一层级；真要做得在配置 / `accounts.json` 里加 admin 字段，届时另立决策。
- **已结清：公开会话的"只读打开"**。做法是**观众不激活**（而不是给观众开 `open` 权限）：Desktop `can_write === false` 时输入框禁用 + placeholder 提示"只读会话"，且 `openSession` / `closeTab` 分别跳过 `POST /open` / `POST /close`；历史仍由 `GET /messages`（读授权）加载，事件流由通配 MQTT 订阅在会话真正 Active 时送达。**为什么不开 `open` 读授权**（曾实现又被撤回）：`Active` / `Closed` 是 per-session **全局**状态，观众激活会产生一个"观众无权关（close 是写授权，刻意不让旁观者拆会话）、owner 也不知道被谁占着"的常驻会话——正确回收它需要观察者引用计数，而当前 Runtime 恰好**没有** per-session GC（见上一条）。回归防护：Desktop `src/stores/sessionSharing.test.ts`（可见性乐观翻转 + 回滚；`can_write === false` 时 open / close 零请求）。

- **ponytail: MQTT 事件面没有 per-user 订阅隔离**（读侧保密性缺口）。`session 隔离`只覆盖**写侧**与 **HTTP 读侧**：任何能连上 broker 的客户端理论上可以 SUBSCRIBE 任意 `agents/{id}/sessions/{sid}/messages/#` 看到他人会话的事件流。这是 ceiling——`rumqttd` 0.20 没有 per-topic ACL 能力，**无法在现有 broker 上修复**（用户决策：MQTT 用户身份验证先暂缓）。缓解：broker 默认只 bind `127.0.0.1`（攻击者须先能访问本机回路）。升级路径 = 换 mosquitto（Phase 5b 评估）或给事件面加 token 化订阅代理。已在 [mqtt.md §10](../../protocols/zh/mqtt.md) 标注为"暂缓 / 已知缺口"。
- **已结清 + 已实现：普通用户"发起"会话**（用户聊天，§决策 8）。原缺口：解析收件人需要一份用户名录，而 `GET /api/users` 是 admin-only，"发消息"入口只挂在 admin-only 的侧栏 `UserList` 右键菜单上（普通用户只看得见自己那一行）——普通用户只能**回复**，无法发起。**决策（用户授权"你来定"）**：新增 `GET /api/users/directory`，**任何已认证账号可读**，返回三字段 `user_id` / `username` / `display_name`，**排除已禁用账号**，**排除调用者自己**。取舍依据：① 这些展示字段本就是部署的公开视图——`user_profiles.json` 从 `accounts.json` 重建后被 Runtime 当 `last_user_profile` 消费（§决策 1），名字在此语境里不是秘密；② 一个无法指认收件人的聊天功能，对**除 admin 外的所有人**（也就是它服务的所有人）不可用，"隐私死锁"比"可枚举用户名"更糟。Desktop 侧 `MessagesView` 左栏头部加"新会话"选择器（联系人来自该端点），admin 的 `UserList` 右键"发消息"降级为快捷方式而非唯一入口。回归测试：`user_directory_is_readable_by_any_account_but_bounded`（普通用户可读 + 禁用账号不出现 + admin 可读）。
  - **ponytail: 名录对任何认证账号是全量可枚举**（残余 ceiling）。端点不暴露邮箱 / 时区 / 自定义字段，也不暴露 admin 才需要的字段，但 `username` 全集对每个账号可见。个人 / 小团队部署（本 ADR 的目标规模）这是可接受的；要收敛就得上"先按精确 username 查询 / 只回已有会话对手方 / 通讯录邀请制"——都需要一个新的产品决策，不是加个 filter 能解决的。
- **ponytail: 附件有四处已知 ceiling**（§决策 9，均在代码中就地点标注）。① **blob 无回收**：先写 blob 后写元数据，两次写之间崩溃会留一个没人能引用的孤儿文件；上传本身需要认证，且失败窗口极窄，所以不为此加一个后台清扫线程——真要回收时按"无 sidecar 且早于 N 天"扫 `files/` 即可。② **下载整文件读进内存**：`load_attachment` 返回 `Vec<u8>`，单次上限 100 MiB（本机回路的桌面场景可接受）；要改成流式就换 `tokio::fs::File` + `ReaderStream`，代价是多一个依赖。③ **上传时限按客户端声明的 mime 分档**：把 100 MiB 的文档声明成 `image/png` 只会**更严格**（25 MiB），反向没有漏洞——真正决定处理方式的是存储方自己规整过的 mime，限额只影响接受的体积。④ **上传路由的 body 上限是"文档上限 + 1 MiB 信封余量"的估算**（`chat_api::UPLOAD_BODY_LIMIT`）：multipart 信封里还有 boundary + 文件名 + 头部，所以**贴着 100 MB 边界的文件仍可能被外层上限拒掉**（表现为 413 而非业务错误）。不用精确计算是因为它只能靠"解析 multipart 才能知道真实开销"——那时请求体已经读进来了，限制就失去意义。真要精确，就在路由前读 `Content-Length` 并按"文件大小 + 固定信封估算"两段判定。

---

## 6. 改动清单（按 crate / 文件）

> **实现状态（截至 Phase 6 Desktop ���天 UI）**：本节标注 **（已实现）** 的条目已合并并测试通过——core `src/account.rs`；gateway `src/account/store.rs` / `password.rs` / `auth/{mode,token,revoked,service}.rs` / `http/{auth_middleware,auth_api,account_api,restricted_mode}.rs` / `chat.rs` / `http/chat_api.rs` / config `auth_mode` + `[multi_user]` + `effective_auth_mode()` / cli `--auth-mode` / `Gateway::new` 的 **passwordless-seed + 受限模式**（§决策 12 v2，取代原 bootstrap_admin fail-fast）/ `proxy.rs` 的 14 条会话控制路由（生命周期 7 + 会话动作 7）；runtime `src/http/session_control.rs` + `conversation.rs` 的 scope/visibility + `agent/session/session_manager.rs` 的 `create_frontend_session` / `resume_session`；Desktop `authStore` / `authFetch` / `account/*` / `user-list/*` / `lib/user-chat-api.ts` / `stores/userChatStore.ts` / `views/MessagesView.tsx` / `components/account/SetupRequiredView.tsx`。**未实现**的条目 = 后续 PR（`chat/attachments`），属计划范围，非本节遗漏。原稿的 `protocol.rs AccountPublicView` **未新增**——`acowork_core::account::AccountView` 已承担脱敏 API 返回类型，再建一个同类是重复。
### 6.1 core/acowork-core

- 新增 `src/account.rs`（**已实现**）：`UserAccount`、`Role`、`AccountListFile`、`DISABLED_PASSWORD_HASH`；`UserAccount::to_public_profile()` 产出 `UserProfile` 公开视图
- 修改 `src/protocol.rs`：保留 `UserProfile`（展示用）——**不新增 `AccountPublicView`**：`acowork_core::account::AccountView` 已是脱敏后的 API 返回类型（`/api/auth/me` + `account_api` 共用），原稿的 `AccountPublicView` 与之重复
- 修改 `Cargo.toml` 依赖（如需 jsonwebtoken crate）——**未引入**：HS256 签名自研最小实现（固定 header + 常量时间校验），不依赖 jsonwebtoken，见 §决策 3

### 6.2 core/acowork-vault

- **零改动**：复用 `Vault::store` / `Vault::retrieve` 接口，仅新增调用方（account 模块）

### 6.3 core/acowork-runtime

> **Phase D 状态**：全部**已落地**（见 §决策 4 的 Phase D 实施记录）。

- ✅ 修改 `src/conversation.rs`：`SessionMeta` 加 `user_id` + `visibility`（均 `serde(default)` + `skip_serializing_if`，向后兼容）；`SessionScope` 枚举 + `from_header_value`；`is_readable_by` / `is_writable_by` 谓词；`ConversationSession::set_user_id`（write-once）/ `set_visibility` / `is_private`；`scan_sessions_async` 加 `scope` 入参并在**分页前**过滤；`SessionInfo` 加 `visibility`
- ✅ 修改 `src/usecases/session_metadata.rs` + `session_metadata_impl.rs`：`list_sessions` 加 `scope: &SessionScope` 参数并透传（原稿的 `user_id: Option<&str>` + `"__admin__"` sentinel 被 `SessionScope` 取代——sentinel 是字符串魔法值，多一个变体表达不了"local 不过滤"）
- ✅ 修改 `src/http/server.rs`：`/sessions` / `/sessions/{sid}` / `.../messages` / `.../latest` 从 `x-user-id` 头解析 scope 并校验（不是 `?user_id=` query——身份由 Gateway 注入的头承载，客户端无法伪造）；注册 **14 条控制面路由**（生命周期 7 + 会话动作 7）；反转 ADR-034 §11.2 的"控制面不在 HTTP"注释
- ✅ 新增 `src/http/session_control.rs`：**14 个 handler**（第一批生命周期 7 个：create / open / close / delete / visibility / workspace / config；第二批会话动作 7 个：messages / stop / continue / approval / answer / cancel-tool / compress）+ `dispatch_session_action` 共享辅助 + `authorize_read` / `authorize_write` / `scope_from_headers`（被 `server.rs` 的读路径共用）
- ✅ 修改 `src/agent/session/session_manager.rs`：`create_frontend_session` 加 `user_id` / `visibility` 入参；新增 `resume_session`（封装 ADR-038 激活状态机，返回 `Option<SessionOpenOutcome>`，`None` = not found）——原稿的 `create_session_with_id_and_conversation` 名字是猜的，实际入口是 `create_frontend_session`
- ✅ 修改 `src/startup/gateway_loop.rs`：MQTT `open_session` 命令改为调 `resume_session`（同一份状态机，不再重复实现）
- ✅ 修改 `src/startup/session_init.rs` + `tests/conversation_session_tokens.rs`：`SessionMeta` 构造点补 `user_id: None, visibility: None`
- ✅ 修改 `src/http/server.rs`（读/写守卫补齐）：`GET`/`PUT /sessions/{sid}/config` 与 `POST /sessions/{sid}/files` 此前**未做任何 scope 校验**（能读改他人 session 的 model / workspace / title，也能往他人会话上传附件），现补 `authorize_read` / `authorize_write`

### 6.4 core/acowork-gateway

**新增**：
- `src/http/auth_middleware.rs`（**已实现**）：token 校验 + `AuthContext { user_id, role, as_user }` 注入 + `effective_user_id()`；局部白名单 + `OPTIONS` 放行；`as_user` 非 admin → 403
- `src/http/auth_api.rs`（**已实现**）：`/api/auth/login` `/refresh` `/logout` `/change-password` `/first-login` `/me`（`/first-login` 消费 `invite_token`，随 Phase E 落地）；Argon2 调用全部走 `spawn_blocking`
- `src/auth/service.rs`（**已实现**）：`AuthService` —— 登录 / 刷新（轮换 + 复用检测）/ 登出 / 改密 / `first_login` / 账号 CRUD（`create_account` / `update_account` / `set_role` / `disable_account` / `reset_password`）/ `verify_access` / `ensure_bootstrap_admin`；`PasswordPolicy`、`BootstrapAdmin`、`AuthError`（含 `Conflict`）、`TokenPair`、`AuthPrincipal`、`ProfilePatch`（展示字段 patch，见 §5.5「已结清」条目）的定义处
- `src/http/account_api.rs`（**已实现**）：账号 CRUD（`GET/POST /api/users`、`GET/PUT/DELETE /api/users/{id}`、`/disable`、`/reset-password`），multi_user 下取代 `users_api.rs` 的 `/api/users` 路径，并把 `accounts.json` 重新投影为 `user_profiles.json`；`PUT /api/users/{id}` 经 `UpdateAccountRequest`（`#[serde(flatten)] ProfilePatch`）承接全部展示字段；另有 `GET /api/users/directory`（**任何认证账号**可读的联系人名录，唯一非 admin-only 的账号列表，见 §5.5）
- `src/http/chat_api.rs`（**已实现**）：用户-用户聊天 API —— `GET /api/users/{user_id}/chats`（按 `last_active_at` 倒序，含 `peer_user_id` / `peer_display_name` / `unread_count` / 预览）、`GET|POST .../chats/{chat_id}/messages`（`offset` 自尾部倒数，默认 50 / 上限 200；body trim 后非空且 ≤ 8000 字符）、`POST .../chats/{chat_id}/read`。读 = self-or-admin（admin "view as" 只读 scope），写 = **self-only**（admin 亦不能代发，`from` 强制取 token 身份）；`chat_id` 非规范、或调用者不是参与者 → 404（不泄露存在性）。`peer_display_name` 由服务端解析（`display_name` → `username` → id）并对整页只读一次账号表——非 admin 读不到 `/api/users`，否则无从给对端打标签
- `src/account/store.rs`（已实现）：账号权威表 `accounts.json` 读写（原子写：temp + rename）；`src/account/password.rs`（已实现）：Argon2id PHC 哈希 / 校验
- `src/auth/token.rs`（已实现）：HS256 token 签发 / 校验 / refresh family 管理；签名密钥 `data_dir/auth/secret`（首次启动生成，`0600`）
- `src/auth/revoked.rs`（已实现）：`revoked_families.txt` 文件管理（`r:{family}` 轮换 / `x:{family}` 显式撤销 / `{user_id}.*` 通配，见决策 3 实施记录）
- `src/chat.rs`（**已实现**）：`conversation.json`（`participants` / `last_active_at` / `last_message_preview` 截 80 字符 / `unread` / `version`）+ `messages.jsonl`（append-only）读写。**单文件模块**——原稿的 `src/chat/{persistence,attachments}.rs` 拆分未采用：附件最终也落在这个文件里（以 `// ── Attachments ──` 分节，未拆文件），一个配对目录的读写逻辑拆两个文件只是目录噪音。配对目录 `users/{min(a,b)}/chats/{max(a,b)}/`（wire 形态 `chat_id = min__max`，id 是 UUIDv4 故 `__` 不会出现在 id 内）；`conversation.json` 原子写（temp + rename）；JSONL 逐行解析，**单行损坏只 warn 不失效**；未读按 `user_id` 计（不按 a/b 位序，避免配对顺序重算时计数漂移）。已知 ceiling（已标 `ponytail:`）：列表按目录全扫，尾部翻页会读整个 JSONL
- `src/chat.rs` 的附件子模块（**已实现**，原稿的独立文件 `src/chat/attachments.rs` 未采用）：附件落盘 `users/{min}/chats/{max}/files/{id}`（blob，`{id}` = UUIDv4）+ 同目录 `{id}.json`（元数据：filename / mime / size / kind），与 conversation 同目录树故天然按会话隔离；存储文件名经 `safe_filename` 清洗（剥路径分隔符 / 控制字符 / 引号——`file_name` 会进 `Content-Disposition`），并截断到 200 字符；路由为 `POST .../chats/{chat_id}/files`（multipart，**self-only 写**）与 `GET .../chats/{chat_id}/files/{attachment_id}`（self-or-admin 读）；限额按**上传时客户端声明的 mime** 分档（`image/*` 25 MiB，其余 100 MiB，超限 413）——这是**上限**不是白名单，声明 image 只会更严格；下载 `Content-Type` 取自存储时写入的 metadata（不信客户端声明），`Content-Disposition` 支持 RFC 5987 UTF-8（CJK 文件名）；`messages.jsonl` 的 `attachments` 字段（id 数组）由 `append_message` 第 6 个参数写入
- `src/auth/mode.rs`（**已实现**）：`AUTH_MODE` 推断 + bind 地址解析 + CLI flag 解析（决策 12）；`pub enum AuthMode { Local, MultiUser }`；`pub fn resolve_auth_mode(cli: Option<AuthMode>, toml: Option<AuthMode>, bind_host: &str) -> AuthMode`；pub `is_loopback_host(host: &str) -> bool` helper（loopback 判定含 `127.0.0.0/8`、`::1`、`localhost`；`0.0.0.0` / `fe80::/10` / LAN IP / 域名 → MultiUser 安全侧）

**修改**：
- `src/http/routes.rs`（**已实现**）：`AppState` 加 `auth_mode: AuthMode` + `auth_service: Option<Arc<AuthService>>`（**`Some` 仅在 multi_user**）；`/api/auth/*` 仅在 `auth_service.is_some()` 时 `merge`（local 模式**不注册**，返回 404 而非 403）；`auth_middleware` 作为全局层挂在 CORS 内层、路由外层，`auth_service == None` 时 no-op
- `src/http/server.rs`（**已实现**）：`start_http_server` 增加 `auth_mode` + `auth_service` 两个参数，写入 `AppState`
- `src/gateway/mod.rs`（**已实现**）：`Gateway::new` 解析 `effective_auth_mode()`；multi_user 时构造 `AuthService` 并调用 `ensure_bootstrap_admin()`（**构造函数期 fail-fast**）
- `src/http/proxy.rs`（**已实现**）：加 5 条 session 控制面反代路由（`POST /sessions`、`POST .../{sid}/open`、`POST .../{sid}/close`、`DELETE .../{sid}`、`PUT .../{sid}/visibility`，含 body / method 透传）。**注意：反代层不做 owner 校验、不注入 user_id**——`x-user-id` 由 `auth_middleware`（全局层）统一剥/注，owner 判定由 Runtime 做（见 §决策 4 职责划分）。原稿"代理层加 `Extension(auth)` 注入 query + 二次校验 owner"因此**未采用**：那会在 Gateway 造第二份真相，还要处理"meta 刚被删"的竞态
- `src/http/auth_middleware.rs`（**已实现**）：`as_user` 仅 admin 且**仅只读方法**（GET / HEAD）——写请求携带 `as_user` 直接 403（§9 开放问题 5 的强制点）
- `src/http/pm_proxy.rs` / `src/http/doc_proxy.rs`（决策 10，**已实现**）：`build_trusted_headers` 签名加 `actor: &str` 参数；multi_user 模式 REST 分支从注入常量 `"human"` 改为 `auth.effective_user_id`（经 `trusted_rest_actor` helper 解析 `Option<Extension<AuthContext>>`）；**local 模式保留 `X-Actor: human` 常量注入**（决策 12 联动）；MCP 分支 `X-MCP-Actor` 校验逻辑不变（两模式一致）。测试：`rest_path_injects_token_identity_under_multi_user`（multi_user + token → 注入 `u-alice`，伪造 `X-Actor` 被丢弃）；原有 `rest_path_injects_human_when_absent` / `rest_path_overrides_forged_x_actor`（local 回退）不变
- `src/http/users_api.rs`：**保留为 local 模式的实现**，原稿的"废弃 + 别名到 `account_api`"方案**未采用**——`routes.rs` 用 `match &state.auth_service` 在 `account_api::account_routes()` 与 `users_api::users_routes()` 之间二选一（同一组路径不能同时注册，axum 会 panic），比别名少一层间接。Phase E 已先修其 multi_user 缺陷：① `/api/user/avatar-*` 路由 mode-independent 注册，原先直改 `user_profiles.json` 的共享 active user（会被 `sync_profiles` 冲掉），现 multi_user 分支按 `AuthContext.user_id` 写穿 `accounts.json`（复用 `account_api` 的 `blocking` / `sync_profiles` / `account_err`），local 路径不变；② 头像文件改 per-user 命名空间 `assets/avatars/{user_id}/`（upload / list 按 caller 收敛，delete 归属守卫在 unlink 前，GET 全池可读），local 保留 `assets/` 根（见 §5.5「资产归属」）
- `src/resource_cache.rs`：`UserProfileListFile` 改名或保留——保留 `user_profiles.json` 作为公开视图；local 模式下维持现有 `UserProfile` 写入路径（不引入 `password_hash` sentinel）
- `src/account/store.rs`（决策 2 配套）：**local 模式不创建 `accounts.json`**，不碰 `user_profiles.json`（零改动）
- `src/bootstrap/orchestrator.rs`：**未采用**——启动检查改落 `Gateway::new`（构造期 `Result` 才能真正拒绝启动；orchestrator 是运行时子系统，那里报错只会打日志）
- `src/config.rs`（**已实现**）：顶层 `auth_mode: Option<AuthMode>`（`None` = bind 自动推断）+ `effective_auth_mode()`；新增 `[multi_user]` 段：`bootstrap_admin: Option<BootstrapAdmin>`、`password_policy: PasswordPolicy`、`registration_open: bool`（**已接线**，Phase E：非 admin 可建普通账号，永不建 admin）
- `src/cli.rs`（CliArgs）：`--auth-mode <local|multi_user>`（**已实现**，含 env `ACOWORK_GATEWAY_AUTH_MODE`）；"显式 `--auth-mode` 与 `--bind` 冲突 → warn" 联动——**已实现**：显式模式始终优先，`Gateway::new` 在 `config.auth_mode.is_some()` 且与 bind 推断不一致时打 `warn`（决策 12 允许该覆盖，仅提示）

### 6.5 apps/acowork-desktop

**新增**：
- `src/stores/authStore.ts`：token 管理 + 账号切换 reset 流程
- `src/components/account-switcher/`：顶栏账号菜单 + 登录 modal + 改密 modal + 注册 modal（admin 可见）
- `src/components/user-list/UserList.tsx` + `partitionAccounts.ts`：侧栏 User 折叠分组
- `src/components/chat/`：用户-用户聊天 UI（列表 / 会话 / 附件上传）
- `src/lib/api/auth.ts`：HTTP client 注入 Authorization header

**修改**：
- `src/components/agent-list/AgentList.tsx`：在 agent 分组下方插入 `<UserList />`
- `src/stores/userProfileStore.ts`：**保留**（展示用），新增 `authStore` 作为更高层（auth state 决定当前 userProfile；userProfile 是脱敏副本）
- `src/App.tsx`：登录态判断；未登录 → 渲染 LoginView；登录后 → 现有主界面
- `src/lib/types.ts`：新增 `UserAccount`、`AuthState`、`Role` 类型
- `src/i18n/`：新增 `account.*` / `userList.*` / `chat.*` 文案键

**已实现（Phase D 收尾，session 隔离的 Desktop 侧）**：
- 新增 `src/lib/session-control.ts`：会话控制面 HTTP 封装（取代 `invoke("mqtt_publish_control")`）
- 新增 `src/components/chat/SessionVisibilityToggle.tsx`：输入框工具行的 per-session 可见性开关（🌐 / 🔒，仅 owner 可点）
- 修改 `src/stores/agentStore.ts`：`setSessionVisibility`（乐观翻转 + PUT + 失败回滚）
- 修改 `src/stores/chatStore.ts`：8 个控制调用点 MQTT → HTTP；`closeTab` 在 `can_write === false` 时跳过 `POST /close`
- 修改 `src/components/chat/ChatPanel.tsx`：`can_write === false` 时禁用输入框 + 只读 placeholder；挂载可见性开关
- 修改 `src/lib/types.ts`：`SessionInfo` 增加 `visibility` / `can_write`

**已实现（Phase 2 残留 + Phase 5，本次）**：
- 新增 `src/lib/auth-api.ts`：`/api/auth/*` + `/api/users` 的薄 HTTP 封装（登录 / 刷新 / 登出 / 改密 / me / 账号列表 / 软删除），错误统一为 `AuthApiError(status, message)`
- 新增 `src/lib/session/tokens.ts`：**token 对的唯一所有者**（事后重构，修"登录后约 13 分钟必被踢下线"）。原实现把 token 生命周期**碎片化**在四个调用点——`authStore` 的 proactive 定时器、`authStore.init()` 的 401 分支、`authFetch` 拦截器的 401 分支、`gatewayAuthBridge` 的 `AUTH_REQUIRED_EVENT` 分支——每个都是 token 的**需求方**却同时在做**管理动作**（直接调 `refreshTokens()`）。后果：同一 webview 内相隔 3 ms 的两个 refresh 必然发生，第二个被服务端判成重放 → 连坐撤销 → 弹登录 → 循环。单飞字段（`_refreshPromise`）当时**只覆盖** `refreshTokens()` 这一个入口，跨入口的并发完全没被串行化——不变量写在数据结构里，却没有被架构守住。
  - **边界**：模块拥有持久化、`exp` 记账、proactive 定时轮换、单飞、以及"这个会话已死"的判定；需求方只有两个动作——`acquire()`（取一个还能用的 token）与 `reportRejected(token)`（上报"这个被拒了"这一**事实**，拿回一个可用 token）。需求方**永远不命令轮换**，也拿不到 refresh token（`refreshToken` 已从 `authStore` 移除）。
  - **高内聚 / 低耦合**：模块零 store 依赖，轮换传输由 `configure({ refresh, logout })` 注入（在 `authStore` 模块加载处接线）。直接 import `lib/config` 会成环（config → settingsStore → authStore → 本模块），注入把这条边断掉，同时给出唯一的可测接缝——单测不需要任何 store。
  - **开闭**：新增 token 需求方（未来的移动端 / 新拦截器）只调 `acquire()`，不需要新增任何轮换逻辑；新增**轮换策略**（改宽限、改提前量、换退避）只动模块内部，需求方零改动。
  - **proactive 定时器保留且必须保留**：relay 模式 MQTT 的 CONNECT 密码**就是** access token，坏凭证被 `rumqttd` 无 CONNACK 丢弃，401 惰性路径对它完全无效——那个消费者只能靠"过期前就新鲜"。
  - `authStore` 净减约 150 行；四个 refresh 入口 → **0 个**（唯一入口在模块内部）。测试迁移：删除 `authStore.proactive.test.ts`，其覆盖面（提前量 / 重挂 / 停滞上限 / 不可读 `exp`）连同新增的单飞合并、`reportRejected` 去重、会话终结信号，落到 `src/lib/session/tokens.test.ts`（22 例，纯模块、无 store）。
- 新增 `src/lib/authFetch.ts`：**全局 `fetch` 拦截器**（替代原稿的 `src/lib/api/auth.ts` 逐调用点包装）——只对 Gateway origin 注入 `Authorization`，跳过 `/api/auth/*`，401 → `reportRejected` → 重放一次；`AUTH_MODE=local` 下纯透传。取舍理由：Desktop 有 ~170 处裸 `fetch(` 调用点，逐点包装极易漏一处（漏点 = multi_user 下静默 401），拦截器保证零遗漏
- 新增 `src/stores/authStore.ts`：模式解析（`/api/status` 的 `auth_mode`）+ 登录门禁状态 + 账号 UI 状态；账号切换 / 退出 / 注销 / 改密均**清 token + `window.location.reload()`**（§9 问题 6 的"已登出未登入"中间态 = 重载落 LoginView，沿用仓库既有 `location.reload` 恢复范式，替掉原稿的逐 store reset）。**token 不在此处管理**（原稿的 `localStorage` 读写 + 单飞 `refreshTokens` 已迁出，见上条 `lib/session/tokens.ts`）；`accessToken` 只是模块的**只读镜像**，因为十余个组件绑定它并需要在轮换时重渲染
- 新增 `src/components/account/`：`LoginView`（App 门禁）+ `AccountMenu`（复用 `common/ContextMenu` 弹层：切换账号 / 改密 / 注销 / 用户偏好 / 退出）+ `ChangePasswordModal`
- 新增 `src/components/user-list/UserList.tsx` + `partitionAccounts.ts`：侧栏 "Users (N)" 折叠分组（admin 列全部账号，普通用户只列自己）；admin 点账号 → `authStore.viewAsUserId` → `agentStore.fetchSessions` 追加 `?as_user=`（只读视图）
- 修改 `src/components/layout/NavBar.tsx`：头像入口改用 `AccountMenu`（local / 未登录回退到旧"编辑资料"行为）
- 修改 `src/App.tsx`：`installAuthFetchInterceptor` 在 `main.tsx` 启动期挂载；`logged_out` → LoginView，`unknown`（模式解析中）→ 空面，其余 → AppLayout
- 修改 `src/lib/types.ts`：`UserAccount` / `AccountListResponse` / `TokenPair` / `AuthMode` / `AuthState` / `Role`；`SystemStatusResponse.auth_mode`
- 修改 `src/stores/agentStore.ts`：`fetchSessions` 按 `viewAsUserId` 追加 `as_user`
- 修改 `src/i18n/locales/*`：`account.*` / `userList.*` 键（en / ja / ko / zh-CN / zh-TW）
- Gateway `src/http/routes.rs`：`GET /api/status` 新增 `auth_mode` 字段（决策 12 的模式探测入口；`/api/status` 本就在中间件白名单）
- 未做（留后续）：无——本 block 列出的三项（`registration_open` 的 Desktop 入口、用户-用户聊天 UI、附件上传 API + UI）均已在下方的 Phase 6 / 非 admin 入口 block 结清

**已实现（本轮：非 admin 建号入口结清，§决策 6 / §9 问题 9）**：
- Gateway `src/http/routes.rs`：`GET /api/status` 再增 `registration_open: bool`；取值 = `auth_service.is_some() && config.multi_user.registration_open`——账号系统没跑时恒 `false`，前端因此不可能渲染出一个必然 403 的按钮
- 修改 `src/lib/auth-api.ts`：`fetchAuthMode()` → `fetchAuthPolicy()`，一次 `/api/status` 探测同时返回 `{ authMode, registrationOpen }`（原函数只有一个调用点，改名比多探一次端点便宜）
- 修改 `src/stores/authStore.ts`：新增 `registrationOpen`（`init()` 里随模式一起落库）
- 修改 `src/components/user-list/UserList.tsx`：`canInvite = isAdmin || registrationOpen` 决定分组顶部 "+"；非 admin 的 `onCreated` **不** `reload()`（`GET /api/users` 是 admin-only，非 admin 只会拿到 403 并亮出"加载失败"横幅；他们的分组本就只列自己，没有可刷新的内容）
- 测试：新增 `src/components/user-list/UserList.registration.test.tsx`（3 条：非 admin + 开 → 有按钮；非 admin + 关 → 无按钮；admin → 恒有）。`cargo test -p acowork-gateway --lib test_system_status` 2 passed（local 模式断言 `!registration_open`）；Desktop 全套 803 passed（唯二失败为预存的 `formatTime` 时区 / `DocRichEditor` tiptap 模块）

**已实现（Phase 6 Desktop 聊天 UI，本次）**：
- 新增 `src/views/MessagesView.tsx`：用户聊天视图（左会话列表 + 右会话线程 + 输入框）；`Enter` 发送 / `Shift+Enter` 换行；发送失败把草稿**放回**输入框（不吞用户输入）；会话标题优先用 `GET /chats` 的 `peer_display_name`，新建会话（还没有列表行）回退到 `UserList` 传进来的标签，最后才是裸 id
- 新增 `src/stores/userChatStore.ts`：`chats` / `activePeerId` / `messages` + `send`（**用 Gateway 的回显**，绝不本地铸造 `ts` / `from`）/ `openChat`（只加载线程，**不再**发已读回执）/ `markThreadRead`（由**渲染线程的组件**调用；按"最新消息 ts"去重，所以 5 s 轮询不会变成 5 s 一次 POST）/ `reset`；轮询是 **store 自己拥有的单一 interval**，由 `authStore` 订阅驱动（`syncPolling`→登录即启动、登出即停），**不挂在任何组件上**
- **修订（轮询宿主，2026-10）**：上面这条原写作 `startPolling`（引用计数，由"当前挂载的收件箱视图"调用）。全屏 `MessagesView` 被改成嵌在聊天窗格里的条件组件 `InboxPanel` 时，`useEffect` 跟着 JSX 一起搬家，宿主语义从"登录即在"静默变成"`currentView === "chat"` **且** `selectedAgentId === null` 才在"——选中任一 agent 会话就停掉全部轮询，另一台设备发来的消息永远不到达（`unread` 因此长期挂在服务端）。引用计数对"零个宿主"无感，所以没有任何信号暴露这次退化。**结论：数据同步循环的生命周期不得寄生在渲染树上**，因为渲染树正是重构会移动的东西；改由 store 订阅会话状态。已读回执**保留**在渲染者一侧（"用户看见了"只有渲染者知道），二者分离是刻意的：若将来再被孤立，后果只是角标滞后，而非数据静默丢失。测试：`src/stores/userChatStore.test.ts`（"polls with zero components mounted and dies with the session" / "opens a thread and loads it WITHOUT acking"）
- 新增 `src/lib/user-chat-api.ts`：4 条路由的薄封装。**刻意不传 token**——这些路径不在 `/api/auth/*`，由全局拦截器注入 + 401 刷新，在这里再抄一份 token 流转只会更弱；`auth-api.ts` 的 `readError` 改为导出以复用（`{error}` / `{detail}` 解码只留一处）
- 新增 `src/components/common/MessagesIcon.tsx`：nav 图标的 outline / filled 变体。**刻意不复用 `ChatIcon`**：两个 nav 目标在同一根 40px 竖条里，24px 下必须能分辨
- 新增 nav 未读红点：由 `userChatStore` 的未读合计驱动，红点挂在常驻的 `NavBar`（`NavButton` 的 `badge` prop，40px 图标上只显点不显数字）；轮询由会话状态驱动而非由 `NavBar` 挂载（见上条修订）——消息在用户正看 agent 会话、或停在任意其它视图时到达也看得见。`local` 模式下该红点天然不出现（`chats` 恒为空，路由本就不存在，避免"显示然后 404"）
- 修改 `src/stores/layoutStore.ts` + `src/components/layout/AppLayout.tsx`：新增 `requestNavView(view)` 的 seq 契约（沿用 `workspaceSearchFocusSeq` 的 consume-once 范式）。`currentView` 是 `AppLayout` 的私有 state，侧栏要"跳到收件箱"只能走这个通道
- 修改 `src/components/user-list/UserList.tsx`：右键菜单新增"发消息给该用户"（自己、已禁用账号不显示），打开线程并切到 `users` 视图
- 修改 `src/lib/types.ts`：`NavView` 增 `"users"`；新增 `UserChatSummary` / `UserChatMessage` / `UserChatMessagesPage` / `UserDirectoryEntry`（对齐 `ChatSummary` / `ChatMessage`）
- 新增"新会话"选择器：`MessagesView` 左栏头部按 `GET /api/users/directory` 渲染联系人下拉（端点失败 / 暂无他人 → 不渲染，收件箱降级但**回复仍可用**）；`user-chat-api.ts` 加 `listUserDirectory` / `contactLabel`。admin 的 `UserList` 右键"发消息"保留为快捷方式，不再是唯一入口
- 修改 `src/i18n/locales/*`：`navBar.users` + `messages.*`（共 11 键 × 5 locale，用脚本逐键核对通过）
- 附件 UI：composer 的回形针 + 待发附件条（可逐个移除，上传失败的**不入队**）+ 气泡内图片缩略图 / 文件条（点击下载）+ `attachmentObjectUrl` 的 blob 缓存。**图片走 `fetch` + `createObjectURL` 而不是 `<img src="/api/...">`**——下载路由要 Bearer，全局拦截器只拦 `fetch`，把 token 塞进 URL 才是更差的做法
- 未做（Phase 6 剩余）：无；普通用户发起会话已闭环（见 §5.5，残余 ceiling = 名录全量可枚举）

**已实现（Phase 5 admin 账号管理 UI + Phase 6 后端，本次）**：
- 新增 `src/components/account/CreateAccountModal.tsx`：admin 建号（username / display_name / 可选 password；不填 → 返回 `invite_token` 走首次登录激活）
- 新增 `src/components/account/InviteTokenModal.tsx`：`invite_token` 展示 + 复制（24h 过期、用后即焚）。建号与重置密码共用该弹窗
- 修改 `src/components/user-list/UserList.tsx`：admin 右键菜单接线（禁用 → 确认弹窗 / 重置密码 → `InviteTokenModal` / 以该用户视角查看 → `setViewAsUser`）；分组顶部 `+` 打开 `CreateAccountModal`
- 修改 `src/lib/auth-api.ts` / `src/stores/authStore.ts`：`createAccount` / `disableAccount` / `resetPassword` / `deleteAccount` + `accounts` 缓存
- 修改 `src/i18n/locales/*`：本轮新增的 `account.*` / `userList.*` 键在 5 个 locale 中均已补齐（用 `jq` 逐键核对通过）
- Gateway：新增 `src/chat.rs`（含附件存储：`store_attachment` / `load_attachment` / `content_disposition` / mime 与文件名的入口规整）+ `src/http/chat_api.rs` + `AuthService::data_dir()`；`routes.rs` 新增 `ApiError::payload_too_large`（413）；`routes.rs` 在同一个 `auth_service.is_some()` 分支内 `merge(chat_api::chat_routes())`（`dev/ci.sh` 的 auth-mode 红线扫描通过）
- Gateway：`src/http/account_api.rs` 新增 `GET /api/users/directory`（**任何认证账号**可读的联系人名录，§5.5）+ `UserDirectoryResponse` / `DirectoryEntry`；`routes.rs` 注册在 `/api/users/{user_id}` **之前**（axum 0.8 本偏好字面量，写在前是为了读代码的人不必知道那条规则）
- 测试：Gateway 新增 **24** 条。`chat::tests` 16 = 配对规范化 / 未读语义 / 双方可见性 / 尾部翻页 / 坏行跳过 / 预览截断 / 自我对话拒绝 / 非参与者拒读 + 附件 8 条（参与者收敛 / **跨会话不可借用** / 元数据以服务端为准 / 注入式 mime 回落 / 路径不可穿越 / 限额按 mime 分档 / CJK 文件名落 `filename*` / 无正文消息回退到附件名预览）；`http::chat_api::tests` 7 = 收发 + 未读 + admin 只读视图 / 第三方与 admin 越权 / 空 body 与无 token / **附件上传→发送→下载全链路** / **附件读写权限边界** / **限额与空附件 413·422** / **上传路由突破 64 MiB 全局 body 上限**；`http::account_api::tests` 1 = 名录普通用户可读且被有界收窄。`cargo clippy --all-targets -D warnings` 干净，`cargo test -p acowork-gateway --lib` **596 passed**；Desktop `tsc --noEmit` 在改动文件上零错误，`authStore` / `authFetch` / `partitionAccounts` / `userChatStore` **48 passed**

### 6.6 dev/ci.sh 新增 ceiling lint（**已实现**）

```bash
# ADR-076 §决策 4：身份注入只有一个可信写入方（auth_middleware）。
# USER_SCOPE_HEADER 常量与 "x-user-id" 字面量都只应出现在其定义处 + 测试里（都在 auth_middleware.rs）。
run_gateway_auth_scope_redline() { ... }   # grep (USER_SCOPE_HEADER|"x-user-id")，排除 auth_middleware.rs

# ADR-076 §决策 12：账号 API 只在 multi_user 下注册——必须在 auth_mode 分支内。
run_gateway_auth_mode_redline() { ... }    # awk：auth_routes()/account_api 注册行前 4 行须含 auth_service/auth_mode
```

两者已挂到 `dev/ci.sh` 的 `check` 与 `all` 模式。第一条防止身份注入被绕开（proxy 层自行注入 `x-user-id`）；第二条防止后续 PR 把 multi_user 路由改成无条件注册。

> **为什么不 lint "proxy 是否注入 user_id"**：原稿的这条 lint 基于"Gateway 反代时注入 + 校验 owner"的设计，而实现把 owner 判定放在 Runtime（§决策 4 职责划分）。Gateway 反代层**本来就该看不到 `user_id`**——lint 应当反过来断言它**不出现**在那里。

### 6.7 core/acowork-pm（决策 10 + 11 联动）

> **实现状态（本次实施，决策 11 已落地）**：
> - `src/types.rs`：新增 `MemberKind { Agent, User }`（`#[serde(default)] = Agent`）与 `ProjectMember.kind`；`MemberKind::from_actor()` 从 actor 值推断（`human`/`unknown` → `User`，其余 → `Agent`）——**已实现**
> - `src/store/tree.rs`：`PmStore` 新增 `create_project_as` / `create_task_as`（显式 `kind`；裸 UUID 无法按值区分 user_id 与 instance_id，故入口必须显式声明）；`create_project` / `create_task` 保留为便捷包装（内部 `from_actor` 推断，供测试与遗留路径）；`create_project_as` 自举改为 User / Agent 创建者**都**入 members 并带 `kind`；`create_task_as` 的 `review_status` 判定从 `== "human"` 改为按 `kind`——**已实现**
> - `src/api/projects.rs` / `src/api/tasks.rs`：REST 面（人类操作面）显式传 `MemberKind::User`——**已实现**
> - `src/mcp/tools.rs`：MCP 面（Agent 工具面）显式传 `MemberKind::Agent`；`pm_get_project` 成员投影新增 `kind` 字段，User 成员不查 Agent 目录；`creator_is_user()` 让 User 创建者的 `created_by_meta` 短路为 `null`——**已实现**
> - `src/mcp/agent_dir.rs`：成员校验沿用 `agent_exists` 兜底；User 成员不做 MCP 校验——**未改动（无必要）**
> - `apps/acowork-desktop/src/lib/pm-types.ts`：`PmProjectMember` 新增 `kind?: "agent" | "user"`——**已实现**
> - 测试：`tree.rs` 的 `creator_is_auto_member_for_agent_and_human`（原 "human 建项目 members 为空" 断言改为"人类创建者入 members 且 kind=User"）、新增 `explicit_kind_drives_review_status_and_member_kind`（形似 UUID 的 user_id 不被误判为 Agent）；`member_add_remove_roundtrip` / `remove_member_with_open_tasks_conflicts` 计数随"创建者入 members"调整——**已实现**

---

## 7. 测试策略

### 7.1 单元测试

- `core/acowork-gateway/src/account/`: 账号创建 / 改密 / 注销 round-trip；Vault locked 状态下元数据可见
- `core/acowork-gateway/src/auth/token.rs`: HS256 签发 / 校验；refresh family rotation；篡改 token 拒绝
- `core/acowork-gateway/src/auth/revoked.rs`: revoke family 后旧 refresh_token 失效；`r:` 轮换与 `x:` 显式撤销可区分
- `core/acowork-gateway/src/auth/service.rs`（**已实现，16 项**）：登录成功 / 大小写不敏感 / 错密码 / 未知用户 / `$disabled$` / `disabled_at` 全部 401 且不可区分；access/refresh 不可互换；access 过期；篡改签名；refresh 轮换 + 复用检测连坐；登出只杀本设备、不连坐；改密需旧密码 + 杀全部 family；签名密钥跨重启复用；bootstrap 空表必配 / 配置一次后忽略 / 密码策略校验；展示字段 patch（`update_account_applies_the_display_patch`：`None` 不动 / 空 avatar 清除 / 空白 display_name 忽略 / 落盘一致）
- `core/acowork-gateway/src/http/auth_api.rs`（**已实现，4 项，走真实 `build_router`**）：local 模式 `/api/auth/login` → 404（未注册）；multi_user 无 token 访问 `/api/agents` → 401、`/health` 放行；登录 → `/me` 脱敏（无 `password_hash`）→ 非 admin `as_user` → 403 → refresh 轮换 → 改密后旧 refresh 失效；bootstrap admin 可登录
- ✅ `core/acowork-runtime/src/conversation.rs`: `SessionMeta` 序列化含/不含 `user_id` 兼容（无字段的旧 jsonl 仍可加载）；`set_user_id` write-once
- ✅ `core/acowork-gateway/src/http/auth_middleware.rs`（**已实现，1 项，走真实 `build_router` 层**）：客户端伪造 `x-user-id` 被剥；无 token 401；admin 得到 `*`；`as_user` 收窄 scope；非法 `as_user` → 403
- ✅ `core/acowork-runtime/src/conversation.rs`（**已实现**）：`SessionScope::from_header_value` 三态（`*` → `Unfiltered`、具体 id → `User`、空/缺失 → `Unfiltered`）；`is_readable_by` / `is_writable_by` 在 `(user_id 归属 × visibility 三态 × scope)` 组合矩阵上的判定；`visibility` 缺省为公开 + 向后兼容
- ✅ `core/acowork-runtime/src/http/session_control.rs`（**已实现**）：`only_an_admin_may_re_share_an_unowned_session`——`may_change_visibility` 的真值表（owner 可 / 无主会话的普通账号不可 / admin 可），对应 `PUT .../visibility` 对无主会话的 403
- ✅ `apps/acowork-desktop/src/lib/agent-start.test.ts`（**已实现**）：`opens the caller's newest readable session instead of retrying`（`/latest-session` 404 且自己有会话 → 开自己最新的那条，**一次都不重试**、不新建）+ `creates a session when the account genuinely has none`（两个来源都空 → 建一条自己的）
- ✅ `apps/acowork-desktop/src/components/user-list/UserList.registration.test.tsx`（**已实现，3 项**）：非 admin + `registration_open` → 分组顶部 "+" 可见；非 admin + 关闭 → 不可见；admin → 恒可见
- ✅ `core/acowork-runtime/src/agent/session/session_manager.rs`（**已实现**）：`owned_sessions_start_private_and_ownerless_stay_public`——有主会话创建即落盘 `Private`（断言**磁盘 meta**，因为列表/鉴权读的是文件，只存在内存里的值就是 bug），另一账号读不到、owner 与 admin 读得到；无主会话仍写 `None`（local 模式与升级前数据不受影响）
- ✅ `core/acowork-runtime/src/conversation.rs`（**已实现**）：`visibility_and_ownership_gate_read_and_write`（private 非 owner 读/写均拒；public 非 owner 可读不可写；**无主 + Private → 普通账号读/写均拒**，无主 + `None`/`Public` → 可读可写）、`session_scope_from_header_value`（`*` / 具体 id / 缺失三态）、`scan_filters_by_scope_before_paginating`（过滤先于分页，`total_count` 反映可见行数）、`session_meta_visibility_is_absent_by_default_and_means_public`、`set_visibility_persists_and_clears_back_to_public`
- ✅ 不可读 / 不可写一律 **404 而非 403**（`session_control::not_found` 这一处共享映射）：handler 接线由既有 HTTP server 测试覆盖——`test_session_config_unknown_session_and_visibility_gate`、`test_http_upload_file_docx_lands_with_real_extension`（上传先要 session 存在）
- ✅ `core/acowork-runtime/src/usecases/session_metadata_impl.rs`：`list_sessions(page, size, scope)` 只返 scope 可见的 session，**且在分页前过滤**（`total_count` 反映可见行数）

### 7.2 集成测试（e2e）

> 第 1 条的**部署面**（拒启动 / 模式 / 落盘）已在 §7.5 做了**进程级**验证，**数据面**（session 过滤）仍是跨进程 e2e 的残余项。第 2、3 条（改密 / 注销流程）的**单测**已由 `account_api::tests` 覆盖（invite 生命周期 / 非 admin 自我封闭 / 冲突拒绝 / `registration_open` 门控 / 展示字段持久化与头像写穿——`update_account_persists_display_fields`、`avatar_config_writes_through_accounts_under_multi_user`）——账号 CRUD 与改密的进程级验证仍待做（§7.5 只覆盖了部署模式，见下）。第 4 条（用户聊天）**已落地并覆盖**。

- ◐ 完整流程：admin 创建 → alice 创建 → alice 创建 session → bob 看不到 alice 的 session → admin 用 `?as_user=alice` 看到（scope 解析 / 过滤 / owner 校验已有单测；账号创建 → 登录 → 越权拒绝这段也已有 router 级集成测试 `auth_api::tests`，**仅剩**「跨进程」这段——需要 Gateway + Node + Runtime 同时起）
- 改密流程：alice 改密 → 旧 refresh_token 失效 → 必须重新 login
- 注销流程：alice DELETE self → alice 无法再 login → 历史 session 仍可被 admin 读
- ✅ 用户聊天：A → B 发文字 + 图片 + 文档 → B 收到 → unread_count 增加 → B read 后清零（**已实现**：`chat_api::tests::send_read_and_list_across_two_accounts` + `attachment_round_trips_from_upload_to_download`，走真实 `build_router`）
- Vault locked 流程：Vault 锁上后 alice 仍可登录（`password_hash` 在明文的 `accounts.json`），但 admin 想读 `vault/accounts/*.enc` 时报错

### 7.3 协议兼容性

- `SessionMeta.user_id` 字段为 `Option<String>` + `skip_serializing_if`：旧 meta.json 无此字段 → 加载为 `None`（行为 = 旧版）
- `user_profiles.json` 保留兼容：现有 `UserProfile` 字段不动，新增字段（如 `username` / `role`）作为可选

### 7.4 安全测试（手动 checklist）

- [x] 普通 user 用 `?as_user=<admin>` 访问 → 403（已单测）
- [x] admin 用 `?as_user` 调**写**方法（POST / DELETE）→ 403（已单测；强制点在 `auth_middleware`，见 §6.4）
- [x] 普通 user 改 `X-User-Id` header → **Gateway 中间件直接剥除**并注入 token 派生值（local 模式只剥不注），客户端值永远到不了 Runtime（已单测）
- [x] 非 owner 读 private session → 404（不是 403，不泄漏存在性）；非 owner 写公开 session → 404（公开 ≠ 可改）（已单测）
- [x] `visibility` 开关对 **config** 同样生效（走同一个 `authorize_read`）：`test_session_config_unknown_session_and_visibility_gate` 断言 private 时非 owner 读 config → 404、同 owner 翻成 public 后非 owner 读 → 200、且 public 不放开写（PUT → 404）
- [x] 注销账号后旧 refresh_token → `disable_account` / `reset_password` 调 `revoke_user` → 拒绝（`account_api` 单测断言注销后 login 失败；`AuthService` 单测覆盖 `revoke_user` 命中）
- [x] admin 用 `as_user` 调 POST（写操作）→ 403（`auth_middleware` 强制点，已单测）
- [x] 跨用户聊天路径：`POST /api/users/{A}/chats/{B}/messages` 以 A 身份发送，从 token 拿 from 字段 → from=B 拒绝（**已实现**：`chat_api::tests::third_party_and_admin_cannot_write_as_someone_else`——第三方与 admin 都不能代写；`upload_attachment` / `download_attachment` 的 self-only 写 + self-or-admin 读由 `attachment_writes_are_self_only_and_reads_are_scoped` 覆盖）
- [ ] **未覆盖（已知缺口，见 §10.2 第 14 项）：登录端点的暴力破解防护**——`POST /api/auth/login` 没有速率限制，也没有账号 lockout（连续失败不锁定）。当前唯一的缓解是**部署面**：`multi_user` 由 bind 地址推断，默认部署是 `127.0.0.1` 回路；密码强度（`password_policy`）是仅剩的在线防线。**把 Gateway 暴露到非信任网络之前必须先补这一项**——它属于"信任边界上的防护"，不是可以留白的 ceiling。

### 7.5 部署模式测试（决策 12）

**单元测试**（`core/acowork-gateway/src/auth/mode.rs`）：

- `resolve_auth_mode` 真值表：
  - `bind = 127.0.0.1:19876`，无显式 flag → `Local`
  - `bind = 0.0.0.0:19876`，无显式 flag → `MultiUser`
  - `bind = 192.168.1.20:19876` → `MultiUser`
  - `bind = [::1]:19876` → `Local`
  - `bind = [fe80::1]:19876` → `MultiUser`（link-local 默认安全侧）
  - 显式 `--auth-mode local` + `bind = 0.0.0.0` → `Local`（显式覆盖 bind）
  - 显式 `--auth-mode multi_user` + `bind = 127.0.0.1` → `MultiUser`（显式覆盖 bind，仅 warn）
- 优先级链：CLI > TOML > bind 推断 > default

**集成测试**（`core/acowork-gateway/tests/auth_mode_e2e.rs`，**已实现，4 个用例**）：

> 真起二进制（`target/debug/acowork-gateway --daemon --home <tmp> …`，私有端口 + 私有 `--home`），断言只取"进程级才看得见"的东西：退出码、stderr、端口是否能连、`--home` 落盘。**不引入 HTTP 客户端**——路由注册本身已由 `build_router` 的 in-process 测试覆盖，再加一个客户端只是把同一件事验两遍。

- ✅ **multi_user 拒启动**：空账号表 + 无 `bootstrap_admin` → 退出码 != 0，stderr 含 `bootstrap_admin` 与 `multi_user`，且**不留下** `accounts.json`（`multi_user_without_bootstrap_admin_refuses_to_start`）
- ✅ **multi_user 正常启动**：从 **TOML**（而非 CLI flag）读 `auth_mode` + `[multi_user.bootstrap_admin]` → 端口可连；`accounts.json` 落盘、含 `root`、且**明文不含** bootstrap 密码（`multi_user_from_config_bootstraps_an_admin_and_serves`）
- ✅ **local 模式启动**：bind `127.0.0.1` + 无 flag → 正常启动；`accounts.json` **不存在**；`data_dir/auth/` **不存在**（`local_mode_leaves_no_account_state_behind`）；`user_profiles.json` 保持现状（不升级 schema、不写 sentinel）
- ✅ **显式 local 压过公网 bind**：`--addr 0.0.0.0:<port> --auth-mode local` → 仍是 local，不创建任何账号状态（`an_explicit_local_mode_overrides_a_public_bind`）。这是唯一一个会让服务裸奔的组合，必须在进程级钉死
- ◐ **local / multi_user 的 session 过滤**：判据（`SessionScope` 三态 / `is_readable_by` / 分页前过滤）已有单测（§7.1）；**跨进程** e2e 需要真起 Runtime + Node，仍未做（见下方"仍未覆盖"）

**已落地的替代覆盖**（Phase C-2 + Phase E，走真实 `build_router`，见 `core/acowork-gateway/src/http/auth_api.rs` 与 `account_api.rs` 的 `tests`）：
local 模式 `/api/auth/login` → 404、`/api/users` 走展示路由（`/reset-password` → 404）；multi_user 无 token `/api/agents` → 401 而 `/health` 放行；登录 → `/me` 脱敏 → 非 admin `as_user` → 403 → refresh 轮换 → 改密后旧 refresh 失效；bootstrap admin 可登录；**Phase E**：invite 生命周期单次有效（创建无密账号 → first-login 激活 → replay 失败 → 新密码可登录）、invite 24h 过期 + 非法时间戳 fail-closed、`reset-password` 铸新 invite + 清密码 + 杀全部 refresh family、非 admin 只能读/删自己（list / disable / reset → 403）、`registration_open` 开关（关 → 非 admin 建号 403；开 → 可建普通账号但**永不**是 admin）、重名 409、末位 admin 不可 disable（409）。**本次补上**：`Gateway::new` 拒启动 / 起得来 / local 不留痕 / 显式 local 压过公网 bind——四条的**真实进程级**验证见上（`tests/auth_mode_e2e.rs`）。**仍未覆盖**：session 过滤的跨进程 e2e——它需要同时起 Gateway + Node + Runtime（`?as_user=` 走到 Runtime 的 `authorize_read` 才有意义），成本是一套多进程 harness；判据逻辑本身已在 §7.1 用单测穷举（三态 × 归属 × visibility 矩阵）。

**回归防护**（`dev/ci.sh` 新增 ceiling lint，**已实现**）：实际落地为**三条** redline 函数——本次新增 `run_gateway_chat_path_redline`（§决策 8：`.join("users"|"chats"|"files"|"conversation.json"|"messages.jsonl")` 只允许出现在 `src/chat.rs`，防止第二处自行推导 `min__max` 配对路径——顺序推错的第一个后果就是读到别人的消息）；另两条为 `run_gateway_auth_scope_redline`（`x-user-id` 只能出现在 `auth_middleware.rs`，防止反代层自行注入身份）与 `run_gateway_auth_mode_redline`（`auth_api::auth_routes` / `account_api::` 的注册必须被 `auth_mode` / `auth_service` 条件保护），已在 `check` / `all` 模式注册并验证过负例。原稿里的 `accounts.json` 分流 lint / session 路由清单 lint 未单独落地——前者由 `account_api` 只在 multi_user 下 `merge` 覆盖（`routes.rs` 的 `match &state.auth_service`），后者由 Runtime 侧 handler 自带 `authorize_read` 覆盖。

```bash
# 决策 12：local 模式下不得注册 auth/admin 路由——防止后续 PR 误把 multi_user 路由变成无条件注册
grep -rn "auth_middleware\|account_api::router" core/acowork-gateway/src/http/routes.rs \
  | grep -v "auth_mode\|AuthMode" && echo "FAIL: 路由注册未按 auth_mode 分流" && exit 1

# 决策 12：local 模式下 `accounts.json` 不得创建
grep -rn "save_accounts\|account_list_path" core/acowork-gateway/src/ | grep -v "auth_mode\|auth::mode\|AuthMode" \
  && echo "FAIL: accounts.json 写入未按 auth_mode 分流" && exit 1

# 决策 4：session 路由必须逐个显式列出——新增路由时强制人工确认校验路径
# 期望：8 行，且每行的 handler 要么在 session_control:: 内（自带校验），
#        要么自己调用 authorize_read（get_session / get_messages / /latest）
grep -nE '"/sessions' core/acowork-runtime/src/http/server.rs
```

### 7.6 Phase D 数据面落地后的验证结果

- `cargo test`：core **216** / gateway **531** / runtime **1486** 全过（含 scope / visibility / `session_control` / `as_user` 只读守卫 / `can_write` 下发 / MQTT 写路径拒绝边界测试）
- `cargo clippy -p acowork-core -p acowork-gateway -p acowork-runtime --all-targets -- -D warnings`：三 crate 干净
- Desktop：`chatStore.test.ts` **60 passed**；`vitest` 全套 695 passed / 1 failed；`tsc --noEmit` 在我改动的文件上零错误
- 4 个预存失败与本次改动无关：`git_api_e2e` 2 个 case（git 环境差异）、`formatTime.test.ts` 的时区骨架断言、`DocRichEditor.test.tsx`（缺 `@tiptap/react` 模块）；另有 `doc_supervisor_integration`（缺 `acowork-doc` 二进制）、`acowork-embed`（缺 ONNX 运行时）不在本次跑的范围内

---

## 8. 实施里程碑（建议）

> **进度（本次实施）**：Phase 1-4 的**后端全部完成并测试通过**——`UserAccount` 模型 + Argon2id + `accounts.json` + `/api/auth/*`（含 `/first-login`）+ token 中间件 + bootstrap_admin fail-fast（Phase 1-2）；`SessionMeta.user_id` + `visibility` 开关 + Runtime scope 过滤 + owner 校验 + **会话写路径全量 MQTT→HTTP 迁移**（Phase 3，见 §决策 4 Phase D 实施记录），MQTT 侧**全部用户操作命令**（两批共 16 条：生命周期 8 + 会话动作 8）的 proto 字段 / Runtime 变体 / 命令名映射表已**删除**并重排为连续，`can_write` 下发到前端用于禁用写控件；**Phase 4 账号 CRUD**（`account_api.rs` + `registration_open` 接线 + invite/first-login 生命周期）已完成。Phase 5-7 当时未动（Desktop UI / 用户聊天 / 进程级 e2e + 手册）——**这一段是历史记录，随后已全部落地，见下一段「补充」；本 ADR 当前的剩余项以 [§10 遗留清单](#10-遗留清单仍未做全集) 为唯一权威**（正文里不再散标 ⬜，避免出现两处互相矛盾的状态）。
>
> **补充（本次）**：Phase 2 残留（Desktop `authStore` + 全局 fetch 拦截器 + `LoginView` 门禁 + 顶栏账号菜单）与 Phase 5（Sidebar User 折叠分组 + admin 账号管理 UI + `?as_user=` 过滤）已落地，见 §6.5「已实现（Phase 2 残留 + Phase 5，本次）」与「已实现（Phase 5 admin 账号管理 UI + Phase 6 后端，本次）」。Phase 6 的**后端**（`src/chat.rs` 持久化 + `src/http/chat_api.rs`，见 §6.4）与 **Desktop 聊天 UI**（`MessagesView` + `userChatStore` + nav 未读红点 + `requestNavView`，见 §6.5「已实现（Phase 6 Desktop 聊天 UI，本次）」）均已落地并测试；**普通用户发起会话**所需的 `GET /api/users/directory`（§5.5，本次决策）与 `MessagesView` "新会话"选择器一并完成——§决策 8 除附件外已闭环。**附件上传/下载**（§决策 9）也已落地；§决策 8 + 9 至此全部闭环。**Phase 7 本次收尾**：进程级 e2e（`tests/auth_mode_e2e.rs`，4 用例，见 §7.5）、第三条 ceiling lint（`run_gateway_chat_path_redline`，见 §7.5 回归防护）、用户手册（[`docs/runbooks/multi-user-accounts.md`](../../runbooks/multi-user-accounts.md)）、协议文档（`http.md` §4.14 / §4.15）。**唯一残余**：session 过滤的跨进程 e2e（需 Gateway + Node + Runtime 三进程 harness；判据逻辑已在 §7.1 穷举单测）。**再补充（本轮）**：最后一处"后端已接线、前端无入口"的洞已补——`registration_open` 的 Desktop 入口（非 admin 在开关打开时可见 "+"，见 §决策 6 / §9 问题 9），同时把两项产品决策写成决议（名录枚举维持现状 §5.5；上传配额否决 per-user 框架、改记全局水位 §5.5 / §9 问题 10）。**收口**：所有仍未做完的事已汇总为 [§10 遗留清单](#10-遗留清单仍未做全集)（13 项有意留白 + 8 项未实现 + 2 项测试缺口 + 7 项已否决），每项带触发条件与升级路径——后续不必重读全文就知道还剩什么。

| Phase | 内容 | 估时 | 状态 |
|---|---|---|---|
| 1 | `UserAccount` 数据模型 + Argon2id password_hash + Vault 加密扩展字段 | 1 周 | ✅ 模型 / Argon2id / store 完成；Vault 加密扩展字段未做（Vault locked 登录已可用，扩展字段非阻塞） |
| 2 | `/api/auth/*` + token middleware + `authStore` + 顶栏账号菜单（登录 / 改密 / 注销） | 1 周 | ✅ 后端完成（`AuthService` + 5 条路由 + 中间件 + bootstrap_admin）；Desktop `authStore` / 账号菜单**已完成（本次）**：`authStore` + 全局 `fetch` 拦截器（`authFetch`）+ `LoginView` 门禁 + 顶栏 `AccountMenu` + 改密 modal |
| 3 | `SessionMeta.user_id` + `visibility` + Runtime scope 过滤 + owner 校验 + 控制面 HTTP 化 | 1.5 周（含 grep ceiling lint） | ✅ schema（`user_id` write-once + `visibility`）+ Gateway 头部卫生（剥/注 `x-user-id`）+ Runtime scope 过滤（分页前）+ 读/写 owner 校验 + **14 条 HTTP 控制路由**（生命周期 7 含 `PUT .../workspace`，会话动作 7）+ `can_write` 下发 + **MQTT 全部用户操作命令删除**（两批共 16 条；proto 字段一并移除并重排为连续）+ Desktop `session-control.ts` + 前端写控件禁用。grep ceiling lint 仍属 Phase 7 |
| 4 | admin 角色 + `as_user` + bootstrap_admin + 首位 admin 创建 | 0.5 周 | ✅ `as_user` 校验 + 只读守卫 + 数据面；bootstrap_admin / 首位 admin；账号 CRUD（`account_api.rs`）+ `/api/auth/first-login` + `registration_open` 接线均已完成（**本轮再补 Desktop 入口**：`/api/status` 暴露 `registration_open`，非 admin 在开关打开时也看到 "+"，见 §决策 6 / §9 问题 9） |
| 5 | Sidebar User 折叠分组（`partitionAccounts` + UserList.tsx） | 0.5 周 | ✅ 已落地：`partitionAccounts` + `UserList.tsx` + `AgentList` 接入 + admin `?as_user=` 过滤（`viewAsUserId`）+ admin 建号（`CreateAccountModal`）/ 邀请 token（`InviteTokenModal`）/ 禁用 / 重置密码 UI |
| 6 | 用户-用户聊天（persistence + API + Desktop UI + 附件上传） | 2 周 | ✅ **完成**：`src/chat.rs`（`users/{min}/chats/{max}/conversation.json` + `messages.jsonl`，原子写、尾部翻页、单条坏行跳过、按 user_id 计未读）+ `src/http/chat_api.rs`（4 条路由；读 = self-or-admin，写 = self-only 且 `from` 强制取 token；`peer_display_name` 服务端解析）+ **附件上传/下载**（`files/{id}` + sidecar + `Content-Disposition`/`nosniff` + 每路由 body limit 抬升；见 §决策 9）+ `MessagesView` / `userChatStore`（引用计数轮询 + 待发附件队列）/ `user-chat-api` / nav 未读红点 / `requestNavView` + **发起会话闭环**（`account_api` 的 `GET /api/users/directory` + `MessagesView` "新会话"选择器） |
| 7 | ceiling lint + 集成测试 + 文档（README / 用户手册） | 1 周 | ✅ 单元测试 + router 集成测试已就位；**进程级 e2e**（`tests/auth_mode_e2e.rs`，4 用例）落地见 §7.5；**ceiling lint** 落地第三条 `run_gateway_chat_path_redline`（§7.5 回归防护）；**用户手册**：[`docs/runbooks/multi-user-accounts.md`](../../runbooks/multi-user-accounts.md)（配置 / 建号 / Desktop 用法 / 落盘备份 / 状态码 / 排查）；**协议文档**：`http.md` 新增 §4.14 认证与账号 + §4.15 用户间聊天，`mqtt.md` 命令树与 ADR-034 §11.2.B 早前已更新为「MQTT 控制面清空 + 全部迁 HTTP + 字段号重排」，`node_proto_golden.rs` golden 已重算。**残余**：session 过滤的跨进程 e2e（需 Gateway + Node + Runtime 三进程 harness，见 §7.5） |

总计 ~7.5 周。建议分两个 PR 合并：**PR1 = Phase 1-4**（账号 + 隔离 + admin），**PR2 = Phase 5-6**（UI + 聊天），**PR3 = Phase 7**（lint + 文档）。

---

## 9. 开放问题（评审请重点看）

> **状态更新**：1-7 已按主流技术路线决议（行业标准 + "做就做最好的"原则），作为设计输入纳入决策；问题 8 已在 §决策 11 决议。
>
> **模式分流补充（§决策 12）**：本地（bind `127.0.0.1`）部署下，**§1-§11 整套决策退化为 no-op**——问题 1（首位 admin = 物理 OS 用户）、问题 2（注销策略 = OS 账户注销）、问题 3-7（隔离/聊天/原子性/并发 = 单用户场景下不触发）按 `AUTH_MODE=local` 分流消解，不需单独决议。multi_user 模式（bind `0.0.0.0`）下问题 1-7 才进入实施路径。

1. **✅ 已决议 — 首位 admin 创建流程**：选 **`bootstrap_admin` 配置驱动**（无人值守优先）。理由：Kubernetes / Consul / etcd / Docker 等集群系统均采配置文件 / 环境变量方式；"Gateway 是 keep-alive 进程不应有 stdin" 是既有架构原则（`AGENTS.md` 已明确）。交互式场景由**独立 CLI 子命令** `acowork-gateway admin create` 提供（独立进程，不破坏 keep-alive 边界），对应 [apps/cli/](apps/cli/) 增量。配置缺失时：~~`accounts.json` 为空 → 拒启动（fail-fast，非告警）~~ （**v2 修订**：空库改为 seed 无密码 admin + 受限模式，见 §决策 12 v2 / v3；原为"拒启动"）；非空 → 忽略（见决策 5 实施修订）。
2. **✅ 已决议 — 注销策略**：本期**只软删除**（`disabled_at`，与决策 6 一致），不提供硬删除。理由：Slack / Discord / Teams 均默认软删除保留历史；硬删除仅在 GDPR 等法律强制场景需要（会破坏 session / 聊天的引用完整性），本期 YAGNI；后续若需硬删除再立独立 ADR（带宽限期 + 异步清理任务 + 引用重映射策略）。
3. **✅ 已决议 — 群聊（group chat）**：`conversation.json` 的 `participants` 字段类型保留 `Vec<String>`，本期运行时断言 `len() == 2`；未来 group chat 通过 "len() > 2 + group metadata（name / avatar / owner）" 扩展，**零迁移**（schema 已兼容）。理由：Slack / Discord / 微信 / Telegram 均采用 DM / Group 统一 schema；本期前端仅暴露 2 人路径，schema 留口。
4. **✅ 已决议 — 聊天附件大小限制**：**图片 25 MB / 文档 100 MB / 不引入 virus scan**。ponytail 标记：个人 / 小团队场景下 trade-off 已知；超过此规模需引入 ClamAV（独立进程）+ 对象存储分拆（独立 ADR）。理由：Discord 25 MB（图片 / 视频）、Telegram 100 MB（任意文件）是公认的 sweet spot；virus scan 在用户量 < 100 时 ROI 为负，是 over-engineering。
5. **✅ 已决议 — `as_user` 是否需要写操作**：本期**只读视图**（与决策 4 一致），写操作保持 token 实际身份。理由：避免 XSS / CSRF 攻击链（决策 4 已分析：身份冒用 = 横向越权入口）。若运营场景需要"admin 代用户发消息给 agent"，按 **OAuth 2.0 Token Exchange (RFC 8693)** 模式扩展——新增 `X-On-Behalf-Of` header + `act` claim + 单独 ACL 策略，**独立 ADR** 设计，不污染本期 schema。
6. **✅ 已决议 — Desktop 账号切换的原子性**：保留 ADR 原建议——**保留失败时的"已登出但未登入"中间态**，UI 引导用户重新登录（LoginView 直接渲染）。理由：VS Code / Google 账号切换均采用此模式；事务性账号切换是过度设计，失败回滚反而引入新的不一致风险（旧 token 已撤销 / 新 token 没拿到 / MQTT 状态半连接——三者纠缠）。
7. **✅ 已决议 — 多设备并发**：本期**允许多设备并发**（每设备独立 `token_family`），不引入 `device_id` 与并发上限。理由：Slack / Discord / Google / Microsoft 均默认支持多设备并发；OAuth 2.0 RFC 6749 本身不限制。后续若需要按 `device_id` + `max_sessions_per_user` + "新登录踢旧登录"策略扩展，立独立 ADR。
8. **~~PM `"human"` 特例的演化~~ ✅ 已决议（选 B）**：人类操作者成员化——`ProjectMember` 新增 `kind`（`Agent`/`User`），人类与 agent 成员对称，`assignee ∈ ∅ ∪ members` 无特例。完整设计见 **§决策 11**。~~选项 A（放宽为"任意已登录用户可被指派"）~~ 已拒绝：语义模糊（A 指派的活 B 可认领），且与 ADR-073 三层身份范式不对齐。
9. **✅ 已决议 — 非 admin 建号的 Desktop 入口（本轮结清）**：`registration_open = true` 的语义定为"**任何已登录账号都可以邀请新账号**"，Desktop 侧通过 `/api/status` 新增的 `registration_open` 字段决定是否在 `Users (N)` 分组渲染 "+"。**匿名注册（`allow_public_signup`）明确不做**——它需要把 `/api/users` 移出认证中间件，属无认证攻击面的净扩张，而本 ADR 的目标部署没有对应场景。详见 §决策 6。
10. **✅ 已决议 — 上传配额不做 per-user**（本轮结清）：磁盘是共享资源，per-user 上限保护不了它；真要做就在 `data_dir` 上设一个**全局水位**，触发条件是出现真实磁盘压力或引入互不信任的多租户。详见 §5.5。

---


---

## 10. 遗留清单（仍未做全集）

> **这一节是索引，不是第二份描述**：每项只写「类型 + 触发条件 + 详见」，细节留在被指向的那一节——两处各写一份，迟早会漂移成两种说法。
>
> 边界（本节覆盖截至本 ADR "本次收尾"时的状态）：§5.5 中标了 **已结清** / **已实现** 的条目**不在**此列（那些是变更记录，不是债务）。本节的目的是让下一个人（或下一个我）不必重读 1000 行就能知道"还有哪些洞、什么时候才需要去堵"。
>
> **判据**：某一行只有在"触发条件成立"时才值得动手。在此之前动它，就是给一个没有消费者的场景写代码。

### 10.1 有意留白（有明确升级路径，不是遗漏）

| # | 项 | 触发条件（什么时候才真要做） | 详见 |
|---|---|---|---|
| 1 | **MQTT 事件面无 per-user 订阅 ACL**——能连上 broker 的客户端理论上可 SUBSCRIBE 任意 `agents/{id}/sessions/{sid}/messages/#` | 部署从"本机回路"变成"跨机 / 多租户"；或在 `rumqttd` 上找到 ACL 能力 | §5.5 + [`mqtt.md §10`](../../protocols/zh/mqtt.md)（已标注"暂缓 / 已知缺口"） |
| 2 | **access token 校验无状态**——账号被禁用后旧 token 仍可用，窗口 = `ACCESS_TTL_SECS`（15 分钟） | 用户量 > 100，或需要"立刻踢下线" | §5.5（升级路径：内存 `user_id → revoked_at` 集合） |
| 3 | **✅ 已结清（原：refresh 无 grace window）**——响应丢包后的重试被判"复用"连坐全部 family；且 `{user_id}.*` 无界 + `login()` 不清登记簿 → **循环锁死**（登录 → 约 13 分钟必 401 → 弹登录 → 再登录）。触发条件已成立，但成立的方式与原预测不同：不是多客户端，是同一 webview 内相隔 3 ms 的两个 refresh（碎片化的 token 管理自撞） | — | §决策 3 实施记录（宽限窗口 + `iat <= cutoff` 时间基准撤销）+ §5.5 |
| 4 | **`revoked_families.txt` 是扁平文件、无 GC** | 历史上万次 refresh | §5.5（升级路径：SQLite 表 + 过期列） |
| 5 | **聊天列表是 fs 全扫（O(pairs)）** | 会话对数接近 1000 | §6.4（`chat.rs` 已知 ceiling）+ 代码内 `ponytail:` |
| 6 | **翻页要读整个 `messages.jsonl`** | 单会话消息上万 | §6.4（同上） |
| 7 | **附件 blob 无回收**——崩溃会留无人引用的孤儿文件 | 出现真实磁盘压力 | §5.5（升级路径：扫"无 sidecar 且早于 N 天"） |
| 8 | **下载整文件读进内存**（`Vec<u8>`） | 需要 > 100 MiB 附件 | §5.5（升级路径：`ReaderStream`，代价是多一个依赖） |
| 9 | **上传路由的 body 上限是"100 MB + 1 MiB 信封"的估算**——贴着上限的文件可能被外层拒掉（413 而非业务错误） | 用户开始上传 99–100 MB 的文件 | §5.5（④） |
| 10 | **上传配额未做**（框架已从 per-user 改为**全局 `data_dir` 水位**） | 出现真实磁盘压力，或引入互不信任的多租户 | §5.5 + §9 问题 10 |
| 11 | **`user_profiles.json` 仍是全局派生视图**（`is_active` 取"最近登录 / 第一个 admin"），只喂遗留的 `last_user_profile` 主题 | Runtime 要做 per-owner 的 profile 推送 | §决策 2「派生视图的残余 ceiling」 |
| 12 | **用户名录对任何认证账号全量可枚举**（`username` 全集） | 出现"同一实例上有陌生人"的场景 | §5.5（升级路径：精确 username 查询 / 只回已有对手方 / 邀请制通讯录） |
| 13 | **per-agent 默认可见性**（admin 能否强制某 agent 的会话对所有账号可见） | 真出现"这个 agent 的对话是团队共享日志"的需求 | §决策 4 + §5.5 |

### 10.2 未实现（明确留给后续工作 / 独立 ADR）

| # | 项 | 触发条件 | 详见 |
|---|---|---|---|
| 14 | **登录端点没有速率限制 / 没有账号 lockout**——在线暴力破解只能靠密码强度挡（部署侧缓解：只 bind `127.0.0.1`） | 暴露到非信任网络之前**必须**补 | §7.4（已列为已知缺口）+ §5.5 |
| 15 | **多设备并发上限 / `device_id`**——本期按"每设备独立 family"允许多端并发 | 需要"新登录踢旧登录" | §9 问题 7 |
| 16 | **密码过期强制改密**（只记录 `password_expires_at`，不强制） | 有合规要求 | §5.5 |
| 17 | **Vault 加密扩展字段**（`vault/accounts/{user_id}.enc`：`api_secrets` / `recovery_codes`） | 出现"每个账号自己的 API key"需求 | §决策 2 + §8 Phase 1 行 |
| 18 | **`acowork-gateway admin create` CLI 子命令**（交互式首位 admin；无人值守场景已有 `bootstrap_admin`） | 有人要在终端里交互建号 | §9 问题 1 + [apps/cli/](apps/cli/) |
| 19 | **admin 代用户写**（OAuth 2.0 Token Exchange / RFC 8693 的 `X-On-Behalf-Of`） | 出现"运营替用户发消息"的真实流程 | §9 问题 5 |
| 20 | **群聊**（`participants` 已预留 `Vec<String>`，运行时断言 `len() == 2`） | 需要 3 人以上会话（schema 零迁移） | §9 问题 3 + §决策 8 |
| 21 | **病毒扫描**（附件不做 ClamAV） | 附件来源不可信 + 规模上去了 | §9 问题 4 |

### 10.3 测试缺口

| # | 项 | 现状 | 详见 |
|---|---|---|---|
| 22 | **session 过滤的跨进程 e2e**（Gateway + Node + Runtime 三进程） | 判据逻辑已在单测里按 `(归属 × visibility × scope)` 矩阵穷举；端到端只有 Gateway 单进程的部署面 e2e | §7.2 + §7.5 |
| 23 | **账号 CRUD / 改密的进程级 e2e** | 单测已覆盖（invite 生命周期 / 非 admin 自我封闭 / 冲突拒绝 / `registration_open` 门控 / 展示字段持久化） | §7.2 |

### 10.4 已否决（评审时不要再重新讨论，除非触发条件变了）

| 方案 | 否决理由 | 详见 |
|---|---|---|
| **匿名注册**（`allow_public_signup`） | 需要把 `/api/users` 移出认证中间件 = 无认证攻击面的净扩张；目标部署没有对应场景 | §决策 6 |
| **per-user 上传配额** | 磁盘是共享资源，按账号限额保护不了它（100 MiB × 50 人也只是 5 GiB） | §5.5 + §9 问题 10 |
| **硬删除账号** | 破坏 session / 聊天的引用完整性；主流实现都是软删除 | §9 问题 2 |
| **把 `user_profiles.json` 升级成账号 schema** | 会把凭据混进"非敏感展示元数据"语义（ADR-059 §7.3） | §决策 1/2 |
| **第二条真相源索引文件**（会话索引落盘） | 缓存索引只放进程内存，权威永远是 meta 文件 | §决策 2 + §5.5 |
| **`as_user` 写操作** | 身份冒用 = 横向越权入口 | §9 问题 5 |
| **给公开会话开 `open` 读授权**（观众激活） | `Active` 是 per-session 全局状态，会造出"无责任人"的常驻会话；改成"观众不激活" | §5.5 |
