# ADR-062 M4：检索质量 before/after Benchmark 报告

**报告日期**：2026-09
**分支**：`bugfix/memory`
**依据**：ADR-062 §5.2（指标与阈值）/ §5.4（报告存档）/ §6.4（auto_inject min_score）
**harness**：`core/acowork-memory/tests/memory_m4_bench.rs`
**探针**：`core/acowork-runtime/tests/memory_m4_probe.rs`、`memory_m4_tiediag.rs`（临时，验证分数域与 MRR 波动；已删除，见下方路径说明）

> 路径说明：本报告的数字在 Runtime 内置记忆时代测得，当时这些文件位于 `core/acowork-runtime/tests/`。ADR-101 把记忆引擎拆成独立服务后，仍存在的 benchmark（m4/m5/retrieval_eval/lifecycle/chains）随逻辑迁入 `core/acowork-memory/tests/`，两个临时探针已删除——它们是一次性诊断，结论已在下文记录。指标数学（Precision@K / MRR / nDCG）与本报告各数字同源，统一定义在 dev-only 的 `core/acowork-eval`，因此本报告与迁入后的 harness 仍可逐列对比。

---

## 1. 摘要

对 ADR-062 P2 门禁的两个改动跑 before/after 检索质量 benchmark：

- **D1（P0）：检索路径排除 Dormant 节点**（`MemoryQualityConfig.exclude_dormant`，默认 true）
- **D2（P1）：auto_inject 的 `min_score` 从写死 0.3 改为走 `quality.min_score`（默认 0.0）**

**结论：D1 效果显著且可量化（Precision@5 +41%，Dormant 垃圾进上下文比例 0.3 → 0）；D2 修复方向正确，但本环境无法直接量化其命中率收益（详见 §4 分数域实证）。auto_inject 门禁可通过（§6）。**

| 指标 | before（D1 off, min_score 0.3） | after（D1 on, min_score 0.0） | §5.2 阈值 | 判定 |
|---|---|---|---|---|
| Precision@5 | 0.5667 | **0.8000** | ≥0.5 且提升 ≥20% | ✅ 达标 |
| Recall@5 | 1.0000 | 1.0000 | —（参照） | ✅ |
| MRR（确定性管线） | 1.0000 | 1.0000 | —（参照） | ✅ |
| Dormant 垃圾进上下文比例 | 0.3000 | **0.0000** | =0 | ✅ 达标 |
| auto_inject 命中率 | 100% | 100% | ≥60% | ✅ 达标* |

\* 见 §4：本环境下命中率 before/after 相同是**合理且一致**的（分数域为 BM25，min_score=0.3 本就不过滤）；不构成 D2 无效的证据。

---

## 2. 方法

### 2.1 固定语料（deterministic）

10 个 Knowledge 节点，经真实写入链（`MemoryStoreTool → process_memory_store`）写入 in-memory `GrafeoStore`：

- 5 个 ground-truth 相关节点（A1–A5，Active，conf=0.9/imp=0.8）
- 3 个垃圾节点（D1–D3，写入后经 `transition_to_dormant` 置为 Dormant，imp=0.1）
- 2 个干扰节点（N1–N2，Active，部分词重叠）

### 2.2 固定查询集（5 条，每条带 ground-truth node id）

```
dark mode editor   → A1
Shanghai river home→ A2
Acme backend engineer→ A3
Japanese language code→ A4
cats pets at home  → A5
```

### 2.3 指标

- **Precision@5 / Recall@5 / MRR**：复用 `grafeo::retrieval_metrics::evaluate_retrieval_quality`
- **Dormant 垃圾比例**：检索结果中 status == Dormant 的占比（对每条 query 的 top-10 结果）
- **auto_inject 命中率**：5 条 query 走 `MemoryQuery::auto_inject` 检索，返回非空的比例

### 2.4 环境与确定性

- 使用 `DeterministicEmbedding`（`procedural_embedding_fallback`，384 维，同文本同向量），检索语义跨运行可复现。
- **MRR 在默认 `graph_expand=true` 下不可复现**（根因见 §5），故主表采用确定性管线 `graph_expand=false` 跑。P@5 / Dormant 垃圾 / auto_inject 在两种管线下的数值一致（成员型指标不受排序随机性影响）。

---

## 3. Before/After 结果（3 次运行完全一致）

```
metric                                  before       after
----------------------------------------------------------
Precision@5                             0.5667      0.8000
Recall@5                                1.0000      1.0000
MRR                                     1.0000      1.0000
Dormant garbage ratio                   0.3000      0.0000
auto_inject hit rate %                  100.00      100.00
----------------------------------------------------------
```

- **D1 收益**：Precision@5 提升 **+41%**（0.567→0.800，达标 ≥20%）；Dormant 垃圾比例 **0.3→0**（达标 =0）。Recall 不受损（保持 1.0），说明 Dormant 排除未过度收紧召回。
- **D2 收益（本环境）**：命中率 before/after 相同（见 §4 解释）。

---

## 4. 关键发现一：分数域实证（修正 ADR-062 §6.4 假设）

### 4.1 探针结果

`memory_m4_probe.rs` 对单节点 + `min_score` 变体的实测：

```
raw text search scores:                [(NodeId(0), 0.8630462173553426)]
hybrid scores (no min_score):          [(NodeId(0), 0.8630462173553426)]
hybrid scores (min_score=0.3):         [(NodeId(0), 0.8630462173553426)]
auto_inject min_score=Some(0.3) → 1 result, score 0.6437
auto_inject min_score=None    → 1 result, score 0.6437
```

### 4.2 事实链

1. **hybrid 搜索退化为单 source（text-only）**：`hybrid_search_full` 内部调 `db.hybrid_search`，但当 vector 索引无数据时只剩 text source；`grafeo-engine fuse_results` 对**单 source 直接返回原始 BM25 分数**（不做 RRF）。
2. **分数域是 BM25（~0.86），不是 ADR §6.4 假设的 RRF（k=60 → 最高 ~0.016）**。
3. 因此 **`min_score=0.3` 在 BM25 域下不过滤任何结果**——auto_inject before/after 命中率相同（100%）是合理一致的，**不**是 D2 无效。

### 4.3 根因：写入路径绕过 vector 索引填充

- `GrafeoStore::store_knowledge → store_node → db.create_node_with_props`
- `grafeo-engine create_node_with_props` **只自动插入 text index，不插入 vector index**；而 `set_node_property` 才会插入 vector index。
- 结果：通过标准写入链写入的节点，`embedding` 属性已落库，但 **HNSW vector 索引为空** → vector source 不参与 → hybrid 退化 text-only。
- 生产环境需经 `rebuild_embeddings` / `migrate_embedding_dimension` 才填充 vector 索引。

### 4.4 对 ADR-062 的修正建议

- ADR-062 §6.4 原文："`min_score: Some(0.3)` 落在 RRF 分数域（`1/(k+rank)`，k=60 → 最高约 0.016）会过滤掉几乎全部结果。"
- **修正**：该论断仅在 vector 索引参与融合（双 source RRF）时成立。在 vector 索引未填充（text-only BM25 域）的环境下，`min_score=0.3` 不过滤任何结果。
- **D2 修复仍应保留**：它消除的是"生产环境 vector 索引被填充后 min_score=0.3 静默过滤全部"的隐患（防御性修复），只是本次 benchmark 环境无法量化其命中率收益。
- **后续航向**：该"融合分阈值"机制已于 [ADR-082](./ADR-082-memory-storage-sqlite-vector-fts.md) 整体废除，改为各源在各自分数域把门 + 名次融合（`min_cosine`，余弦绝对域）。本文 §4.1 的分数域事实链是该决策的直接输入。

---

## 5. 关键发现二：MRR 在默认管线不可复现（生产缺陷）

### 5.1 现象

- 初始 harness（`graph_expand=true` 默认）下，MRR 跨进程波动：0.6667 / 0.7667 / 0.8667 / 0.9 / 1.0。
- 同一对节点（如 A1/D1）的分数跨进程**对调**（3.5987 ↔ 2.1558），即不仅是排序 tie，而是**分数本身依赖无序遍历**。

### 5.2 决定性实验

`memory_m4_tiediag.rs` 三种配置 × 2 次运行：

| 配置 | 同对节点分数跨进程 | 各 query rank |
|---|---|---|
| `graph_expand=true`（默认） | 对调（3.5987↔2.1558） | 波动（1/2/3） |
| `graph_expand=false` | 稳定 | 全部 rank=1，跨运行一致 |
| `graph_expand=false, pagerank=0` | 稳定 | 全部 rank=1，跨运行一致 |

### 5.3 根因

- `manager.rs` 在 `enable_graph_expand=true` 时对检索结果做 **PageRank boost**（`apply_pagerank_boost`）。
- `compute_pagerank`（小图走 `CALL grafeo.pagerank`，失败回退 `compute_pagerank_fallback`）内部使用 **HashMap/HashSet（RandomState 随机种子）迭代构建邻接与分数**，导致 PageRank 分数跨进程不确定 → 近 tie 节点排序随机。
- **这是生产检索的真实非确定性缺陷**（同一次查询在不同进程返回不同排序），与 D1/D2 无关。

### 5.4 影响与建议

- 对 M4 门禁：成员型指标（P@5 / Dormant / auto_inject）不受影响；报告改用确定性管线测得 MRR。
- **建议后续**：`compute_pagerank_fallback` 的 HashMap/HashSet 迭代改为确定性（按 NodeId 排序）；或在 `manager.rs` 最终排序对 tie 加确定性二级键（node_id）。可作为 ADR-062 后续/独立 P 级事项排期。

---

## 6. 门禁结论（M5 建议）

### 6.1 auto_inject：**打开（方案 A）**

- §5.2 全部可测指标达标：Precision@5（+41% ≥20% 且 ≥0.5）、Dormant 垃圾 =0、auto_inject 命中率 ≥60%（100%）。
- D1（Dormant 排除）是当初关闭 auto_inject 的核心原因（"垃圾进上下文"），现已关闭该漏洞。
- D2（min_score 0.3→0.0）防御性修复已随 M2 落地，消除了未来 vector 索引启用后误杀全部结果的风险。

**触发策略（方案 A：首次触发，与 [ADR-060 §6.3](../adr/zh/ADR-060-prompt-cache-friendly-context-block-reorg.md#63-auto_inject_enabled-触发策略未来开启时) 一致）**：

- **每 session 最多触发一次**——在 session 第一条 user message 时执行 `retrieve_and_inject_memories()`，成功后置 `AgentLoop.memory_retrieved_for_session = true`；后续轮次通过该标记在 [loop_memory.rs:93](core/acowork-runtime/src/agent/loop_memory.rs#L93) 早返回。
- **不采用每轮触发**（方案 B）：每轮注入到 Block A 会让 Open 128-token hash 链与 Anthropic prefix cache 全失效（ADR-060 §3.2 已否决）；每轮触发还需先把注入位置从 Block A 挪到 append/Block C-style 路径（ADR-060 §11 #2 未做），属独立工作项。
- **不采用首次+显著变化重触发**（方案 C）：场景概率低——auto_inject 是会话开始的"热身上下文"，session 中段新增 memory 通常已经被后续 `memory_recall` 工具调用或下一 session 覆盖，追加重触发收益有限；事件通道（consolidation_bg → AgentLoop）目前未实现（ADR-060 §11 #4）。
- **per-agent 差异**：触发模式（`FirstTurn` / `FirstTurnPlusChange`）未来做成 manifest 配置项（M5 默认 `FirstTurn`），不同 agent 类型可按需覆盖。

**M5 默认值变更**：`MemoryManagerConfig.auto_inject_enabled: false → true`（[manager.rs:172](core/acowork-memory/src/manager.rs#L172)）。零配置 = 首次触发；agent 在 manifest `[memory.quality]` 显式设 `auto_inject_enabled: false` 可关闭。

> ⚠️ **后续回退（2026-09）**：该默认值已回退为 `false`（per-agent opt-in，经 manifest `[memory.quality].auto_inject_enabled = true` 显式开启，开启后首轮触发一次）。原因：与 LLM 自主 `memory_recall` 双路径召回重复——两条路径同以 user 消息为 query（auto_inject：全 4 labels / limit=5 / expand_hops=0 / hint=Identity；deep_recall：全 4 labels / limit=10 / expand_hops=2 / hint=Semantic），核心节点必然重叠。配套措施：`memory_recall` 工具描述已加防重复召回提示（"do NOT re-run the same query"），是否去重交由 LLM 判断。

> 📝 **后续修订（2026-10）**：上文引用的 `expand_hops` 已随双源查询拆分改造整体删除——该字段从未被 `MemoryManager::retrieve()` 消费（死字段），且后端已从 Grafeo 图库迁移至 SQLite（ADR-082）。`memory_recall` 现以 `MemoryQuery.embedding_text`（当前轮用户消息）驱动向量源、`query_text`（LLM 关键词）驱动 BM25 源；工具描述中的 "graph neighbors" 说法同步移除。详见 `docs/design/zh/05-memory.md` §检索能力。

### 6.2 keyword_index：**打开（方案 Y：写时拼入 `object`）**

**事实基线**：

- 关键词当前仅以 `metadata["keywords"]` JSON 数组持久化（[instant.rs:229-245](core/acowork-grafeo/src/consolidation/instant.rs#L229-L245)），不参与任何 BM25 索引。
- BM25 索引字段白名单固定（[grafeo.rs:200-221](core/acowork-grafeo/src/grafeo.rs#L200-L221)）：Knowledge 的 "content"（由 subject/predicate/object 派生）、Procedural/Autobiographical 的 "content"、以及 KNOWLEDGE_TEXT_FIELDS 列出的具体字段。
- `MemoryQualityConfig.keyword_index: bool` 已定义（[quality.rs:163](core/acowork-memory/src/quality.rs#L163)），默认 `false`、manifest 镜像同步（[manifest.rs:389](core/acowork-core/src/manifest.rs#L389)），零落地实现。

**方案 Y 决策**：

- **写时拼入**：当 `quality.keyword_index=true` 且 `input.keywords` 非空时，把 keywords 拼接到 Knowledge `object` 字段的 BM25 索引面。`object` 已为 derived 字段（content index），追加 `format!(" Keywords: {kw1} {kw2} ...")` 既不污染用户可见内容（object 派生不影响 subject/predicate/object 显示），又让 BM25 自动覆盖。
- **检索路径零改动**：text 索引命中 keywords 后，hybrid_search 既有路径自然带回该节点；不需要新增 hybrid source、不依赖 grafeo-engine 版本升级。
- **可逆性**：门控在 `quality.keyword_index`；false 时维持 metadata-only 现状（与 M4 报告基线一致）。切换零成本。
- **不采用方案 Z（每 keyword 独立 property）**：需扩 KNOWLEDGE_TEXT_FIELDS，影响面外溢到所有读路径的字段白名单扫描；零收益大于零成本。
- **不采用方案 W（grafeo-engine 加 keyword source）**：依赖外部 crate 版本同步，跨边界且收益不对称。

**M5 验收条件**：

- 单元测试：写 `keywords=["shanghai"]` → BM25 `object` 索引可命中 "shanghai"；`keyword_index=false` 时 `object` 不含 "shanghai"。
- benchmark 复测：新增 keyword 专项查询集（基于"仅靠 keywords 命中"的查询），P@5 / Recall@5 验证打开 vs 关闭的差值。
- Manifest `[memory.quality].keyword_index = true/false` 双向生效。

> 📝 **2026-09 后续验收事实补注**：验收条件中 "仅靠 keywords 命中" 隐含了 "BM25 唯一性" 假设——`hybrid` 检索路径下、`vector` 分支在任何 query embedding 上都报 cosine ≠ 0（`DeterministicEmbedding` 是确定性 hash），所以 "只靠 text 拒命" 不能严格成立。memory_m5_bench 以 `hit_rate` 为 饱和指标时 both before/after 都为 1.0；实际能区分 keyword_index 效果的是 MRR（0.7917 → 0.9375）；现测试断言已改为 `after.mrr > before.mrr`，保留 p5/r5/mrr 回归护栏。本节计划验收中 `单元测试 / keyword_index=false` 仍然有效（决定 BM25 索引能否拒绝命中），但 "benchmark 必须验证 hit@5 开/关差值" 一项变为以 MRR 作为实质度量——待 M5 harness 进一步在真实语料下补入“vector signal 去除”隔离开关。

### 6.2.1 Keyword 质量门（M5 必做前置）

**问题**：当前 keyword 唯一来源是 LLM（`memory_store` 工具调用参数，[memory_store.rs:271-276](core/acowork-runtime/src/tools/builtin/memory_store.rs#L271-L276)），零清洗/限长/去重/停用词过滤。[05-memory.md §3.3](../design/zh/05-memory.md) 原本设计 Runtime 从 `memory_hint.e` 提取作为**确定性主源** + LLM 可选补充（"LLM 甚至可以不填"），但 v3.10 简化（[05-memory.md:144,1151](../design/zh/05-memory.md#L1151)）把 Runtime 链路删了，**未同步更新设计文档**——这是 §6.2 直接折入 `object` 的潜在放大风险（garbage keywords → 放大进 BM25 → 检索污染；与 ADR-062 §1 已对 `confidence`/`importance` 警告的同类 LLM 锚定问题）。

**决策（轻量写时门，Option A）**：在写入路径增加确定性清洗，**无论 `keyword_index` 是否打开都生效**（保证 `metadata["keywords"]` 自身也干净，不分裂）。

- **位置**：纯函数 `acowork_memory::keyword::sanitize(input: Vec<String>) -> Vec<String>`；在 `memory_store.rs`（LLM 边界）与 `instant.rs`（防御兜底、幂等）双向调用。
- **规则**（按序）：
  1. `trim()` + 长度过滤：`0 < len ≤ 30`（防整句污染）
  2. 小写化（与 BM25 分词器 token 大小写一致）
  3. 字符过滤：必须含至少一个 ASCII alpha 或 CJK 字符（过滤纯数字 / 纯标点）
  4. 去重（case-insensitive，按小写后字符串）
  5. 停用词过滤：内置 ~30 个常见无意义 token（`the, a, user, fact, memory, note, info, data, thing, item, stuff, kind, type, way, something, anything, everything, one, two, three, yes, no, ok, okay, um, uh, hmm, oh, ah, wow`，中文标点空串等）
  6. 数量上限：单节点 ≤ 8 keywords（超出截断，保留前 8）
- **可观测性**：清洗掉的 keyword 数量 + 各规则触发分布埋点（`memory_write_keyword_gate` structured event），用于 M3.6 阈值校准后续分析。
- **工具描述同步更新**：`memory_store` 工具描述加入 "Provide short lowercase tokens (≤30 chars), avoid duplicates and common stopwords"——降低 LLM 锚定到默认 bad case 的概率。

**不做的**（避免范围蔓延）：
- 词频/全局常见 token 黑名单（依赖跨节点统计，需另设 P3）
- 词干提取 / 同义词归并（依赖 NLP 库，超出 M5 范围）

**M5 验收条件扩展**：
- 单元测试覆盖每条规则的边界（空串、超长、纯数字、停用词、重复、超 8 个）
- BM25 命中测试：清洗后的 keyword 在 `object` 索引中可被搜到
- 回归测试：现有 e2e 用例 `test_memory_store_metadata_params_inmemory` 在清洗后仍通过
- manifest `[memory.quality]` 不新增 keyword 相关开关（清洗是 always-on 的写入约束，非可配参数）

### 6.5 M5 总体步骤

| 步骤 | 改动文件 | 验证手段 |
|---|---|---|
| 1. `auto_inject_enabled` 默认值改 true | [manager.rs](core/acowork-memory/src/manager.rs) | 单元测试（默认值断言）+ benchmark harness 验证命中率 |
| 2a. **keyword 写时质量门**（§6.2.1 前置） | `acowork-memory/src/keyword.rs`（新模块）+ [memory_store.rs](core/acowork-runtime/src/tools/builtin/memory_store.rs) + [instant.rs](core/acowork-grafeo/src/consolidation/instant.rs) 双向调用 | 单元测试覆盖 6 条规则边界 + 工具描述更新 + 现有 e2e 回归 |
| 2b. keyword_index 写时拼入 `object`（依赖 2a） | [instant.rs](core/acowork-grafeo/src/consolidation/instant.rs) | 单元测试（清洗后 BM25 命中）+ benchmark |
| 3. manifest `[memory.quality].keyword_index` / `auto_inject_enabled` 反序列化已就位（无需改动） | — | harness 显式设值覆盖验证 |
| 4. 扩展 benchmark：keyword 专项查询集 + auto_inject 状态扫描 | [memory_m4_bench.rs](../../../core/acowork-memory/tests/memory_m4_bench.rs) | 重命名为 `memory_m5_bench` 或追加 keyword 配置开关 |
| 5. 跑 before/after 对比 | — | 输出表格（Precision@5 / Recall@5 / MRR / Dormant / auto_inject hit / keyword hit） |

**回滚路径**：所有改动参数化（`quality.auto_inject_enabled` / `quality.keyword_index`），回滚成本 = 改配置。代码逻辑无破坏性改动。

### 6.6 设计原则（保留与新增）

- **保留**：`MemoryQualityConfig` 集中化参数（ADR-062 §4.1）、Dormant 排除（M1）、min_score 修复（M2 D2）。
- **新增**：触发策略配置化（per-agent 覆盖），但 M5 不引入新维度，仅保留 `auto_inject_enabled`/`keyword_index` 两个布尔门控。
- **明确不做**：方案 B（每轮触发 + Block C append）、方案 C（首次+变化重触发）—— 见 §6.1 决策理由。

### 6.7 后续待办（聚合）

按优先级与依赖关系整理（M5 之后的剩余事项）：

| 优先级 | 事项 | 来源 | 工作量 |
|---|---|---|---|
| **P1** | `compute_pagerank` 非确定性修复（`compute_pagerank_fallback` HashMap/HashSet → 按 NodeId 排序） | M4 §5.4 | 小（局部修改 + 回归测试） |
| **P1** | 验证生产 vector 索引填充路径（`rebuild_embeddings` / 启动迁移）是否在写入链后自动执行 | M4 §4.3 | 中（生产环境实证） |
| **P2** | 删除 hint_weights 空转代码（`_text_weight`/`_vector_weight`/`_graph_weight` 在 [manager.rs:318](core/acowork-memory/src/manager.rs#L318) 空转） | ADR-062 §4.2 / M4 报告 §6.3 | 小 |
| **P2** | `RRF k` 参数化（grafeo-engine 需支持） | ADR-062 §4.2 | 中（依赖上游版本） |
| **P2** | 泛化路径 0.8 vs 离线 0.7 阈值统一决策（M3.6 阈值校准延伸） | ADR-062 §4.2 | 中 |
| **P3** | auto_inject 注入从 Block A 挪到 Block C append（解锁方案 B） | ADR-060 §11 #2 | 大（需 Provider cache_control 改造） |
| **P3** | consolidation_bg → AgentLoop 通知通道（解锁方案 C） | ADR-060 §11 #4 | 中（事件通道 + 通知机制） |
| **P3** | M3.6 阈值校准（confidence/importance 分布方差 + 阈值合理性） | ADR-062 §6.6 | 中（需先有埋点数据） |
| **P3** | M4 score-domain 探针发现的根本解决：补 `create_node_with_props` 写 vector index | M4 §4.3 | 小（grafeo-engine wrapper 1 处） |

**原则**：每项都有明确入口（来源列）与工作量估算，避免"待办黑洞"。

---

## 7. 复现

```bash
cd core
cargo test -p acowork-memory --test memory_m4_bench   -- --nocapture   # 主 benchmark
cargo test -p acowork-runtime --test memory_m4_probe    -- --nocapture   # 分数域探针（文件已删除，见开头路径说明）
```

自包含、使用 in-memory `GrafeoStore`，不触碰运行中的 Gateway / Runtime / Desktop 进程或端口。

---

## 8. M5 执行结果（2026-09）

**harness**：`core/acowork-memory/tests/memory_m5_bench.rs`（自 `15654af0` 安全快照恢复，`set_quality` 辅助因 `b117f901` 撤回而替换为 trait 方法 `apply_quality_config`）。

### 8.1 代码改动清单（对应 §6.5 步骤）

| 步骤 | 改动 | 验证 |
|---|---|---|
| 1 | `MemoryManagerConfig.auto_inject_enabled` 默认 `false → true`（[manager.rs](core/acowork-memory/src/manager.rs)），首轮触发逻辑（`loop_memory.rs` `memory_retrieved_for_session`）已存在 | 默认值断言更新 |
| 2a | 新增 `acowork-memory/src/keyword.rs`（`sanitize`/`sanitize_with_stats`，6 条规则 + 30 停用词 + cap 8）；`memory_store.rs` LLM 边界与 `instant.rs` 持久边界双向调用（幂等）；工具描述更新；`memory_write_keyword_gate` 埋点 | 10 个规则边界单测 + e2e 回归 |
| 2b | `instant.rs`：`quality.keyword_index=true` 时清洗后 keywords 拼入 BM25 `object` 字段（Plan Y） | 单测（off 时 object 干净 / on 时含 `Keywords: …`）+ bench |
| 3 | manifest `[memory.quality].auto_inject_enabled` 新增（草案与 §6.5 声称"已就位"均不实）+ `agent_core::init_memory_manager` 注入；`keyword_index` 镜像已有 | core manifest 测试 |
| 4/5 | `memory_m5_bench` before/after | 见 §8.2 |

### 8.2 Before/After 结果（`enable_graph_expand=false` 确定性管线）

```
metric                                  before       after
----------------------------------------------------------
Precision@5 (full)                      0.5000      0.8750
Recall@5 (full)                         0.6250      1.0000
MRR (full)                              0.6250      1.0000
keyword hit@5 rate (K* only)            0.0000      1.0000
keyword any-rank rate (K* only)         0.0000      1.0000
----------------------------------------------------------
```

- **Plan Y 生效**：`keyword_index=false` 时 BM25 完全无法命中 K*（content 词法隔离，keyword hit@5 = 0）；`true` 时写时拼入 `object` 使 BM25 全量命中（1.0）。
- **无回归**：after P@5 / R@5 ≥ before（M4 基线不损）。
- **质量门前置**：K_CORPUS 的 keywords 全部通过 `sanitize`（`santorini/vacation/summer-2024`、`graphql/schema-stitching/federation`、`beekeeping/apiary/honey-extraction` 均为干净 token），fold 内容无污染。

### 8.3 结论

- **auto_inject**：默认关闭（per-agent opt-in），经 manifest `[memory.quality].auto_inject_enabled=true` 显式开启（开启后首轮触发一次，ADR-060 §6.3）。M5 曾默认开启，因与 LLM 自主 `memory_recall` 双路径召回重复回退为 opt-in（见 §6.1 注记）。
- **keyword_index**：默认 `false`（per-agent 经 `[memory.quality].keyword_index=true` 开启）——能力 + 质量门落地，M4 基线保持；benchmark 已证明开启后 keyword 专项检索从 0 命中到全量命中。
- 质量门（`sanitize`）**always-on**，与 `keyword_index` 解耦，保证 `metadata["keywords"]` 自身干净。

### 8.4 复现

```bash
cd core
cargo test -p acowork-memory --test memory_m5_bench -- --nocapture   # M5 keyword before/after
cargo test -p acowork-memory --lib keyword                             # 质量门规则单测
```

