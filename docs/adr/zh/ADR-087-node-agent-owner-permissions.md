# ADR-087: Node 与 Agent 的 Owner 权限模型 — 堵住"整台机器对全体用户默认可写"的洞

**状态**：草案（v2 修订稿；Q1/Q2/Q4 与"单 owner + 多 guest"模型已决策，剩余见 §12）
**日期**：2026-10-28（v2 修订：新增 D9 归属基数与 guest 模型——评审两问定调：不许多 owner，guest 是 use 档使用授权（非 manage）；Q1/Q2/Q4 落决策。v3 修订：权限轴从两档改为**三档 manage/use/view**——manage 收窄为"高权限/破坏性/agent 定义修改"，use 覆盖其余全部写，view 放开**元数据/定义类只读**；工作区文件内容、git 历史、memory、全局 search 等"内容读"仍归 use 不归 view（防 §7 方案 G 窃取）；start 归 use、stop 归 manage；permissions 名单公开归 view）
**决策者**：（待定）

**前置**：
- [ADR-076](./ADR-076-multi-user-account-system.md)（多用户账号系统 — 本 ADR 把 §决策 4 的 session 维度隔离范式（`user_id` + 服务端单点判定 + `can_write` 下发）推广到 node 与 agent 两个维度；业务语义仍有效，实现形态见 ADR-084）
- [ADR-084](./ADR-084-user-standalone-process.md)（用户域独立进程 — 账号权威在 `acowork-user`；本 ADR 的 owner 是 **Gateway 侧的授权策略数据**，不是账号数据，两者不冲突）
- [ADR-075](./ADR-075-node-identity-uuid-and-node-name.md)（node_id = UUID v4 稳定路由键 — owner 以 `node_id` 为键，改名/迁移不受影响）
- [ADR-073](./ADR-073-agent-instance-identity-decomposition.md)（instance_id = UUID v4 — agent owner 以 `instance_id` 为键，package 多实例各自有主）
- [ADR-055](./ADR-055-remote-runtime-node-topology.md)（§6.2 enrollment / §6.5 node 上报 inventory 是存在性权威 — 本 ADR 只加"归属权威"，不改存在性权威）
- [ADR-009](./ADR-009-gateway-workspace-isolation.md)（Gateway 红线：不直接碰 Agent 私有文件 — 本 ADR 的鉴权全部发生在**反代入口**，不引入任何 Gateway 侧文件系统访问）
- [ADR-077](./ADR-077-system-agent-demotion-to-default-agent.md)（default agent 是普通 agent — 同样受 owner 模型约束，见 §12 Q1）

---

## 1. 决策摘要

### 1.1 一句话

**给 Node 和 Agent instance 各增加一个 Gateway 侧持久化的 `owner_user_id`（单 owner）+ `guests` 使用授权名单，三档门控：**manage = owner ∨ admin**（窄，仅高权限/破坏性——安装/卸载/克隆/停止/升级/调试、权限设置、agent 定义类配置写、memory 破坏写、删 session）；**use = owner ∨ guest ∨ admin**（宽，除 manage 外一切写操作——启动、聊天、session 增改、文件/git 操作、memory 检索/全局 search）；**view = use ∨ published（shared/public）**（只读信息尽量提供，session 内容再叠加 private/public 第二道墙）。agent 级 `visibility`（private/shared）只决定**可见性**（shared 让全体登录用户"看得到 + 只读"，但不放开"用"），session 维度沿用 ADR-076 不变。鉴权在 Gateway 反代入口单点执行（那里已有 `AuthContext` 与 `instance_id→owner` 映射），fail-closed：未登记路由默认落 manage，无主资源只有 admin 可管理。**

### 1.2 关键决策表（详细理由见 §5）

| # | 决策 | 结论 |
|---|---|---|
| D1 | owner 存哪 | **Gateway 侧新增两个持久化归属表**（`node_owners.json`、`agent_owners.json`，与 `node_tokens.json` 同目录同范式）。**不进 MQTT proto**：Node 上报的 inventory 仍是"存在性权威"（ADR-055 §6.5），归属是 Gateway 的策略数据，Node 不感知用户 |
| D2 | node owner 从哪来 | **enrollment token 绑定创建者**：multi_user 模式下新增 `POST /api/nodes/enrollment-tokens`（需登录），token 记录带 `owner_user_id`，enroll 成功即写 `node_owners.json`。CLI 签发的 token 无主 → 该 node 无主（admin-only）。`gateway_managed` 本机节点默认无主 |
| D3 | agent owner 从哪来 | **谁装谁所有**：`POST /api/agents/install` / `ensure` / `clone` 成功派发时，以 `instance_id` 为键写入 `agent_owners.json`，owner = 调用者 `AuthContext.user_id`。admin 天然可装任意 node（Q2 已决策，见 §12），装出的 agent owner = admin；guest 在共享 node 上装的 agent owner = guest 本人（D9 规则 3） |
| D4 | 三档门控 | **manage = owner ∨ admin**（窄）：Node 门控——install/uninstall 到该 node、fs browse、rename、enroll 归属；Agent 门控——stop/upgrade/clone/debug、agent 定义类配置写（config/prompts/skills/model/workspace 增删改/avatar/manifest）、memory 破坏写、删 session、权限设置。**use = owner ∨ guest ∨ admin**（宽）：除 manage 外一切写——start、聊天、session 增改、文件/git 读写、memory 检索、全局 search、interactions；**内容类读**（工作区文件/git 历史/memory/search）也归 use。**view = use ∨ published**：agent 定义/元数据只读（列表/详情/config 定义读/status/头像/permissions 名单）。详见 D4 门控表 |
| D5 | 执行点 | **Gateway 反代入口**（`http/proxy.rs` 路由策略表 + `agents.rs`/`nodes_api.rs`/`fs_browse.rs` handler 头部）。Runtime 不引入 owner 概念（它拿不到也不该拿归属真相）；`x-user-id` scope 机制原样保留，只用于 session 过滤 |
| D6 | agent 可见性与使用 | agent 新增 `visibility`：`private`（仅 use 名单 = owner ∨ guest ∨ admin 可见、可用）与 `shared`（**所有登录用户可见**，但**仍只有 use 名单能用**——开 session 聊天需 owner 授权为 guest，session 之间按 ADR-076 隔离）。**visibility 只决定"看得到"，不决定"能不能用"**；"用"永远走 use 名单。默认值：用户手动 install/ensure/clone 出的实例落 `private`；**onboarding 预装的 default agent 落 `shared`**（Q1 已决策，见 §12）——注意这只让它对全员**可见**，能否聊天仍取决于是否被加进 guest 名单 |
| D7 | 无主与迁移 | multi_user 模式下**fail-closed**：`owner=None` 的资源只有 admin 可 manage；admin 可通过 `PATCH .../owner` claim/转移。Local 模式整体 no-op（与 ADR-076 §决策 12 同一开关 `AuthMode`） |
| D8 | 服务端单一真相 | `GET /api/agents` / `GET /api/nodes` 每条记录服务端算好 `can_manage` / `can_use` 布尔下发，Desktop/Mobile **只消费布尔渲染，不再自行推导**（延续 ADR-086 不变量 1 的纪律） |
| D9 | 归属基数与协作 | **单 owner + 多 guest**。owner 唯一（责任唯一、转移两元）；guest 是"**use 授权名单**"（被授权使用/聊天，不可配置）而非归属——不改变所有权、不可加/删 guest、不可转移、不可改 visibility、**不可 manage**（配置/workspace/文件/生命周期仍仅 owner ∨ admin）；撤销即失权且不回收 guest 以个人身份装的 agent 的归属。admin 与 owner 共同维护名单（详见 §5 D9） |

---

## 2. 背景：现状与威胁模型

### 2.1 现状盘点（代码事实）

ADR-076 把"用户"提升为一等身份维度后，**session 维度**已有完整隔离：

| 维度 | user 绑定 | 鉴权执行点 | 状态 |
|---|---|---|---|
| Session（对话） | `SessionMeta.user_id` + `visibility` | Runtime `is_readable_by`/`is_writable_by`（[core/acowork-memory/src/session_meta.rs:258](../../../core/acowork-memory/src/session_meta.rs#L258)），scope 来自 Gateway 注入的 `x-user-id` | ✅ 已隔离 |
| **Node** | ❌ 无。`NodeInfo`（[mqtt_payload.proto:1188](../../../core/acowork-core/proto/mqtt_payload.proto#L1188)）、`NodeInfoState`（[node_registry.rs:26](../../../core/acowork-gateway/src/mqtt/node_registry.rs#L26)）、`NodeTokenRecord`（[enrollment.rs:197](../../../core/acowork-gateway/src/mqtt/enrollment.rs#L197)）均无用户字段 | 无 | ❌ |
| **Agent instance** | ❌ 无。`AgentInfo`（[state.rs:28](../../../core/acowork-gateway/src/gateway/state.rs#L28)）、`InstalledAgentInfo`（proto:1228）均无用户字段 | 无 | ❌ |
| **Workspace / 文件** | ❌ 无。`proxy_add_workspace`（[proxy.rs:1781](../../../core/acowork-gateway/src/http/proxy.rs#L1781)）纯转发；Runtime `create_workspace`（[workspace_mutation_impl.rs:301](../../../core/acowork-runtime/src/usecases/workspace_mutation_impl.rs#L301)）只校验字段格式，不校验调用者 | 无 | ❌ |
| **fs browse** | ❌ 无。`GET /api/fs/browse?target={node_id}`（[fs_browse.rs:214](../../../core/acowork-gateway/src/http/fs_browse.rs#L214)）反代到任意 node 的 `/fs/browse` | 无 | ❌ |

### 2.2 攻击/误用场景（multi_user 模式，任意低权限账号持有效登录 token）

1. **整盘读写**：`POST /api/agents/{任意在线 agent}/workspaces` 传 `{"path":"C:\\Users\\<受害者>\\Documents","access":"read-write"}` → 该 node 机器任意路径被挂成工作区 → `PUT /workspaces/file` 覆写文件、`GET /workspaces/file` 窃取内容。
2. **整盘浏览**：`GET /api/fs/browse?target={node_id}&path=C:\&show_hidden=true` 逐目录枚举 node 机器文件系统（也是 D5 里挑路径的侦察手段）。
3. **资源占用与供应链位**：`POST /api/agents/install` 指定任意 `node_id`，把恶意/挖矿 agent 装到别人的机器上；`POST /api/agents/{id}/start` 起进程。
4. **破坏他人**：对别人的 agent `stop` / `uninstall` / `PUT config`（改模型、改 prompt）/ `DELETE workspace`。
5. **越权窥探**：`?target=` 反代不校验调用者与 node 的关系；`GET /api/nodes` 泄露全集群 hostname/OS/arch。

场景 1 是**默认即破**：不需要任何配置错误，装好 Gateway 接进第二个用户就成立。这正是本 ADR 的动机。

### 2.3 为什么 session 隔离挡不住

session 的 `is_writable_by` 只保护**对话数据**。workspace 配置与文件 API 在 Runtime 侧完全没有 scope 概念（`USER_SCOPE_HEADER` 仅被 `http/session_control.rs` 消费）。攻击者不需要碰别人的 session——在自己的 session 里让 agent 用被注入的工作区干活，或直接调 workspace API，效果相同。

---

## 3. 目标与非目标

**目标**
1. multi_user 模式下，Node 所在机器的文件与进程资源，默认只对 Node owner（与 admin）开放。
2. Agent instance 的配置面（config/prompts/skills/workspace/文件/生命周期）默认只对 agent owner（与 admin）开放。
3. 保留合法的共享诉求：agent 可显式声明 `shared` 让其他用户"看得到"它；"用"它（聊天）则由 owner 通过 guest 名单逐个授权，两者都不改变"拥有"。
4. Local 模式（单人自用）零行为变化；升级 multi_user 后迁移路径明确、fail-closed。
5. 权限判定单一执行点、单一真相（服务端下发布尔，客户端不推导）。

**非目标**
1. **不做细粒度 RBAC**（per-workspace ACL、per-tool 授权、团队角色矩阵）。三档（manage/use/view）+ 两角色（owner/admin）覆盖当前全部已知场景；出现第三个稳定需求再扩展（Rule of three）。
2. **不改 MQTT 数据面 ACL**。broker `can_subscribe` 仍是 Phase-1 permissive（[acl.rs:178](../../../core/acowork-gateway/src/mqtt/acl.rs#L178)），跨账号 retained 事件扇出问题（已知，e2e flake 根因）由后续 ACL ADR 处理。本 ADR 只收紧 **HTTP 控制面**。
3. **不防 Node 本身作恶**。Node 是用户自己的机器、跑用户自己的进程，OS 层面它本来就看得见自己盘上的东西；本 ADR 防的是**其他登录用户**经由 Gateway 操作这台机器。
4. **不引入审批工作流**（申请-批准-授权时限等）。转移/共享都是 owner 主动 PATCH 一步完成。

---

## 4. 术语与角色

| 术语 | 定义 |
|---|---|
| **manage** | 会改变 agent 定义、机器状态或具破坏性的高权限操作：生命周期（安装/卸载/克隆/停止/升级/debug/dev-mode）、agent 定义类配置写（config/prompts/skills/model/workspace 增删改/avatar/manifest）、memory 破坏写、删 session、fs browse、enroll 归属、权限设置 |
| **use** | 驱动 agent 干活的其余操作（除 manage 外一切写）：创建/打开**自己的** session 并聊天、start、工作区文件与 git 读写（含 revert）、memory 检索、全局 search、interactions。**内容类读**（工作区文件内容、git 历史、memory、search）也归 use 不归 view——它们读 owner 真实数据、无二级墙兜底（见 §7 方案 G） |
| **owner** | 资源归属的**唯一**用户（`user_id`，UUID，来自 `acowork-user` 账号体系）。责任锚点：增删 guest、改 visibility 属于 owner ∨ admin；owner 字段本身的转移仅 admin（D7） |
| **guest** | 被 owner 或 admin 授予该资源 **use** 档的用户（名单在归属表，即"使用/聊天授权名单"）。guest 是授权不是归属：可开自己的 session 聊天、start、读写工作区文件、检索 memory/search，但**不可 manage**（agent 定义类配置写 / workspace 增删改 / 生命周期 / 删 session / 权限设置仅 owner ∨ admin）、无归属操作力，撤销即失权，不级联（见 D9） |
| **admin** | `Role::Admin`，可 manage 一切资源、可 claim/转移无主资源、可维护任意资源 guest 名单；沿用 ADR-076 "能看不能冒充"纪律（写操作以 admin 自己身份执行，归属不因此改变） |
| **无主（ownerless）** | `owner=None`。multi_user 下 = admin-only（fail-closed） |

判定函数（Gateway 内唯一实现）：

```text
can_manage(user, resource) := user.is_admin
                            ∨ (resource.owner = Some(user.id))
                            // guest 不在 manage 档：guest 是"使用授权"，不是"共同维护"（D9）
can_transfer(user, resource) := user.is_admin ∨ (resource.owner = Some(user.id))
                            // 归属操作（增删 guest / 改 visibility）不含 guest；
                            // 其中 owner 字段本身的变更仅 admin（D7"能看不能冒充"延伸）
can_use(user, resource)    := can_manage(user, resource)
                            ∨ resource.guests.contains(user.id)
                            // use 档 = 授权名单（guest）∨ owner ∨ admin；visibility 不放开 use
can_view(user, resource)   := can_use(user, resource)
                            ∨ resource.visibility.is_published()
                            // view 档 = use 名单 ∨ 发布态（shared agent / public node）
```

---

## 5. 决策详解

### D1：owner 是 Gateway 侧策略数据，不进 proto / 不进 Node

**方案**：Gateway `{data_dir}` 下新增两个归属表，与 `node_tokens.json` / `enrollment_tokens.json` 同目录、同"原子写 JSON + 内存镜像"范式（[enrollment.rs](../../../core/acowork-gateway/src/mqtt/enrollment.rs)）：

```text
node_owners.json    # node_id (UUID v4, ADR-075) → { owner_user_id: Option<String>, guests: [user_id], visibility, created_at, claimed_by_enroll_token }
agent_owners.json   # instance_id (UUID v4, ADR-073) → { owner_user_id: Option<String>, guests: [user_id], visibility: Private|Shared, created_at }
```

**为什么不塞进 `NodeInfo` / `InstalledAgentInfo` proto 让 Node 上报**：
- Node 不感知用户账号（账号权威在 `acowork-user`，Node 只有 node token）。让 Node 携带/回显 owner 等于把策略真相交给数据面，Gateway 重启后靠 retained inventory 重建归属——**Node 侧被篡改的 inventory 就能改所有权**，鉴权根基不能建立在可被更低信任级组件覆写的通道上。
- 存在性权威与归属权威分离：inventory 说"这个 instance 在这台机器上"（Node 权威），owner 说"谁能操作它"（Gateway 权威）。卸载后 owner 表条目延迟清理（见 D7 清理策略）。

**代价**：Gateway 多两个本地状态文件；node 迁移/重装不丢归属（键是稳定 UUID，这正是 ADR-073/075 提前打好的地基）。

### D2：node owner 来自 enrollment token 的创建者

现状：enrollment token 只能由 CLI `acowork-gateway nodes token create` 签发（[cli.rs:496](../../../core/acowork-gateway/src/cli.rs#L496)），签发时无用户上下文，`EnrollmentTokenRecord` 只有 `consumed_by: Option<node_id>`。

**决策**：
1. `EnrollmentTokenRecord` 增加 `owner_user_id: Option<String>`。
2. multi_user 模式新增 `POST /api/nodes/enrollment-tokens {ttl}`（需登录）：签发的 token `owner_user_id = 调用者`。Desktop「添加设备」向导改走这个端点拿 token + 复制 `acowork-node start --token ...` 命令。
3. enroll 成功路径（`decide_enroll` → Accept，[dispatch.rs:1114](../../../core/acowork-gateway/src/mqtt/dispatch.rs#L1114)）把 token 的 `owner_user_id` 写入 `node_owners.json`。
4. CLI 签发的 token（无主）→ node 无主 → admin-only。`gateway_managed` 本机节点（Gateway 自 spawn）默认无主：这台 Gateway 机器属于部署者（admin）。
5. 同一 node 重复 enroll（重连/重装身份未丢）不改变 owner；identity.json 丢失后**用新 node_id 重新 enroll** 视为新设备（token 是谁的就是谁的）。

**为什么不"首个使用者即 owner"**：enroll 动作本身就是"把机器接入集群"的声明，token 创建者是最诚实的归属信号，无需事后认领。

### D3：agent owner = 安装者

`install_agent`（[agents.rs:1032](../../../core/acowork-gateway/src/http/agents.rs#L1032)）已经由 Gateway 生成 `instance_id`（install 时刻、调用者已鉴权），是写归属的天然锚点：

- `POST /api/agents/install`、`POST /api/agents/ensure`（声明式，首次创建实例时）、`POST /api/agents/{id}/clone`：派发成功后**暂存**待提交 owner（内存 pending 表，键为 Gateway 铸造的 `instance_id`）；Node 回报 `ok` 或 retained inventory 首次出现该实例时提交写入 `agent_owners.json[instance_id] = { owner: AuthContext.user_id, visibility: Private }`，回报 `error` 时丢弃——安装失败不留孤儿行（评审 M4）。clone 是同步确认路径，直接写入。
- `ensure` 命中已存在实例时**不改** owner（幂等语义只约束存在性，不约束归属）。
- Local 模式无 `AuthContext` → 写入 `owner=None`，反正 Local 不校验（D8）。
- **visibility 初始值**：install/ensure/clone 写表时默认 `Private`；唯一例外是 Desktop onboarding 预装的 default agent（ADR-077 bundled 包）落 `Shared`——团队部署开箱即用地让 default agent 对全员**可见**（注意：`shared` 只放开可见，能否聊天仍需 owner 逐个加进 guest 名单，见 D6）。default agent 的 owner 仍 = 执行 onboarding 的首个用户，之后 owner 可改回 private。

### D4：两级门控 + 权限矩阵

**Node 门控**（对机器的所有权，node owner ∨ admin）与 **Agent 门控**（对实例的所有权）分层。三档定义：**manage = owner ∨ admin**（guest 不在内）；**use = owner ∨ guests ∨ admin**（guest 名单即使用授权）；**view = use ∨ published（shared/public）**。门控档按"最小必要"分配：**只有会改动 agent 定义、机器状态，或具破坏性的操作才进 manage；驱动 agent 干活的写进 use；只读信息尽量进 view**。下表：

| 操作（Gateway HTTP 路由） | 门控档 | 需要的权限 |
|---|---|---|
| `POST /api/nodes/enrollment-tokens` | 开放 | 登录即可（自己 enroll 自己的机器） |
| `GET /api/nodes` | visibility 过滤 | `private`：use 名单可见；`public`：所有登录用户可见 id/name/online/`can_manage`，敏感字段仅 manage 名单（D6 B 方案） |
| `PATCH /api/nodes/{id}`（rename）、`PATCH .../{id}/visibility`、`/guests`、`/owner` | Node-manage / Node-归属 | manage 名单（owner ∨ admin）；owner 转移为 admin-only，guest 无归属权（D9 R1） |
| `GET /api/nodes/{id}/permissions` | Node-view | **归属名单公开**：能看到这个 node 的人即可读名单（知道找谁申请权限，不算隐私） |
| `POST /api/agents/install`、`ensure`、`clone`（选目标 node） | Node-manage | 目标 node 的 manage 名单（装到别人机器先要有机器权限；guest 仅 use，**不能**安装） |
| `DELETE /api/agents/{id}`（uninstall） | Node-manage ∧ Agent-manage | 两个名单都过（卸载同时动机器与实例） |
| `POST /api/agents/{id}/stop` / `restart` / `upgrade`、`debug/*` | Agent-manage | agent manage 名单（改动/替换运行态、调试=高权限） |
| `POST /api/agents/{id}/start` | Agent-use | use 名单——唤醒一个已停 agent 是"使用"而非"管理"，非破坏性 |
| agent **定义类配置写**：`PUT config`/`prompts`/`skills`/`model`/`providers`/`mcp-*`/`builtin-tools`/`tools`/`shell-risk-rules`/`avatar-config`、`workspaces*` 增删改、manifest 上传 | Agent-manage | agent manage 名单（重写 agent 是什么，属高权限配置修改） |
| agent **定义/元数据读**：`GET config`/`prompts`/`skills`/`model`/`providers`/`mcp-*`/`tools`/`avatar`/`avatar-file`/`manifest`/`cron`/`status`/`health` 等只读 | Agent-view | view 名单（use ∨ shared）——描述"agent 是什么"的元数据尽量提供 |
| **内容类读写**：`GET/POST/PUT/DELETE /api/agents/{id}/files*`、`git*`（含 diff/log 读、revert 写）、`workspaces` 树/文件读 | Agent-use | use 名单——读的是 owner 机器上的真实工作文件（§7 方案 G：内容读归 view = 整盘可被窃取），故读写都留 use；纯 viewer 不可读 |
| `GET /api/fs/browse?target={node_id}` | Node-manage | node manage 名单（读的是整台机器文件系统，非某 agent 工作区，敏感） |
| `PATCH /api/agents/{id}/visibility`、`/guests`、`/owner` | Agent-归属 | agent 归属名单（owner ∨ admin；guest 无归属权，D9 R1） |
| `GET /api/agents/{id}/permissions` | Agent-view | **归属名单公开**（同 node，知道找谁申请） |
| `GET /api/agents`、`GET /api/agents/{id}`（列表/详情/头像/status/health） | visibility 过滤 | private：use 名单可见；shared：所有登录用户可见（仅可见）；**无归属记录 = admin-only**（D7 fail-closed） |
| session **读**（`GET sessions`/`sessions/{sid}`/`messages`/`latest-session`/`stream`） | Agent-view ∧ ADR-076 | view 名单先过；**内容再由 session 自身 private/public 第二道墙**（Runtime `is_readable_by`）过滤，private session 对 view 不可见 |
| session **写**（create/open/close/messages/answer/approval/config/workspace-switch/stop/continue/compress） | Agent-use | use 名单——开自己的 session 聊天即 use；跨 session 安全由 Runtime `is_writable_by` 兜底 |
| `DELETE /api/agents/{id}/sessions/{sid}` | Agent-manage | manage 名单（销毁 session 及其文件，破坏性） |
| `GET /api/agents/{id}/memory/*`（nodes/graph/stats）、`GET search`、`POST rag/query` | Agent-use | use 名单——检索跑的是覆盖全语料的查询，会浮出跨对话记忆，且无 session 那种 private/public 二级墙，故**归 use 不归 view** |
| memory **破坏性写**（`POST memory/distill`、`rebuild-embeddings`、`PUT/DELETE memory/nodes*`） | Agent-manage | manage 名单（重写 agent 的知识底座） |
| `POST /api/agents/{id}/interactions` | Agent-use | use 名单——活动戳记，发消息流程内触发，guest 聊天须能写 |

**三档边界的两条原则要说透**：
1. **`shared` 只放开 view（可见 + 只读），绝不放开 use**。要开 session 用它聊天，必须被 owner 加进 guest 名单（use 档）。被授权为 guest 的人驱动 agent 用它挂载的工作区干活时，agent 以工作区权限读写文件、内容可能进入对话——这是真正的风险面，也是 use 档的授权含义。不在 use 名单的用户能看列表/详情/配置只读，但**不能聊天、不能改 agent 定义、不能检索记忆**。文档与 UI 必须把"shared=仅可见+只读、聊天与改配置需逐个授权"说清楚。
2. **读默认 view，但"检索类读"例外归 use**：memory 读、全局 search、rag/query 是对整个语料跑查询、会浮出跨对话内容，且没有 session 的 private/public 二级墙兜底，所以它们虽为 GET 仍归 use；而 config/prompts/skills/model/files/git 的读是静态元数据/内容，归 view。

### D5：执行点 = Gateway 反代入口，Runtime 不感知 owner

**方案**：
1. `auth_middleware` 之后新增一个轻量 **授权策略层**：路由模式 → `Permission::{None, NodeManage, NodeTransfer, AgentManage, AgentTransfer, AgentUse}` 的静态表（Transfer = 归属名单 owner ∨ admin，见 D4/D9），落在 [routes.rs](../../../core/acowork-gateway/src/http/routes.rs) 组装处（与 `restricted_mode_middleware` 同款 layer）。handler 从 `Extension<AuthContext>`（已有，[auth_middleware.rs:29](../../../core/acowork-gateway/src/http/auth_middleware.rs#L29)）拿身份，从 `agent_owners`/`node_owners` 表拿归属，产出 403。
2. `instance_id → node_id` 用现有 `resolve_agent_node_id`（[agents.rs:561](../../../core/acowork-gateway/src/http/agents.rs#L561)）；`{id}` 路径参数仍是"instance 或 package id"，沿用 `resolve_agent_identity` 解析后再查归属。
3. 拒绝语义：`403 {"error":"forbidden","code":"not_authorized","resource":"agent|node","required":"manage|use|transfer"}`（结构化错误码，沿用 Phase 3.2 风格；码名不叫 `not_owner`，因为 guest 被拒时并非"无主"而是"名单外"）。列表类是**过滤**而非 403。
4. **Runtime 不加第二判定**：Runtime 没有归属真相（D1），加判定就要把 owner 表同步下发——复制真相、双倍漂移。Runtime 继续只消费 `x-user-id` 做 session 过滤。Gateway 是 multi_user 下唯一入口（Desktop/Mobile 都不直连 Runtime），单点即完备。

**已知残余**：node 机器本机用户绕过 Gateway 直接打 Runtime localhost 端口——OS 信任边界问题（ADR-076 §auth/mode 已声明"Local 的信任边界是物理 OS 用户"），不在本 ADR 范围。

### D6：agent / node 的 visibility 字段

- `agent_owners.json.visibility: Private(默认) | Shared`，`PATCH /api/agents/{id}/visibility`（归属名单：owner ∨ admin，guest 不可改，见 D9 R1）切换；`GET /api/agents` 条目带 `visibility` + 服务端算好的 `can_manage`/`can_use`。默认值规则见 D3（onboarding default agent 例外落 Shared，Q1 已决策）。
- `node_owners.json.visibility: Private(默认) | Public`：`Public` 仅指**元数据**（name/online）对全体可见，便于团队知道"这台 GPU 机存在"；**不放开任何 manage 权限，也不放开 install**（install 永远需要 node manage 名单 = owner ∨ admin）。让别人能在这台机器上装/维护 agent 的唯一姿势是**其人为 node owner 或 admin**（node guest 是 use 档，不含机器管理权）；让别人能**用**某个 agent 聊天的姿势是**把其加进该 agent 的 guest（use）名单**。
- 无 `visibility` 历史条目读作 `Private`（fail-closed 默认）。

**列表过滤（B 方案，2026-10 修订）**：`private` 的 node 对 manage 名单之外的调用者**整条不出现在 `GET /api/nodes`**，而不仅是字段裁剪。理由：对无权者保留这一行没有任何可执行动作（install / fs browse / rename / LSP 配置全部 Node-manage 门控），却泄露了"存在这台机器、在线与否、装了几个 agent"这样的拓扑信息。owner / guest / admin 保留该行——侧边栏是对 node 唯一可操作的地方，隐藏它等于把 guest 授权架空。

**统一判定入口 `ownership::can_view`**：agent 列表、agent 详情、node 列表三处可见性过滤**必须**共用同一函数，不得各自推导：

```text
can_view = is_admin ∨ can_use ∨ visibility.is_published()
```

- `is_admin` 先行：否则无主资源（`owner=None`）对 admin 也不可见，admin 将无法发现并认领它（D7 的 claim 流程会死锁）。
- `can_use` 含 guest：guest 必须看得到自己被授权使用（聊天）的资源。
- 无记录 / `owner=None` 一律对非 admin **不可见**（D7 fail-closed 的一致延伸：既然无主资源是 admin-only，它就不该出现在任何普通账号的列表里）。

**已修复的两个反向 fail-open**（二者叠加即"agent 看得见但点不开、永远 loading"）：

| 位置 | 原实现 | 问题 |
|---|---|---|
| `list_agents` | `rec.is_some_and(\|r\| r.visibility == Private) && !can_use` | 仅过滤**已存在**的行 → 无记录 agent 全量泄露给所有账号 |
| `get_agent_detail` | `rec.is_none_or(\|r\| …)` | 无记录 = **可见**（与列表语义相反）→ 列表放行、详情 404 |

同一语义在两处用相反的默认值实现，是这类漂移的典型形态；`can_view` 的存在意义就是让它不再可能发生。

**第三处漂移修复（agent 列表未走节点级联，2026-10）**：`list_agents` 曾只对 **agent 自身行** 求 `can_view`，绕过了 §4 定义的 `Node → Agent` view 级联。后果：一个 `shared` agent 挂在调用者**看不见的节点**上时，仍会出现在 `GET /api/agents` 里，但点开它会被节点档 403/404（详情/use 走的是级联），且客户端按节点分组时找不到对应的可见节点。修复：agent 列表读取**必须复用 `acl::evaluate(Op::View, Resource::Agent(instance_id))`**——与详情/use 同一裁决源——agent 行与其 hosting node 行都通过 view 档才列出；`auth=None`（Local 模式）仍整层 no-op。由此「列出的每个 agent 都有可见节点」成为结构性保证，移动端/桌面端按节点分组**无需任何兜底桶**。

### D7：无主资源、转移与清理（fail-closed）

| 情形 | 规则 |
|---|---|
| 升级迁移：存量 node/agent 无 owner 记录 | 首次进入 multi_user 时**不自动认领**。admin 登录后可见全部无主资源，`PATCH .../owner` claim。普通用户升级瞬间失去对存量他人 agent 的（原本就不该有）操作力——这是修复本身，不是回归 |
| owner 账号被禁用/注销 | 资源回落无主（admin-only）。Gateway 侧不做级联删除（数据在 node 上，删不删是机器主人/admin 的决定） |
| owner 想移交 | `PATCH /api/agents/{id}/owner {user_id}` 仅 admin 可调用（转移=授权变更，owner 自己只能"共享"不能"改主"，避免把无审计的所有权踢来踢去） |
| uninstall 后 | `agent_owners.json` 条目在 Gateway 观察到 retained inventory 清空（dispatch.rs 的 remove 路径）时删除；孤儿条目（对应 instance 30 天未见）启动时 WARN + 保留（宁可留审计不留悬空权限） |

### D8：服务端单一真相 + Local 模式 no-op

**单一真相**：`GET /api/agents` / `GET /api/nodes` 每条记录由 Gateway 用 §4 判定函数算好 `can_manage` / `can_use` / `is_guest` / `visibility` 下发；Desktop/Mobile 只消费布尔渲染（禁用/隐藏 manage 入口），**不得**从 `owner`/`guests` 名单自行推导（延续 ADR-086 不变量 1）。403 兜底与布尔下发必须同源（同一判定函数，杜绝漂移）。

**Local 模式 no-op**：与 ADR-076 §决策 12 完全同构：`AuthMode::Local` 时 `auth_middleware` 不产生 `AuthContext`，授权层直接放行（策略表存在但不评估），owner 表照常写（Local 也记 owner，为将来切 multi_user 留数据）。**不新增任何配置项**——一个 `AUTH_MODE` 旋钮已经是既有决策，加 `owner_enforcement=off` 这类旋钮等于给安全修复开后门。

### D9：归属基数 = 单 owner + 多 guest（guest 是使用授权，不是共同维护）

**问题**：① owner 是否要再分权限档（读写 vs 只读）？非 owner 是否根本不可见？② 是否允许多 owner（admin 帮 node 加 owner）？

**决策 9a：权限轴三档（manage/use/view）；guest 落在 use 档，view 由 shared/public 放开。**
本模型**有**只读档（view），但"只读"只覆盖**描述 agent 本身的元数据/定义**（详情、status、config/prompts/skills/model 读、头像、permissions 名单）；**读取 owner 真实数据的接口**（工作区文件内容、git 历史、memory、全局 search）不落在 view，而落在 **use**——因为它们会浮出 owner 机器上的实际内容且没有 session 那种 private/public 二级墙兜底（详见 D4 门控表"检索类读归 use"原则与 §7 方案 G 的更新说明）。非 owner 的合法形态有两种：
- **guest**（use 授权）：开**自己的** session 聊天、启停中的"启"、读写工作区文件、检索 memory/search——但**不能改 agent 定义类配置、不能装卸/停止/克隆、不能改权限、不能删 session**（这些是 manage，永远只有 owner ∨ admin）；
- **viewer**（shared/public 仅 view 档）：看得到列表/详情/配置只读元数据，**不能聊天、不能碰任何写、不能读工作区文件内容/memory**；
- **viewer**（visibility 发布态）：`shared` agent / `public` node 对全体登录用户**仅可见**（列表/详情/元数据），**不含 use，也不含 manage**。
外加一个既有例外：admin 的 `as_user` 只读视图（ADR-076 §决策 4，原样保留）。

**决策 9b：owner 唯一；manage 不可委派给非 admin。**
多 owner（对等共主）被否决：谁能转移、谁能撤销对方、冲突谁裁决——所有权语义塌方，且"这台机器谁负责"从锚点退化为集合，正是本 ADR 要消灭的状态。单 owner + guest 名单下：**guest 是 use 授权（可增删、可收回、无残留），owner 是归属（唯一、转移走审计）**。本模型**不支持把 manage 委派给某个对等协作者**——需要多人共同配置/维护一台机器或实例，走 admin 角色，不走 guest；guest 只解决"让更多人能用（聊天）"这一诉求。

**guest 三条边界规则**：

| # | 规则 | 含义 |
|---|---|---|
| R1 | **授 use，不授 manage/归属** | guest 只能使用（聊天），**不能 manage**（配置/workspace/文件/生命周期/安装/卸载），不能转移 owner、不能增删 guest、不能改 visibility。manage 与归属操作永远只有 owner ∨ admin 两元（`can_transfer`，§4） |
| R2 | **不级联** | node guest ≠ 该 node 上 agent 的 guest。node 名单答"谁能看到/使用这台机器（不含机器管理）"，agent 名单答"谁能用（聊天）这个实例"，两级名单独立维护 |
| R3 | **撤销即失权，归属不回收** | 移除 guest 后其立即失去 use；但他作为 owner 拥有的其他 agent 仍归他所有（归属独立于授权）。卸载那些 agent 需要其本人/admin 或目标 node 的 manage 名单 |

**guest 名单操作**：`PATCH /api/nodes/{id}/guests`、`PATCH /api/agents/{id}/guests`（全量替换语义，PUT-list 风格，归属名单可调）；`GET /api/agents|nodes` 条目下发 `is_guest: bool`（并入 `can_use` 计算，客户端不自行查名单——D8 单一真相不变）。

**为什么 guest 不分子档（editor/viewer）**：guest 就是单一 use 档——"能不能用（聊天）"是二值的，配置/文件/生命周期（manage）不在 guest 授予范围内，永远只有 owner ∨ admin。可见性（view）由 visibility 全局解决（shared/public 即对全员可见），无需再为 guest 分"仅聊天 vs 可配置"两态——后者根本不属于 guest。

---

## 6. 鉴权流程

```mermaid
sequenceDiagram
    participant C as Desktop / Mobile（登录用户 U）
    participant G as Gateway auth_middleware
    participant P as Gateway 授权层（新增）
    participant R as Runtime / Node
    C->>G: POST /api/agents/{id}/workspaces (Bearer token)
    G->>G: 验签 → AuthContext{user_id=U, role}
    G->>P: 路由匹配 → Permission::AgentManage
    P->>P: agent_owners[id] → owner, guests, visibility
    alt U == owner ∨ U == admin（manage 名单，guest 为 use 档不在此列）
        P->>R: 反代转发（原样携带 x-user-id）
        R-->>C: 200
    else 不在 manage 名单
        P-->>C: 403 {code:"not_authorized", required:"manage"}
    end
```

---

## 7. 被否决的方案

| 方案 | 否决理由 |
|---|---|
| **A. owner 进 proto，由 Node 上报/回显** | 归属真相交给数据面（Node inventory 可被 Node 侧改写），Gateway 重启即被 inventory 覆盖；Node 不感知账号体系。见 D1 |
| **B. Runtime 侧做 owner 校验（Gateway 只透传）** | Runtime 需要完整归属表同步 + node/agent 双层真相复制；Gateway 本来就是 multi_user 唯一入口与既有鉴权点（`AuthContext` 在此），加第二执行点违反单一真相 |
| **C. 通用 RBAC（角色×资源×动作表）** | 当前只有三档权限、两种角色；泛化框架无近期消费者（YAGNI），且客户端渲染复杂度暴涨 |
| **D. per-workspace ACL（工作区级共享授权）** | 规则三尚未触发；workspace 权限天然跟 agent 权限一致（都是"动这台机器的文件"），细分先等真实需求 |
| **E. 默认 shared、owner 只管不问** | 与威胁模型相反——本 ADR 的动机就是"默认即公开"，必须默认 private fail-closed |
| **F. 存量数据自动归首个登录的 admin** | 静默改所有权无审计；无主+显式 claim 更安全，代价只是 admin 多点一次认领 |
| **G. 把 workspace 文件内容读放进 view** | 已按新三档修订：view 只覆盖**描述 agent 的元数据/定义读**（config/prompts/skills/model/status/头像/permissions 名单）；**读取 owner 真实数据的接口**（工作区文件内容、git 历史、memory、全局 search）一律落在 **use**，不对 shared agent 的纯 viewer 开放。理由不变——`GET /workspaces/file` 若归 view，等于让任意登录用户把 owner 机器上的工作文件拖走；只是实现方式从"读写同级全归 manage"改为"内容读归 use、元数据读归 view"（见 D4 表"检索/内容类读归 use"原则、D9a） |
| **H. "非 owner 只读"= 放开全部内容读** | 修订：本模型**有** view 档，但它**不是**"能读机器上的任何文件"。viewer（shared/public 非 guest）只看得到 agent 是什么（详情、配置定义、头像、权限名单），看不到 owner 的工作文件内容、memory、聊天记录（session 内容另由 private/public 第二道墙兜底）。"只读"若指整盘文件，仍等于 node 机器数据外泄，故内容读必须留在 use |
| **I. 多 owner（对等共主，admin 帮资源加 owner）** | 所有权语义塌方：转移/撤销/冲突裁决无主，"这台机器谁负责"从锚点退化为集合。协作诉求由 guest 名单承接，"加 owner"的真实需求落进模型都是"加 guest"，见 D9b |

---

## 8. 影响

**正面**
- 默认拓扑从"整台机器对全体用户公开"变为"机器属于 enroll 它的人"；攻击场景 1-5 全部关闭。
- 共享从"意外默认"变为"显式声明"（agent visibility）。
- 客户端权限渲染统一为服务端布尔，消灭前端推导（延续 ADR-086 不变量）。

**代价与风险**
- 新增两个 Gateway 本地状态文件（与既有 token 表同范式，运维面几乎不增）。
- 升级当天：所有存量 agent 对普通用户变 private——需要 release note 明确"admin 认领"步骤。
- 所有经 Gateway 反代的 agent 路由都要进策略表，**漏一条 = 留一个洞**。缓解：classify() 按"默认拒绝"设计（未登记的 `/api/agents/{id}/**` 路由**任何方法**一律落 Agent-manage，真实读路由必须显式登记为 view/use），并为该不变量写测试（§9）+ `dev/ci.sh::run_permission_route_redline` 枚举门。
- MQTT 数据面仍 permissive：本 ADR 不解决"别人 session 的事件扇出到所有 localhost 客户端"的已知问题，需后续 ACL ADR（§13 Q3）。
- **Follow-up（未在本实现中处理）**：owner 账号被禁用/注销时，其名下 node/agent 的 owner 行不会自动失效——当前行为是 fail-closed（`can_manage` 对禁用账号自然 403，资源转为事实无主），但缺少"注销即批量转无主 + 审计"的显式联动，需与账号注销流程（acowork-user）一并设计。
- **已修复（visibility 静默回弹）**：`OwnershipStore::upsert_with` 的 `ownerless ⇒ not Shared` 归一化发生在调用方 mutation **之后**，因此把无主 agent 置为 `shared` 时：handler 回 `200 {"visibility":"shared"}`、存储仍是 `private`、Desktop 保存后重新 `load()` 读到旧值 → 开关无报错地弹回关闭。修复分两半且都必要：① `PATCH .../visibility` 在目标无主且请求"发布"（agent `shared` / node `public`）时前置拒绝，返回 `409` 并在错误文案里给出两步恢复（先 `PATCH .../owner` 认领，再设 visibility）；② Desktop 权限弹窗保存失败时把草稿回滚到服务端真值，避免"被拒的写入"与"根本没发生写入"在 UI 上无法区分。归一化本身保留——它是数据不变量而非授权判定（`can_view = is_admin ∨ can_manage ∨ shared` 已经保证无主资源对外不可见），删掉它反而会让无主 agent 借 visibility 逃逸 fail-closed。回归测试：`patch_agent_visibility_rejects_shared_on_ownerless_row` / `node_visibility_public_persists_once_an_owner_is_claimed` 及前端 `reverts the switch to the server value…`。**补 UI 入口（第一次修复的遗漏）**：409 文案虽然给出了两步恢复，但第一步要手发 HTTP 请求，而弹窗内 owner 一栏原本是只读的、也没有任何认领控件——对所有存量资源（无主是升级后的默认状态）而言恢复路径实际不可达，等于把死路换了个说法。因此补 `patchAgentOwner`/`patchNodeOwner` 前端封装 + 弹窗内 admin 专属的「认领归属」按钮（`canClaim = isOwnerless && account.role === admin`），认领后对话框自动 reload，有主资源不再显示该按钮。认领是独立的一次调用（不是 Save 的一部分），所以仍支持「先认领、再单独设可见性」的两步路径，按钮只是把 409 已经规定的那条路缩短成一次点击。

---

## 9. 验证计划

1. **单测（Gateway）**：策略表纯函数——路由×角色×归属 → allow/deny 全矩阵；重点回归"未登记路由默认拒绝"。
2. **结构不变量测试（进 `dev/ci.sh`，与 `run_gateway_fs_redline` 并列）**：枚举 `proxy.rs`/`agents.rs`/`fs_browse.rs` 注册的全部 `/api/agents/{id}/**` 与 `/api/fs/browse` 路由，断言每条都在策略表中有显式档位或落入默认拒绝桶——**新增路由不登记就红**。
3. **集成（multi_user 双账号）**：alice enroll node A（token 绑 owner）→ bob `POST install@A` 403；alice 装 agent → bob `POST workspaces` 403（写=manage）/ `GET workspaces/file` 403（内容读=use，非 guest 拒）/ `GET fs/browse?target=A` 403；alice 仅标 `shared`（未加 bob 为 guest）→ bob 可见该 agent 且能 `GET config`/`GET model`/`GET permissions`（元数据/定义读=view 放行）但 `POST sessions` **403**（shared 只放开可见+只读，不放开 use）、`GET files`/`GET memory` **403**（内容读归 use）；alice 加 bob 为 agent guest → bob `POST sessions` 201、`GET files` 200、`GET memory` 200（use 放行）且 bob 的 session 对 alice 之外的第三方仍按 ADR-076 隔离；admin 全通 + claim + 转移。
4. **guest 语义（D9 三规则逐条断言）**：alice 加 bob 为 agent guest → bob `POST sessions` 200（use 放行）但 `POST workspaces` **403**（guest 不含 manage）、bob `PATCH guests/visibility/owner` 403（R1）；bob 加为 node A guest → bob `POST install@A` **403**（guest 不含机器管理权，安装需 node owner ∨ admin）；alice 撤销 bob 的 guest 后 bob 的 use 调用（sessions）立即 403（无缓存残留）。
5. **Local 模式回归**：`AuthMode::Local` 全部行为与升级前逐字节一致（no-op 断言）。
6. **迁移演练**：带存量数据升级 → 普通用户列表只剩自己可见项、无主资源 admin 认领后功能恢复。
7. **e2e（Desktop + Mobile）**：非 owner/admin 的 manage 入口按 `can_manage=false` 禁用；guest（use 授权）的聊天路径畅通、非 guest 对 shared agent 仅可见不可聊（use 写操作 403）；guest 徽标与名单管理 UI 可用。

---

## 10. 实施拆解（按模块，供排期）

| 步 | 内容 | 涉及 |
|---|---|---|
| 1 | `node_owners.json` / `agent_owners.json` 存储（原子写 + 内存镜像，仿 enrollment.rs） | `acowork-gateway/src/gateway/` 新模块 `ownership.rs` |
| 2 | enroll 绑定 owner（token 记录 + `decide_enroll` 落表）；`POST /api/nodes/enrollment-tokens` | `mqtt/enrollment.rs`、`mqtt/dispatch.rs`、`http/nodes_api.rs`、`cli.rs` |
| 3 | install/ensure/clone 写 agent owner；uninstall/retained-clear 清理 | `http/agents.rs`、`mqtt/dispatch.rs` |
| 4 | 授权策略层（默认拒绝 + 显式档位）+ 403 结构化错误；`can_manage/can_use` 下发 | `http/routes.rs`、`http/proxy.rs`、`http/agents.rs`、`http/fs_browse.rs` |
| 5 | visibility + guest：`PATCH /api/agents/{id}/visibility`、`PATCH /api/agents/{id}/guests`、`PATCH /api/nodes/{id}/visibility`、`PATCH /api/nodes/{id}/guests`（归属名单可调）、`PATCH .../owner`（admin）；列表过滤含 guest 维度；`can_manage/can_use/is_guest` 下发 | 同上 |
| 6 | ci.sh 路由登记不变量测试 + 集成/e2e | `dev/ci.sh`、`tests/` |
| 7 | Desktop：设备添加向导改走 HTTP enrollment token；AgentList/NodeList 消费布尔；owner 徽标 + claim UI；Mobile 同步消费 `can_manage/can_use`（对齐 ADR-086 决策 11 的"后端单一真相"） | `apps/acowork-desktop`、mobile |

步骤 1-4 是安全修复的最小闭环（先上线即堵洞），5-7 可跟进。

---

## 11. 兼容性红线

- 不改 `NodeInfo` / `InstalledAgentInfo` proto 字段（归属不进数据面）；MQTT topic 结构不变。
- 不改 session 维度任何语义（ADR-076 §决策 4 原样有效）；`x-user-id` 注入机制不动。
- 不改 Gateway 红线（ADR-009 §5）：鉴权全部在反代入口，Gateway 依旧不碰 Agent 私有文件。
- 无兼容层：multi_user 下未登记路由直接 403，不静默放行（"silent fallback 掩盖不安全状态"是本 ADR 要消灭的反模式本身）。

---

## 12. 开放问题

| # | 问题 | 现状 |
|---|---|---|
| Q1 | **default agent（ADR-077）的归属**：onboarding 由首个用户安装 → owner=该用户，其他用户默认不能用（除非被加进 guest 名单）。是否应把"onboarding 装的 default agent"默认 `shared`（团队开箱即用地**可见**）？ | ✅ **已决策（2026-10-28）：默认 `Shared`**。onboarding 预装的 default agent 落 shared（全员可见），owner 仍可改回 private；用户自行安装的实例维持默认 private。**注意 `shared` 只放开可见，聊天仍逐个 guest 授权**（见 D6） |
| Q2 | **admin 的 install 落点**：admin 把 agent 装到 user X 的 node，需要 X 的 manage 授权吗？还是 admin 天然可装任何 node？ | ✅ **已决策（2026-10-28）：admin 可装任意 node**。admin 天然通过所有 Node-manage/Agent-manage 门控，不加 `admin_overrides_nodes` 旋钮（无近期需求即不加配置面，YAGNI）。所有权仍记 admin 自己，node owner 事后经 admin 转移可回收 |
| Q3 | **MQTT 数据面 ACL**：permissive subscribe 导致跨账号事件扇出（已知 e2e flake 根因）。owner 模型落地后，ACL 的订阅过滤规则（`user:{id}` 只能订自己可见资源的事件）应与其对齐 | 另立 ADR（与本 ADR 的 HTTP 控制面正交，不阻塞本 ADR 落地） |
| Q4 | **agent 使用的成本归属**：guest 用我的 agent 烧的是我的 provider key / 配额。预算（budget tracker）是否需要按 caller 记账或按 agent 限额？ | ✅ **已决策（2026-10-28）：按 agent 记账，不做 user 维度**。用量归属 agent（即其 owner 的 key/配额），budget tracker 维持 agent 粒度；caller 级分摊/限额出现真实需求再议 |
| Q5 | **`GET /api/agents/{id}/avatar` 等读端点**：private agent 的头像对非 owner 404 还是允许？ | 建议 404（列表已过滤，避免探测存在性），实现时定 |

---

## 13. 参考文件索引

| 文件 | 角色 |
|---|---|
| [core/acowork-gateway/src/http/auth_middleware.rs](../../../core/acowork-gateway/src/http/auth_middleware.rs) | `AuthContext` 来源；授权层的挂载点 |
| [core/acowork-gateway/src/http/proxy.rs](../../../core/acowork-gateway/src/http/proxy.rs) | 全部 agent-scoped 反代路由；策略表主战场 |
| [core/acowork-gateway/src/http/agents.rs](../../../core/acowork-gateway/src/http/agents.rs) | install/ensure/clone/start/stop/uninstall；owner 写入点 |
| [core/acowork-gateway/src/http/fs_browse.rs](../../../core/acowork-gateway/src/http/fs_browse.rs) | `?target=` 反代；Node-manage 门控 |
| [core/acowork-gateway/src/mqtt/enrollment.rs](../../../core/acowork-gateway/src/mqtt/enrollment.rs) | token 记录扩 `owner_user_id`；ownership.rs 的持久化范式模板 |
| [core/acowork-gateway/src/mqtt/dispatch.rs](../../../core/acowork-gateway/src/mqtt/dispatch.rs) | `decide_enroll`、installed inventory 聚合（owner 绑定/清理时机） |
| [core/acowork-gateway/src/mqtt/node_registry.rs](../../../core/acowork-gateway/src/mqtt/node_registry.rs) | Node 在线视图；`can_manage` 渲染输入 |
| [core/acowork-memory/src/session_meta.rs](../../../core/acowork-memory/src/session_meta.rs) | session 维度判定范式（`is_readable_by`/`is_writable_by`）的参照物 |

---

## 附录 A：修订记录 — 409 不是 UI 契约（2026-10 修订）

**触发**：multi_user 现场。admin 登录，右键 agent → 权限 → 打开可见性开关 → 保存，得到
`409 this agent has no owner yet…`（`owner_user_id: null` 是所有存量资源的默认状态，见 §8 升级风险）。

**诊断**：409 本身没撒谎——`upsert_with` 的 `ownerless ⇒ not published` 归一化确实会让那次写入静默回滚。
但**它把一个可预知的数据状态包装成了运行时错误**，且不看调用者身份：唯一有权认领的 admin 反而被这条 409 挡住。
D8 早就写明"客户端只消费服务端算好的布尔"，而 `GET .../permissions` 当时只下发 `can_attribute`
（**权限**答案）——admin 在无主行上照样 `true`，前端于是照常启用开关，点了才被服务端打回。
**权限**与**数据状态**被混为一谈，这是根因；409 只是它的症状。

**修订**（D8 的补齐，非新决策）：

1. `ownership::can_publish` / `ownership::is_ownerless` 成为共享判定函数——`upsert_with` 的归一化规则、
   `permissions` 响应、两个 `PATCH .../visibility` 的 guard 四处此前各自推导同一件事，现在共用（§4 "统一判定入口"的同理要求）。
2. `GET /api/{agents,nodes}/{id}/permissions` 新增 `can_set_visibility` + `ownerless`。
   客户端据此 `disabled` 开关并显示原因（`needsOwnerHint`），**不再靠点击去发现**。
3. 409 保留，仅作非 UI 调用方的兜底（防止绕过 `can_set_visibility` 直连 HTTP 把静默归一化说成成功），
   文案去掉"请手动 PATCH"——终端用户不该被要求打开 devtools。
4. 弹窗内 admin 专属「认领归属」按钮（`canClaim = isOwnerless && role === admin`）保留。

**明确否决的方案**：认领时"顺手"把可见性设为发布态。
无主行按构造就是 private，把它默认开放给全体登录用户是一次**静默的授权扩大**——
虽然省掉一次点击，但"点一下认领 = 把 agent 公开给所有人"不该由一个按钮替 admin 决定。
故认领与发布是两个显式动作，与 ADR 正文 D7 的两步恢复一致。

**未决**：存量 ownerless 行的**归属迁移**尚未设计（§8 已列为升级风险，本次只保证恢复路径可达、不再报错）。
迁移需要回答"这台机器/这批 agent 归谁"——ADR-087 故意没定，留给部署方决策。

**回归测试**：`permissions_reports_an_ownerless_row_as_unpublishable` /
`permissions_reports_an_owned_row_as_publishable`（后端数据状态）、
`disables the visibility switch instead of offering a write that 409s` /
`keeps an owned resource's switch enabled` / `claims ownership without also changing visibility`（前端交互）。

## 附录 B：修订记录 — 无主资源不再由构造产生（2026-11 修订）

**触发**：附录 A 解决了"已存在的无主行如何恢复"，但无主行本身仍在被持续制造。
盘点出 7 条产生路径：

| 路径 | 场景 | 原行为 |
|---|---|---|
| N1 | Gateway 启动自 spawn 本机 node | `create_token(3600, None)` → node 无主 |
| N2 | CLI `nodes token create` | 无用户上下文 → token 无主 → node 无主 |
| N3 | `mqtt.auth_enabled=false` 下裸 enroll | 无 token 可解析 → 无主 |
| N4 | re-enroll `put_if_absent` | 已无主则继续无主（固化） |
| A1 | CLI `install` 走 MQTT 派发 | 完全绕过 owner 写入 |
| A2 | Local 模式 install/ensure/clone | `ctx=None`（**正确**，见下） |
| A3 | onboarding 默认 agent 在登录前安装 | 无调用者 → 无主 |

**修订原则**：owner 绑定发生在**用户交互层**，token 是运行时内部物，绝不让用户从日志/配置里
捞 token 塞命令行。除"服务器上 CLI 启动 Gateway"这一刚需场景（操作者必是 admin）外，
其余场景全部在 Desktop 内闭环。

**新规则**（对 D2 第 4 条、D7 第 1 行的修订）：

1. **两个汇聚点兜底**：enroll Accept 落库与 agent inventory 首次落地（`commit_pending`）时，
   若解析不出 owner，回退到 `default_owner`——Gateway 启动时经 user service
   （`acowork-user first-admin`）解析的最早创建的 admin `user_id`。单点 guard 覆盖所有旁路。
2. **N1**：Gateway spawn 本机 node 的 enrollment token 直接绑 admin（服务器场景，合理）。
3. **N2/A1**：CLI `nodes token create` / `install` 在 multi_user 下默认绑 admin。
4. **新机器接入（替代命令行捞 token）**：
   - Desktop：agent 列表 `+` 菜单新增「创建本机 Node」→ Tauri 命令内部
     `POST /api/nodes/enrollment-tokens`（Bearer=登录者，owner 即登录者）→ spawn 随包
     `acowork-node`（detached）。token 不出现在任何用户可见面。幂等：已 enroll 则跳过签 token，
     变为纯"启动"。
   - headless CLI：`acowork-node start` 无 identity 且无 `--token` 时交互式输入账号密码，
     node 自行 `POST /api/auth/login` → `POST /api/nodes/enrollment-tokens` 换绑定 token 再 enroll。
5. **认领端点**：`POST /api/nodes/{id}/claim`、`POST /api/agents/{id}/claim`——仅无主行可 claim；
   本机 node 放宽到任意登录用户（人在机器旁自证），远程 node/agent 仍 admin-only。
6. **存量 adopt**：Gateway 启动时若解析到 admin，一次性 adopt 全部 ownerless 行
   （marker-gated，只跑一次）；此后仍残留的 ownerless 行启动 WARN 汇总（node/agent 分别计数）。

**Local 模式不变**（A2 不是 bug）：单机 loopback → `resolve_auth_mode` = Local，账号系统整体
关闭，`owner=None` 是正确语义——"机器即 owner"（D8 no-op）。local → multi_user 切换时由第 6 条
adopt 完成历史交接。

**修订后 D7 语义**：ownerless 从"出生即可能的常态"收窄为**过渡态**——只由 owner 账号被禁用/注销
产生；构造路径全部绑定 owner。

**回归测试**：`enroll_with_ownerless_token_falls_back_to_default_owner`、
`installed_landing_without_staged_row_gets_default_owner`（汇聚点兜底）、
`adopt_ownerless_binds_all_null_rows`（存量交接）、claim 端点 4 例（无主可 claim / 有主 409 /
本机放宽 / 远程 admin-only）。Gateway 全量 619 通过。
