# 记忆沉淀重构：从"价值筛选"到"归一化提取"

> 版本：v0.1（草案）| 日期：2026-10-01
>
> 作废：[ADR-068 §3.4.2](../../adr/zh/ADR-068-memory-layer-promotion-two-axis-orthogonal.md) Step 2b/3/5 的 embedding 聚类否决与数值晋升门槛
> 保留：ADR-068 两轴不变量（LLM 只写经历层；沉淀层唯一生产者是蒸馏器）
> 关联：[ADR-071](../../adr/zh/ADR-071-distiller-runtime-config-and-trigger.md)（触发与运行时配置，本次不改）、[ADR-082](../../adr/zh/ADR-082-memory-storage-sqlite-vector-fts.md)（存储后端）
>
> **一句话**：蒸馏器的职责是**把碎片化经历归一化成单条表述**，不是二次筛选价值——价值判断在 `memory_store` 写入时已由掌握完整上下文的 LLM 做过。本次删除全部数值晋升门槛与聚类否决，把蒸馏器收敛为「投影 + 合并」两个动作；同时放弃三元组结构（无图存储，结构无消费者）。预估 **4-5 人日**。

---

## 1. 起因：一次静默失败

Ponytail agent 的记忆面板开了沉淀开关很久，产出 0 条沉淀记忆。排查结果：

```
$ grep -h "EpisodicDistiller triggered" *.log | wc -l   → 29
$ grep -h "Step 2a JSON parse failed"   *.log | wc -l   → 24
$ grep -h "run complete"                *.log | wc -l   →  0
```

触发条件全部满足（backlog 269 ≥ 阈值 50，间隔 60min 到点），**每一次都跑了，每一次都死在 Step 2a**：

```
EpisodicDistiller run failed (ADR-068) error=Memory error:
  Step 2a JSON parse failed: invalid JSON: EOF while parsing a string at line 236 column 7
```

根因：[distiller.rs:464](../../../core/acowork-memory/src/consolidation/distiller.rs#L464) 把整批 `batch_size`（默认 100）条 episode 拼成**一条** user message，要求 LLM 返回逐条对应的 JSON 数组，而 [llm_adapter.rs:79](../../../core/acowork-runtime/src/memory/llm_adapter.rs#L79) 写死 `max_tokens: Some(2048)`。100 条 episode 的输出需要 6000–8000 token，必然在数组中段被截断，`from_str` 整块失败，`?` 上抛，整批作废。episode 不标记 consolidated → 积压不降 → 每小时重跑同一批 → 永久循环。

错误报在 line 182 / 220 / 236 / 302 各处，全部落在数组中段而非开头——与"生成到一半撞上 token 上限"完全吻合。

**这个 bug 本身很小（分块即可）。但它暴露的架构问题不是。** 见下节。

---

## 2. 设计问题：蒸馏器在做一件它不该做的事

### 2.1 价值筛选已经在写时完成

`memory_store` 的入参已经包含 LLM 的完整判断：

| 字段 | 语义 |
|---|---|
| `category` | fact / preference / relation / procedure |
| `confidence` | "reflecting how certain you actually are" |
| `importance` | "**Higher importance resists forgetting**"，core identity ≈1.0 / trivia ≈0.1 |
| `privacy` | public / personal / sensitive |

写时那次调用手上有**完整对话上下文 + 用户原话 + 为什么这条值得记的理由**。蒸馏器看到的只有 `[Episode 42] (fact): <content>` 一行脱离上下文的文本。

现在的链路是：让信息更多的判断先做一次，再让信息更少的一次把它推翻。蒸馏器里的 `min_evidence` / `confidence` 门槛本质是**第二次问同一个模型同一个问题，然后不信它第一次的回答**。

### 2.2 蒸馏的真正职责是写时做不到的那件事

**写时判断是局部的。** 模型写下 "Nancy 在 Shanghai" 的那一刻，看不见库里已有的 "Nancy 在 Beijing"。它不是不够聪明，是**没有全局视图**。

所以蒸馏存在的理由收敛成两件事，且都需要全局视野、只有离线批处理能做：

1. **归一化**：碎片 → 单一规范表述
2. **冲突消解**：新旧矛盾时判定 evolution / correction

这两件事都由 LLM 做。其余一切（数值门槛、embedding 聚类否决、`evidence_score` 公式）都是在**用不可观测的公式给 LLM 的判断打折扣**。

### 2.3 三元组没有消费者（本次核实）

`KnowledgeNode` 的 `subject` / `predicate` / `object` 全库只有两处消费：

1. **去重键** [lib.rs:518](../../../core/acowork-sqlite/src/lib.rs#L518)：`WHERE json_extract(props,'$.subject') = ?2 AND json_extract(props,'$.predicate') = ?3` —— 裸字符串相等。ADR-068 D11 明确放弃 canonical predicate 词表、改自由生成，于是 `lives_in` 与 `prefers_residence` 永不碰撞，**这个索引实际失效**。
2. **FTS 文本** [lib.rs:997](../../../core/acowork-sqlite/src/lib.rs#L997)：`format!("{} {} {}", subject, predicate, object)` —— 拆成三元组后**立刻拼回一句文本**喂全文检索和 embedding。

**没有第三处。** [schema.rs](../../../core/acowork-sqlite/src/schema.rs) 只有 `nodes` / `vectors` / `sessions` / `meta` / `purge_log` 五张表，**不存在 `edges` 表**；`hybrid_search` 对每种 label 一视同仁做 FTS + 向量，不消费结构。[05-memory.md](../../design/zh/05-memory.md) 描述的"沉淀层 ──(关联扩散)──→ 多跳检索结果"在 SQLite 后端**尚未实现**。

结论：三元组的价值全在可遍历与精确匹配，两者此处皆无。当前实现是**做一次昂贵的结构化，再立刻还原成非结构化**。

叠加实测经验：fact / procedure 的结构化质量尚可，**三元组质量差，编程场景尤甚**（"Gateway 只负责通信+资源管理+反向代理" 这类内容没有自然的 SPO 分解）。

### 2.4 公式化门槛的具体问题

```rust
// distiller.rs:1299
let count_score = (count as f32 / (2.0 * min_evidence)).min(1.0);
```

分母 `2 × 阈值` 是凭空构造，不测量任何东西，只是把"重复次数"映射成一个看似置信度的小数，再喂给 `promotion_confidence_threshold = 0.85` 做数值比较。

而 `confidence` 来自**同一个模型、同样的上下文、没有任何 ground truth** 的第二次提问。未经校准的自报浮点数被用来 gate 一个**不可逆**决定（skip 写永久墓碑）。

### 2.5 成本不对称：假阴性不可逆，假阳性可逆

[distiller.rs:827](../../../core/acowork-memory/src/consolidation/distiller.rs#L827)：judge 一次 `skip` 或 confidence 0.84 → 整簇 episode 永久打 `distiller_skip`，永不重试。

漏记不可逆，记错可逆（面板已有编辑/删除，用户纠正成本很低）。设计把成本算反了：**它在拼命避免一个便宜错误，代价是制造一个昂贵的沉默。**

### 2.6 两个静默失效点

| 问题 | 位置 | 机制 |
|---|---|---|
| **队首饿死** | [provider.rs:546](../../../core/acowork-sqlite/src/provider.rs#L546) `ORDER BY created_at ASC` + [:135](../../../core/acowork-sqlite/src/provider.rs#L135) 取前 100 | `Deferred` 既不标记也不写墓碑 → 最老 100 条若全被 Deferred，永远占满窗口，第 101–269 条永不被扫描 |
| **指标口径不一致** | [lib.rs:367](../../../core/acowork-sqlite/src/lib.rs#L367) | `count_unconsolidated_episodes` 统计全部 269 条（含 7 条无 subtype、根本进不了扫描的）；面板 backlog ≠ 候选池 ≠ 实际扫描窗口 |

---

## 3. 目标设计

### 3.1 写时：`memory_store` 增加 `normalized` 字段

```
content, category, confidence, importance, privacy, keywords    ← 现有
+ normalized: string                                            ← 新增
```

`normalized` = **一句话的归一化表述**（第三人称、去场景、可长期为真）。不是三元组，不是 SPO，就是一句话。这是模型最擅长的一次性输出，编程场景无压力：

> **content**：「用户确认了架构边界：Gateway = 通信 + 资源管理 + 反向代理，Node Agent = 通信转发 + Runtime 生命周期（装载/卸载/启动/停止），不部署业务逻辑到 Agent Runtime 内部，私域数据读写一律走 HTTP」
> **normalized**：「ACowork 的 Gateway 只负责通信、资源管理与反向代理，Node Agent 只负责通信转发与 Runtime 生命周期管理，Agent 私有数据只经 Runtime HTTP 读写」

向后兼容：缺省时蒸馏器回退用 `content`。

### 3.2 蒸馏器：只剩两个动作

```
for episode in 扫描窗口:
    # ── 动作 1：投影 ──
    text = episode.normalized or episode.content
    candidates = vector_recall(text, threshold=0.65)      # 召回，故意宽松，不做否决
    if candidates.is_empty():
        store(text, subtype=episode.knowledge_subtype,
              source_episode_ids=[ep.id],
              importance=episode.importance)               # 单条即晋升
    else:
        # ── 动作 2：合并 / 冲突消解（唯一需要 LLM 的地方）──
        match llm(text, candidates):
            same_fact → 更新已有节点（重写 normalized，追加 source_episode_ids）
            conflict  → evolution / correction 判定（沿用 ADR-068 现有三分类）
            distinct  → 新建节点
    mark_consolidated(episode.id)
```

**关键性质：**

- **无批量截断风险**：每次 LLM 调用只处理 1 条新记忆 + K 条候选，输出是单个小 JSON，2048 token 绰绰有余。
- **单条失败隔离**：一条 episode 的调用失败 → 该条不标记、下轮重试；其余照常。**这是本次事故的根本解法，不是打补丁。**
- **无候选时零 LLM 调用**：全新事实直接落库。绝大多数蒸馏运行不需要调 LLM。
- **去重从字符串相等改为语义召回**：`store_knowledge` 里已有的向量 dedup（[lib.rs:451](../../../core/acowork-sqlite/src/lib.rs#L451) `cosine > 0.95`）本来就在，只是被前面那个失效的字符串键挡住了入口。

### 3.3 保留与删除清单

**保留：**

| 项 | 理由 |
|---|---|
| 两轴不变量（LLM 只写经历层） | 单一写入者、可审计，是正确的架构约束 |
| `source_episode_ids` | 天然幂等键，同一 episode 不会被投影两次 |
| ~~冲突消解三分类（evolution / correction / ambiguous）~~ | **计划原文有误**：这套分类在代码里已经没有任何生产者 —— `ConflictType`、`conflicts_evolution/correction/ambiguous`、`should_trigger_confirmation` 全库零引用，旧管线停用后就是死代码。合并分支是重写时新写的，不是「接上原有的」。见 §8 |
| `importance` 驱动的衰减 | 价值控制量的正确机制（已有），取代晋升门槛 |
| episodic forgetting | 同上 |
| `promote_event`（History 里程碑） | 事件触发通道，与 episode 聚类无关 |
| Relationship 30 天跨度规则 | 规则型，无 LLM，无门槛 |
| ADR-071 D7 两个 prompt override 槽位 | 机制不变，只是默认 prompt 内容变了 |

**删除：**

| 项 | 位置 |
|---|---|
| Step 2a 批量抽取（整批一次调用） | [distiller.rs:459](../../../core/acowork-memory/src/consolidation/distiller.rs#L459) `extract_structures` |
| Step 2b embedding 聚类否决 | [:581](../../../core/acowork-memory/src/consolidation/distiller.rs#L581) `cluster_candidates` |
| 5 个 `*_min_evidence` 阈值 | [mod.rs:645](../../../core/acowork-memory/src/consolidation/mod.rs#L645) |
| `autobio_min_span_days = 14` | [mod.rs:657](../../../core/acowork-memory/src/consolidation/mod.rs#L657) |
| `evidence_score` 公式 | [:1299](../../../core/acowork-memory/src/consolidation/distiller.rs#L1299) |
| `promotion_confidence_threshold = 0.85` | [mod.rs:659](../../../core/acowork-memory/src/consolidation/mod.rs#L659) |
| skip 永久墓碑 | [:827](../../../core/acowork-memory/src/consolidation/distiller.rs#L827) |
| `EXTRACTION_SYSTEM_PROMPT` 三元组指令 | [:381](../../../core/acowork-memory/src/consolidation/distiller.rs#L381) |
| `KnowledgeNode` subject/predicate/object 的填充 | 字段暂留（§6），不再写入 |

### 3.4 语义层会变密——这是有意的

去掉门槛后 semantic 节点数将接近 episodic 数（当前 262 条 → 约 262 个节点，合并后更少）。

**密度由衰减和检索排序处理，不由晋升门槛处理。** `importance` 与 episodic forgetting 已经为此存在。用门槛控量是用错误机制解决正确问题。

风险对冲：M2 提供 `[memory.distiller].min_importance` 单一开关（默认 0 = 全放行），真出现噪声可一键收紧，而不需要恢复 5 个阈值。

---

## 4. 里程碑

| 阶段 | 内容 | 估时 | 出口条件 |
|---|---|---|---|
| **M0** | 可观测性 + 截断修复（独立于重构） | 0.5d | 面板能区分"没跑 / 跑了失败 / 跑了没产出" |
| **M1** | `memory_store` 增加 `normalized` | 0.5d | schema + 写路径 + 工具描述；缺省回退 content |
| **M2** | 蒸馏器重写为「投影 + 合并」 | 2d | 删除清单全部落地；单条失败隔离测试通过 |
| **M3** | backfill 存量 episode | 0.5d | 262 条走 content 回退路径，产出非零沉淀 |
| **M4** | 文档与 ADR 修订 | 0.5d | ADR-068 修订 + 05-memory.md 同步（zh + en） |

### M0 — 可观测性 + 截断修复（0.5d，**先做，与重构无关**）

这次排查花了几天的真正原因不是 bug 复杂，是**看不见**。

| ID | 任务 | 验收 |
|---|---|---|
| M0-1 | `extract_structures` 按 20 条分块调用，逐块解析；单块失败降级为该块 `ExtractionFailed`，不影响其余 | 复现当前 269 积压场景，能产出部分结果 |
| M0-2 | `LlmResponse` 携带 `finish_reason`；`== "length"` 时 warn，而非伪装成解析错误 | 截断在日志里可读 |
| M0-3 | 漏斗计数进 `DistillerResult`：`scanned → clustered → gated → judged → promoted` | `GET /memory/consolidation/status` 可见 |
| M0-4 | 失败也写 `DistillRunRecord`（带 `error` 字段），status 的 `last_run` 不再永远为 null | 面板能显示"上次运行失败：<原因>" |

> M0 单独提交、单独可回滚。即使重构不做，M0 也应该落地——它同时是 M2 的验证工具。

### M1 — `memory_store` 增加 `normalized`（0.5d）

| ID | 任务 | 位置 |
|---|---|---|
| M1-1 | `Episode` 增加 `normalized: Option<String>` | [types.rs:386](../../../core/acowork-memory/src/types.rs#L386) |
| M1-2 | 工具 schema 增加字段 + 描述（"a single self-contained sentence, third-person, decontextualized, durable"） | [memory_store.rs:87](../../../core/acowork-runtime/src/tools/builtin/memory_store.rs#L87) |
| M1-3 | 写路径落库；缺省回退 `content` | [memory_store.rs:342](../../../core/acowork-runtime/src/tools/builtin/memory_store.rs#L342) |
| M1-4 | episode embedding 改用 `normalized`（若有）——归一化文本比场景化原文更适合做检索键 | 同上 |

### M2 — 蒸馏器重写（2d）

| ID | 任务 | 验收 |
|---|---|---|
| M2-1 | 扫描窗口改 newest-first 或轮转，堵住队首饿死 | 单测：200 条积压时第 150 条能被扫到 |
| M2-2 | `count_unconsolidated_episodes` 与候选池口径对齐（只计有 subtype 的） | status 数字自洽 |
| M2-3 | 投影路径：无候选 → 直接落库，零 LLM 调用 | 单测：全新 episode 不调 LLM |
| M2-4 | 合并路径：LLM 输入 = 1 条新 + K 条候选，输出 = `same_fact / conflict / distinct` | 单测覆盖三个分支 |
| M2-5 | ~~接现有 evolution / correction 判定~~ → 合并分支新写（旧分类无生产者，见 §8） | 单测：`no_merge` 新建节点、幻觉 `target_id` 不采纳 |
| M2-6 | 删除 §3.3 清单中的全部门槛与墓碑 | `cargo clippy -- -D warnings` 无 dead code |
| M2-7 | 单条 episode 失败隔离 | 现有 `test_d15_single_episode_failure_isolated` 保持绿 |
| M2-8 | 新增 `[memory.distiller].min_importance`（默认 0）作为唯一兜底开关 | 可一键收紧 |

### M3 — backfill（0.5d）

存量 262 条 episode 只有 `content`，无 `normalized`。

**决策：用 `content` 直接投影，不重新抽取。** 理由：重新抽取等于再引入一次低质量结构化，正是本次要移除的东西；`content` 本身已经是写时 LLM 的判断产物，符合 §2.1 的立场。

**实施修正**：不需要脚本。`Episode::statement()` 的 normalized→content 回退本身就是 backfill —— 存量行读出来就能投影。改用一个 `#[ignore]` 的手动测试在**真实 store 的副本**上验证（`memory_lifecycle_e2e.rs::backfill_on_a_real_store_drains_the_backlog_without_a_model`），结果见 §8。

### M4 — 文档（0.5d）

| ID | 任务 |
|---|---|
| M4-1 | ADR-068 增补修订节：Step 2b/3/5 的门槛与聚类否决作废，蒸馏职责收敛为「投影 + 合并」 |
| M4-2 | [docs/design/zh/05-memory.md](../../design/zh/05-memory.md) 巩固管道一节同步 |
| M4-3 | 明确记录"三元组字段保留但不再填充"，并指向 §6 的图需求决策点 |
| M4-4 | 中英双份（AGENTS.md：设计文档 zh + en） |

---

## 5. 验证

重构后必须能回答"它到底有没有在工作"，否则下次坏掉还是几天后才发现。

1. **可运行的回归检查**：`core/acowork-memory/src/consolidation/` 内一个小 assert 自检——构造 3 条同义 episode + 1 条无关 episode，跑一轮蒸馏，断言产出 1 个合并节点 + 1 个独立节点，且 4 条 episode 全部 `consolidated=true`。
2. **真实数据验证**：Ponytail 实例（269 积压）手动 `POST /memory/distill`，断言 `promoted > 0` 且 backlog 下降。
3. **漏斗指标**：连续 3 小时观察 status 的 `scanned / promoted` 比值，确认不是"跑了但全 0"。

---

## 6. 明确不做（YAGNI）

| 项 | 理由 |
|---|---|
| 引入 `edges` 表 / 图遍历 / 关联扩散 | 三元组失去填充者后，图需求是一个**独立的产品决策**。等真要做多跳检索时再引入，届时结构由那时重新定义——现在为它保留 SPO 填充没有消费者 |
| 删除 `KnowledgeNode` 的 subject/predicate/object 字段 | 牵动 schema、面板、序列化、既有节点。留空即可，删字段是另一次变更 |
| 改 ADR-071 的触发参数（interval / accumulation / idle） | 触发机制本次工作正常，日志已证明 |
| 给蒸馏器加 canonical predicate 词表 | 与 §2.3 结论冲突：没有结构消费者，词表是纯成本 |
| 冲突消解逻辑改动 | 独立问题，混进来会让验证面翻倍 |
| per-agent prompt override 机制变更 | 槽位保留，只是默认 prompt 内容变了 |

---

## 7. 待确认

| # | 问题 | 我的倾向 |
|---|---|---|
| Q1 | `normalized` 必填还是选填？ | **选填**。必填会增加每次 `memory_store` 调用的失败面，且该工具是所有 agent 共用的契约 |
| Q2 | 合并候选数 K 取多少？ | **K=5**。再多就把合并判断变成又一次批量处理 |
| Q3 | 召回阈值 0.65 是否偏低导致误合并？ | 宁可宽松。误合并可被用户纠正（面板可编辑），漏合并不被察觉——但漏合并**不会永久丢失**（无墓碑），与 §2.5 的不对称性一致 |
| Q4 | M0 是否先独立发布？ | **是**。M0 是 M2 的验证工具，且即使重构取消，M0 也该落地 |

---

## 8. 实施结果与计划偏差

提交：`d2745c21`（M0-M2 重构）、`5be91f91`（statement 收敛 + 端到端验证）。

### 8.1 与计划不符的三处

| # | 计划假设 | 实际 |
|---|---|---|
| 1 | 冲突消解逻辑「原样保留、本次不动」 | 该逻辑**早已无生产者**。`ConflictType`、`conflicts_evolution/correction/ambiguous`、`should_trigger_confirmation` 全库零引用，属死代码。合并分支是重写时新写的，不是接上旧的 |
| 2 | M3 需要一次性 backfill 脚本 | 不需要。`Episode::statement()` 的 normalized→content 回退**本身就是** backfill，存量行读出来即可投影。改为在真实 store 副本上跑 `#[ignore]` 测试验证 |
| 3 | `PromotionKind` 保留 Autobio 各变体 | 删除 `AutobioLimitation` / `AutobioPreference`（聚类管线已移除，无生产者）。保留 `AutobioRelationship`（30 天规则）与 `AutobioHistory`（里程碑事件）——两者与 episode 聚类无关 |

### 8.2 真实 store 实测（回答「开关开了很久没产出一条记忆」）

Ponytail 实例 `8f7be9a6`，重构前的 `private.sqlite` 副本：

```
episodic rows   : 269      pending backlog : 262      sediment before : knowledge=0 procedural=0
无模型：run1 投影 30 / 延后 70 → run3 收敛，累计 projected=33, llm_calls=0
有模型：3 轮清空积压，sediment after = knowledge=141 procedural=121
```

实例 `1de45b14`（7 条 episode，全部无向量）：无模型即投影 5 条，有模型 1 轮清空。

结论：旧管线产出 0 不是因为触发条件（周期/积压/空闲都满足，日志可证），而是因为**提取→聚类→判定链路上的门槛从未同时通过**。重写后同一条数据在零 token 下即产出 33 条沉淀。

### 8.3 需要知道的行为边界

- **无模型时积压会停在「延后」状态。** 有向量近邻的 episode 需要模型裁决，没有就等。这是刻意的（猜错两个方向都不可逆），但面板上看起来像「卡住」。生产路径 `run_episodic_distiller_step` 始终传入模型，所以只在配置异常时出现。
- **`batch_size=100`，262 条积压需要 3 轮。** 不是单轮清空。
- **`KeepDistinct`（模型总说「不合并」）下 262 条 → 262 个节点**，即语义层密度上界由模型判断决定，不由代码决定。若面板过密，收紧 `min_importance` 是唯一兜底开关。

### 8.4 验证方式

`core/acowork-runtime/tests/memory_lifecycle_e2e.rs`（验证时路径；该文件已随 ADR-100 迁入 `core/acowork-memory/tests/`）（9 通过 + 1 ignored）。每条断言都做过变异测试确认能失败：

| 注入的错误 | 被捕获的测试 |
|---|---|
| 衰减扫描同时扫 Knowledge | `forgetting_the_episode_keeps_the_sediment`、`sediment_stays_recallable_after_its_episodes_are_forgotten` |
| 忽略 `dormant_threshold`（见到就休眠） | `a_recent_episode_survives_even_an_aggressive_decay_scan` |
| 忽略 `archive_days`（见到 Dormant 就清除） | `a_dormant_episode_waits_out_its_grace_period_before_being_forgotten` |

前两项最初**没有**失败，原因是测试只回拨了 episode 的时间、没有回拨沉淀节点的时间，以及断言只看「节点还在」而 Dormant 节点依然可召回。补齐后三项才真正咬人。
