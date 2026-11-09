# agent↔agent 执行会话的人类介入与恢复通道实现计划

> 版本：v1.0（待人类确认） | 日期：2026-11-09
>
> **一句话**：agent A 发起 intent 让 agent B 干活时创建的执行 session，`initiator` 是 A（agent UUID）⇒ 按 [`SessionMeta::is_writable_by`](../../../core/acowork-core/src/session_meta.rs#L905) 只有 A 能写，人类既发不了消息也改不了 `model`；一旦开局参数不可用（选了一个额度已尽的 model / 凭据失效），这一轮以 `failed` 收口、session 停在 Idle，**没有任何入口能把这次运行救回来**（A 已经收到 failed 回执，不会重发；B 的会话里也没有第二个人）。本计划把"人类介入"做成一等能力：**能力分层授权 + 代发言溯源 + 真人入局 + model 自动降级**四条轴，拆成 8 个可独立交付/测试的模块。
>
> 前置调查已完成，证据均带 file:line（见 §1）。§7 列出**必须由人类拍板的开放问题**，未确认前不动手。

---

## 1. 缺陷与事实（调查结论）

> 所有 file:line 均按 `HEAD = f5d93d84` 逐条核过。工作区当时正被另一条会话改动（17:26 有一次整树重写），**动手前请重新核对行号**，语义以符号名为准。

### 1.1 写权限只有一个人：`initiator == uid`

- 统一写入口 [`post_send_message`](../../../core/acowork-runtime/src/http/session_control.rs#L852) 第一步就是 [`authorize_write`](../../../core/acowork-runtime/src/http/session_control.rs#L207) → [`SessionMeta::is_writable_by`](../../../core/acowork-core/src/session_meta.rs#L905)，`Some(initiator)` 分支只有 `initiator == uid` 一条路；注释明确"`Members` 从不转移所有权"、admin 也不例外。
- `PUT /sessions/{sid}/config`（[server.rs:1437](../../../core/acowork-runtime/src/http/server.rs#L1437)）同样先 `authorize_write`，所以 **model / provider / reasoning_effort 人类也改不了**。
- agent↔agent 的 `initiator` 是 agent UUID：[`route_intent_session(..., Some(env.from))`](../../../core/acowork-runtime/src/agent/session/session_manager.rs#L596) → 人类 uid 永远不等于它。
- 只读侧上一轮已经放开：`aware_users`（[session_meta.rs:729](../../../core/acowork-core/src/session_meta.rs#L729)）只进 `is_readable_by`（[L808](../../../core/acowork-core/src/session_meta.rs#L808)），注释写死了"`is_writable_by` 刻意不查它"。见 [intent-agent-to-agent-human-visibility-plan.md](./intent-agent-to-agent-human-visibility-plan.md)。

**结论**：不是权限算错，是**能力维度缺失**——"能不能读"与"能不能以什么身份写"被压成了一个 bool。

### 1.2 失败后的真实状态：turn 收口，session 停在 Idle，没有恢复入口

- 非可重试错误（`is_retryable()==false`，含 402 / `insufficient_quota`）在 [`decide_retry_action`](../../../core/acowork-runtime/src/agent/retry.rs#L83) 返回 `GiveUp`（[L90](../../../core/acowork-runtime/src/agent/retry.rs#L90)）→ `transition_status(Idle)` + `Err`。
- `session_task` 的 Err 分支照常给 A 回 `failed` 回执（[session_task.rs:1845](../../../core/acowork-runtime/src/agent/session/session_task.rs#L1845)），于是 **A 的请求被"答完了"**，A 不会再试；B 这边只剩一条失败 turn，人看不到、也进不去。
- 额度/余额的判定函数已经存在（[`is_balance_exhausted`](../../../core/acowork-core/src/providers/error_patterns.rs#L162)），但**只在错误发生后**可用：`ProviderListItem`（[protocol.rs:233](../../../core/acowork-core/src/protocol.rs#L233)）里没有任何额度字段，provider 侧没有"这个 model 还有没有钱"的查询面。真正能定位钱的是 `(provider, account_id)`——[provider-multi-key-plan.md](./provider-multi-key-plan.md) 与 [`get_provider_account_api_key`](../../../core/acowork-runtime/src/agent/agent_core.rs#L1239) 证明 vault 是 `provider → [account]` 的多键结构，`SessionMeta.account_id` 就是"这笔账记在哪个 key 上"。
- 开局只做"模型存在性"裁剪：intent 的 `session_config` 经 [`clamp_to_responder`](../../../core/acowork-runtime/src/agent/session/intent_routing.rs#L127) 过滤到 responder 的 provider 清单内；`apply_config`（[session_config_impl.rs:88](../../../core/acowork-runtime/src/usecases/session_config_impl.rs#L88)）**只校验 workspace 存在性，完全不校验 model/provider**——人类随手填一个不存在的 model 也会被接受，然后在第一次调用时爆。

### 1.3 代发言在链路里没有任何坐标

- 执行会话的 `user` 行是唯一写入点：[`append_message_with_id("user", …, user_row_meta, message_id)`](../../../core/acowork-runtime/src/agent/loop_.rs#L1257)，`user_row_meta` 来自 [`turn_user_row_meta`](../../../core/acowork-runtime/src/agent/loop_.rs#L545)，目前只有 agent-origin intent 的展示投影（`metadata.intent`）用它。
- 那枚键的现状正好是本模块要照抄的形状：键常量 + 结构 + **唯一 reader** 全部收在生产者一侧——[`META_KEY_INTENT_DISPLAY = "intent"`](../../../core/acowork-runtime/src/agent/intent_turn.rs#L698)、[`IntentDisplay`](../../../core/acowork-runtime/src/agent/intent_turn.rs#L494)、[`IntentDisplay::from_metadata`](../../../core/acowork-runtime/src/agent/intent_turn.rs#L725)（注释写明"**唯一** reader，也是唯一判空处"）。
- chat 侧的块投影从这一行派生：[`RoundUserRow.display`](../../../core/acowork-runtime/src/rounds.rs#L672) 直接复用生产者的 `IntentDisplay`（不再是投影侧的一份子集类型），由 [`project()`](../../../core/acowork-runtime/src/rounds.rs#L698) 填好交给两个前端。
- 前端"执行界面右侧是谁"由 [`speakerBubble.ts`](../../../apps/acowork-desktop/src/components/chat/speakerBubble.ts) 按 `initiator` 解析出的 kind 决定（[`ChatPanel.tsx:667`](../../../apps/acowork-desktop/src/components/chat/ChatPanel.tsx#L667)）。**人类一旦能写，这一行会被无区别地画成 A 的话**——事实必须随行落库，否则 UI 只能撒谎。

### 1.4 人类不可能被"提问"：`has_human` 与会话参与者

- `has_human` 由 Gateway 从 Chat 参与者推出：[`chat_client::has_human_participant`](../../../core/acowork-gateway/src/intent/chat_client.rs#L95)（`identity == "human"` 才算），随 [`chat_grant`](../../../core/acowork-gateway/src/http/intent_api.rs#L1106) 上 wire，落到 [`TurnSurface.has_human`](../../../core/acowork-runtime/src/agent/intent_turn.rs#L74)。
- agent↔agent 的 pair 会话参与者只有两个 agent ⇒ `Some(false)` ⇒ 迭代上限 / 环检测直接 `aborted` 而不 park（[loop_.rs:1524](../../../core/acowork-runtime/src/agent/loop_.rs#L1524)、[L1724](../../../core/acowork-runtime/src/agent/loop_.rs#L1724)）；`ask_user_question` 的卡片即使被记录也**没有可回答的人**（`answerable_by` 在 [`TurnSurface::new`](../../../core/acowork-runtime/src/agent/intent_turn.rs#L100) 里取 `env.from`，即 agent A）。
- 唯一的"加入人类"通道被封在路由层：[`add_participants_route`](../../../core/acowork-user/src/http/chat_sessions_api.rs#L762) 只允许 `user_group`，`agent_agent` / `user_agent` 的第三人一律 422；而 store 层 [`add_participant`](../../../core/acowork-user/src/chat_sessions.rs#L3207) 本身没有这个限制。
- 另有一处既存不一致：`POST /sessions/{sid}/answer` 只要 [`authorize_read`](../../../core/acowork-runtime/src/http/session_control.rs#L1074) 就能答卡，比其它写动作弱一档——今天因为 turn 从不 park 所以打不到，一旦人类可介入就会变成"能答题的人 ≠ 被问的人"。

### 1.5 卡在"等待重试"的人叫不醒

[`wait_for_retry_or_user`](../../../core/acowork-runtime/src/agent/loop_.rs#L777) 只 select `ContinueExecution` / `Stop` / 定时器；新到的 `ChatMessage` 被塞进 `deferred_inbound`（[L808](../../../core/acowork-runtime/src/agent/loop_.rs#L808)），要等定时器（指数退避到 30 分钟）或人点"立即重试"才动。人类想插话修配置，得先知道要去点那个按钮。

---

## 2. 设计决策

### D1 — 权限是一张"权威层级 × 配置字段"策略表，不是一个 bool

在 `acowork-core` 新增单一推导点（**唯一**计算权限的地方，HTTP 层与前端都不再推导）：

```rust
pub enum SessionAuthority { Owner, Intervener, Reader, Admin, None }

pub enum ConfigField {  // PUT /sessions/{sid}/config 的每个字段都在这张表里有一行
    Model, Provider, ReasoningEffort, Temperature, ContextWindow,
    Workspace, ToolPolicy, Title, Visibility, Delete,
}
```

- `SessionAuthority::of(meta, scope)`：`initiator == uid` → `Owner`；`uid ∈ interveners` → `Intervener`；chat 参与者（`scope.chats()` 命中 `meta.chat_session_id`，且 `visibility == Members`）→ `Intervener`；`uid ∈ aware_users` 或公开会话 → `Reader`；admin/unfiltered 保持既有特例。
- `ConfigField::min_authority()`：`Model | Provider | ReasoningEffort | Temperature | ContextWindow` → `Intervener`（介入层，本轮放开）；`Workspace | ToolPolicy | Visibility | Delete | Title` → `Owner`（会破坏任务原始假设，本轮不动）。
- `is_writable_by` 保留为 `authority.may_turn()` 的薄封装（等价于今天 `Owner`），`SessionInfo.can_write` 继续下发，老前端零改动。
- **理由**：ADR-102 已经确立了"新增来源 = 加枚举变体 + 策略行，编译器强制所有决策点表态"的范式（[ADR-102](../../adr/zh/ADR-102-turnorigin-vocabulary-and-pm-dispatch-chain.md) D1）。权限这个维度比 turn origin 更容易长出临时补丁（今天三个 tier 已经在 UI 里被压成了一个 `readOnly` 布尔），策略表是唯一能挡住"下一个新字段忘了校验"的形状。

### D2 — `interveners` 与 `aware_users` 分开存，即使今天同一个种子

新增 `SessionMeta.interveners: Vec<String>`（SQLite 加列，`SCHEMA_VERSION` 13 → 14，仿 [schema.rs](../../../core/acowork-memory/src/sqlite/schema.rs#L325) 的 v12 步）；写入方只有一处：Gateway 的 [`aware_users_seed`](../../../core/acowork-gateway/src/http/intent_api.rs#L1129) 同一次计算产出两个名单，随 wire 下发、Runtime 在建会话时 stamp（写一次，与 `set_aware_users` 同形）。

**为什么不合并成一个名单**：读与写是两个正交能力，合并就是 ADR-076 修订时踩过的同一个坑（`initiator` 曾经同时承担"谁发起"和"谁是人类归属"）。今天 agent↔agent 没有人类隐私主体、两个名单必然相同；将来 PM 任务会话、跨组织共享 agent 一定需要"能看不能写"。数据先分开，语义后收敛。

### D3 — 代发言是"行上的一等事实"，不是 UI 的推断

`acowork-core` 新增一枚结构化标记 + 一枚元数据键，形状照抄 `IntentDisplay` 的既有范式（键常量 + 结构 + 唯一 reader，全部归生产者所有）：

```rust
// core/acowork-core/src/turn_speaker.rs
pub const META_KEY_TURN_SPEAKER: &str = "speaker";

pub struct TurnSpeaker {
    pub origin: SpeakerOrigin,        // Human | Agent
    pub author: String,               // 真人 uid（origin=Human 时）
    pub on_behalf_of: Option<String>, // 被代言的 agent instance id（mode 1 才 Some）
}
impl TurnSpeaker {
    /// 唯一的读取/判空点（对照 `IntentDisplay::from_metadata`）
    pub fn from_metadata(metadata: &serde_json::Value) -> Option<Self>;
}
```

派生链（**每跳只有一个派生点，禁止旁路推断**）：

```
HTTP 写入口（caller uid 已由 Gateway 校验）
  → SessionMessage::ChatMessage{ speaker }              // runtime 侧新增字段
  → turn_user_row_meta（loop_.rs:1257 的唯一写入点）     // metadata.speaker
  → RoundUserRow.speaker（rounds.rs project()，紧邻 display）
  → Desktop 执行界面气泡徽标 + Chat 侧执行块徽标
```

**语音（voice）规则**：`on_behalf_of = Some(initiator)` 当且仅当写者不是该会话 chat 的参与者（mode 1，代 A 发言）；写者是 chat 参与者（mode 3，真人入局）时 `on_behalf_of = None`，行属于他自己。**语音由"是否入局"决定，不由权限决定**——一个入局的真人不需要被代言，硬代他反而抹掉了他的在场。

### D4 — 两种模式共用一套权限，差别只在"能不能被提问"

| | mode 1 代发言 | mode 3 真人入局 |
|---|---|---|
| 谁能写 | `interveners`（`aware_users` 同批） | chat 参与者（`Members`）+ `interveners` |
| 行的作者 | A（`on_behalf_of` = initiator） | 真人自己 |
| `has_human` | 不变（`Some(false)`） | `Some(true)`（由既有 `has_human_participant` 自动得出） |
| B 的提问 | 不能 park，卡片仍不可答 | 可 park，卡片 `answerable_by` = 真人 |
| 系统块 | 不宣称"有真人在旁" | 宣称"有一位真人在旁，可直接向他提问" |

**系统块文本必须按模式分叉**：给 B 一句"有真人在旁"而 `has_human` 仍是 `Some(false)`，会让它反复提问却永远等不到答案——这正是 ADR-106 §2.2 那条可用性门禁存在的理由。

### D5 — 人类介入的 turn 不重开 intent 义务；"重试"是 `replay`，不是重发

- 人类写的 turn 走 `pending_intent == None` 分支，因此**不会**给 A 补发第二条 response（`response_id` 是 `resp-{request_id}` 的确定性值，见 [intent_api.rs:1625](../../../core/acowork-gateway/src/http/intent_api.rs#L1625)，第二条本来也无法成为新行）。A 侧收到的 `failed` 就是这次协作的终态；要让 A 知道后续结果，应当是 A 的新 intent（或人在 A 的 chat 里说），不是伪造第二条应答。
- 重试复用既有的 [`AgentLoop::replay`](../../../core/acowork-runtime/src/agent/loop_.rs#L1089)（debug resume 已在用，天然不重复写 `user` 行）；内容取转录最后一条 `user` 行，`message_id` 沿用原值 ⇒ 轮次坐标不变、块不重复。
- 拒绝的备选：客户端重发同一条消息（产生重复 `user` 行，模型会看到两遍）。这属于"打补丁"，不进方案。

### D6 — 自动降级 = 负缓存 + 一次性重试，不假装能做"额度查询"

真实可用的额度事实只有"刚才那次调用失败了"（402 / `insufficient_quota` / 401）。所以：

1. **负缓存** `ModelHealthRegistry`：`(provider, account_id, model) → Fatal{kind: Auth | Quota | ModelGone, at}`，TTL 有效，只在进程内（重启即清空——额度随时可能充值，持久化的毒化比漏判更危险）。
2. **单一决策点** `resolve_session_model(meta, health, providers) → Chosen | Fallback{reason}`：在会话激活时（[session_manager.rs:1496](../../../core/acowork-runtime/src/agent/session/session_manager.rs#L1496) 附近解析 `initial_account` 的同一处）与 `apply_config` 改 model 时调用。约束不变：候选必须在 responder 自己的允许集内（复用 `clamp_to_responder`）。
3. **降级阶梯**：同 provider 的另一个 account（同 model，换 key）→ agent 默认 model（可能是别的 provider，需 O5 确认）→ `Failed` + 可见通知。
4. **首次调用失败时的一次性重试**：`GiveUp` 且 `is_balance_exhausted`/auth → 记毒 → 若能降级则切配置并重放本轮（每轮至多 N 次）→ 本轮完成而不是死掉。
5. **记录**：降级写一条 ALREADY-EXISTS 的词表 notice `budget_exhausted`（[intent.rs:291](../../../core/acowork-core/src/intent.rs#L291) 的 `LIMIT_NOTICES`）进转录 + round 投影，并把真正生效的 `model / provider / account_id` 落回 `SessionMeta`（三个列都已存在）。

**诚实边界**：这是"开局尽力而为 + 出事必自愈"，不是"开局保证有钱"。任何声称能在调用前判定额度的方案都要先有 provider 额度 API，本项目没有。

---

## 3. 模块分解（每个模块独立可 review / 可测 / 可单独 revert）

| # | 模块 | 落点 | 依赖 | 交付价值 |
|---|---|---|---|---|
| M1 | `SessionAuthority` + `ConfigField` 策略表 | `acowork-core/src/session_meta.rs` | — | 纯函数，`is_writable_by` 语义不变，零行为变化 |
| M2 | `SessionMeta.interveners` + SQLite v14 + wire | `acowork-core`、`acowork-sqlite`、`acowork-memory/sqlite/schema.rs` | M1 | 字段恒空 = 现状 |
| M3 | Gateway 种子 + Runtime stamp | `intent_api.rs`（`aware_users_seed` 同址）、`session_manager.rs`、`conversation.rs`（`set_interveners`） | M2 | 名单开始有值 |
| M4 | 写入口按能力分档 | `session_control.rs`（messages / config / stop / continue / approve / answer）、`server.rs`、`SessionInfo` 能力集合 | M1 | **人类开始能发消息、改 model** |
| M5 | `TurnSpeaker` 溯源：core 类型 + metadata 键 + 唯一 reader + `RoundUserRow.speaker` + 两前端徽标 | `acowork-core`（新 `turn_speaker.rs`）、`session_task.rs`、`loop_.rs`、`rounds.rs`、`speakerBubble.ts` 旁新增 `speakerProvenance.ts`、执行界面/Chat 块 | M4 | 溯源可区分 |
| M6 | 干预 turn 语义：抢占重试等待 + `POST /sessions/{sid}/retry` + 失败必须留 notice | `loop_.rs`（`wait_for_retry_or_user`）、`session_control.rs`、`session_task.rs` | M4 | **恢复入口闭环** |
| M7 | `ModelHealthRegistry` + `resolve_session_model` + 一次性降级重试 | 新 `agent/model_health.rs`、`retry.rs` 观察点、`session_manager.rs`、`session_config_impl.rs`、`llm_availability.rs`（不改语义） | — | 不硬卡死 |
| M8 | mode 3 真人入局：加入通道 + park 放行 + 卡片作答对齐 + 分模式系统块 | `chat_sessions_api.rs`（新路由）、`loop_.rs:1524/1724`、`session_control.rs:answer`、`ContextBuilder` | M4、M5 | 第三种模式 |

### 3.1 关键实现要点（逐模块）

**M1**：`SessionAuthority::of` 与 `ConfigField::min_authority` 都是纯函数，配真值表测试（5 种 authority × 10 个字段）。`is_writable_by` 改成一行转发，保持 ADR-076 原文语义，老测试不改。

**M2**：`aware_users` 的 `#[serde(default)]` + NULL 读空列表是模板（[acowork-sqlite/session_meta.rs:92](../../../core/acowork-sqlite/src/session_meta.rs#L92)）；`SCHEMA_VERSION 13 → 14`，migration 步长仿 v12（[schema.rs:597](../../../core/acowork-memory/src/sqlite/schema.rs#L597) 的幂等判断）。wire 上加 `IntentEnvelope` 字段（proto 追加，旧发送方缺省 = 空）。

**M3**：`aware_users_seed` 改名/扩为 `human_audience_seed() -> (aware_users, interveners)`，**同一处**计算；两条路径都要写（请求 `deliver_intent` 与 `deliver_response` 各自已有一次调用）。Runtime 侧 `ConversationSession::set_interveners` 写一次（`set_aware_users` 的孪生）。

**M4**：`put_session_config` 改成"逐字段校验 + 一次回报"：先由 `authority` 过滤出被拒字段集合，非空 → 403 `{refused: ["workspace"], allowed: ["model"]}`，**不静默丢弃**（静默丢弃正是今天 UI 半灰半亮的成因）。同时在 `apply_config` 加 model/provider 存在性校验（复用 `global_provider_list`），把"人类填错 model"变成 400 而不是一次失败的 turn。`SessionInfo` 增 `authority` + `config_fields: [允许的字段]`，前端 [`session-write-access.ts`](../../../apps/acowork-desktop/src/lib/session-write-access.ts) 从"一个 bool"改为"能力集合 hook"，`ChatPanel` 的 model picker（[L3231](../../../apps/acowork-desktop/src/components/chat/ChatPanel.tsx#L3231)）/reasoning（[L3577](../../../apps/acowork-desktop/src/components/chat/ChatPanel.tsx#L3577)）/workspace selector 各自按自己那个字段开关——保持"控件自己持门禁"这条既有约定。

**M5**：`TurnSpeaker` 只在 `session_task` 的 `ChatMessage` 分支赋值（与 `turn_surface` 同一处），`loop_.rs:1257` 写行不变；`core` 的新模块提供 `META_KEY_TURN_SPEAKER` + `from_metadata`（唯一 reader，判空规则与 `IntentDisplay::from_metadata`（[L725](../../../core/acowork-runtime/src/agent/intent_turn.rs#L725)）同构）；`RoundUserRow` 增 `speaker: Option<TurnSpeaker>`，由 `project()`（[rounds.rs:698](../../../core/acowork-runtime/src/rounds.rs#L698)）填。前端：气泡仍锚在 `initiator` 侧（A 的头像/名字），新增一行小徽标"baobei 代 SSE.Kevin 发言"，`speakerBubble` 的 markdown/皮肤规则**不变**（代发言的内容是人类输入的，按 [`speakerBubble.ts`](../../../apps/acowork-desktop/src/components/chat/speakerBubble.ts) 的既有语法应保持字面渲染；这条要在测试里钉死，否则会出现"人话被当 markdown 解析"的老缺陷）。

**M6**：`wait_for_retry_or_user` 的 `Some(other)` 分支对 `ChatMessage` 改为"记录 → 作为 `Stop` 处理（本轮以 `failed` 收口）→ 该消息留在 `deferred_inbound` 下一轮执行"。`/retry` 端点：`authorize` 用 `Intervener` 级以上；取转录最后一条 `user` 行 → `replay`；**明确不重建 intent 义务**（D5）。失败可见性：`Err` 分支在有 `Intervener` 的会话里追加 `system_notice`（`notice_type` 取 `LIMIT_NOTICES` 已有词，不新增词汇）。

**M7**：观察点放在 `decide_retry_action` 返回 `GiveUp` 且 `is_balance_exhausted` / auth 错误处（`reliable.rs` 已有 402 分类，[L555](../../../core/acowork-runtime/src/providers/reliable.rs#L555)）；成功调用处清除。`resolve_session_model` 是纯函数，输入 `(meta, health, providers)`，输出决策 + 原因，可单测穷举。降级生效必须走既有 `ConversationSession::apply_config`（单写者），不另开写路径。**注意 `BudgetGuard`（[budget_guard.rs:9](../../../core/acowork-runtime/src/agent/budget_guard.rs#L9)）是本地 token 预算预估，与本模块无关，不要混用。**

**M8**：新路由把人类以 `identity = human` 加入 `agent_agent` chat（store 层已允许，只在路由层放行，且要求调用者本身就是该 agent 的 owner/guest）；随后 `has_human_participant()` 自动变 `Some(true)`，`chat_grant` 与 `TurnSurface` 无需改动。`loop_.rs:1524/1724` 的 `pending_intent.is_some() || has_human == Some(false)` 改为查 `TurnSurface` 的"可 park"判据（新方法 `may_park_on_human()`：`has_human == Some(true) && answerable_by 是一个人类 uid`），让 responder turn 在有真人时也能 park。卡片 `answerable_by`：`TurnSurface::new` 在 chat 有真人时取该真人（需 O2 拍板）；`answer` 端点同步从 `authorize_read` 提到同一 authority 并校验"你就是被问的人"。

---

## 4. 批次与顺序

| 批次 | 内容 | 依赖 | 风险 |
|---|---|---|---|
| 1 | M1 + M2（策略表 + 恒空字段） | — | 极低：纯追加，零行为变化，可先合 |
| 2 | M3 + M4（种子 + 分档校验）**成对** | 1 | 中：写权限面放宽，必须同批测泄露不变量 |
| 3 | M5（溯源） | 2 | 低：只加字段与徽标 |
| 4 | M6（干预 turn + retry） | 2 | 中：动 `wait_for_retry_or_user` 状态机 |
| 5 | M7（自动降级） | — | 中：独立于权限链，可并行 |
| 6 | M8（真人入局） | 2、3 | 高：同时改 park 判据与卡片作答 |

M7 与 1–2 完全解耦，可以先行/并行；M8 最后做，且必须等 O2 有结论。

---

## 5. 验收不变量（回归测试清单）

1. **不泄露**：共享 agent 下，A 的 user↔agent 私聊 session，B 既不可读也不可写（`authority == None`）。
2. **只读仍在**：`aware_users` 命中但不在 `interveners` 的账号 → `Reader`，`PUT config` 403 且列出被拒字段。
3. **介入层边界**：`Intervener` 改 `workspace` / `tool_policy` / `visibility` → 403；改 `model` / `provider` / `reasoning_effort` → 200 且 `SessionMeta` 真被改。
4. **代发言溯源**：mode 1 写入的行 `metadata.speaker = {origin:human, author, on_behalf_of:initiator}`，`RoundProjection.user.display.speaker` 与之一致；前端渲染仍是字面文本 + 徽标。
5. **不伪造第二应答**：人类干预 turn 结束后，Gateway 上针对原 `request_id` 的 response 行数仍为 1。
6. **retry 不重复**：`/retry` 后转录中 `user` 行数量不变，`round_id` 不变，`answer` 位置前移。
7. **降级可观测**：把 `(provider, account, model)` 标记为 Quota 后新建会话 → 生效 model 变更 + 一条 `budget_exhausted` notice + `SessionMeta.model/provider/account_id` 落新值。
8. **降级有底**：允许集内无候选 → turn `failed` 但会话留下 notice，人类可改 model 后 `/retry` 成功。
9. **mode 3 park**：真人入局后，responder 撞迭代上限 → `Paused` + 卡片 `answerable_by` = 该真人，真人作答后继续（不再是 `aborted`）。
10. **模式隔离**：mode 1（未入局）时 responder 撞上限仍 `aborted`，且**不**注入"有真人在旁"系统块。
11. **向后兼容**：无 `interveners` 的旧 `SessionMeta` 反序列化为空、权限不变；旧 envelope 无新 wire 字段仍可投递。

---

## 6. 风险与回滚

| 风险 | 影响 | 缓解 |
|---|---|---|
| 写权限面放宽引入越权 | 高 | M4 与 M3 成对交付；不变量 1/2/3 进 CI；`interveners` 只在 agent↔agent 种子，PM/人类起源保持空 |
| `SessionMeta` 加字段波及结构体字面量/SQLite | 编译期可见 | `#[serde(default)]` + migration 幂等判断（v12 的先例） |
| `metadata.speaker` 成为第二个"非注册键" | 中 | 只经 core 的 `TurnSpeaker` 序列化，禁止字符串拼装；单一 `META_KEY` 常量 |
| 抢占重试等待改变 turn 终态 | 中 | 只对 `ChatMessage` 生效（该路径今天只能来自校验过的人类）；`failed` 回执语义与现状一致 |
| 自动降级悄悄换模型，成本/质量漂移 | 中 | 每次降级留 notice + 落 `account_id`；阶梯限定在 agent 允许集内；见 O5 |
| 真人入局改变 `has_human` 影响既有 PM 链路 | 中 | 仅新增路由能加真人，PM 的 ensure/sync 不碰 `agent_agent` |

**回滚粒度**：M1–M8 各自独立提交、可单独 revert；批次 1 合入后对现网零行为影响。

---

## 7. 待人类确认的开放问题（未确认不动手）

- **O1｜代发言之后 A 要不要知道结果。** 建议"不"：原 `request_id` 的应答已是 `failed`，人类介入是新的事实，A 的知情应由 A 的新 intent（或人在 A 的 chat 里）完成。若要求"自动告知 A"，需要新开一条 `agent_message` 通道并定义幂等身份，规模翻倍。
- **O2｜B 的卡片该问谁。** 已按人类 2026-xx 的更正收敛：**不引入 mode 3，不做群聊**。执行会话里的人就是"以发起方 agent 的名义发言"，所以卡片要问的是**介入名单里的人**，不是把真人加成 participant。落法是让 `answerable_by` 指名单中的人（见 §8 批次 7），不做参与者模型改造。
- **O3｜介入名单的范围。** 建议：agent↔agent 用与 `aware_users` 完全相同的种子（无人类隐私主体）；PM 任务会话（`initiator` = MCP 建任务的 agent）**本轮不开放**，留待单独评审（project owner 的"可看可写"另有语义）。
- **O4｜介入者能不能把 model 设成 agent 允许集之外的。** 建议"不能"，仍走 `clamp_to_responder`——否则人类可以把 B 换成它 manifest 里根本没声明的模型，任务原始假设被静默破坏。
- **O5｜自动降级是否允许跨 provider。** 建议阶梯止于"同 provider 换 account"→"agent 默认 model（可能跨 provider）"，但跨 provider 降级必须留 notice 且**不**写回 agent 级默认配置（只影响本会话）。若要求"永不跨 provider"，极端情况下只能 `Failed` + 等人工。

---

## 8. 实施记录

### 批次 1 — 策略表 + 恒空字段（已落，零行为变化）

| 落点 | 内容 |
|---|---|
| `core/acowork-core/src/session_meta.rs` | `SessionMeta.interveners`（`serde(default)`）；`SessionAuthority{ReadOnly,Intervener,Owner}` + `may_turn()` / `may(ConfigField)`；`ConfigField` 10 字段真值表；`SessionCapabilities`；`is_writable_by` 改写为 `authority_for(..).may_turn()` |
| `core/acowork-sqlite/src/session_meta.rs` | 第 38 列 `interveners`（JSON）进 `SELECT_ALL_COLUMNS` / upsert，`row_to_meta` 读空列表 |
| `core/acowork-memory/src/sqlite/schema.rs` | `SCHEMA_VERSION 13 → 14`、`MIGRATIONS` 追加、`TARGET_SCHEMA` 追加列、`apply_migrations` 加 `has_column` 守卫（幂等） |

批 1 留下两处**产品后果**，随批 2 一起生效：

* `is_readable_by` 追加 `is_intervener(uid)` 分支。原计划"读轴不动"，实际必须动：介入者要改 model，就得先打开这个 session 的表面，"能写不能读"是一个用不上的授权。两名单今天同种子，所以在生产里这条是冗余的——但"隐式冗余"恰恰最危险，写成显式分支后，将来两条轴各自收窄时不会互相带偏。
* `ConfigField::Account`：降级阶梯第一级是"同 provider 换 account"，它必须属于 `Intervener` 档，否则"换把钥匙"要 owner 才做得动，而它恰是最常见的一级。

字段名的 wire 一致性已被测试钉死（`every_field_spells_the_same_name_on_the_wire_and_in_a_refusal`）：本轮它当场抓到 `Account` 序列化成 `account`、而请求体字段是 `account_id`，`Workspace` 同病（`workspace` vs `workspace_id`），两者都补了显式 `#[serde(rename)]`。这类不一致的后果正是本方案要消灭的那一类失败——界面点亮一个后端看不见的键。

### 批次 2 — 种子 + 分档校验 + 不卡死（已完成，待评审）

| 模块 | 落点 | 内容 |
|---|---|---|
| M3 | `session_manager.rs::create_intent_session` | 在 `aware_users` 种子同一处再 `set_interveners(...)`；**未加 wire 字段**（决策修正 1） |
| M3 | `conversation.rs` | `interveners` 镜像槽 + `set_interveners`（write-once，与 `set_aware_users` 同契约）+ `interveners()`；`new` / `from meta` / `to_meta` / `Clone` 四处同步 |
| M4 | `server.rs::put_session_config` | `authorize_write` 之后按 `present_fields()` 逐字段过表；任一超档 → **403 + `{field, refused[], allowed[]}`**，不静默丢弃 |
| M4 | `session_control.rs` | 新增 `require_field(meta, scope, field)`（写入口与字段表的唯一粘合点）；`delete_session` 过 `Delete` 档；`put_session_visibility` 过 `Visibility` 档（**先于** `may_change_visibility`） |
| M4 | `conversation.rs` / `usecases/session_metadata*.rs` | `SessionInfo` / `SessionSummary` 增 `authority` + `config_fields`；`can_write` 改为 `authority.may_turn()` |
| M6（部分） | `session_task.rs` 失败分支 | turn 失败落一条 `role: "system"` 的**持久 notice**（`metadata.notice = "turn_failed"`）。此前失败只有 `ChunkEvent::Error`：没有客户端在线的那个瞬间过去，转录里一个字都不留，人打开看到的是一页正常的 Idle 记录 |
| M8（部分） | `loop_.rs`（两处 park 判据）+ `loop_interaction.rs` | `human_can_reach_pause()`：有介入名单的 session，responder 撞迭代上限 / 死循环时**停车等待**而非 `aborted`。无名单时行为与之前逐字一致 |

**决策修正 1：不加 wire 字段。**

原计划在 `IntentEnvelope` / MQTT proto 上加 `interveners`。实施时确认：PM 派发路径有自己的 `DeliveryRequest` 字面量且传 `aware_users: Vec::new()`（[intent_api.rs:641](../../../core/acowork-gateway/src/http/intent_api.rs#L641)），而 `aware_users_seed` 只有 `deliver_intent`、`deliver_response`、sweeper 三处调用——**这三处恰好就是要开放介入的会话**。于是"知晓名单非空 ⟺ 该会话应可介入"成立，Gateway 只承载一个事实，分层（读 vs steer）留在 core。省掉一次 protobuf 生成加三进程兼容窗口，代价是同一个名字只算一次。

代价与触发条件：将来若要"知晓但不介入"（只读利益相关方），必须回到 wire 加字段或给 session 加档位开关。`create_intent_session` 的注释已把这条前置条件写明。

**决策修正 2：`/answer`、`/approval` 的闸门本轮不动。**

确认的事实：agent↔agent 的 responder turn 在没有介入名单时**根本不 park**（直接 `aborted`），所以 `/answer` 在 exec session 里今天不可达；这两个端点只要求 `authorize_read`，因为 chat 卡片必须能被群成员作答，收紧会 404 掉今天可用的路径。把"你就是被问的人"校验与"卡片真的会 park 等待"放同一批改才安全——第 4 批（M6）与第 6 批（M8）。本轮已做掉前置条件：有名单的会话现在会 park，卡片的"可答性"第一次有了意义。

**批次 2 验收不变量**

| 不变量 | 测试 |
|---|---|
| 名单持久化、且与 `aware_users` 互不覆盖 | `acowork-sqlite` `tests/session_meta.rs` 的 `interveners` 往返用例 |
| 升级库与新库 `sessions` 形状一致（第 38 列） | `shared_store_e2e::fresh_and_upgraded_databases_share_one_sessions_shape` |
| 种子落盘、介入者可 steer 不可重构 | `session_manager::intent_session_stamps_aware_users_on_meta`（断言已改写） |
| 名单对外可见、字段集合与档位一致 | `session_intervention_e2e::the_intervention_tier_is_advertised_on_the_session_row` |
| 介入者不能改 workspace（403 且指明字段） | `…::an_intervener_cannot_re_premise_the_run` |
| 介入者能改 model / provider / account / effort | `…::an_intervener_may_re_point_the_run_at_another_model` |
| 介入者不能删、不能改可见性 | `…::an_intervener_cannot_delete_or_reshare_the_run` |
| 无名单 = 现状（不列出、写入 404 而非 403） | `…::without_a_roster_the_run_stays_an_agents_business` |
| 名单不拓宽读 | `…::the_roster_does_not_widen_read_access` |
| delta 每个字段都被档位表分类 | `session_config::delta::every_delta_field_is_classified_by_the_authority_table` |
| 字段名在 wire 与拒绝体里一致 | `session_meta::authority_tests::every_field_spells_the_same_name_on_the_wire_and_in_a_refusal` |

### 批次 3 — M5 溯源（已落）

`TurnSpeaker{origin, author, on_behalf_of}` 随 `ChatMessage` 旅行，落在 JSONL 的 `metadata.speaker`；
`ConversationEntryView` 读回；Desktop `speakerProvenance.ts` + `SpeakerProvenanceBubble` 把"代 A 发言"与 A 本人
渲染成两种气泡，五语文案齐。见 §3 M5。

### 批次 4 — M6 重试（已落）

| 落点 | 内容 |
|---|---|
| `http/server.rs` | `POST /sessions/{sid}/retry` → `post_retry_turn` |
| `http/session_control.rs` | 抽出 `open_for_action(state, sid, label)`：`/messages` 与 `/retry` 共用同一套 "Closed 先恢复再派发" 逻辑（人类按 Retry 的场景恰好是"隔了很久回来"，那时会话通常已 evict） |
| `session_control.rs::post_retry_turn` | `authorize_write` → `open_for_action` → **清空模型负缓存** → 派发 `InboundMessage::RetryTurn`。清缓存是这一步的语义：人按 Retry 意味着"我可能已经充值/换钥匙了"，而负缓存看不到这个越界修复 |
| `loop_.rs::wait_for_retry_or_user` | `select!` 增 `RetryTurn` 分支：等待期被抢占，立即回到 `LlmAwaitingFirstChunk` |
| `loop_.rs::drain_inbound_queue` | live 与 deferred 两条路径都消费 `RetryTurn` |
| `session_task.rs` | 复用 `last_user_message` + `replay()`，**不追加 user 行**，`message_id`/round 坐标不变；内存里没有（重启后）时用新增的 `last_user_row()` 从转录回读 —— "打开旧会话按 Retry"才有东西可放 |
| `acowork-gateway/src/http/proxy.rs` | `POST /api/agents/{id}/sessions/{sid}/retry`（无 body，重试的内容只能来自 Runtime 自己的转录，调用方无法把它变成另一个问题） |
| `apps/acowork-desktop` | `sessionControl.retryTurn()` + 错误横幅上的 **Retry** 按钮（五语 `chatError.retry`）。此前横幅只有 Dismiss：一个已经结束的 turn，界面上唯一的动作是"关掉这条错误" |

### 批次 5 — M7 自动降级（已落）

| 模块 | 落点 | 内容 |
|---|---|---|
| 策略（纯函数） | `agent/model_fallback.rs` | `ModelHealthRegistry` 负缓存（`(provider, model, account)` 三元组，TTL 15 分钟，读时过期即丢，无需清扫）；`classify(&RuntimeError) -> Option<ModelFailure>{BalanceExhausted,KeyRejected}`；`next_usable()` 阶梯过滤；`DegradationStep` 三级 |
| 库存（数据） | `agent_core.rs::fallback_candidates` | 只做枚举：同 provider 其它 account → agent 默认 model 同 provider → agent 默认 model 跨 provider。默认 model/provider 从 `agent_config.json`（ADR-110 的同一份事实）读，**不新增缓存** |
| 副作用 | `agent/loop_fallback.rs::degrade_after_capacity_failure` | 记负缓存 → 选候选 → `build_provider_for` 重建 → 同步 `SessionState` / `ContextBuilder.override_model` → 写回会话配置 → 落一条 `metadata.notice="model_degraded"` 的 system 行 |
| 接线 | `loop_.rs` `RetryAction::GiveUp` 分支 | 收口前问一次"还有没有能服务的三元组"；有则计数器归零（新三元组拿自己的重试预算）、重取 `current_model`、`continue`；没有则**逐字保持**原来的 Idle + `Err` |

三点必须写下来的判断：

1. **"启动时校验 model 有没有预算"做不到，除非我们撒谎。** `ProviderListItem` 没有额度字段，平台唯一拿到过的额度事实就是 provider 拒绝调用本身。
   所以这一层的诚实做法是把它变成持久事实：一旦某个三元组答"没钱"，本 agent 的所有会话都不再从它起步（负缓存跨 session 共享——额度是钥匙的属性，不是撞墙那个 session 的属性）。
2. **只有容量类失败才降级。** 网络抖动、上下文溢出、工具错误说明不了账号的事，在这些上面轮换模型比失败本身更糟（retry 分层仍然管它们）。
3. **切换写回会话，不写回 agent。** 一次 402 是"这个任务的账户/模型"的证据，从失败路径改人的默认设置会静默改掉之后所有 session；同时写回会话才让 model 菜单显示"真正在跑的模型"。

### 批次 6 — M8 真人入局（部分已落）

只落了"能问得到人"这一半，因为它是**不卡死**的必要条件：

| 落点 | 内容 |
|---|---|
| `loop_interaction.rs::interactive_human()` | 可回答性 = 名单（Runtime 自己的行，transport 伪造不了）∪ `TurnSurface.has_human`。与 `human_can_reach_pause()` 读同一个事实 —— "能停车"和"能发问"用两套判据，正是"问了没人看得见"的成因 |
| `loop_context.rs::hidden_tools_for_turn` 调用点 / `loop_tools.rs` 两处（工具集 + 幻觉兜底） | 三处全部改读 `interactive_human()`，名单非空的执行会话保留 `ask_user_question` 与审批等待（两者都有 5 分钟超时，不是无限停车） |

### 批次 7 — 会话内告知"有真人在旁"（已落）

口径由人类当场收紧：**执行会话里的人就是"以发起方 agent 的名义发言"，不做 mode 3，不把会话变成群聊。**
于是 B 侧需要的信息不是"谁加入了会话"，而是"你读到的、以发起方名义写下的指令，有可能是那个人打的"。

| 落点 | 内容 |
|---|---|
| `agent/session/session_task.rs::operator_presence_note` | 这段文案的唯一出处：说明会话与人类操作者共享、消息可能被那个人以 `{initiator}` 之名写下、要当成 `{initiator}` 的指令执行、但不要当成 `{initiator}` 对本轮计划的确认；并说明操作者能改 model/provider/effort、能重试，出错时要说清失败点而不是当作已完成 |
| `agent/context.rs` | `ContextBuilder.operator_presence` 槽 + `## Operator Presence` 段，位置紧跟 `## Task Assignment`（Block A，会话生命周期、随名单 write-once 而恒定，因此 Block A 仍字节稳定） |
| 注入点 | session task 启动时：`interveners()` 非空才设 |

为什么放在 Block A 而不是挂在操作者那一轮：挂在轮上的后果是"操作者第一次开口之前的所有指令"仍被读成 agent 本人的话；而这个事实从会话创建起就为真，不是从某一条消息开始的。

**还没做（按批次）**

* 第 6 批 M8 剩余（已按 O2 更正收窄）：让 `answerable_by` 在有名单、且发起方是 agent 时指向名单里的真人，并给 `/answer`、`/approval` 补"你就是被问的人"校验（名单内兜底）。顺序不能反：先有人可指，才能收紧校验；反了就把最后一条能跑的路堵死。
* **明确不做**：把真人加成三方 participant / 执行会话变群聊 / mode 3（"真身发言"）。`SessionKind` 里没有 2 agent + 1 人的形态，要动 user service 的参与者模型，而人类的需求只到"能以 agent 名义发消息"，该路径批次 3 已通。
* 前端接线：`config_fields` 已下发，Desktop 的 `session-write-access.ts` 仍是单 bool；执行界面的 model picker 需按字段开关。
* 批次 2 的同一个未验证口径仍然有效：session_state / chunk 的**推送**是否到达介入者界面没有实测。若收不到，M8 剩余部分要顺带核对 Gateway 的 session_state 扇出是否按 scope 过滤。

### 回归基线（本 session 实测）

`acowork-core` 399 / `acowork-runtime --lib` 1985 / `acowork-sqlite` 全绿；`session_intervention_e2e` 8 项（新增
`every_control_the_run_can_offer_is_pressable_by_the_intervener`、`a_stranger_cannot_press_the_controls_either`）、
`session_visibility_e2e` 2 项、`session_meta_sqlite_e2e` 2 项通过；`clippy -D warnings` 干净（runtime + gateway，含 tests）。
Desktop `tsc --noEmit` 干净；`vitest` 3088 passed / 6 failed，6 项全部落在 `GroupChatPanel.blockRef/pmDispatch`
的 `@` 提及文本（`@agent-a` vs `@Agent A`），**已用 stash 对照确认在 HEAD 上同样失败**，与本方案改动无关。

### 新增不变量

| 不变量 | 测试 |
|---|---|
| 介入者能按 Stop / Continue / Retry（档位不挡按钮） | `session_intervention_e2e::every_control_the_run_can_offer_is_pressable_by_the_intervener` |
| 无关人按不动（404，不是 403） | `…::a_stranger_cannot_press_the_controls_either` |
| 容量失败才降级，网络错误不降级 | `model_fallback::tests::network_errors_are_not_capacity_failures` |
| 阶梯不会把刚失败的三元组再给回来 | `…::ladder_skips_the_triple_that_just_failed` |
| 已知不能用 / 已过期的判定 | `…::ladder_skips_a_triple_known_to_be_out_of_service`、`…::an_expired_verdict_stops_blocking_the_triple` |
| 显式 account 与"默认 account"是两个三元组 | `…::explicit_account_and_default_account_are_different_triples` |
| 阶梯顺序（换钥匙 → 同 provider 默认 model → 跨 provider） | `agent_core::tests::the_ladder_is_other_account_then_agent_default_then_other_provider`、`…::the_default_model_on_the_same_provider_comes_before_another_provider` |
| 无默认 model 时阶梯止于第一级（要失败得响亮，不是死循环） | `…::without_an_agent_default_the_ladder_stops_after_another_account` |
| 名单让 turn 可回答，无名单/未知不改变原判据 | `loop_interaction::tests::an_intervener_roster_makes_the_turn_answerable`、`…::without_a_roster_the_gate_still_says_no_human`、`…::an_unknown_answerability_still_fails_open` |
| 人以发起方之名发言是一条独立事实（不是从会话所有者推断的） | `turn_speaker::tests::an_intervener_speaks_as_the_initiator` |
| 有名单只在 Block A 多一段，且不进用户轮；无名单逐字节不变 | `agent::context::tests::operator_presence_is_a_session_section_not_a_turn` |


**一个未验证的口径**：`human_can_reach_pause()` 读当前会话名单，"能点 Continue"由同一张表保证（介入档含 `may_turn`），但 session_state / chunk 的**推送**是否到达该人的界面没有在本轮验证。若实测收不到，第 6 批要顺带核对 Gateway 的 session_state 扇出是否按 scope 过滤。
