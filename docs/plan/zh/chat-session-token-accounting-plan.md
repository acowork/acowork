# Chat Session Token 用量统计方案

状态：已实施（2026-11-01）。M1 schema v9 + `SqliteChatUsageStore`（acowork-sqlite `chat_usage.rs`）、M2 `ChatUsageService` + chat_router 逐次记账接线、M3 盘扫描恢复与 `token_stats` 一致性/重建并入台账、M4 搜索摘要补 agent 计数器，全部落地；ADR-099 已加修订记录（zh/en）。per-chat 读取端点与 UI（阶段二）亦已于同日落地，见 §4.6。

## 1. 背景与问题

ADR-099（design 29）之后，每条人类 Chat 消息都会触发一次 Chat Router 的 LLM 轮次（信息轮 + dispatch 参数重试时一轮可达 2–3 次模型调用）。这些调用真实计费，但**既不持久化、也不进 agent 级计数器**，导致：

- agent 的 `token_stats`（桌面模型用量环）低报；
- agent 累计计数器（ADR-028/066，`context_usage` 实时推送的 agent 总量）低报；
- 重启后按盘扫描恢复（ADR-066 merge）也恢复不出这部分——因为它从未落盘。

Router 轮次是系统里**唯一成规模、未记账**的 LLM 消耗面：Block B 默认预算 8000 tokens/轮，一个活跃群聊里 chat 侧 token 可以接近甚至超过执行侧。

## 2. 现状梳理（事实）

### 2.1 执行会话的 token 方案（四层）

| 层 | 位置 | 机制 |
|---|---|---|
| 单次调用折叠 | [conversation.rs `record_llm_round`](../../../core/acowork-runtime/src/conversation.rs) | `SessionTokens`（ADR-027 零跳过规则：`prompt_tokens==0` 不进 total）+ `model_usage` 按 `"{provider}\|{model}"` 明细 + 成本估算（`estimate_call_cost`）+ perf，一次调用一次持久化写 |
| 持久化 | [session_meta.rs](../../../core/acowork-sqlite/src/session_meta.rs) | `sessions` 表（token_* 平铺列 + `model_usage` JSON），每次 upsert 用 `apply_stats_delta` 增量维护派生表 `token_stats`（v6 迁移，schema 现版本 8） |
| agent 计数器 | [agent_core.rs](../../../core/acowork-runtime/src/agent/agent_core.rs) | `accumulate_llm_usage` 原子加；`merge_token_totals` 以 `max(counter, scanned)` 从盘扫描恢复（[session_metadata_impl.rs:92](../../../core/acowork-runtime/src/usecases/session_metadata_impl.rs) 与 startup/session_init.rs 两处调用点） |
| 读取 | [server.rs `/sessions/token-stats`](../../../core/acowork-runtime/src/http/server.rs) | Gateway 代理 `/api/agents/{id}/token-stats` → 桌面 `ModelTokenDonut`；一致性由 `token_stats_consistent`（Σ sessions.model_usage vs token_stats）+ `rebuild_token_stats` 兜底 |

不变量（测试钉死）：`Σ model_usage 明细 == tokens 总量`；`token_stats` 与 sessions 明细一致；一次 LLM 调用一次全行写。

### 2.2 Chat Router 现状

[chat_router/runner.rs](../../../core/acowork-runtime/src/agent/chat_router/runner.rs)：

- `response.usage`（`UsageInfo`）在轮次末尾取一次，只写进两处观测面：
  - 决策日志 `{work_dir}/logs/chat-router-decisions.jsonl`（只有 prompt/cache_read/cache_write，**缺 completion tokens**，且**只记最后一次调用**——信息轮与重试轮的 usage 被 `response = retry` 覆盖丢失）；
  - debug trace 事件。
- 没有任何路径写入 `sessions` 表、`token_stats`、或 `core.accumulate_llm_usage`。
- ADR-099 §与相关 ADR 一节写着「复用 ADR-066 cache token 统计口径」——口径复用了，记账本身没接。

### 2.3 其他未入账调用点（顺带修正）

- [http/search_summary.rs:381](../../../core/acowork-runtime/src/http/search_summary.rs)：搜索摘要 LLM 调用，usage 只回显给前端，不累计。
- 标题生成（loop_.rs:1053）与压缩（loop_context.rs）**已经**双写 conv + core 计数器，可作为接线范本。

## 3. 方案对比

### A. 伪会话行：每个 chat session 在 `sessions` 表建一行（`session_id = "chat:{chat_session_id}"`）

复用一切（fold、delta、扫描恢复、UI 全免费），但语义污染严重：

- `list_sessions`、懒恢复（`SessionManager::open` 扫盘把行当可恢复会话）、`prune_to` 配额驱逐、visibility ACL——每一处都要新增 kind 过滤，漏一处就是 bug（比如 chat 行被当作可 resume 的执行会话打开）。
- chat session 没有历史、没有 ConversationSession，伪造一个「会话」实体违背 ADR-099「router 无状态、不拥有 LLM 对话」的设定。

**否决**：改动面看似小，实际是把「执行会话」实体的不变量复制到一个永远无法满足它的对象上。

### B. 专用用量台账（推荐）

新增 `chat_token_usage` 表（同一 SQLite 文件），每个 chat session 一行**纯聚合**（无对话状态），复用执行会话的折叠规则、`apply_stats_delta`、成本口径和 agent 计数器。chat session 在统计里是「用量归属键」，不是「会话实体」——与 ADR-099 的无状态设计一致。

### C. 上报 Gateway 聚合

违反 AGENTS.md 的 Gateway 边界红线（agent 私有数据只经 Runtime HTTP 读写），且 Gateway 没有 token 存储 schema。**否决**。

## 4. 推荐方案 B 详细设计

### 4.1 数据模型（schema v9 迁移）

```sql
CREATE TABLE IF NOT EXISTS chat_token_usage (
    chat_session_id          TEXT PRIMARY KEY,   -- 与 SessionMeta.chat_session_id / Gateway chat log 同 id 空间
    chat_kind                TEXT,               -- direct / group / agent（runner 已有 chat_kind）
    created_at               INTEGER NOT NULL,
    last_active_at           INTEGER NOT NULL,
    llm_call_count           INTEGER NOT NULL DEFAULT 0,
    token_last_input         INTEGER NOT NULL DEFAULT 0,
    token_last_output        INTEGER NOT NULL DEFAULT 0,
    token_total_input        INTEGER NOT NULL DEFAULT 0,
    token_total_output       INTEGER NOT NULL DEFAULT 0,
    token_last_cache_read    INTEGER NOT NULL DEFAULT 0,
    token_last_cache_write   INTEGER NOT NULL DEFAULT 0,
    token_total_cache_read   INTEGER NOT NULL DEFAULT 0,
    token_total_cache_write  INTEGER NOT NULL DEFAULT 0,
    model_usage              JSON                -- 与 sessions.model_usage 同构：{"{provider}|{model}": ModelUsageEntry}
);
```

- `model_usage` 复用 `ModelUsageMap` / `parse_model_usage` / `apply_stats_delta`（同 crate，直接可见）。
- 零跳过、`last_*` 恒覆盖、cache 口径，全部照抄 ADR-027/066 规则——单一 fold 函数抽成 `pub(crate)` 供两个表共用，避免规则漂移。

### 4.2 记账服务（新 use case）

`core/acowork-runtime/src/usecases/chat_usage.rs`（trait）+ `chat_usage_impl.rs`：

```rust
pub trait ChatUsageService: Send + Sync {
    /// 一次模型调用一条账：折叠台账行 + diff token_stats + 累加 agent 原子计数器。
    fn record_chat_llm_usage(
        &self,
        chat_session_id: &str,
        chat_kind: &str,
        usage: &UsageInfo,
        attribution: &ModelUsageAttribution,
    );
    fn list_chat_usage(&self, page: u32, size: u32) -> Result<ChatUsageListResponse>;
}
```

实现持有 `Arc<SqliteChatUsageStore>` + `Arc<dyn AgentTokenService>`（或 core 句柄），一次调用 = 一次 upsert + 一次原子累加，与执行会话的「一次调用一次写」不变量对齐。

### 4.3 Runner 接线（修正「只记最后一次调用」）

记账点下沉到 `call_model` 包装层：**每次成功的模型调用**（首调、每个信息轮、dispatch 重试）都 `record_chat_llm_usage`，而不是在 `Outcome` 分支里记终稿。落在 `chat_router/runner.rs` 的 `ModelCalls::call`（`run_turn` 每个模型轮次的唯一入口，内部只剩 `call_provider` 做传输）——记账是"调用"的属性，不是"调用点"的责任，因此每个调用点都无法漏记。归属：

- `chat_session_id` = `envelope.source_chat_session_id`；
- provider/model = `prepared.provider_label` / `prepared.model`；
- cost = `estimate_call_cost(core.get_model_capabilities(&model).cost, usage)`（与执行侧同口径）。

顺带：`DecisionRecord` 补 `completion_tokens` 字段，观测口径与台账对齐（JSONL 仍是观测面，不作聚合源）。

### 4.4 agent 计数器与重启恢复

- 实时：4.2 的服务直接走 `core.accumulate_llm_usage`，`context_usage` 推送的 agent 总量即刻含 chat。
- 恢复：`merge_token_totals` 的扫描结果需含台账合计——在现有两处调用点（`session_metadata_impl.rs:92`、`startup/session_init.rs:913`）把 `SUM(chat_token_usage.token_total_*)` 并入 scanned 元组。**必须做**，否则重启后 agent 计数永久低报 chat 部分（max-merge 无法凭空恢复从未进过 counter 的账）。

### 4.5 token_stats 一致性与重建（必须配套）

- `rebuild_token_stats`：改为 `sessions.model_usage UNION ALL chat_token_usage.model_usage` 再 fold。
- `token_stats_consistent`：校验 SQL 同样并入台账，否则 chat 账进 `token_stats` 后校验恒报不一致、触发无谓 rebuild。
- `prune_to`：只动 `sessions`，台账行不受影响（每 chat 一行、天然有界）。

### 4.6 读取路径与 UI（已实施）

- **阶段一（修失真）**：agent 级 donut / token-stats 端点零改动自动正确。
- **阶段二（已落地）**：runtime `GET /chat-usage/{chat_id}`（单行台账，404 = 该 agent 从未为此 chat 记账）→ Gateway 透明代理 `GET /api/agents/{id}/chat-usage/{chat_id}`（照 `proxy_token_stats` 模式，permission 与 `token-stats` 同桶 `Manage`）→ 桌面 `lib/chatUsage.ts` 按 chat 名册里的 agent 参与者扇出并在客户端合并（跨 agent 求和 + per-model 摊平），展示于单聊/群聊输入框的 `ChatUsageIcon` 菜单（合计 + per-agent 明细）与右侧状态面板的「会话用量」卡片。chat 无上下文窗口（ADR-099 router 无状态），所以该视图只呈现台账（调用次数/累计 tokens/成本/模型占比），不伪装成 `ContextUsageIcon` 的环形占比。

### 4.7 边界与归属

- `usage == None`（provider 未报）→ 跳过，与 ADR-027 同规则。
- 群聊 per-user 归属：台账只到 chat 粒度，**不做**消息级归因（那需要把 router 轮次拆到触发消息，超出范围；chat 本身即用户可理解的归属单位）。
- chat 删除下达 Runtime（已实施，替代原「v1 保留行」）：Gateway `user_proxy` 在 `DELETE /api/chat-sessions/{id}` 代理成功（2xx）后，先读参与者名册（删除后行已不存在，必须前置读），再向每个 **agent 参与者** 的 Runtime best-effort 扇出 `DELETE /chat-usage/{chat_id}`（Runtime 新增路由）；台账行与其在 `token_stats` 的明细在同一事务里删除/扣减（`SqliteChatUsageStore::forget`，与会话行删除对称）。名册只发给该 chat 自己的 agent（与消息投递同一隐私纪律）；Runtime 未注册时留行，属可接受的残余（不谎报为已闭环），LRU 上限方案不再需要。
- 多节点：router 跑在 agent 所在节点的 Runtime 里，台账与 `sessions` 同库同侧，口径一致。
- 预算守卫：`BudgetGuard` 是 session 级，不受影响；agent 总量展示变准是预期收益。

## 5. 实施拆分（每步独立可评审、可测）

| 模块 | 内容 | 验收 |
|---|---|---|
| M1 | schema v9 + `SqliteChatUsageStore`（acowork-sqlite）+ fold 规则单测 | 表建/迁移幂等；零跳过、Σ明细==总量 不变量测试 |
| M2 | `ChatUsageService` use case + runner 接线（含 call_model 包装、DecisionRecord 补字段） | mock provider 多轮（信息轮+重试）→ 台账/`token_stats`/agent 计数三者==逐调用期望值 |
| M3 | 一致性/重建 SQL + `merge_token_totals` 并入台账合计 | 重启恢复测试：kill 进程前 chat 账在、恢复后 agent 总量含 chat |
| M4 | chat-usage 端点 + Gateway 代理 + 桌面展示（已实施） | 面板数字与台账一致 |

## 6. 测试要点

- 回归钉死：`token_stats_consistent` 在含 chat 账后仍为 true；`rebuild_token_stats` 后 Σ 不变。
- 不变量：一次 router 轮次 N 次模型调用 → `llm_call_count += N`（当前实现只可能记 1，这是行为修正，需在 ADR-099 修订记录里补一句）。
- 降级路径（degrade/fallback）未产生成功调用 → 不记账。

## 7. 待讨论决策点

1. 方案 B vs A（本方案推荐 B：台账表，不伪造会话实体）。
2. 信息轮/重试轮逐次记账的行为修正是否接受（当前 JSONL 只记末次）。
3. 阶段二（per-chat UI）是否进本期。
4. `search_summary` 是否顺带并入 `ChatUsageService` 之外的最小路径（建议：只补 `core.accumulate_llm_usage` 一行，不进台账——它不属于任何 chat）。
5. 文档落点：本方案落地后是否补 ADR-099 修订节 + ADR-028 影响面一句话。
