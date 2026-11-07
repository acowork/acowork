# Intent agent↔agent 会话的人类知情权计划（aware_users）

> 版本：v1.0 | 日期：2026-11-07
>
> **一句话**：agent 通过 intent 触发的执行 session 把「发起方」和「归属人类」两件事压进了同一个 `initiator` 字段——agent↔agent 时 `initiator` 是 agent UUID，于是该会话对任何人类不可见（黑盒）；PM 派发时 `initiator` 是 `task.created_by`（MCP 下是 agent），于是 project owner 能看不能写。本计划把 `initiator` 的语义还原为「谁发起了这个 session」（agent↔agent 保持 agent），另立第三个正交字段 `aware_users` 承载「哪些人类对这个 session 有只读知情权」，由 Gateway 按 agent ACL 解析、随 Intent wire 带到接收侧 stamp。拆成 M1–M5 五个可独立交付/测试的模块。
>
> 前置调查已完成，证据均带 file:line（见 §1）。本文档记录设计与排期；§9 记录实施进度。

---

## 1. 缺陷摘要（调查结论）

### 1.1 问题 1：agent↔agent 执行 session 对人类不可见（黑盒）

链路逐段：

1. **工具丢弃人类 origin**：[intent_send.rs:170](../../../core/acowork-runtime/src/tools/builtin/intent_send.rs#L170) 请求体只有 `"sender": self.config.instance_id`（发送 agent 的 UUID）。`ToolContext` 其实带 `chat_session_id`（[traits.rs](../../../core/acowork-core/src/tools/traits.rs)），但没把它作为 `source_chat_session_id` 传出，也没带 session 的 `initiator`。
2. **Gateway 建零人类 pair 会话**：[intent_api.rs:1100](../../../core/acowork-gateway/src/http/intent_api.rs#L1100) `deliver_intent` 因请求无 source chat，走 `ensure_pair(sender, target, "agent_agent")` —— 参与者仅两个 agent，**无人类**。
3. **from = agent**：[intent_api.rs:1143](../../../core/acowork-gateway/src/http/intent_api.rs#L1143) `from_instance: req.sender.clone()`。
4. **接收侧 initiator = agent**：[gateway_loop.rs:1586](../../../core/acowork-runtime/src/startup/gateway_loop.rs#L1586) `route_intent_session(..., Some(env.from.as_str()))`。
5. **stamp Private → Members**：[session_manager.rs:1330](../../../core/acowork-runtime/src/agent/session/session_manager.rs#L1330) `initiator.is_some()` → `Private`；`stamp_chat_binding`（[conversation.rs](../../../core/acowork-runtime/src/conversation.rs) `set_chat_binding`）绑 pair 会话、提升 `Members`。
6. **人类读不到**：[session_meta.rs](../../../core/acowork-core/src/session_meta.rs) `is_readable_by` 的 Members 分支要求 `chats` 命中或 `initiator==uid`；Nicholas 既不在 pair 会话的 chats 里，uid 也 ≠ agent UUID → 不可读。

`IntentEnvelope`（[inbound.rs:90](../../../core/acowork-runtime/src/agent/inbound.rs#L90)）结构里**没有**任何人类 origin 字段——这是设计缺口，非 bug。

### 1.2 问题 2：PM 任务 session 对 project owner 可见但不可写

1. **owner = agent**：[dispatch.rs:233](../../../core/acowork-pm/src/dispatch.rs#L233) 把 `&task.created_by` 作 owner 传 Gateway；MCP 建任务时 `created_by` = `X-MCP-Actor`（agent instance UUID，[pm_proxy.rs:30](../../../core/acowork-gateway/src/http/pm_proxy.rs#L30)）。
2. **代码注释自认**：[intent_api.rs:420](../../../core/acowork-gateway/src/http/intent_api.rs#L420)「`req.owner` is the task's *creator* (`Task::created_by`, an agent for MCP-created tasks), not `Project::owner()`」。
3. **可见**：[intent_api.rs:552](../../../core/acowork-gateway/src/http/intent_api.rs#L552) `from_instance = req.owner`（agent），绑定项目群聊 → `Members`；Nicholas 是群聊参与者 → `is_readable_by` Members 分支命中 → 可读。
4. **不可写**：[session_meta.rs](../../../core/acowork-core/src/session_meta.rs) `is_writable_by` 对 `Some(initiator)` 只判 `initiator == uid`（设计 27 §8.2：Members 是只读授权，从不转移所有权）。agent UUID ≠ Nicholas → `can_write=false` → 前端 [session-write-access.ts](../../../apps/acowork-desktop/src/lib/session-write-access.ts) 禁用输入框。
5. **project owner 是人类且拿得到**：`Project::owner()` = `owner_id ?? created_by`（[types.rs:340](../../../core/acowork-pm/src/types.rs#L340)），是**人类** uid。

### 1.3 关键事实：两套权限各管各的，读 session 要过两道闸

- **第一道闸（Gateway，agent ACL）**：`GET /api/agents/{id}/sessions` 要求 `can_view` = admin ∨ owner ∨ **guest** ∨ published（[permission.rs:232](../../../core/acowork-gateway/src/http/permission.rs#L232)）。guest 能过。
- **第二道闸（Runtime，session 可见性）**：过了第一道后逐行过 `is_readable_by(scope)`（[conversation.rs:3733](../../../core/acowork-runtime/src/conversation.rs#L3733)），`scope.chats` 仅「该用户参与的 chat 列表」（[chat_blocks.rs:607](../../../core/acowork-gateway/src/http/chat_blocks.rs#L607) `list_session_ids(who)`），**不含 agent ACL**。
- 注释直白（[permission.rs:224](../../../core/acowork-gateway/src/http/permission.rs#L224)）：「a private session is absent to a viewer」——第二道闸刻意按人隐私设计，**不**因你有 agent view 权限就放行。
- **后果**：agent↔agent 会话若只靠 `initiator=单个发起人`，则**其他有 agent read/chat 权限的 guest、以及非 initiator 的 owner 都看不见**。这正是需要 `aware_users` 补的洞。

### 1.4 已确认无误、本计划不触碰的部分

- **问题 3（卡片状态不实时）独立于本计划**：刷新链路（doorbell + 500ms 重试 + 15s interval + retained session_state）已完整且正确，残留是「500ms 单次重试后到 15s 轮询之间的恢复空窗」这一时序问题，另案处理。本计划不涉及。
- **agent ACL 表**：`agent_owners.json` = `instance_id → OwnerRecord{owner_user_id, guests[], visibility}`（[ownership.rs:87](../../../core/acowork-gateway/src/gateway/ownership.rs#L87)），`state.agent_owners` 在 intent 投递处可直接读（[acl.rs:168](../../../core/acowork-gateway/src/http/acl.rs#L168)），`default_owner`(admin) 兜底（[mod.rs:866](../../../core/acowork-gateway/src/gateway/mod.rs#L866)）。这是 seed `aware_users` 的权威数据源。
- **TurnOrigin 已存在**：[intent.rs:24](../../../core/acowork-core/src/intent.rs#L24) `Chat`/`PmTask`（人类说话）vs `AgentRequest`/`AgentResponse`（peer, not a person）。「是不是人类发起」本就该由它承载，不该让 `initiator` 兼职。

---

## 2. 目标与非目标

**目标**

1. agent↔agent 执行 session 对「有 agent 权限的人类 + 链上发起人」可读，杜绝无人认领的黑盒。
2. PM 任务的 project owner 对该任务 session 可读**且可写**。
3. `initiator` 语义还原为「发起方」，agent↔agent 保持 agent，user↔agent/PM 保持人类——不再混淆。

**非目标**

1. **不让人类「进入」agent↔agent 会话发言**（写/交互）——复杂度未想清楚，先不做。`aware_users` 严格只读。
2. **不动态跟踪 ACL**——`aware_users` 是创建时快照，后加入的 guest 不自动获得旧 session 知情权。
3. 不改 `is_writable_by` 的单发起人性（ADR-076 §决策 4）。
4. 不处理问题 3（卡片刷新时序）。

---

## 3. 核心不变量（不可违反，测试锁死）

> **按 agent ACL 灌 `aware_users` 只对 `origin ∈ {AgentRequest, AgentResponse}` 成立。** `origin ∈ {Chat, PmTask}` 的会话有明确的人类隐私主体，`aware_users` **不从 ACL seed**，读权仍锁在 `initiator` + chat 参与者。

反面后果（若违反）：共享 agent 下用户 B 被灌进用户 A 的 user↔agent 私聊 `aware_users` → A 的隐私泄露。不变量同时封住「黑盒」（agent↔agent 必须有人类可见）与「越权」（人类私聊不得被 agent ACL 泛化）。

**第二条不变量（黑盒原则）**：任何 `AgentRequest`/`AgentResponse` 会话的 `aware_users` **永不为空**——至少含 sender 的 owner；ownerless 兜底到 `default_owner`(admin)。local 单机模式经 `SessionScope::Unfiltered`（无账号体系，全部可读）天然满足，seed 允许为空，不做显式 fail-open（见 §9 评审轮修正）。

---

## 4. 数据模型：三个正交维度

| 字段 | 回答的问题 | agent↔agent | user↔agent / PM 任务 |
|---|---|---|---|
| `initiator` | 这个 session 的**单一归属/写者** | 发起的 **agent**（保持不动） | 发言的**人类**（Chat=该用户；PmTask=`Project::owner()`） |
| `origin: TurnOrigin` | 这轮是**人话还是 peer 请求** | `AgentRequest`/`AgentResponse` | `Chat`/`PmTask` |
| `aware_users`（新增） | 哪些**人类有只读知情权** | owner ∪ guests ∪ 发起人（sender+target 都算） | 空（由 initiator + chat 覆盖） |

`aware_users: Vec<String>` 是 `is_readable_by` 在现有分支之外的**一条附加只读放行**，不改任何现有分支，`is_writable_by` **不咨询它**。

---

## 5. 模块分解

每个模块可独立 review、独立测试、独立回滚。M1 是其余模块的契约前提。

### M1 — 数据模型 & wire（acowork-core，~0.5d）

- `SessionMeta` 新增 `pub aware_users: Vec<String>`，`#[serde(default, skip_serializing_if = "Vec::is_empty")]`（旧行反序列化为空 = 行为不变）。落点：[session_meta.rs](../../../core/acowork-core/src/session_meta.rs) `chat_session_id` 之后。
- proto `Intent` 新增 `repeated string aware_users = 17;`（15 reserved、16 已用 chat_row_id）。落点：[mqtt_payload.proto:1076](../../../core/acowork-core/proto/mqtt_payload.proto#L1076)。
- `TurnOrigin` 不改。
- **需排查**：所有 `SessionMeta { ... }` 结构体字面量与 SQLite store 的列映射（`acowork-sqlite`）是否需补该字段。

### M2 — Gateway 侧 seed（acowork-gateway，~1d）

- `DeliveryRequest`（[delivery.rs:117](../../../core/acowork-gateway/src/intent/delivery.rs#L117)）+ envelope 构造（[delivery.rs:257](../../../core/acowork-gateway/src/intent/delivery.rs#L257)）：加 `aware_users: Vec<String>`，随 `Intent` wire 带到接收侧（与 `has_human_participant` 同构）。
- `deliver_intent`（[intent_api.rs:1137](../../../core/acowork-gateway/src/http/intent_api.rs#L1137)）：origin=AgentRequest/Response 时
  `aware_users = {originator} ∪ owner(sender) ∪ guests(sender) ∪ owner(target) ∪ guests(target)`，从 `state.agent_owners` 读。
  `originator = req.on_behalf_of ?? agent_owner(sender) ?? default_owner`。保证非空。
- `dispatch_task`（[intent_api.rs:371](../../../core/acowork-gateway/src/http/intent_api.rs#L371)）：问题 2 根修——`from_instance`/owner 由 `task.created_by`（agent）改为 **`Project::owner()`（人类）**；PmTask 不 seed ACL。chat 行 `from` 随之变人类（与 `TurnOrigin::PmTask`「the human owner speaks」一致，属可见行为变化，测试固化）。
- `IntentRequest` HTTP body：加 `on_behalf_of: Option<String>`。

### M3 — Runtime 侧 stamp（acowork-runtime，~1d）

- `ToolContext`（[traits.rs](../../../core/acowork-core/src/tools/traits.rs)）+ 构造处（[loop_.rs:715](../../../core/acowork-runtime/src/agent/loop_.rs#L715)）：加 `initiator: Option<String>`，取 `conv.initiator()`（getter 已存在，[conversation.rs:2544](../../../core/acowork-runtime/src/conversation.rs#L2544)）。
- `intent_send`（[intent_send.rs:170](../../../core/acowork-runtime/src/tools/builtin/intent_send.rs#L170)）：body 补 `on_behalf_of`（发送 session 的 initiator，仅当它是人类 uid）+ 补 `source_chat_session_id = ctx.chat_session_id`（让「人类会话里代发」的执行 session 落回**同一个**人类 chat，卡片出现在你在看的流）。
- `IntentEnvelope`（[inbound.rs:90](../../../core/acowork-runtime/src/agent/inbound.rs#L90)）：加 `aware_users: Vec<String>`。
- `create_intent_session` / `route_intent_session`（[session_manager.rs:1410](../../../core/acowork-runtime/src/agent/session/session_manager.rs#L1410)）：`initiator` 继续 = `env.from`（agent↔agent 保持 agent）；新增把 `env.aware_users` stamp 到 SessionMeta。
- `ConversationSession`：加 `set_aware_users`（write-once，仿 [set_initiator:2519](../../../core/acowork-runtime/src/conversation.rs#L2519)）。

### M4 — 权限判定（acowork-core，~0.3d）

- `is_readable_by`（[session_meta.rs](../../../core/acowork-core/src/session_meta.rs)）：`User{uid,..}` 各分支统一加 `|| self.aware_users.iter().any(|u| u == uid)`。**只读**。
- `is_writable_by`：**不动**。

### M5 — 测试（~1d）

见 §7。

---

## 6. 实施顺序与批次

| 批次 | 内容 | 依赖 | 可独立交付 |
|---|---|---|---|
| 1 | M1（契约 + wire） | — | 是（纯追加，旧行为不变） |
| 2 | M4（判定分支） | M1 | 是（aware_users 恒空时无影响） |
| 3 | M3（stamp 链路） | M1 | 与 M2 同批联调 |
| 4 | M2（seed + PmTask owner） | M1/M3 | 是 |
| 5 | M5（回归） | 全部 | — |

M2/M3 需一起验证（wire 生产端 + 消费端）。M1+M4 可先合（默认空 = 无行为变化），风险最低。

---

## 7. 验收指标 / 测试用例

1. **防泄露（不变量反面）**：共享 agent 下 A、B 各自的 user↔agent 私聊 session，A 读不到 B 的、B 读不到 A 的。
2. **agent↔agent 多人类可读**：A、B 作为同一 agent 的 guest，都能读该 agent 的 agent↔agent session（aware_users 命中）。
3. **黑盒非空**：纯 agent 自主（无人类发起）intent → aware_users 至少含 owner/admin，非空。
4. **PM 可写**：PM 派发 → project owner 可读且可写（initiator=owner，`is_writable_by` 命中）。
5. **initiator 语义**：agent↔agent 的 session `initiator` 仍是 agent UUID（不被人类覆盖）。
6. **代发落回原会话**：人类会话里让 agent 代发 intent → 执行卡片落在原人类 chat（群聊/单聊各一例）。
7. **向后兼容**：旧 SessionMeta（无 aware_users）反序列化为空、行为不变；旧 sender（无 field 17）wire 兼容。
8. `is_readable_by` 真值表新增 aware_users 列：命中但非 initiator/非参与者 → 可读**不可写**。

---

## 8. 风险与回滚

| 风险 | 影响 | 缓解 |
|---|---|---|
| `SessionMeta` 加字段波及结构体字面量 / SQLite 列 | 编译期可见 | 排查所有 `SessionMeta {` 与 store 映射；`#[serde(default)]` 保证反序列化不破 |
| `dispatch_task` owner 改人类 → 群聊 dispatch 消息发言者显示变化 | 可见行为变化 | 与 PmTask 语义一致；测试固化 |
| aware_users 快照 vs 后加 guest 不同步 | 新 guest 看不到旧 session | 已确认接受（非目标）；将来另做 ACL reconcile |
| local 单机模式无 ACL 表 | seed 逻辑空跑 | 不报错；可读性由 `Unfiltered` scope 兜底（local 无账号可限制），无需显式 fail-open（评审轮确认，见 §9） |
| seed 覆盖 sender+target 两 agent → 名单偏大 | 可读面变宽 | 已确认「都算」；仅 agent↔agent，无人类隐私主体 |

**回滚粒度**：M1/M2/M3/M4 各自独立提交，均可单独 revert。M1+M4（默认空）合入后对现网零行为影响，是最安全的先行批次；M2/M3 成对回滚。

---

## 9. 实施记录（完成）

### 模块落地

| 模块 | 状态 | 落点 |
|---|---|---|
| M1 `aware_users` 字段 + proto field 17 + SQLite v12 | 完成 | [session_meta.rs](../../../core/acowork-core/src/session_meta.rs)、[mqtt_payload.proto](../../../core/acowork-core/proto/mqtt_payload.proto)、[schema.rs](../../../core/acowork-memory/src/sqlite/schema.rs)、[sqlite/session_meta.rs](../../../core/acowork-sqlite/src/session_meta.rs) |
| M2 Gateway seed + PM owner 兜底 + on_behalf_of | 完成 | [intent_api.rs](../../../core/acowork-gateway/src/http/intent_api.rs) `aware_users_seed` / `pm_task_initiator`、[delivery.rs](../../../core/acowork-gateway/src/intent/delivery.rs)、[dispatch.rs](../../../core/acowork-pm/src/dispatch.rs) |
| M3 Runtime stamp + ToolContext.initiator | 完成 | [conversation.rs](../../../core/acowork-runtime/src/conversation.rs) `set_aware_users`、[session_manager.rs](../../../core/acowork-runtime/src/agent/session/session_manager.rs)、[intent_send.rs](../../../core/acowork-runtime/src/tools/builtin/intent_send.rs) |
| M4 `is_readable_by` aware 放行（只读） | 完成 | [session_meta.rs](../../../core/acowork-core/src/session_meta.rs) `is_readable_by`；`is_writable_by` 刻意不查 |
| M5 不变量回归测试 | 完成 | core `aware_users_tests`(5) + gateway `aware_users_seed_tests`(6) + runtime `intent_session_stamps_aware_users_on_meta` + sqlite shape |

### 附带修复 1：`chat_sessions_e2e` 既有失败（非本任务引入）

根因：commit `35a843eb` 新增 ADR-106 D15「坐标必须可解析」的存储否决，晚于该测试最后更新，且未同步 Gateway 写路径。3 个用例失败：
- `card_answer_for_unknown_card_falls_through_as_plain_turn`：Gateway `deliver_chat_message` 仍把无法解析的 `in_reply_to` 写进行 → 存储 422 → 整条 5xx，与 §5.5.4「人类消息不被丢弃」矛盾。**修复**：Gateway 只在坐标解析成功（`replied.is_some()`）时记录 `in_reply_to`，否则落为 origin 行（D15 恒接受）。`deliver_mentions` 同此协调。
- `agent_agent_pair` / `default_pair_doorbell`：测试 setup 写了引用不存在 origin 行的回复。**修复**：先写 origin 行（agent 用 `meta.task_id` 的 assignment 形态开线程，human 用 user_message origin）。

### 附带修复 2：问题 3「群聊执行卡片不实时」

根因：终态 round 记录经 `end_round` → `enqueue_round_snapshot` 异步投递给 writer 落 side table；doorbell 经 chunk 通道（另一条路径）紧随其后发出。两者无序保证，doorbell 可先于 side table 落盘到达前端 → reload 读到旧「running」→ 卡片卡住直到 remount 或 15s 轮询。`publish_round_state` 的注释本已声明「Chat 不得在 turn 最终事件交给 publisher 之前被要求重读」，但该保证只覆盖了 chunk 通道，没覆盖 side table 的持久化路径。

选定方案（消除竞态，非缩短空窗）：在 doorbell 的唯一收口 `publish_round_state` 内、发 bell 之前做一次同步 writer 屏障（新增 `WriterCommand::SyncBarrier` + `ConversationSession::flush_now`，std sync_channel，任意上下文可阻塞，仿 `announce_round`）。FIFO 单消费者保证屏障 ack 时先前 `RoundSnapshot` 的 `append_record` 已落盘。放在收口而非各调用点，使「通知不先于事实」成为代码属性而非需记忆的规则，统一覆盖终态关闭 / 卡片打开 / 卡片解决三条 doorbell 路径。

回归测试：`closing_a_turn_makes_the_terminal_record_durable_before_the_doorbell`——bell 发出后用 `read_record` 直接读（不轮询）断言终态已 durable，正是区分「已修」与「竞态」的判据。

### 评审轮修正（2026-11-07）

提交前 review 发现两处，已修；两处「知悉即可」项按决定只补注释/文档、不改行为：

1. **响应路径 seed 缺口（不变量 2 的真实漏洞，已修）**：M2 初版只在 `deliver_intent`（请求路径）seed；`deliver_response` 与 sweeper 的 fallback receipt 都传空列表。当 response / 超时回执到达时 initiator 的原 session 已关闭，`route_intent_session` 会以 `AgentResponse` origin 新开 session——seed 为空即黑盒，违反 §3 不变量 2。**修复**：`aware_users_seed` 提为 `pub(crate)`，`deliver_response` 与 `emit_fallback_receipt` 以 (responder, initiator) 两侧 ACL 补 seed（originator 不穿线：mailbox 未存，auth 模式下 ACL 已保证非空）。Runtime 侧 stamp 仍是 creation-only，对复用既有 session 的常态路径零影响。
2. **ADR-076 引用未落地（已补）**：代码约 20 处注释引用「ADR-076 §决策 4 (rev.)」而 ADR 本体无该修订。已在 [ADR-076](../../adr/zh/ADR-076-multi-user-account-system.md) 增设「§决策 4 v3：aware_users——agent↔agent 会话的只读知情权」（zh+en 同批）。
3. **`on_behalf_of` 信任假设（知悉，不改）**：Gateway 不向账号目录校验声明的 originator uid，恶意 agent 理论上可往**其新建**的 session 只读名单塞任意人类。影响面单向且仅只读（不能收窄、不能写、不影响既有 session，名单为 creation-time write-once），且 ACL seed 实际已覆盖绝大多数名单来源；user-service 往返会把新依赖压进投递热路径，刻意不做。已在 `deliver_intent` originator 解析处注释声明。
4. **local 模式 fail-open（知悉，不改）**：§8 原写「显式 fail-open 到唯一本地账号」，实现未做——local 模式 scope 解析为 `Unfiltered`，唯一本地账号本就全可读，不变量经 scope 层而非 seed 层满足。已在 `aware_users_seed` doc 注释与 §8 风险表修正表述。

### 验证

workspace build 绿；`clippy --all-targets -D warnings`（core/sqlite/memory/gateway/runtime/pm）绿；core 382 / gateway lib 870 + `chat_sessions_e2e` 14 + `intent_delivery` 7 / memory 208 / pm 130 / sqlite 9 / runtime lib 1931 + round_projection/session_* e2e 全过。唯一失败 `mqtt_e2e_full::integration_multiple_messages` 经隔离复跑证实为既有环境性 flake（纯 MQTT 控制命令搬运，不经任何 turn/round 路径，与本任务改动无关）。
