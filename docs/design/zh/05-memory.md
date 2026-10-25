# Memory 仿生分层架构

> 版本：v3.7 | 更新日期：2026-04-22

> 本文档基于 `docs/_internal/archive/review/zh/07-memory-competitive-review.md` 与 `08-memory-benchmark-review.md`（本地归档）的设计补充。主要变更：新增 Abstention 拒答机制（§6.5）、冲突检测升级为三层信号模型（§6.4）、新增质量评估框架章节（§11）、即时/离线巩固边界明确化（§4）、检索权重动态调整（§6.6）。

> **v3.8 变更（2026-05-28）**：上下文压缩策略大幅简化，程序化折叠策略全部放弃——见 [ADR-010](../../adr/zh/ADR-010-context-compression-simplification.md)。核心变更：移除内容折叠（Phase 1）、三阶段渐进裁剪、检索结果 8 级优先级、弹性预算分区。瞬态层压缩简化为：70% 告警 → 80% LLM 摘要（完整上下文） → 95% emergency_trim 安全网。

> **v3.9 变更（2026-05-28）**：经历层写入来源简化——见 [ADR-011](../../adr/zh/ADR-011-compaction-as-distillation.md)。核心变更：移除每轮对话实时写入记忆层，经历层仅通过 Compaction 摘要和 Session 关闭蒸馏写入。Compaction 与 Distillation 统一为单次 Compact Model 调用（"摘要即蒸馏"）。

> **v4.0 变更**：记忆层切到 SQLite（单库 + WAL + FTS5 + sqlite-vss 向量；沉淀层图关系由应用层 `MemoryNode.edges` 边表承担，不再依赖存储引擎原生 LPG/GQL；详见 [ADR-082](../../adr/zh/ADR-082-sqlite-memory-cutover.md)）。`acowork-grafeo` crate 已删除，记忆引擎代码（`EpisodicDistiller` / `RetrievalMetrics`）迁移至 `core/acowork-memory/`。

> **v4.1 变更（已实施，2026-10-23）**：记忆模块已从 Runtime 剥离为 Gateway 托管的独立进程 `acowork-memory`；Agent 侧经 MCP 调用该服务的工具（注册名 `mcp_memory__memory_store` / `mcp_memory__memory_recall`，工具级 opt-in 走 `agent_mcp_tools.json`，ADR-069），面板经 Gateway 反代访问；auto-inject 移除、`memory:read` / `memory:write` 权限项删除，Runtime 达到零记忆业务代码——详见 [ADR-101](../../adr/zh/ADR-101-memory-standalone-mcp-service.md)。**本文档其余章节仍以剥离前的（内置）形态行文**；分层数据模型（瞬态 / 经历 / 沉淀）、蒸馏与遗忘流水线语义不变，变的只是进程归属与访问通道。

---

Memory 采用**仿生分层**设计，以人类认知科学为参照。每个 Agent 拥有完全独立的私有 Memory（单文件 SQLite 数据库 `memory/private.sqlite`，应用层隔离；详见 [ADR-009](../../adr/zh/ADR-009-gateway-workspace-isolation.md)），不存在 Gateway 维护的公共数据库。跨 Agent 的数据共享通过 Intent 查询和系统 Agent 服务实现，而非共享存储。

**设计哲学**：记忆不是存储，是认知。一个没有遗忘的记忆系统是垃圾场，一个没有巩固的记忆系统是碎片堆，一个没有自我认知的记忆系统是数据库。Memory 模块要回答的不是"怎么存"，而是"怎么记、怎么忘、怎么想"。

```
┌─────────────────────────────────────────────────────────┐
│  瞬态层（Transient）                                     │
│  ───                                                    │
│  工作记忆 — LLM 上下文窗口                               │
│  当前对话、推理链、注意力焦点                             │
│  生命周期：单次会话                                      │
│  仿生对应：前额叶持续放电                                 │
├─────────────────────────────────────────────────────────┤
│  经历层（Experiential）                                  │
│  ───                                                    │
│  情景记忆 — episodes 表（`category` / `consolidated` 字段）│
│  交互片段、对话快照、感知原始记录                         │
│  sqlite-vss HNSW 向量索引 + SQLite FTS5 全文检索          │
│  生命周期：天→周，巩固后晋升至沉淀层                      │
│  仿生对应：海马体临时编码                                 │
├─────────────────────────────────────────────────────────┤
│  沉淀层（Consolidated）                                  │
│  ───                                                    │
│  语义记忆 — 事实、偏好、关系（KnowledgeNode）             │
│  程序记忆 — 行为模式、操作规则（ProceduralNode）          │
│  自传体记忆 — 自我认知、能力边界（AutobiographicalNode）  │
│  LPG 知识图谱 + GQL 原生关联扩散检索                      │
│  生命周期：长期至永久，遗忘衰减但不轻易删除               │
│  仿生对应：新皮层长期存储                                 │
└─────────────────────────────────────────────────────────┘

         ┌─── 巩固管道（ADR-068 2026-09 revision）───┐
         │                                          │
    瞬态层 ──(LLM 摘要)──→ 经历层   ← compaction / session 关闭
         │                                          │
    LLM ──(memory_store,即时)──→ 经历层   ← 只写 Episode,带 knowledge_subtype
         │                                          │
    经历层 ──(EpisodicDistiller 离线蒸馏,opt-in)──→ 沉淀层   ← 沉淀层唯一生产者
         │                                          │
    manifest ──(bootstrap,权威导入)──→ 沉淀层   ← 仅 Identity/Capability 直写
         │                                          │
    沉淀层 ──(遗忘衰减)──→ dormant → (可选 purge)
         │                                          │
    沉淀层 ──(关联扩散)──→ 多跳检索结果
```

## 0. 分层原则

**为什么按认知功能分而不是按存储位置分？**

旧版三层（工作记忆 / 私有记忆 / 云端同步）混淆了两个维度——"工作记忆"是认知功能，"私有记忆"是存储位置，"云端同步"是同步机制。仿生分层统一按认知功能划分，每层有明确的职责边界和信息流动规则。

**三层之间的流动规则：**

| 流动方向            | 机制     | 触发条件                                                                                                     |
| ------------------- | -------- | ------------------------------------------------------------------------------------------------------------ |
| 瞬态层 → 经历层     | 摘要写入 | Compaction 触发（80% token 使用）或 Session 关闭时，LLM 摘要异步写入经历层 SQLite `episodes` 表。不再每轮写入，避免与 JSONL 冗余 |
| 经历层 → 沉淀层     | 巩固管道 | 唯一管道 = 离线 `EpisodicDistiller`（ADR-068,per-agent opt-in:`[memory.distiller].enabled = true`）。**已下线**:即时提取直写沉淀层、PendingKnowledgeNode、rule-based generalization、offline compress_history_nodes、Relationship 自动生成(2026-09 revision) |
| 沉淀层 → 瞬态层     | 检索注入 | 用户输入到达时，检索相关记忆注入上下文。**默认关闭（per-agent opt-in）**：`MemoryManagerConfig::auto_inject_enabled = false`，开启后每 session 首轮触发一次（ADR-060 §6.3）；开启方式：manifest `[memory.quality].auto_inject_enabled = true`。历史：2026-09-12 因召回质量不足默认关闭（Dormant 垃圾进上下文等）；ADR-062 M5 曾默认开启（Dormant 排除 + min_score 修复 + keyword 质量门），后因与 LLM 自主 `memory_recall` 双路径召回重复（两条路径同以 user 消息为 query，核心节点必然重叠）回退为 per-agent opt-in，`memory_recall` 工具描述已加防重复召回提示。显式 `memory_recall` 工具不受影响 |
| 沉淀层/经历层内流动 | 关联扩散 | 检索时沿图边 1-2 跳扩展                                                                                      |
| 沉淀层 → Dormant    | 遗忘衰减 | 后台定期计算 decay_score                                                                                     |

**不可逆的单向门：** 经历层 → 沉淀层是信息精炼过程（原始片段 → 结构化知识），天然单向。但沉淀层 → 经历层可以通过"回忆"机制实现——用户或 Agent 主动触发时，从沉淀层提取关联知识，作为新的情景上下文注入瞬态层。

**分层与 SQLite 存储的映射：**

| 认知层 | 内容                        | SQLite 表                                                  | 说明                                              |
| ------ | --------------------------- | ----------------------------------------------------------- | ------------------------------------------------------------ |
| 瞬态层 | 工作记忆                    | 无（进程内）                                               | LLM 上下文窗口，纯进程内存                                |
| 经历层 | 情景记忆                    | `episodes`（+ `episodes_vec` 虚拟表 + `episodes_fts` 虚表） | sqlite-vss HNSW + FTS5 + 元数据；详见 [§2](#2-经历层情景记忆) |
| 沉淀层 | 语义/程序/自传体/Skill 经验 | `nodes`（`kind` ∈ {Knowledge, Procedure, Self}）+ `edges`  | 应用层知识图谱，关联扩散由应用层多跳实现 |

不存在"经历层节点存在沉淀层 `nodes` 中"的歧义——认知分层和 `kind` 列是一一映射的，存储格式为单文件 `memory/private.sqlite`（含 WAL/SHM 兄弟文件）。

## 0.1 LLM 优先原则

**信任 LLM 超过信任规则——除非规则能解决 LLM 不能解决的问题。**

记忆系统中涉及大量语义判断（什么值得记、confidence 多高、是否冲突、如何分类），这些判断由 LLM 完成而非规则引擎。具体应用：

- **即时提取**：LLM 自主判断是否调用 memory_store、评估 confidence（high/medium/low），Runtime 不做语义层面的二次检查
- **离线巩固**：三元组提取、冲突分类、证据验证由 LLM 在有完整上下文时执行，而非实时阶段的规则近似
- **Runtime 仅做机械护栏**：内容长度限制、调用频率限制、安全过滤——这些是 LLM 无法自我约束的机械性限制
- **摘要时实体/三元组提取**：Compaction 触发时，Compact Model 在生成摘要的同时提取实体和三元组。不再每轮提取（v3.10 简化）

## 1. 瞬态层：工作记忆

工作记忆是 Agent 当前正在"思考"的内容，直接映射到 LLM 的上下文窗口。

```
┌─ System Prompt ──────────────────────────┐
│  Agent 身份定义                            │
│  自传体记忆摘要（来自沉淀层注入）           │
│  Skill Instructions                       │
│  工具定义                                  │
├─ Retrieved Memory ───────────────────────┤
│  巩固层检索结果（语义/程序/自传体）         │
│  经历层检索结果（相似情景）                 │
│  关联扩散结果                              │
├─ Conversation ──────────────────────────┤
│  用户消息 + Agent 回复                     │
│  工具调用与结果                            │
│  （工具调用与结果保存在对话历史中）       │
├─ Scratchpad ────────────────────────────┤
│  Agent 内部推理链                          │
└──────────────────────────────────────────┘
```

**Compact Model 输出格式（v3.10 简化 + 2026-XX-XX 进一步收敛）：**

Compaction 不再生成 `entities` / `triples` 块——LLM 在压缩场景下生成的三元组质量不稳定（subject/predicate/object 简化为压缩信息），落地为 KnowledgeNode 反而污染沉淀层；entities 提取与即时提取路径重叠且缺少 sub_type/confidence 等元数据，价值低于 `memory_store` 工具路径。Compaction 的职责收敛为「生成可检索的摘要 + 可回放的意图」，沉淀层落地由专用管道承担（职责分离）。

```
<summary>
自然语言摘要文本...
</summary>
<user_intent>
当前对话用户的核心意图（可选）
</user_intent>
```

- **summary**：自然语言摘要文本，存入 `Episode.content`，同时用于向量检索（HNSW）与 BM25 全文匹配
- **user_intent**：当前对话用户的核心意图（可选块），存入 `Episode.metadata.user_intent`，便于历史回放时还原用户目标

沉淀层落地收敛为两条语义管道（ADR-068 2026-09 revision）：
1. **LLM 即时写经历层**：`memory_store` 只落 Episode（content + `knowledge_subtype`），不直写沉淀层——沉淀层类型判定与晋升全部交给离线蒸馏（§4.1）
2. **离线蒸馏**（唯一沉淀层生产者）：`EpisodicDistiller` 批量扫描 episode → 服务端 LLM 结构化提取 + embedding 聚簇 + LLM Judge → 晋升 Knowledge/Procedural/Autobiographical 节点（含 `promotion_metadata` 审计）；per-agent opt-in（`[memory.distiller].enabled = true`）。manifest bootstrap 仅对 Identity/Capability 权威直写（§3.3）

设计理由：
- 每轮提取的成本（~65 tokens/轮）在 ADR-011 之后不再合理——经历层不再逐轮写入，存储目的已消失
- 沉淀层落地质量优先于压缩阶段的一次性抽取：摘要用于检索回放，沉淀由专用管道分阶段精炼
- 检索策略始终使用默认权重，不做类型驱动的动态调整——经评估 f/r 类型的微调收益未被验证

**检索策略（v3.10 简化）：**

所有检索统一使用默认 RRF 权重（vector: 0.7, text: 0.3），不再基于 memory_hint 类型动态调整。HintType 枚举保留但仅在 `memory_store` 工具调用时由 LLM 显式指定（用于即时提取管道的 sub_type 分类）。

> **v3.13 更正（[ADR-082](../adr/zh/ADR-082-memory-storage-sqlite-vector-fts.md) C4 / P4）**：实现为**等权 RRF（`k = 60`）**——`hint_weights` 传入 `hybrid_search_full` 后即被忽略，加权 RRF 曾被显式否决（*"weight scaling after RRF is meaningless"*）。上文所述 vector 0.7 / text 0.3 的默认权重**尚未接入**，排序不体现查询意图。

**瞬态层的管理策略（v3.8 简化）**：

上下文压缩是一个语义理解任务，只有 LLM 能可靠判断哪些信息可以丢弃。程序化策略（字符截断、FIFO、角色折叠）本质是用 proxy 指标替代语义理解，必然失效。因此所有日常程序化折叠策略已被放弃，压缩简化为三阶段：

| 阶段              | 触发条件                  | 行为                                                                                                                                   |
| ----------------- | ------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| Stage 1: 监控     | 70% context 使用率        | 日志记录，不干预                                                                                                                       |
| Stage 2: LLM 摘要 | 80% context 使用率        | Compact Model 对完整上下文做 LLM 摘要。不做任何折叠/截断预处理。保护 system prompt + 最近 2-3 轮，中间段压缩。完整历史归档至临时文件。 |
| Stage 3: 紧急裁剪 | 95% / API ContextOverflow | emergency_trim（保留最后 N 条非 system），作为安全网                                                                                   |

> **设计决策**：详见 [ADR-010](../../adr/zh/ADR-010-context-compression-simplification.md)。

## 2. 经历层：情景记忆

情景记忆存储 Agent 与用户的交互片段，是记忆的"原始素材"。

```
SQLite `episodes` 行
├── episode_id: String              // 唯一 ID
├── timestamp: DateTime             // 发生时间
├── role: Role                      // user / agent / tool
├── content: String                 // 内容（对话原文或 Compaction 后的摘要）
├── embedding: Vec<f32>             // 语义向量
├── metadata: HashMap<String, Value>  // 上下文元数据（话题、情感倾向等）；不预存关联 node_id，跨层扩散通过 source_episode 反向查询实现
├── session_id: String              // 所属会话
├── consolidated: bool              // 是否已巩固到沉淀层
└── importance: f32                 // 重要性评分（写入时 LLM 打分 0.0-1.0）
```

> **v3.10 设计简化**：已移除 Episode 的 `content_type`（ContentType）和 `artifact_refs`（ArtifactRef）字段。
> Episode 内容不再做分类压缩——原始对话直接存储，Compaction 时由 Compact Model 输出摘要。
> 理由详见 [ADR-011](../../adr/zh/ADR-011-compaction-as-distillation.md)：Compaction = Distillation，摘要即蒸馏。

**关键设计决策：Episode 内容存储策略**

- **v3.10**：Episode 内容不再做分类压缩。对话原文直接完整存储，摘要由 Compaction 阶段的 Compact Model 生成。
- **Compaction**：当上下文使用率达 80% 时，Compact Model 对完整上下文做自然语言摘要（含实体和三元组提取），摘要写入经历层蒸馏 Episode。
- 理由详见 [ADR-011](../../adr/zh/ADR-011-compaction-as-distillation.md)：Compaction = Distillation，摘要即蒸馏。

**检索能力（基于 `SqliteStore` provider trait）：**

- **语义检索**：`MemoryStore::vector_search` — sqlite-vss HNSW 向量索引，支持余弦距离
- **关键词检索**：`MemoryStore::text_search` — SQLite FTS5 全文索引，`unicode61` 分词器
- **混合检索**：`MemoryStore::hybrid_search` — 应用层 RRF 融合排序（vector + text 分数按 `1/(k+rank)` 加权求和）
- **双源查询拆分（memory_recall，2026-10）**：`MemoryQuery.embedding_text` 承载当前轮用户完整消息驱动向量源（整句 embedding 质量远高于关键词袋），`query_text` 承载 LLM 提取的关键词驱动 BM25 词法源；`embedding_text` 缺省（或全空白）时回落为 `query_text` 单源（历史行为）。用户消息与 session ID 经 `ToolContext`（`execute_with_context`）在每次工具调用时由 loop 从 per-session 状态注入——**不得**存放在 agent 级共享的 `MemorySessionHandle` 或工具实例上（多会话并发下 last-writer-wins 竞态，2026-10 已修复）。
- **MMR 去重**：`MemoryStore::mmr_search` — Maximal Marginal Relevance，保证结果多样性，避免重复语义
- **时间过滤**：按时间范围缩小检索空间
- **跨层关联扩散**（§6）：**设计目标，当前未实现**。`MemoryQuery.expand_hops` 字段从未被 `MemoryManager::retrieve()` 消费，已作为死字段删除；后端已从 Grafeo LPG 图库迁移至 SQLite（ADR-082），不存在 `edges` 表多跳遍历。§6 保留为未来实现参考。

**Embedding 生成策略：**

Embedding 由 Runtime 层通过 `EmbeddingProvider` trait 生成（而非 `SqliteStore` 内部），以 `Vec<f32>` 形式传入 `Episode` / `MemoryQuery`。

**Provider 降级链**：Ollama local（primary，`nomic-embed-text`，768d）→ Remote API（fallback，OpenAI-compatible `/embeddings`，512-1536d）。`FallbackEmbeddingProvider` 自动管理 primary→fallback 切换（2 次连续失败 + 200ms 超时）。

**生成时机**：
- 检索时：`MemoryManager.retrieve()` 方法头部自动生成 embedding（200ms 超时），超时/失败则 `query.embedding = None`，退回 `text_search` 纯文本检索
- 写入时：episode 蒸馏写入时同步生成 embedding，同样 200ms 超时降级

`SqliteStore` 仅负责存储和索引，不持有 `EmbeddingProvider`。

**经历层的遗忘：**

情景记忆的遗忘比沉淀层更激进——这是自然的，因为海马体本身就是临时编码区。

- **默认保留期**：14 天（可配置）
- **巩固标记**：已被提取到沉淀层的情景标记 `consolidated = true`
- **清理策略**：
  - 已巩固 + 超过 7 天 → 自动清理（知识已转移到沉淀层，原始片段不再需要）
  - 未巩固 + 超过 14 天 + importance < 0.3 → 清理（低价值且未被提取的碎片）
  - 未巩固 + 超过 14 天 + importance >= 0.3 → 保留并尝试离线巩固

> **实现状态（v3.12，P2 G13）**：差异化清理策略已落地于 `core/acowork-memory/src/consolidation/offline.rs` 的 `run_episodic_cleanup()`，由生产路径 `run_offline_consolidation` 调度。单阈值删除接口 `cleanup_episodes` / `cleanup_old_episodes` 无生产调用方（仅 trait 定义 + 测试 stub），按 Rule of three 不再重复实现。

## 3. 沉淀层：长期记忆

沉淀层是 Agent 的"知识根基"，包含三种记忆类型，全部存储在 `nodes` 表（按 `kind` 列区分），图关系由 `edges` 表承载。

### 3.1 语义记忆（KnowledgeNode）

存储从交互中提取的结构化知识——事实、偏好、关系。

```rust
struct KnowledgeNode {
    node_id: String,
    node_type: KnowledgeType,        // Fact / Preference / Relation
    subject: String,                 // 知识主体（通常是"用户"）
    predicate: String,               // 关系/属性
    object: String,                  // 值/目标
    confidence: f32,                 // 置信度 0.0-1.0
    source_episode: Vec<String>,     // 来源情景 ID（可追溯）
    created_at: DateTime,
    updated_at: DateTime,

    // === 遗忘机制字段 ===
    importance: f32,                 // 写入时 LLM 打分 0.0-1.0
    access_count: u32,               // 检索命中次数
    last_accessed: DateTime,         // 最后一次被检索
    decay_score: f32,                // 运行时计算的衰减分数
    status: NodeStatus,              // Active / Dormant / Purged
    dormant_since: Option<DateTime>, // 进入 Dormant 状态的时间（Purge 90 天计时起点）

    // === 隐私级别 ===
    privacy: PrivacyLevel,           // Public / Personal / Sensitive
}

enum KnowledgeType {
    Fact,        // 事实："用户住在北京"
    Preference,  // 偏好："用户喜欢简洁的回复"
    Relation,    // 关系："用户的经理是王五"
}

enum NodeStatus {
    Active,     // 正常参与检索
    Dormant,    // 衰减低于阈值，不参与常规检索但保留
    Purged,     // 已清除（仅 purge 操作）
}

enum PrivacyLevel {
    Public,     // 可跨 Agent 共享（如用户姓名）
    Personal,   // Agent 私有（如用户偏好风格）
    Sensitive,  // 敏感信息，打包分享时剥离
}
```

**节点之间的关系边：**

```
KnowledgeNode:张三 ──[LIVES_IN]──→ KnowledgeNode:北京
KnowledgeNode:张三 ──[PREFERS]───→ KnowledgeNode:简洁回复
KnowledgeNode:张三 ──[MANAGED_BY]→ KnowledgeNode:王五
KnowledgeNode:北京 ──[IS_CAPITAL_OF]→ KnowledgeNode:中国
```

边也有属性——权重（strength）、来源、创建时间。边的权重影响关联扩散的传播强度。

**边权重计算规则：**

```
edge_strength = min(0.8, confidence_avg × recency_factor)

其中：
- confidence_avg = (source_node.confidence + target_node.confidence) / 2
- recency_factor = exp(-0.01 × days_since_edge_created)
  （边的衰减比节点慢，半衰期约 69 天，因为关系比事实更持久）
- 上限 0.8 防止任何单条边权重过高导致扩散偏向
```

边的权重在创建时计算，后续 decay_scan 时同步更新。边不独立存储 decay_score——边的存亡取决于两端节点：任一端被 purge 时，相关边自动删除。

### 3.2 程序记忆（ProceduralNode）

存储"在什么情况下该怎么做"的行为模式，与 Skill 系统互补。

```
Skill 系统的程序记忆：SkillExperience（Skill 级别，特定技能的执行经验）
沉淀层的程序记忆：ProceduralNode（跨 Skill 的通用行为模式）
```

```rust
struct ProceduralNode {
    node_id: String,
    trigger_condition: String,       // 触发条件："用户连续两次纠正格式"
    action_pattern: String,         // 行为模式："停止使用 Markdown 表格，改用纯文本列表"
    confidence: f32,                 // 置信度
    activation_count: u32,           // 被激活应用的次数
    source_skill: Option<String>,    // 来源 Skill（如有）
    learned_from: String,            // "用户反馈" / "执行失败" / "自我评估"

    // 遗忘字段（同 KnowledgeNode）
    importance: f32,
    access_count: u32,
    last_accessed: DateTime,
    decay_score: f32,
    status: NodeStatus,
    dormant_since: Option<DateTime>,  // 进入 Dormant 的时间（Purge 90 天计时起点）

    created_at: DateTime,
    updated_at: DateTime,
}
```

**与 SkillExperience 的关系：**

| 维度     | ProceduralNode           | SkillExperience                                    |
| -------- | ------------------------ | -------------------------------------------------- |
| 作用域   | 跨 Skill 的通用行为      | 特定 Skill 的执行经验                              |
| 来源     | 用户反馈 / 执行失败总结  | Skill 每次执行的记录                               |
| 注入位置 | System Prompt 的行为准则 | Skill Instruction 的经验补充                       |
| 示例     | "用户不喜欢长回复"       | "weekly-report Skill 在 qwen3:8b 上需要扁平化指令" |

**程序记忆与 Skill 经验的联动：**

当一个 ProceduralNode 的 `source_skill` 非空时，它与对应 Skill 的 SkillExperience 形成交叉引用。例如，weekly-report Skill 多次因"输出太长"被用户纠正 → SkillExperience 记录 failure_case → 巩固管道提取出通用 ProceduralNode："此用户偏好简洁输出" → 这个 ProceduralNode 会影响所有 Skill 的执行，不只是 weekly-report。

### 3.3 自传体记忆（AutobiographicalNode）

存储 Agent 对自身的认知——"我是谁、我能做什么、我的边界在哪"。这是人格连续性的基础。

```rust
struct AutobiographicalNode {
    node_id: String,
    aspect: AutobiographicalAspect,  // 自我认知的维度
    content: String,                 // 具体内容
    confidence: f32,
    source: String,                  // "manifest"（bootstrap 权威导入）/ "offline_consolidation"（EpisodicDistiller 晋升）
                                        // ADR-068 2026-09 revision: memory_store 的 autobiographical source 标注已下线,
                                        //        生产者收敛为 manifest bootstrap 与 EpisodicDistiller 两类
    updated_at: DateTime,

    // 自传体记忆不参与遗忘衰减——这是 Agent 的核心身份
    // 但可以被更新（如用户改名、Agent 学会了新 Skill）
    //
    // ⚠️ status 始终为 Active，遗忘扫描跳过此类型节点
    // schema 中 status 列对 AutobiographicalNode 不可修改
}

enum AutobiographicalAspect {
    Identity,           // 身份声明："我是天气助手，帮助你了解天气信息"
    Capability,         // 能力范围："我能查询全球城市天气、给出穿衣建议"
    Limitation,         // 能力边界："我无法预测超过 7 天的天气"
    Preference,         // 自身偏好："我倾向于先给结论再解释原因"
    History,            // 重要经历："2026-04-14 用户教我生成周报，这是我的第一个 Skill"
    Relationship,       // 与用户的关系："我和张三合作了 3 个月，他喜欢简洁风格"
}
```

**自传体记忆的来源（ADR-068 2026-09 revision 收敛）：**

1. **Manifest 派生**（权威导入,启动时直写）：从 `manifest.toml` 的 `agent_id/name/description/display_name/role` 与 `capabilities` 列表生成 Identity 和 Capability 节点,`source = "manifest"`,幂等 upsert(同 key 覆盖)。这是唯一不经 `EpisodicDistiller` 的沉淀层直写——manifest 是权威数据源,不属于"经历→沉淀"语义归纳
2. **离线蒸馏识别**（`EpisodicDistiller`）:服务端 LLM 从 `knowledge_subtype` 标注的 episode 识别 autobiographical 候选(布尔分类 + 4 值 aspect),经 Step 4 LLM Judge 仲裁后晋升为 AutobiographicalNode(含 `promotion_metadata` 证据链/审计)

**已下线（2026-09 revision,ADR-068 M5/M6/M8）:**
- ~~memory_store 的 `autobiographical` category 与 `aspect`/`key`/`source` 参数~~——schema 收窄为 4 类(§4.1),与 agent 相关的内容写 `category: fact/preference` 即可
- ~~自我评估自动生成 Limitation~~——`success_count` 从未被递增,统计驱动路径已整体删除(见 §9 Phase 2 移除原因)
- ~~Relationship 自动生成~~——30 天合作规则的直写路径已删除,Relationship 由蒸馏器从 `knowledge_subtype=Relation` episode 晋升
- ~~autobiographical 幂等 upsert / History append-only 直写~~——沉淀层节点统一由蒸馏器管理生命周期

**自传体记忆的注入：**

自传体记忆摘要始终注入 System Prompt 的最前面（在 Agent 身份定义之后），作为 Agent 的"自我认知背景"：

```
## 关于你自己

你是「天气助手」，帮助用户了解天气信息。
你能查询全球城市天气、给出穿衣建议，但无法预测超过 7 天的天气。
你和张三合作了 3 个月，他偏好简洁的回复风格。
你在 qwen3:8b 模型上复杂推理的成功率约 60%。
```

**自传体容量管理：**

AutobiographicalNode 不参与遗忘，但需要容量控制防止无限膨胀：

- **History 节点摘要压缩（已下线,2026-09 revision）**：离线 `compress_history_nodes`(10 条自动合并)已整体删除——History 摘要由 `EpisodicDistiller` 的事件归纳统一承担,不再存在独立的规则式合并路径
- **注入上限**：自传体摘要注入 System Prompt 时，按重要性取 Top-K（Identity / Capability / Limitation 必注入，History 取最近 5 条摘要 + 最近 3 条明细，Relationship 取 Top-3）
- 总 token 预算：自传体不超过 200 token（约 150 个中文字符）

## 4. 巩固管道

巩固管道是经历层→沉淀层的信息精炼过程，模拟海马体→新皮层的记忆巩固。

### 4.1 即时提取（Phase 1）

即时提取通过 **Tool Call 机制**实现——`memory_store` 作为 Agent 的内置工具之一，LLM 在生成回复时自主判断是否调用。无需额外的 LLM 调用、异步管道或预过滤规则。

**即时提取产出定义（ADR-068 2026-09 revision）**：

`memory_store` 是**经历层薄写入器**——LLM 入口只写 `Episode`（带 `knowledge_subtype` 路由提示），不再直写任何沉淀层节点：

```
Episode：
  content              自然语言内容（不拆三元组）
  normalized           可选——去掉对话语境后仍然成立的单句陈述（ADR-068 2026-10）。
                       蒸馏器的主输入与召回/embedding 键。留空时
                       Episode::statement() 回退到 content，因此旧数据无需迁移
  knowledge_subtype    Fact | Preference | Relation | Procedure（LLM 自选 4 类之一）
  confidence           LLM 自评（high/medium/low → 0.85/0.7/0.5，可选）
  keywords             经 ADR-062 keyword sanitize 门禁后进 metadata（可选）
  privacy / importance 可选

已下线：PendingKnowledgeNode / status=Pending / confidence>=0.85 直写 KnowledgeNode
——这些机制随 ADR-068 M5/M6 整体删除。沉淀层晋升是 EpisodicDistiller 的唯一职责。
```

> **`normalized` 为什么由写时模型产出**：「这句话去掉语境后还成立什么」是语义判断，写时模型手上有完整对话上下文，离线重读者没有。让离线管线重新提取一次，等于用一个信息更少的判断覆盖一个信息更多的判断。

**设计决策：Tool Call 而非单独调用**

| 维度          | 单独调用 LLM                         | Tool Call（当前选择）                    |
| ------------- | ------------------------------------ | ---------------------------------------- |
| 额外 API 成本 | 每轮 0-1 次额外调用                  | 零额外调用                               |
| 架构复杂度    | 高（异步管道 + 队列 + WAL + 预过滤） | 低（工具定义天然集成）                   |
| 预过滤        | Runtime 硬编码规则                   | LLM 自主判断（天然过滤器）               |
| 上下文共享    | 需重新输入对话                       | 共享当前对话上下文                       |
| 用户可观测性  | 黑箱（异步管道不可见）               | 透明（tool call 在对话历史中可见）       |
| Token 开销    | 0（按需调用）                        | 每轮多 ~150 token（工具定义 + 提取指引） |

选择 Tool Call 的核心理由：即时提取的目标是"能用"而非"完美"。LLM 天然具备判断"什么值得记住"的能力——"今天天气如何"不值得存，它自己就知道。Phase 3 的离线巩固再用专用 prompt 做深度提取补漏。

**memory_store 工具定义（ADR-068 M5 收窄版,4 类 Episode-only）**：

```json
{
  "name": "memory_store",
  "description": "把值得长期记住的信息写入经历层。仅在对话中包含新的、重要的、非临时性信息时调用。不要存储显而易见的常识或临时性信息。",
  "parameters": {
    "type": "object",
    "properties": {
      "content": {
        "type": "string",
        "description": "要记住的内容，用自然语言描述（如「用户住在上海」），不需要拆分成三元组"
      },
      "category": {
        "type": "string",
        "enum": ["fact", "preference", "relation", "procedure"],
        "description": "信息类型（= knowledge_subtype 路由提示）：fact=客观事实, preference=用户偏好, relation=人物/实体关系, procedure=行为模式（when X do Y）。沉淀层落地类型（Knowledge/Procedural/Autobiographical 节点）由离线 EpisodicDistiller 依据 episode 语义判定，工具层不承诺类型"
      },
      "confidence": {
        "type": "string",
        "enum": ["high", "medium", "low"],
        "description": "置信度：high=用户明确表达的, medium=推测的, low=不确定的。LLM 自己判断，可选，默认 medium"
      },
      "keywords": {
        "type": "array",
        "items": { "type": "string" },
        "description": "关键词，可选。Runtime 侧经 keyword sanitize 门禁后写入 episode metadata，供 BM25 检索"
      }
    },
    "required": ["content", "category"]
  }
}
```

**接口简化设计理由（ADR-068 修订）**：
- 旧设计要求 LLM 拆分三元组 `{subject, predicate, object}`，负担重且不可靠（同一事实可能拆出不同 predicate）——ADR-068 进一步移除所有结构化字段（subject/predicate/object/trigger_condition/action_pattern/key/aspect/source），LLM 只给自然语言 + 4 类 subtype
- keywords 由 LLM 提供、Runtime 过 sanitize 门禁，不依赖 memory_hint
- 沉淀层三元组结构化与类型判定全部收敛到离线 `EpisodicDistiller`（服务端 LLM 提取 + LLM Judge，见 §4.2）——不存在第二条 LLM 直写沉淀层的通道
- `autobiographical` 类目与 `aspect`/`key` 参数已下线：与 agent 自身相关的内容写 `category: fact/preference/relation` 即可，是否晋升 AutobiographicalNode 由蒸馏器判断（ADR-068 §3.4 Step 4 Judge）

**即时阶段 Prompt 职责（ADR-068 修订）**：

即时提取的 Prompt 职责限定为**轻量写入经历层**，与离线蒸馏的深度晋升有明确分工：

```
即时阶段 Prompt（约 100 tokens）：
  ─ 信息筛选：判断本轮是否包含值得长期记住的新信息
  ─ 类型标注：category = fact / preference / relation / procedure（= Episode.knowledge_subtype）
  ─ 关键词提取：可选，供 BM25 检索（Runtime 侧 sanitize）
  ─ 置信度评估：confidence = high / medium / low

  不做的事：
  ✗ 关联发现（无法跨轮次）
  ✗ 冲突判定（缺乏完整上下文）
  ✗ 模式提炼 / 行为归纳（伪规则已下线,归 LLM Judge）
  ✗ 沉淀层落库（晋升由 EpisodicDistiller 离线完成）
  ✗ 自传体识别（autobiographical category 已移除,由蒸馏器 Step 2a 判定）
```

**System Prompt 中的提取指引（ADR-068 修订）**：

```
## 记忆管理

你可以使用 memory_store 工具把值得长期记住的信息写入经历层。使用原则：
- 用户透露了新的个人信息（住址、职业、家庭成员等）→ 存为 category: fact
- 用户表达了偏好或风格（"我喜欢简洁的回复"）→ 存为 category: preference
- 用户提到了人物/实体间关系（"张三是我同事"）→ 存为 category: relation
- 用户反复纠正你的行为模式（"别用表格了"）→ 存为 category: procedure

不要存储：临时性信息、已存储的重复知识、显而易见的常识。
confidence 由你判断：用户明确表达的 → high，推测的 → medium，不确定的 → low。
```

**即时提取流程（ADR-068 revision）**：

```
用户消息到达
   │
   ▼
LLM 生成回复（含 tool call 判断）
   │
   ├─ LLM 判断"无值得记住的信息"
   │   → 仅生成自然语言回复
   │   → 对话内容仍通过 compaction/session 写入经历层（episode）
   │
   └─ LLM 判断"有值得记住的信息"
       → 生成自然语言回复
       → 同时调用 memory_store({content, category, confidence?, keywords?})
       → Runtime 执行工具调用：
           ├─ category → knowledge_subtype 映射（fact/preference/relation/procedure）
           ├─ keywords 过 ADR-062 sanitize 门禁
           └─ 组装 Episode 落经历层（不直写沉淀层）

离线蒸馏时（EpisodicDistiller，per-agent opt-in）：
  → 批量扫描 knowledge_subtype 标注 + unconsolidated 的 episode
  → Step 2a 服务端 LLM 结构化提取 + autobio 候选识别
  → Step 3 embedding 聚簇 + 证据累计（min_evidence 阈值）
  → Step 4 LLM Judge 仲裁 → 晋升沉淀层节点（含 promotion_metadata 证据链）
  → 失败/证据不足 → 原 episode 保留待重试
```

**关键行为保证：**

- **不强制提取**：LLM 有权不在每轮调用 memory_store。简单问候、天气查询等不存储，这比预过滤规则更智能
- **LLM 入口只写经历层**：memory_store 永不直写 Knowledge/Procedural/Autobiographical 节点——沉淀层晋升是 EpisodicDistiller 的唯一职责（ADR-068）
- **对话始终记录**：每轮对话内容写入 JSONL 文件（瞬态层），经历层仅通过 Compaction 摘要 / Session 关闭蒸馏写入（ADR-011）
- **工具调用可见**：memory_store 的调用记录在对话历史中，用户知道 Agent 记住了什么
- **防重复晋升**（distiller 侧）：蒸馏器晋升前对候选 episode 做 embedding 聚簇——同类证据达到 `min_evidence` 阈值才晋升；对已晋升主题的重复 episode 由 Step 4 LLM Judge 判定合并或拒绝，`promotion_metadata` 保留完整证据链可审计回滚

### 4.2 离线蒸馏（ADR-068，EpisodicDistiller）

> **实现状态（ADR-068 M1-M8 落地；ADR-071 触发/配置接线 2026-09 已实现 W1–W5）**：离线巩固由 `EpisodicDistiller`（`core/acowork-memory/src/consolidation/distiller.rs`）承载。后台调度与运行时配置按 [ADR-071](../../adr/zh/ADR-071-distiller-runtime-config-and-trigger.md)：触发口径与 legacy Pending 解耦（基于 unconsolidated episode 积压/空闲）、配置分层（manifest 初值 → `agent_config.json` 运行时层）、手动蒸馏端点、模型选择复用摘要模型 UI、蒸馏 prompt 纳入 ADR-063 per-agent 覆盖、记忆面板"记忆蒸馏"卡片与"立即蒸馏"按钮已上线。旧 Phase 3 规划中的 PendingKnowledgeNode 升级制、rule-based generalization、自动自我评估、History 压缩均已下线。

**蒸馏输入/输出（2026-10 收敛为「投影 + 合并」，见 ADR-068 Revision(2026-10)）**：

```
输入：consolidated=false 且带 knowledge_subtype 标注的 Episode
  （LLM 端唯一写入方 = memory_store 工具，§4.1）
  陈述文本 = Episode::statement()（normalized 优先，回退 content）——
  写路径的 embedding 与这里的召回键共用同一个函数，避免两处回退逻辑漂移

每条 episode 两步（详见 ADR-068 §3.4）：
  ① 扫描候选 episode（knowledge_subtype 过滤 + batch_size 上限，newest-first）
  ② 按 statement() 召回同 label 下相似度 ≥ merge_recall_threshold 的既有语义节点
     ├─ 召不到（0 候选）→ **投影**：statement() 直接落成
     │    KnowledgeNode / ProceduralNode / AutobiographicalNode，零 LLM 调用
     └─ 召到（1..K 候选）→ **合并**：一次 LLM 调用判
          merge / no_merge / contradicts，输入规模与 store 大小无关
  ③ 标记 episode consolidated（仅投影/合并成功时；失败或无模型时保持未沉淀，
     下一轮重新考虑，不留墓碑）

输出：沉淀层节点（唯一生产者）+ 完整审计（DistillerResult.funnel 记录
  projected / merged / deferred / llm_calls / below_importance，
  DistillerResult.promotion_evaluations 记录每节点证据链与 reasoning）
```

**为什么只剩一次 LLM 调用**：写时模型已经判断过「这条经历值得记」，离线再判一次「它有没有价值」是重复且不更可靠的判断。旧管线（LLM 结构化提取 → embedding 聚簇 → LLM Judge → 5 个 min_evidence 门槛 + 墓碑）在线上出现过开了数周、269 条 episode、0 条沉淀的情况——触发条件每次都满足，问题在链路。新管线对同一份存量数据零 token 即产出 33 条沉淀，有模型时 3 轮清空积压。

**体积控制只有一个旋钮**：`min_importance`（默认 0.0 = 全投影）。旧管线的 5 个证据门槛全部删除。语义层过密时收紧它即可；未投影的 episode 不留墓碑，放宽阈值后会被重新考虑，不丢历史数据。

**可信沉淀方式原则（2026-09 revision）**：沉淀层节点只允许两类语义生产者——
1. **图/统计归纳**：基于 `nodes`/`edges` 表统计节点/边关系（应用层实现）
2. **LLM 分析归纳**：`EpisodicDistiller`（投影 + 合并；LLM 只在合并裁决时被调用）
规则式替代（字符串全等计数、文本特征 grep、30 天/10 条等启发式）一律不得用于"经历→沉淀"语义归纳；规则只保留在幂等/去重门槛、生命周期、权威数据源导入（manifest bootstrap）、事件触发判定四类位置。

**蒸馏触发与配置（ADR-071）**：

- **配置分层**：`manifest [memory.distiller]` = 包作者初值（enabled/参数/模型/周期）；`agent_config.json` = 运行时层，首次运行无参数时按 manifest 初始化一次，此后界面只读写 `agent_config.json`（与 temperature/context_window 等既有参数同构）。字段全 Option，None = 回落 manifest → 系统默认
- **后台触发（与 legacy Pending 计数解耦）**：周期到点（`distiller_interval_minutes`，默认 60）∧（unconsolidated episode 积压 ≥ `distiller_accumulation_threshold`(默认 50) ∨ 空闲 ≥ `distiller_idle_minutes`(默认 30)）→ 跑 `run_episodic_distiller_step`；受 `distiller_enabled` 门控（默认 false，opt-in 保持）
- **手动触发**：`POST /memory/distill` 立即跑一次（绕过周期，与后台共用同一实现；仍守 opt-in，关闭时返回 409）；`consolidation/status` 返回蒸馏配置与上次运行结果。记忆面板底部"合并节点"按钮已退役，替换为"立即蒸馏"（legacy consolidate action 删除，episodic cleanup 随周期 consolidation 自动执行）
- **蒸馏模型**：独立字段 `distiller_model`（provider_id/model_id），UI 复用摘要模型下拉逻辑（vault keys + provider 名，与 Harness compact-model 卡片同源）；解析链 agent_config → manifest → `default_compact_model` → provider 第一模型（现状保底）
- **prompt per-agent**：`distiller-merge.md` 进 ADR-063 覆盖白名单（原 `distiller-extraction.md` / `distiller-judge.md` 两槽随提取段、Judge 段下线合并为一槽），Debug 界面 PromptList 可见可编辑，reload 生效；`DistillerConfig.extraction_prompt_override`/`judge_prompt_override` 在 `distiller_scheduler_config()` 组装时注入；`acowork-memory` 内置常量保留为默认
- **配置热更新**：`ConsolidationTimer` 持有 `RwLock<SchedulerConfig>`，PUT agent config 后 `update_config()` 换值、后台 loop 每 tick 重读（≤60s 生效），不重建后台任务
- 失败/证据不足的 episode 原样保留，下轮重试；不存在降级到规则路径的 fallback

**与即时提取的区别：**

| 维度   | 即时提取（memory_store,LLM 端）          | 离线蒸馏（EpisodicDistiller,服务端）  |
| ------ | ---------------------------------------- | ------------------------------------- |
| 触发   | LLM 在回复时自主调用                     | 后台周期任务（per-agent opt-in）      |
| 粒度   | 单轮对话中的显式信息                     | 跨 episode 的证据聚簇 + 语义归纳      |
| 写入   | 只写 Episode（content + knowledge_subtype） | 晋升沉淀层节点（含审计证据链）     |
| LLM 判断 | LLM 自评 confidence/category + normalized 陈述  | 仅合并裁决时调用模型（投影零调用）  |
| 成本   | 零额外 API 调用（工具定义随主回复）      | 每条 episode 0 或 1 次调用（有候选才 1 次）|

## 5. 遗忘机制

遗忘不是记忆的失败，是记忆的优化。没有遗忘的记忆系统会退化——检索效率下降、无关信息干扰决策、存储资源无限增长。

> **v3.12 重写（ADR-057 §5.3 redesign）**：遗忘模型从"多维度规则分支"收敛为**单一时间衰减**。
> 旧模型（consolidated × importance × 7/14 天分支、decay_score = importance × activity_signal 乘法模型、access_boost 访问加权）
> 存在两个问题：① 规则复杂但缺乏说服力——7/14 天的"一步到位二值踢出"不符合渐进遗忘的直觉；
> ② `access_count` 在检索路径从不自增，access_boost 形同虚设。
> 新模型只回答一个问题：**这条经历多久没被想起了？** 半衰期是唯一核心参数，默认 180 天，开放到
> `agent_config.json` 允许用户自定义。

### 5.1 经历层：单一时间衰减（默认）

经历层（Episodic）是真实事件记录，本身有检索价值。遗忘是**渐进的**：随时间推移检索权重平滑下降，
半衰期前全权参与，之后越老排名越靠后，最终彻底遗忘——不是到点就"删除"。

**保留率公式（半衰期指数衰减）：**

```
retention = exp(-ln2 × age_days / half_life_days)
```

- `half_life_days`：半衰期（默认 **180 天**）。到达半衰期时 retention = 0.5，每过一个半衰期再减半（2⁻¹、2⁻²、2⁻³…）
- `retention` 单调递减、永不归零——即使 3 个半衰期后（540 天）仍有 12.5% 的权重

**两级渐进降级，不做二值踢出：**

1. **检索降权（渐进）**：检索排序时经历层分数 × retention。半衰期前 retention ≈ 1（全权参与），
   之后随年龄平滑下降——旧记忆仍可被检索到，但自然排在更靠后（`MemoryManager` 检索路径，与扫描
   共用同一半衰期曲线）
2. **状态降级（阈值触发）**：retention < `dormant_threshold`（默认 0.1，≈ 3.3 个半衰期 ≈ 600 天）时
   节点 Active → Dormant，退出检索
3. **归档（最终遗忘）**：Dormant 状态持续超过 `archive_days`（默认 90 天）→ 写入 PurgeLog 归档
   （30 天可恢复）

**参数（`agent_config.json`，全部开放可配置）：**

| 字段 | 默认值 | 含义 |
| --- | --- | --- |
| `memory_forgetting_enabled` | false | 总开关，默认关闭（关闭时扫描零开销、检索不降权） |
| `memory_forgetting_half_life_days` | 180 | 半衰期（天）——**核心参数** |
| `memory_forgetting_dormant_threshold` | 0.1 | retention 低于此值 → Active → Dormant |
| `memory_forgetting_archive_days` | 90 | Dormant 满多少天 → 归档（PurgeLog，30 天可恢复） |

**为什么半衰期默认 180 天？**

经历层是真实事件记录，7/14 天就遗忘过快——用户几个月前的一次重要对话仍有检索价值。180 天（≈ 6 个月）
对应"一年内的经历保持高权重"，3 个半衰期后（约 1.6 年）才进入 Dormant，符合直觉。

**调度（`consolidation_bg` 后台任务）：**

- 周期性执行 `run_episodic_decay_scan()`（间隔默认 1 小时，`forgetting_interval_secs`）
- 开关关闭时直接返回零值（零开销）
- 旧 Pending 计数触发（accumulation / idle-timeout）已删除——Pending 状态自 ADR-068 起无生产者

### 5.2 沉淀层：暂不衰减

沉淀层（Knowledge / Procedural / Autobiographical）是长期记忆，**暂时不衰减**：

- 事实性知识（Fact / Relation）只沉睡不删除——"曾经住北京"是历史事实
- 偏好（Preference）、行为模式（Procedural）的过时判定需要语义判断，不能靠时间一刀切
- 自传体节点是核心身份，遗忘 = 人格断裂，永不衰减

沉淀层沿用旧的乘法衰减模型（`forgetting/scan.rs` + `run_decay_scan`，importance × activity_signal），
但**当前不在任何生产路径调度**——保留实现，待语义层衰减需求明确后再启用。

> **实现保证（有端到端测试覆盖）**：`run_episodic_decay_scan` 的扫描集合是 `WHERE label = 'Episodic'`，语义标签根本不在候选集里，因此「沉淀层不被遗忘」由查询形状保证，不靠运行时判断。同理，遗忘是两阶段——Active → Dormant（retention < `dormant_threshold`）→ 超过 `archive_days` 才 purge，且 purge 前整行复制进 `purge_log`，是**可恢复的归档而非删除**。
>
> 覆盖这些不变量的测试在 `core/acowork-memory/tests/memory_lifecycle_e2e.rs`：沉淀在其证据被遗忘后仍可召回；衰减扫描误扫 Knowledge 会被捕获；`dormant_threshold`、`archive_days`、`enabled` 各自的失效都有对应断言。每条断言都经过变异测试确认能失败。

### 5.3 不参与遗忘的节点

| 节点类型                           | 是否遗忘   | 原因                      |
| ---------------------------------- | ---------- | ------------------------- |
| EpisodicNode（经历层）             | 是（§5.1） | 真实事件记录，时间衰减    |
| KnowledgeNode / ProceduralNode     | 否（§5.2） | 沉淀层暂不衰减            |
| AutobiographicalNode               | 否         | 核心身份，遗忘 = 人格断裂 |
| KnowledgeNode（identity 类）       | 否         | 用户姓名、语言等基础身份  |
| SkillExperience                    | 专用衰减   | 按 Skill 系统规则管理     |
| SkillDraft / Iteration / Execution | 开发期保留 | 调试完成后归档            |

### 5.4 实现映射

| 能力 | 代码位置 |
| --- | --- |
| 衰减引擎（扫描 + 归档） | `core/acowork-memory/src/forgetting/episodic_decay.rs` |
| 配置定义 | `core/acowork-memory/src/types.rs::EpisodicDecayConfig` |
| 调度（后台任务） | `core/acowork-runtime/src/memory/consolidation_bg.rs` |
| 检索渐进降权 | `core/acowork-memory/src/manager.rs`（`RetrievalForgettingConfig`） |
| 配置开放 | `agent_config.json` 的 `memory_forgetting_*` 字段 + 前端"记忆遗忘"卡片 |

## 6. 关联扩散检索

> **⚠️ 状态（2026-10）：本章为设计目标，当前未实现。** `MemoryQuery.expand_hops` 从未被 `MemoryManager::retrieve()` 消费，已作为死字段从代码中删除；`memory_recall` 工具描述中的 "graph neighbors" 说法同步移除（后端为 SQLite，ADR-082，无图遍历能力）。本章保留为未来实现参考，其中的字段名（`expand_hops` / `expand_threshold`）不代表现存 API。

传统检索是"查到什么就是什么"，关联扩散是"查到一个，带出一串"——模拟海马体的模式完成和激活扩散。沉淀层节点间关系存于 `edges` 表（应用层图遍历），跨层扩展通过 SQL JOIN + 早停实现，最多 3 跳。

> **v4.0 变更**：原 `MemoryProvider::graph_expand_*` / `create_memory_edge` / `apply_pagerank_boost` 等图原生 trait 方法随 ADR-082 D4 删除。本节描述的功能由应用层多跳查询实现，不再依赖存储引擎 LPG/GQL。`PageRank` / `topology_boost` / `MATCH (m)-[r*1..3]-(other)` / `CALL grafeo.pagerank()` / `CALL grafeo.louvain()` 等 API 全部下线。

### 6.1 检索流程（v4.0）

检索同时查询经历层和沉淀层，并支持跨层关联扩散。`MemoryManager` 根据 SQLite 检索耗时决定降级：

```
Level 0（正常模式）
  前提：vector_search 与 text_search 双通路均可用
  策略：hybrid_search（应用层 RRF 融合）+ 多跳跨层扩展（应用层 edges JOIN）
  SLA：P99 < 200ms

  ↓ 向量索引不可用或 embedding 生成超时

Level 1（无向量模式）
  策略：text_search only（FTS5 trigram）+ 多跳跨层扩展
  SLA：P99 < 100ms

  ↓ SQLite 查询超时（>300ms）或索引异常

Level 2（缓存模式）
  策略：返回 Autobiographical 文本缓存 + 最近 5 条 Episode
  SLA：P99 < 10ms

  ↓ SQLite 完全不可用

Level 3（内存模式）
  策略：仅返回当前会话工作记忆，无持久化检索
  SLA：P99 < 1ms
```

**单次检索预算分配（500ms 硬超时）**：

```
① embedding 生成：≤200ms（超时→跳过向量，text_search only）
② hybrid_search：≤150ms（应用层 RRF 融合，向量与 BM25 并行）
③ 多跳扩展：≤100ms（应用层 edges JOIN，早期终止，超时返回已扩展节点）
④ 排序+格式化：≤50ms
任一环节超时，使用已有部分结果继续后续步骤。
```

**检索流程**：

```
用户输入 / Agent 内部查询
   │
   ▼
① 并行检索两层数据
   ├─ 经历层 hybrid_search（label=Episodic）：sqlite-vss HNSW + FTS5 + RRF
   │   返回 Top-K 相似情景
   └─ 沉淀层 hybrid_search（label=Knowledge/Procedural/Autobiographical）：同上
       返回 Top-K 匹配知识
   │
   ▼
② 多跳跨层扩展（应用层 edges JOIN）：
   - 经历层 episode → 通过 KnowledgeNode.source_episode 反向查询关联的沉淀层节点
   - 沉淀层 node → 通过 edges 表 JOIN 扩展到其他沉淀层节点
   - 1 跳：直接关联（边权重 > 0.3）
   - 2 跳：间接关联（累积路径权重 > 0.1）
   - 3 跳：复杂推理（累积路径权重 > 0.05），通过早期终止实际大多在 1-2 跳停止
   │
   ▼
③ 去重 + 评分
   - 直接匹配节点分数 = RRF 分数
   - 扩展节点分数 = 路径权重 × 源节点 RRF 分数
   - 同一节点可能同时被经历层和沉淀层命中，取最高分
   │
   ▼
④ 截断返回
   - 总结果数受 Token 预算限制
   - 优先返回直接匹配，扩展节点作为补充
```

### 6.2 示例

```
用户问："上海明天下雨吗？"

① 并行检索：
   经历层 → Episode: "上周用户说下周要去上海出差"（语义相似）
   沉淀层 → KnowledgeNode: "用户住在北京"（"上海"关键词匹配）
            KnowledgeNode: "用户经常去上海出差"（"上海"关键词匹配）

② 多跳跨层扩展（应用层 edges JOIN）：
   "上周出差提到" episode → 反向查 source_episode → KnowledgeNode: "出差时关心天气"（经历→沉淀）
   "经常去上海出差" KnowledgeNode ──[PREFERS]→ ProceduralNode: "出差时查天气"（沉淀内）
   "用户住北京" KnowledgeNode ──[PREFERS]→ "简洁的回复风格"（沉淀内）
   "经常去上海出差" KnowledgeNode → 反向扩展 → Episode: "上次上海出差淋了雨"（沉淀→经历）

③ 最终注入上下文：
   - 核心事实：用户住北京、经常去上海出差
   - 跨层扩展：出差时关心天气、上次上海出差淋过雨
   - 关联扩散：偏好简洁回复
   → Agent 回答："上海明天小雨，15-20°C。需要带伞——上次你上海出差淋过雨。"
```

没有关联扩散，Agent 只知道用户"住北京"或"去上海出差"，但不知道这两者之间的关联，也不知道用户出差时关心天气、上次淋过雨。跨层关联扩散让检索从"关键词匹配"升级到"语义推理"。

### 6.3 性能保障

- 扩展深度硬限制 **3 跳**（通过早期终止实际大多在 1-2 跳停止）
- `early_stop_threshold` 随跳数递增（1 跳: 0.1, 2 跳: 0.15, 3 跳: 0.2），越远越严格
- 每跳最多扩展 5 条边（按权重 Top-5）
- 扩展节点总数上限 20（防止 Token 膨胀）
- 只对 Active 节点做扩展，Dormant 节点不参与
- 经历层和沉淀层并行检索，扩展阶段串行（避免并发复杂度）
- 扩散阈值与 `MemoryQuery.expand_hops`/`expand_threshold` 字段联动；超出上限的请求按上限截断

详见 `docs/_internal/archive/review/zh/04-p2-s2-design-review.md` §6.2、§6.3

### 6.4 冲突处理（ADR-068 2026-10 revision — 收敛为合并裁决的一个分支）

**位置**：冲突只在蒸馏的**合并裁决**这一步被识别，即某条 episode 按 `statement()` 召回到 ≥ `merge_recall_threshold` 的既有语义节点、需要判断「是不是同一件事」时。不存在独立的冲突检测阶段——旧文档描述的 Step 3 embedding 聚簇与 Step 4 LLM Judge 已随 ADR-068 Revision(2026-10) 下线（聚簇把「相似」当「同一件事」，而是否同一件事是语义判断；`ConflictType` / `conflicts_evolution/correction/ambiguous` / `should_trigger_confirmation` 在删除前已是零引用死代码）。

**裁决输入/输出**：

```
输入：1 条新陈述 + 至多 merge_candidate_k（默认 5）条候选节点
      → 输入规模与 store 大小无关，回复是固定形状的小 JSON
输出：action = merge | no_merge | contradicts
      target_id（候选 id 或 null）、statement（合并后的单句陈述或 null）、reasoning

时间语义由模型自行从候选与 episode 的时间戳推断，不再有显式窗口规则。
```

**三条防线**（都有对应测试）：

| 风险 | 处置 |
|---|---|
| 模型幻觉出一个不存在的 `target_id` | 只接受本次请求里真实发给它的候选 id，否则整条 episode 延后重试，绝不改写无关节点 |
| 误合并不可逆 | 合并只追加 `source_episode_ids`，证据链完整保留，面板可人工纠正 |
| 召回到候选但没有可用模型 | 延后，不猜。猜错两个方向都不可逆（误合并丢证据、漏合并造重复节点） |

**阈值取向**：`merge_recall_threshold` 默认 0.65，刻意偏宽松。阈值只决定「谁被送去给模型判」，不决定「是否合并」——误候选的代价是一次 `no_merge` 调用，漏候选的代价是一个此后永不被重新聚拢的重复节点。语义层过密时应收紧 `min_importance`，而不是调这个阈值。

**已下线（ADR-068）**：即时路径的 conflict_candidate 标记、`ConflictType::Ambiguous`、`conflict_group_id`、新节点暂不参与 graph_expand——因为 `memory_store` 不再创建沉淀层节点，全部候选冲突只在蒸馏阶段被识别。

### 6.5 Abstention（拒答）机制（v3.7 新增）

**设计动机**：当检索结果的置信度不足时，Agent 应选择拒答而非生成可能不准确的回答。这是检索系统的最终质量门控——比检索降级策略（Level 0-3）更后置，是在检索结果已返回后的语义级别保障。

**门控阈值机制（v3.13 修正，见 [ADR-082](../adr/zh/ADR-082-memory-storage-sqlite-vector-fts.md)）**：

过滤**不作用于融合分**，而是各源在各自分数域内独立过滤后**取并集**：向量源按绝对余弦相似度 `min_cosine`（默认 0.3）过滤；文本源（BM25）命中即保留（BM25 含 IDF，无可靠绝对阈值）。融合分（RRF，仅含名次）不参与任何阈值判定。若过滤后结果为空，触发 Abstention（拒答）：

```
检索结果处理流程：
  ① hybrid_search 返回原始结果集
  ② 各源分别过滤：向量源移除 cos < min_cosine 的；文本源命中保留
  ③ 若过滤后结果为空：
     → 触发 Abstention
     → System Prompt 注入拒答指引：
       "当检索分数不足时回复'我不确定这个信息'，不要猜测"
     → Agent 基于自身通用知识回复，但明确标注不确定性
  ④ 若过滤后结果非空：
     → 正常注入检索结果到上下文
```

**默认阈值**：

| 阶段    | 阈值 | 说明                                               |
| ------- | ---------------- | -------------------------------------------------- |
| Phase 2 | `min_cosine = 0.3` | 余弦域噪声底；注意真实 embedding 各向异性强，实际过滤力度远小于字面（ADR-082 C1） |
| Phase 3 | 基于实际数据校准 | 根据在线评估的 NRR 指标和 LongMemEval 成绩动态调整；升级路径为对语料均值中心化 / 每查询 z-score（ADR-082 P5） |

> **实现澄清（v3.13，取代 v3.12 的 G9 说明）**：代码中现为**两个不同名的量**，勿混淆：
> - `AbstentionConfig.default_min_score = 0.6`（`abstention.rs`）：作用于 `check_abstention` 的 **raw scores**（向量/BM25 原始相似度），用于拒答判定。**当前尚未接入检索链路，属死代码**（ADR-082 P6）。
> - `MemoryQualityConfig.min_cosine = 0.3`（`quality.rs`）：作用于**向量源的绝对余弦**（`cos = 1 − distance`，对外归一化为 `(1 + cos)/2 ∈ [0,1]`），在 `hybrid_search_full` 内完成过滤。
> - 旧的 `MemoryManagerConfig.min_score`（融合 RRF 域，默认 0.0）**已删除**：它对**单源向量路径**（`score = cos − 1`，恒非正）等价于要求 `cos >= 1`，会静默滤光全部结果——即 2026-09 中文查询 `memory_recall` 返回 0 条的根因。详见 [ADR-082](../adr/zh/ADR-082-memory-storage-sqlite-vector-fts.md)。

**与检索降级策略的关系**：

Abstention 在 Level 0-3 降级策略之后生效，是最终的质量门控：

```
检索降级策略（§6.1）→ 返回原始结果集
  ↓
各源门控过滤（向量源 cos < min_cosine 移除；文本源命中保留）→ 移除低质量结果
  ↓
Abstention 判断 → 结果为空则触发拒答
  ↓
Agent 回复（有依据 / 明确拒答）
```

- Level 0-3 解决的是"SQLite 可用性"问题（硬件/软件故障）
- Abstention 解决的是"检索质量"问题（返回结果不可靠）
- 两者正交，互不替代

**LongMemEval Abs 维度目标**：

LongMemEval 的 Abs（Abstention）维度评估 Agent 在信息不足时是否选择拒答而非幻觉：

| 阶段    | Abs 目标 | 当前预期 | 说明                                         |
| ------- | -------- | -------- | -------------------------------------------- |
| Phase 2 | 60%+     | 40-50%   | 通过 min_cosine 门控 + System Prompt 注入实现 |
| Phase 3 | 75%+     | —        | 结合在线评估反馈和轻量 LLM Judge 优化        |

**可配置性**：

`min_cosine` 通过 `MemoryQuery` 参数传入（§10.3 `MemoryQuery.min_cosine` 字段），支持不同 Agent 不同阈值：

```rust
pub struct MemoryQuery {
    pub query_text: String,
    // 向量源查询文本（memory_recall 双源拆分，2026-10）：设置时向量通路 embed
    // 该文本（当前轮用户完整消息），query_text 继续驱动 BM25；None/空白时回落
    // 为 embed query_text。
    pub embedding_text: Option<String>,
    pub filters: MemoryFilters,
    pub limit: usize,
    // 向量源门控阈值（余弦绝对域 [0,1]，归一化 cos）。None = MemoryQualityConfig.min_cosine = 0.3。
    // ⚠️ 与 AbstentionConfig.default_min_score = 0.6（raw scores，拒答判定）语义不同，勿混淆。
    pub min_cosine: Option<f32>,
}
```

- 工具型 Agent（如天气助手）：min_cosine = 0.2（容忍较低匹配，宁可多答）
- 学习型 Agent（如知识库助手）：min_cosine = 0.5（严格匹配，宁缺毋滥）
- 默认值从 manifest.toml `[memory.quality]` 节读取（真实 embedding 各向异性强，绝对阈值实际力度有限，见 ADR-082 C1）

### 6.6 检索权重（v3.10 简化）

**原设计（v3.7）**：通过 memory_hint.type 动态调整 RRF 权重和 graph_expand 参数。四种模式（s/f/r/i）各有不同权重配置。

**v3.10 简化**：移除 per-round memory_hint 提取和规则引擎类型检测。所有检索统一使用默认权重（vector: 0.7, text: 0.3, graph: 0.0），graph_expand 使用默认早停阈值 `[0.1, 0.15, 0.2]`（1跳/2跳/3跳）。

**简化理由**：
- f/r 类型的权重微调收益未被 benchmark 验证，不值得引入规则复杂度
- i（Identity）类型虽然理论上有意义（仅搜 Autobiographical），但需要在检索质量损失（漏掉其他层相关知识）和性能优化之间权衡——当前默认搜全部 4 个 Label 是更安全的选择
- graph_expand 的激进阈值依赖图质量，当前图结构尚未经过充分验证

> **实现对齐（v3.12，P2 G11/G12）**：早停阈值默认值已由 `[0.15, 0.2, 0.25]` 调整为 `[0.1, 0.15, 0.2]`（与设计 §6.3 "1跳: 0.1, 2跳: 0.15, 3跳: 0.2" 一致，`GraphExpandConfig::default()` 与 `get_expand_thresholds()` 的 `"s"`/`"_"` 分支同步更新；`"r"` 分支保持 `[0.1, 0.12, 0.15]` 不变）。边权重自动计算规则（§3.1 `edge_strength = min(0.8, confidence_avg × exp(-0.01 × days_since))`）已在 `semantic/graph.rs` 落地：`create_memory_edge` 在调用方未显式传 weight 时自动读取两端节点 confidence 计算并写入 weight property；读取端优先读 property，缺省回退 `DEFAULT_EDGE_WEIGHT=1.0`（向后兼容旧库）。

HintType 枚举和相关权重查找函数（`get_hint_weights`, `config_from_hint`）保留在代码中，供将来需要时使用。

## 7. 跨 Agent 知识共享

不同 Agent 之间不共享数据库，知识共享通过三种机制实现：

**路径 1：Intent 查询（推荐，主路径）**

Agent A 需要某项知识，直接向拥有该知识的 Agent B 发送 Intent 查询：

```json
{
  "type": "intent",
  "target": "com.example.weather",
  "action": "query_user_city",
  "params": {},
  "id": "msg-123"
}
```

天气 Agent 从自己的私有 SQLite `memory/private.sqlite` 查到结果并返回。这是最小权限方式——日历 Agent 只拿到了需要的那个事实。

**路径 2：Gateway UserProfile（身份与偏好）**

用户身份和偏好等系统级信息由 Gateway 的 UserProfile 模块统一管理，在 Agent 握手时注入，变更时热推送。详见 [18-user-identity-simplified.md](./18-user-identity-simplified.md)。

**身份查询的容错：**

- **本地缓存**：每个 Agent 缓存最近一次 identity 查询结果，TTL 5 分钟。缓存未过期时不发起查询（减少 Gateway 负载）
- **Gateway 不可用**：如果 Gateway 未响应（超时 2 秒），Agent 降级使用本地缓存（即使过期），或使用 manifest.toml 中的 identity_deps 默认值
- **Gateway 恢复**：Runtime 定期重连 Gateway，重连成功后重新获取完整 UserProfile
- **冷启动预加载**：新 Agent 首次启动时，Gateway 在握手阶段（AgentHello）将 UserProfile 注入启动参数（详见 [18-user-identity-simplified.md](./18-user-identity-simplified.md) §3），不依赖运行时缓存

**路径 3：云端 Memory Sync 同步**

云端作为知识同步层，Agent 写入的知识可按规则广播给订阅了该信息的其他 Agent，各 Agent 的本地 SQLite 各自更新。

### 7.1 隐私与同步

记忆节点增加 PrivacyLevel 标记（Public / Personal / Sensitive），LLM 在即时提取时自动判断：

- **Public**：可跨 Agent 共享——"用户名叫张三"、"用户说中文"
- **Personal**：Agent 私有——"用户偏好简洁回复"
- **Sensitive**：Agent 私有，打包分享时剥离——"用户提到健康问题"

**PrivacyLevel 的实际作用域是打包边界控制**：当用户将 Agent 分享给他人时，Personal/Sensitive 节点被自动剥离，只保留 Agent 自身的能力（SkillIteration、ProceduralNode、AutobiographicalNode 中关于 Agent 自身的部分）。PrivacyLevel 不用于网络同步过滤或跨 Agent 隔离——LLM 上下文中的数据无技术访问控制手段，靠 prompt 约定约束。

云端同步按节点类型（Episodic / Knowledge / Procedural / Autobiographical）同步。全部数据明文同步，平台托管（与主流互联网平台一致，详见 00-prd.md ADR-002）。

## 8. 语义记忆节点类型汇总

### 8.1 节点类型（NodeType）— 认知功能分类

节点类型通过 `nodes.label` 字段实现，区分记忆的**认知功能分层**：

| Label              | 用途                           | 遗忘                            | 隐私                      | 详见                                            |
| ------------------ | ------------------------------ | ------------------------------- | ------------------------- | ----------------------------------------------- |
| `Episodic`         | 经历层：交互片段、对话快照     | 激进清理（14天）                | Personal                  | 本文档 §2                                       |
| `Knowledge`        | 语义记忆：事实、偏好、关系     | 乘法衰减 + 节点类型规则（§5.2） | Public/Personal/Sensitive | 本文档 §3.1                                     |
| `Procedural`       | 程序记忆：行为模式、操作规则   | 乘法衰减 + 节点类型规则（§5.2） | Personal                  | 本文档 §3.2                                     |
| `Autobiographical` | 自传体记忆：自我认知、能力边界 | 不遗忘                          | Personal                  | 本文档 §3.3                                     |
| `SkillDraft`       | 草稿 Skill（调试阶段）         | 开发期保留                      | Personal                  | [13-skill-system.md](./13-skill-system.md) §3.2 |
| `SkillIteration`   | 迭代版本快照                   | 开发期保留                      | Personal                  | [13-skill-system.md](./13-skill-system.md) §3.3 |
| `SkillExecution`   | 执行记录（含模型信息）         | 开发期保留                      | Personal                  | [13-skill-system.md](./13-skill-system.md) §3.4 |
| `SkillExperience`  | 已发布 Skill 的运行经验        | 专用衰减                        | Personal                  | [13-skill-system.md](./13-skill-system.md) §3.5 |

**NodeType 的设计原则：**
- 通过 `nodes.label` 列实现（而非枚举字段），利用 label 索引隔离类型
- 每种 label 有独立的 FTS5 虚表和检索路径
- 认知分层与 label 一一映射（见 §0 分层原则）

### 8.1.1 子分类（sub_type / category）— v3.11 新增

部分 NodeType 在认知功能之上还有一层**子分类**，用于面板的二级筛选与离线统计：

| NodeType            | 子分类字段    | 取值集合                                                                   | 存储位置                                | 详见     |
|---------------------|----------------|-----------------------------------------------------------------------------|----------------------------------------|----------|
| `Knowledge`         | `sub_type`     | `Fact` / `Preference` / `Relation` / `Procedure`                           | Node property `sub_type`               | §3.1     |
| `Autobiographical`  | `category`     | `Identity` / `Capability` / `Limitation` / `Preference` / `History` / `Relationship` | Node property `category`               | §3.3     |
| `Episodic`          | —             | 无子分类                                                                  | —                                       | §2       |
| `Procedural`        | —             | 无子分类                                                                  | —                                       | §3.2     |

**写入语义（ADR-068 2026-09 revision）：**
- `Knowledge.sub_type` 是蒸馏产物,不是即时写入值——`memory_store` 只把 LLM 的 4 类 `category` 作为 `Episode.knowledge_subtype` 路由提示写入经历层;蒸馏器 Step 2a 从 episode 语义结构化提取 `(subject, predicate, object)`,落库时按提取结论确定最终节点类型(sub_type 由蒸馏结论而非 category 直通决定)。`Fact`/`Preference`/`Relation`/`Procedure` 与 `category` enum 一一对应仅作为蒸馏输入侧的路由分类
- `Autobiographical.category` 只由两个生产者填充:Manifest 派生(仅 Identity/Capability,§3.3)与 `EpisodicDistiller` 自传体识别晋升(Step 2a 布尔分类 + aspect → Step 4 Judge,History/Relationship 均走蒸馏,无 append-only / key 幂等 upsert 的即时直写)

**读取语义：**
- `AdminNodeRecord.sub_type` 在序列化时只在节点有子分类时携带（`skip_serializing_if = "Option::is_none"`），避免 Episodic / Procedural 节点多出一个恒为空的字段。
- `GET /memory/nodes` 接受 `sub_type` 查询参数（Knowledge / Autobiographical 有效），用于面板的二级下拉筛选。Episodic / Procedural 携带 `sub_type=` 查询参数被服务端忽略（不隐藏节点）。

### 8.2 Zone 概念 — 业务场景分区（暂缓实现）

**⚠️ Zone 功能暂缓实现，本节仅做概念定义，避免与 NodeType 混淆。**

Zone 用于区分记忆的**业务场景分区**，与 NodeType 正交：
- **NodeType** 回答"这是什么类型的记忆？"（认知功能：经历/语义/程序/自传体）
- **Zone** 回答"这个记忆属于哪个业务场景？"（业务分区：work/personal/system）

**预定义的 Zone（Phase 4+）：**
- `work`：工作相关记忆（项目、任务、同事）
- `personal`：个人生活记忆（兴趣、家庭、日常）
- `system`：系统级记忆（配置、元数据）

**Zone 与 NodeType 的关系：**
```
一个 KnowledgeNode 可以属于：
  - NodeType: Knowledge（认知功能：语义记忆）
  - Zone: work（业务场景：工作相关）
  - 示例："用户的项目经理是王五" → Knowledge + work

一个 Episodic 可以属于：
  - NodeType: Episodic（认知功能：经历层）
  - Zone: personal（业务场景：个人生活）
  - 示例："用户提到周末去爬山" → Episodic + personal
```

**实现方式（Phase 4+）：**
- Zone 将作为 **节点属性** 存储于 `nodes.props` JSON
- 在 `KnowledgeNode`、`ProceduralNode` 等结构体中增加 `zone: String` 字段
- 检索时可通过 zone 过滤（如 `filters.zone = Some("work")`）

**⚠️ 当前状态（Phase 1-3）：**
- `MemoryNode.zone` 字段存在于 `acowork-core/src/memory/traits.rs` 中，但**暂未使用**
- `MemoryStore::list_by_zone()` 方法已定义，但 **当前 SqliteStore 未实现**
- Zone 功能推迟到 Phase 4+，当前所有节点默认属于 `default` zone

**设计理由：**
- Phase 1-3 聚焦认知分层架构（NodeType），业务分区需求尚未明确
- 避免过早引入 zone 导致架构复杂度增加
- Phase 4+ 根据实际使用场景再决定是否启用 zone 功能

## 9. 分阶段实现路线

> **ADR-068（2026-09 revision）覆盖声明**：本节为历史分阶段规划（Phase 1/2/3 路线图）。自 ADR-068 M1-M8 落地后,实际实现以"§4.1 即时薄写经历层 + §4.2 EpisodicDistiller 离线蒸馏 + manifest bootstrap 权威导入"为准;本节中与 ADR-068 冲突的规划文字（PendingKnowledgeNode、即时直写沉淀层、`memory_store` autobiographical 类目、rule-based generalization、HypothesisNode、authoritative/alternate 冲突保留制、Skill↔ProceduralNode failure_cases 联动、激活计数驱动降级等）均视为**未落地或被取代的历史设计**,不再作为实现依据。

### Phase 1：记忆基础（对应 Roadmap Phase 2）

**目标：** 让 Agent 能记住用户，能检索，能遗忘，但不做深度抽象和跨时段的巩固。

**交付内容：**

三层架构落地（瞬态层 / 经历层 / 沉淀层），SQLite `nodes` 表通过 `label` 列支持 Episodic + Knowledge + Procedural + Autobiographical 四类节点，`edges` 表存节点间关系。存储为单文件 `memory/private.sqlite`（含 WAL/SHM）。经历层存储对话原始记录（episode），沉淀层存储精炼知识（KnowledgeNode / AutobiographicalNode）。

即时提取通过 Tool Call 机制实现：`memory_store` 工具加入 Agent 内置工具列表，System Prompt 加入提取指引，LLM 在生成回复时自主判断是否调用。即时阶段仅做 embedding 相似度粗筛（相似度 > 0.85 → 标记候选冲突），不做三元组提取。三元组提取和精确去重发生在**离线巩固阶段**（Phase 3），详见 §4.1 和 §6.4。

基础遗忘机制落地：乘法衰减模型 decay_score = importance × activity_signal，activity_signal = clamp(recency_boost + access_boost, 0.05, 1.0)。Dormant 态区分：Fact/Relation 永不清除，Preference/ProceduralNode Dormant 超过 90 天可 Purge。dormant_since 字段计时，reactivate_node 时归零。

关联扩散检索落地：hybrid_search 基础上加 `edges` 表多跳 JOIN 扩展，经历层 episode 通过 KnowledgeNode.source_episode 反向查询建立跨层关联，边权重 = min(0.8, confidence_avg × recency_factor)，扩散阈值 0.2（可配置），硬限制 **3 跳**（通过早期终止实际大多在 1-2 跳停止）。

AutobiographicalNode 从 manifest.toml 自动派生（Identity / Capability），History 节点超过 10 条时摘要压缩，注入上限 200 token。PrivacyLevel（Public / Personal / Sensitive）用于打包分享时的节点过滤。

Episode 内容分类压缩落地：信息性内容原样存储，工件性内容（代码/文件/命令输出）压缩为摘要 + ArtifactRef 引用。代码不住在 SQLite `nodes` 里——`nodes.props` 存"关于代码的描述"，需要实际代码时通过 artifact_refs 的 path + hash 在文件系统/版本控制中查找。

**Phase 1 不做的事：** 离线巩固、ProceduralNode 联动、分页换出、云端同步。这些是 Phase 2/3 的事。

---

### Phase 2：程序记忆与自我认知（对应 Roadmap Phase 3）

**目标：** 让 Agent 从自身经验中学习行为模式，并能认知自己的能力边界。

**核心组件一：ProceduralNode 完整生命周期**

ProceduralNode 的来源（ADR-068 2026-09 revision 收敛为单一语义生产者）：

路径 A — 用户反馈写经历层（Tool Call）：用户明确纠正（"太长了" / "不要用表格"）→ `memory_store(category="procedure")` → Episode(knowledge_subtype=Procedure)。不直写 ProceduralNode。

~~路径 B — 执行失败自动总结~~ **（已移除，v3.12）**：原设计 SkillExecution 的 failure_case 触发，将 failure_case 摘要写入 ProceduralNode。实际代码中 `failure_case → ProceduralNode` 自动写入路径从未落地，且 SkillExecution 的 `success_count` 从未被递增，据此派生任何"成功率"类节点都会产生 false-positive（见 offline.rs DELETED 注释）。

~~路径 C' — rule-based generalization~~ **（已移除，2026-09 revision）**：原 `generalization.rs::detect_simple_patterns`（字符串全等计数 + action/tool_calls 文本特征 hack）已下线,不再被任何生产路径调用。

路径 C — 离线蒸馏提炼（唯一生产者）：`EpisodicDistiller::promote_procedures` 扫描 Episode(knowledge_subtype=Procedure) → Step 2a 服务端 LLM 提取 `(trigger_condition, action_pattern)` → Step 4 LLM Judge 仲裁 → 晋升 ProceduralNode（含 promotion_metadata 证据链）。

ProceduralNode 的激活时机：每次 Agent 生成回复前，检索 relevant ProceduralNode（按 trigger_condition 匹配当前上下文），activation_count += 1，更新 last_accessed。被激活的 ProceduralNode 注入 System Prompt 行为准则区，格式："当 [trigger_condition] 时，优先 [action_pattern]"。

**核心组件二：Skill ↔ ProceduralNode 双向联动**

双向联动设计遵循"处方式知识"框架，但做了一定简化：

Skill → ProceduralNode：当 SkillExperience 的 failure_cases 积累超过 3 条同类失败（如都是"输出太长"），触发联动提取。LLM 阅读 failure_cases 摘要，生成跨 Skill 的通用 ProceduralNode。例如：weekly-report Skill 输出太长被纠正 5 次 + code-review Skill 输出太长被纠正 3 次 → 提炼出通用 ProceduralNode："此用户对输出长度敏感，所有 Skill 应先给结论再展开细节"。

ProceduralNode → Skill：ProceduralNode 的 activation_count 低于阈值（如 < 3）时，向对应 Skill 发送降级建议。例如某个 ProceduralNode 的 action_pattern 是"用表格"，但 activation_count 持续很低，Agent 应该反思：这个行为模式是否还适用？

**⚠️ 困境三的补设计 — 主动假设验证机制**

Phase 2 需要补上"困境三"（记忆泛化与抽象）的设计缺口。ProceduralNode 的提炼不能只是被动的"积累够了就合并"——需要有主动的假设验证：

当同一 trigger_condition 下有 >= 3 个 action_pattern 变体时（如"用户要求简洁"的表达方式有"太长了"、"少说废话"、"简短点"三种），Agent 应主动提出假设："这三种表达是否指向同一个偏好？" 并在后续交互中验证。验证通过后合并为单一 ProceduralNode，验证失败则保留多个变体。

具体实现：在 `acowork-memory` 的 `consolidation/distiller.rs` 内实现 `detect_merge_candidates()`，扫描同类 trigger_condition 下的多个 action_pattern，由 LLM 判断是否应合并。

**~~核心组件三：自我评估驱动的 AutobiographicalNode 更新~~** **（已移除，v3.12）**

原设计目标是让 Agent 认知自身的能力边界，主要通过 Limitation 节点的自动更新实现：

- 触发时机：每次 SkillExecution 完成后，根据 success/failure 和模型信息更新 `SkillExperience.model_compatibility`；某模型某类任务成功率低于 60% 时生成/更新 Limitation 节点。
- 注入时机：Limitation 节点在每次对话的 System Prompt 注入时必须包含（与 Identity / Capability 同级）。

> **移除原因**：该自动路径（含 runtime `MemoryManager::run_self_evaluation` 与 `acowork-memory` consolidation 内的重复实现）已删除——`success_count` 从未在任何代码路径被递增，据此派生的"成功率"必然把 ≥5 次失败的 Skill 误判为 0% 成功率，产生 false-positive Limitation 节点。ADR-068 2026-09 revision 进一步收敛：自传体记忆来源为 §3.3 的两条生产者（Manifest 权威导入 + EpisodicDistiller 离线识别晋升），Limitation 节点的产生依赖蒸馏器从用户/LLM 显式陈述中识别，不再自动生成。

同时原设计新增 Relationship 节点的自动维护（用户与 Agent 合作超过 30 天自动生成）——该自动触发逻辑（manager.rs `run_relationship_generation`）已整体删除（2026-09 revision,回归测试 `post_compaction_tasks_do_not_write_relationship_nodes` 锁定）；当前 Relationship 节点由 `EpisodicDistiller` 从 `knowledge_subtype=Relation` 的 episode 晋升，无第二生产者。

**Phase 2 验收标准：**

- ProceduralNode 有三条明确的来源路径，且 Skill ↔ ProceduralNode 联动可工作
- 同一 trigger_condition 出现 >= 3 个变体时，Agent 能提出合并假设（而非仅被动合并）
- AutobiographicalNode Limitation 节点能根据执行统计自动生成/更新
- 所有 Phase 1 功能在 Phase 2 改动后仍然正常运行

---

### Phase 3：离线巩固与持久化（对应 Roadmap Phase 6）

**目标：** 让 Agent 在空闲时主动整合经验、发现隐式关联，并具备跨设备持久化能力。

**核心组件一：离线巩固（"睡眠"模式）**

离线巩固是 Phase 1 即时提取的补充——即时提取处理"显式信息的即时存储"，离线巩固处理"隐式关联的发现与整合"。

触发条件（OR 关系，任一满足即触发）：Agent 空闲超过 30 分钟（可配置）、未巩固 episode 积攒超过 50 条、用户手动触发。多 Agent 场景下增加全局协调限制：同一时刻只有一个 Agent 执行离线巩固，避免 CPU/内存竞争。

离线巩固的 LLM prompt 设计（关键）：

```
你正在执行记忆巩固。输入是一组未巩固的情景记忆（episode），按时间排列。

你的任务：
1. 发现隐式关联：同一主体在多个 episode 中出现但未被显式存储（如用户多次提到"上海"但从未说"我住在上海"）
2. 检测知识冲突：新提取的知识是否与已有 KnowledgeNode 矛盾
3. 提炼跨 Skill 模式：多个 Skill 的 failure_cases 是否指向同一个根本原因
4. 评估现有 ProceduralNode：哪些已经被反复验证（activation_count 高）？哪些长期未激活（可能过时）？
5. 增强 Artifact 摘要：Phase 1 的模板摘要只是"读取了什么文件、多少行"，离线巩固时可以用 LLM 为带 artifact_refs 的 episode 生成语义摘要（如"这个文件实现了数据处理的管道模式"），替换原有的模板字符串

输出格式：
- 发现的新 KnowledgeNode（带 importance 和 privacy 评估）
- 需要合并或标记冲突的已有节点
- 需要降级或激活的 ProceduralNode
- 需要更新的 AutobiographicalNode（如 Limitation 节点）
- 需要增强摘要的 Episode（原 content 为模板字符串，替换为 LLM 生成的语义摘要）
```

知识冲突的处理策略：冲突时保留新旧两个节点，标记 confidence 更高的为 authoritative，另一条降级为 alternate。长期矛盾的节点（超过 3 次冲突记录）标记为"待用户确认"。

**⚠️ 离线巩固与困境三的关联**

困境三的核心是"缺乏主动假设验证机制"。Phase 3 的离线巩固 prompt 中加入主动假设步骤：LLM 在回放 episode 时，应主动提出"如果…会怎样"类型的假设。例如："用户三次提到加班但未提薪酬，可能反映工作满意度问题"——这类假设不写入 KnowledgeNode，而是生成一个 HypothesisNode（暂存，待后续验证）。 HypothesisNode 不参与常规检索，仅在后续离线巩固时被翻出来验证是否得到更多证据支持。

**Episode 摘要增强：从"读了什么"到"做了什么"**

Phase 1 的内容分类压缩用模板字符串和正则分离生成了确定性摘要（如"读取 src/main.rs，共 200 行，首行: fn main()"）。这类摘要能回答"发生了什么交互"，但无法回答"这个文件做了什么"或"这次改动的影响是什么"。

离线巩固时，LLM 对带 artifact_refs 的 episode 做摘要增强：

增强规则：
- 只增强 content_type = Artifact 的 episode（信息性内容不需要增强）
- 增强时 LLM 可以看到 episode 的自然语言上下文（用户说了什么、Agent 回复了什么），以及相邻 episode 的内容
- 增强后的摘要替换 content 字段，artifact_refs 保留不动
- 增强是有损的：如果 LLM 判断"此代码交互不值得增强"（如只是读取了一个配置文件），保持原摘要不变

增强示例：
```
Phase 1 摘要：
  "读取 src/processor.rs，共 800 行，首行: pub fn process_data(input: &str) -> Result<Data> {"

Phase 3 增强后：
  "读取 src/processor.rs — 数据处理管道的主模块，process_data 函数接受字符串输入，通过验证→清洗→转换三阶段处理，返回结构化 Data。用户要求增加输入验证的错误处理。"
```

为什么不在 Phase 1 就用 LLM 生成摘要？因为离线巩固是批量处理——50 个 episode 一起回放，LLM 有完整的上下文做更准确的摘要。Phase 1 的模板摘要保证零额外成本，Phase 3 的增强摘要保证质量，二者各司其职。

**核心组件二：分页换出（MemGPT 风格）**

分页换出的目标不是"无限扩展上下文"，而是"让 Agent 主动管理记忆的活跃度"。以下情况触发换出：

- Token 使用率 > 90% 且关键信息无法通过截断解决
- 某个 episode 超过 30 天未被任何检索命中
- 用户明确要求"专注新话题"（对应新 episode 写入时，旧 episode 换出）

换出单位是"消息块"（若干连续 episode 组成的事件片段），而非单条消息。换出时：episode 标记为 swapped_out = true，embedding 保留但降级（不参与向量检索，只在特定触发下换入）。换入时：沿时间线反向检索找到 swap_out 边界，重新激活相关 episode 的 embedding。

**⚠️ 分页换出的循环依赖风险（已知的实现风险）**

分页换出会引发一个矛盾：换出的判断本身需要消耗 Token（要分析哪些信息可以换出）。DeepSeek 的评审也指出了这一点。当前设计决策：换出判断使用专用的小模型（如 qwen3:1.7b）或简化的规则引擎，而非主模型。主模型只负责生成"我建议换出 X"的指令，由 Runtime 执行实际的换出操作。

云端同步按节点类型同步。全部数据明文同步，平台托管（与主流互联网平台一致，详见 00-prd.md ADR-002）：

冲突解决策略：Phase 6 阶段先实现单向同步（云端 → Agent），Agent 本地变更记录在本地不上报，避免双向写入导致的冲突。后续 Phase 7+ 若需双向同步，采用"最后写入者胜 + 用户确认"机制。

**Phase 3 验收标准：**

- 离线巩固能发现即时提取无法捕获的隐式关联（需人工评估验证）
- Hypothesis 机制能主动提出并验证假设（追踪假设生命周期）
- 分页换出/换入在极端上下文长度下（> 500 轮）正常工作
- 云端同步按节点类型正确同步，PrivacyLevel 控制打包分享时的过滤

---

### 三阶段总结对照

| 维度         | Phase 1                              | Phase 2                            | Phase 3                                       |
| ------------ | ------------------------------------ | ---------------------------------- | --------------------------------------------- |
| **核心问题** | 能不能记住                           | 能不能学习                         | 能不能整合                                    |
| **即时提取** | Tool Call（显式信息）                | 扩展 Tool Call + failure_case 联动 | 离线 LLM 回放（隐式关联 + Artifact 摘要增强） |
| **遗忘机制** | 乘法衰减 + Dormant                   | ProceduralNode 90 天 Purge         | HypothesisNode 过期清除                       |
| **程序记忆** | ProceduralNode 结构体                | Skill ↔ ProceduralNode 双向联动    | 跨 Skill 通用模式提炼                         |
| **自我认知** | Manifest 派生 Autobiographical       | 自我评估更新 Limitation            | HypothesisNode 主动假设验证                   |
| **云端同步** | 无                                   | 无                                 | 按节点类型同步                                |
| **困境覆盖** | 困境二、困境四（完整）               | 困境三（部分补全）                 | 困境三（假设验证）、困境五（情感信号补充）    |
| **可扩展性** | MemoryStore trait + 生命周期阶段定义 | 中间件管线                         | 存储后端可替换                                |

## 10. 记忆生命周期架构

### 10.1 设计动机

记忆系统是 ACowork Agent 的核心差异化能力——推理能力依赖 LLM，操作能力依赖 Tools，只有记忆系统是 ACowork 自主掌控的。随着平台演进，记忆系统必然经历大量迭代（新检索策略、新遗忘模型、新巩固方式、新存储引擎……）。如果记忆触发点硬编码在 Runtime 主循环里，每次记忆迭代都要改 Runtime 源码，这违反了"Runtime 是稳定执行引擎"的定位。

因此引入**记忆生命周期（Memory Lifecycle）**作为 Runtime 和 Memory 系统之间的标准化接口。Runtime 只负责在固定位置触发生命周期阶段，Memory 系统通过注册 handler 和中间件响应，两者解耦。

### 10.2 生命周期阶段定义

#### 10.2.1 主循环内阶段（同步，由 Runtime 在每轮迭代中触发）

| 阶段       | 触发点                             | 输入                                           | 输出                 | 说明                                                                                                                                                              |
| ---------- | ---------------------------------- | ---------------------------------------------- | -------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Retrieve` | 步骤 ② 构建上下文                  | 用户消息 + 当前上下文摘要                      | `Vec<MemoryContext>` | 记忆检索：SQLite 通道（hybrid_search + 多跳扩展，始终执行）；若 manifest 声明 RAG，并行查询 RAG 通道（RagClient.query，超时 5s 降级）。详见 00-prd.md §1.13.1 |
| `Inject`   | 步骤 ② 构建上下文（Retrieve 之后） | `Vec<MemoryContext>` + Token 预算              | 格式化字符串         | 决定如何将记忆注入 LLM 上下文                                                                                                                                     |
| `Record`   | 步骤 ⑥ 结果追加历史（异步）        | 本轮 user_msg + assistant_reply + tool_results | `()`                 | 记录本轮交互到经历层                                                                                                                                              |

#### 10.2.2 后台阶段（异步，由 MemoryManager 独立调度）

| 阶段          | 触发条件                                              | 输入                | 输出                      | 说明                                             |
| ------------- | ----------------------------------------------------- | ------------------- | ------------------------- | ------------------------------------------------ |
| `Consolidate` | 即时提取（每轮 Record 后检查）+ 离线巩固（空闲/阈值） | 未巩固 episode 列表 | 新建/更新的 KnowledgeNode | 巩固管道（§4）                                   |
| `Decay`       | 定时扫描（每小时，可配置）                            | 当前时间            | `DecayScanResult`         | 遗忘衰减（§5）                                   |
| `Compact`     | 存储维护（启动时 + 空闲时）                           | 存储统计            | 清理数量                  | 索引优化、旧 episode 清理、SQLite WAL Checkpoint |

### 10.3 MemoryStore trait（存储后端抽象）

Runtime 和上层记忆逻辑不直接依赖任何具体存储引擎（SQLite / Sled / LMDB / 远程服务），而是通过 `MemoryStore` trait 交互。这确保存储方案可替换——Phase 1 用 `SqliteStore`（基于 `rusqlite`），未来可无缝切换。

```rust
/// 记忆查询参数（替代裸 &str，支持扩展）
pub struct MemoryQuery {
    pub query_text: String,       // BM25 词法源查询文本
    pub embedding_text: Option<String>, // 向量源查询文本（当前轮用户消息）；None/空白 = 回落 query_text
    pub filters: MemoryFilters,
    pub limit: usize,
    pub min_cosine: Option<f32>,  // 向量源门控阈值（归一化余弦 [0,1]）
}

pub struct MemoryFilters {
    pub node_types: Vec<NodeTypeFilter>,  // 按节点类型过滤
    pub privacy_levels: Vec<PrivacyLevel>,// 按隐私级别过滤
    pub time_range: Option<(DateTime, DateTime)>, // 时间范围
    pub session_id: Option<String>,       // 按会话过滤
}

/// 检索结果（统一经历层和沉淀层）
pub struct SearchResult {
    pub node: MemoryNode,         // Episode 或 KnowledgeNode
    pub score: f32,               // 相关性分数
    pub source: ResultSource,     // DirectMatch / GraphExpansion
    pub context_tokens: usize,    // 预估 token 数（用于裁剪预算计算）
}

/// 记忆上下文（Retrieve 阶段的输出、Inject 阶段的输入）
pub struct MemoryContext {
    pub content: String,          // 格式化后的记忆内容
    pub priority: u8,             // 注入优先级（0 = 最高，7 = 最低）
    pub source: ContextSource,    // 来自哪个认知层
    pub estimated_tokens: usize,  // 预估 token 数
}

pub enum ContextSource {
    Autobiographical,   // 自传体记忆（绝不裁剪）
    SemanticCore,       // 语义记忆核心事实
    Procedural,         // 程序记忆
    UserPreference,     // 用户偏好
    FailureLesson,      // 失败教训
    GraphExpansion,     // 关联扩散结果
    Episodic,           // 经历层情景
    RagChannel(String), // RAG 通道结果（参数为 RAG 工具名，如 "enterprise_knowledge"；仅 manifest 声明 RAG 时出现）
}

/// 记忆存储后端的标准化接口
/// 实现者可以是 SqliteStore（当前唯一生产实现）/ Sled / LMDB / 远程服务 / 内存 mock
pub trait MemoryStore: Send + Sync {
    // ── 经历层 ──

    /// 写入交互片段（自动分类内容类型、工件性压缩）
    fn store_episode(&self, episode: &Episode) -> Result<()>;

    /// 检索情景记忆
    fn search_episodes(&self, query: &MemoryQuery) -> Result<Vec<SearchResult>>;

    /// 标记情景已巩固
    fn mark_consolidated(&self, ids: &[String]) -> Result<()>;

    /// 清理已巩固且过期的情景
    fn cleanup_episodes(&self, older_than: Duration) -> Result<u64>;

    // ── 沉淀层 ──

    /// 写入/更新知识节点（Fact 自动语义去重）
    fn store_knowledge(&self, node: &KnowledgeNode) -> Result<()>;

    /// 写入/更新程序记忆节点
    fn store_procedural(&self, node: &ProceduralNode) -> Result<()>;

    /// 写入/更新自传体记忆节点
    fn store_autobiographical(&self, node: &AutobiographicalNode) -> Result<()>;

    // ── 统一检索 ──

    /// 混合搜索：向量 + 全文 + RRF 融合
    fn hybrid_search(&self, query: &MemoryQuery) -> Result<Vec<SearchResult>>;

    /// 关联扩散：从种子节点出发，沿图边扩展
    fn graph_expand(&self, seeds: &[SearchResult], hops: u8) -> Result<Vec<SearchResult>>;

    // ── 遗忘 ──

    /// 衰减扫描（使用传入的配置，支持按 Agent 定制）
    fn run_decay_scan(&self, config: &DecayConfig) -> Result<DecayScanResult>;

    /// 恢复 Dormant 节点为 Active
    fn reactivate_node(&self, node_id: &str) -> Result<()>;

    /// 清理过期 Dormant 节点
    fn purge_expired(&self, max_dormant_age: Duration) -> Result<PurgeResult>;

    // ── 生命周期 ──

    /// 存储健康检查（用于监控和诊断）
    fn health_check(&self) -> Result<StoreHealth>;

    /// 存储统计信息（节点数、存储大小、索引状态等）
    fn stats(&self) -> Result<StoreStats>;

    /// 关闭存储（释放资源、SQLite WAL 自动刷写）
    fn close(&self) -> Result<()>;
}

/// 存储健康状态
pub struct StoreHealth {
    pub is_healthy: bool,
    pub latency_ms: u64,          // 最近一次操作的延迟
    pub error_count: u32,         // 最近 N 分钟的错误数
    pub details: Option<String>,  // 错误详情（仅不健康时有值）
}

/// 存储统计
pub struct StoreStats {
    pub episode_count: u64,
    pub node_count: u64,          // Active + Dormant
    pub active_node_count: u64,
    pub dormant_node_count: u64,
    pub edge_count: u64,
    pub storage_size_bytes: u64,
    pub index_count: usize,       // 向量索引 + 全文索引数量
}

/// 遗忘配置（支持按 Agent 定制）
pub struct DecayConfig {
    pub lambda: f32,              // 衰减速率（默认 0.03）
    pub floor: f32,               // 最低活跃度（默认 0.05）
    pub access_per_hit: f32,      // 每次访问增量（默认 0.1）
    pub boost_cap: f32,           // 历史访问上限（默认 0.5）
    pub dormant_threshold: f32,   // Active → Dormant 阈值（默认 0.3）
    pub purge_after: Duration,    // Dormant → Purge 时长（默认 90 天）
    pub purge_importance_threshold: f32, // Purge 路径1的 importance 下限（默认 0.5），详见 §5.2
}
```

**设计要点：**

- `MemoryQuery` 替代裸 `&str`：未来扩展只需加字段（如 `temperature` 控制检索创造性、`recency_boost` 控制时间偏好），不破坏已有实现
- `MemoryContext` 带 `priority` 和 `estimated_tokens`：Inject 阶段可以直接按优先级和 token 预算裁剪，无需 Runtime 了解记忆内部结构
- `DecayConfig` 参数化：不同 Agent 可以有不同的遗忘策略（"学习型 Agent"遗忘慢，"工具型 Agent"遗忘快），通过 manifest 配置注入
- `health_check` + `stats`：为 Desktop App 的记忆管理面板和运维监控提供标准数据接口
- trait 中不包含任何 SQLite 或其他存储后端的类型，实现完全隔离

### 10.4 MemoryManager（中间层）

Runtime 不直接调用 `MemoryStore`，而是通过 `MemoryManager` 这个中间层。`MemoryManager` 是记忆系统的"大脑"——它协调生命周期阶段、管理中间件链、注入配置。

```
┌──────────────────────────────────────────────────────────────┐
│  Agent Runtime 主循环                                         │
│                                                              │
│  ② 构建上下文                                                │
│     └─ memory_manager.retrieve(query) → Vec<MemoryContext>   │
│     └─ memory_manager.inject(contexts, budget) → String      │
│  ⑥ 结果追加历史                                              │
│     └─ memory_manager.record(episode) → ()  [异步]           │
│                                                              │
│  后台任务（MemoryManager 内部调度）：                          │
│     └─ memory_manager.consolidate() → ()                     │
│     └─ memory_manager.decay() → DecayScanResult              │
│     └─ memory_manager.compact() → ()                         │
└──────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────┐
│  MemoryManager                                               │
│                                                              │
│  ├── Lifecycle Handler Registry                              │
│  │   └─ 每个阶段可注册多个 handler，按优先级执行              │
│  │                                                           │
│  ├── MemoryMiddleware Chain                                  │
│  │   └─ Record 前后可插入中间件（情感标注/内容过滤/审计）     │
│  │                                                           │
│  ├── Config Provider                                         │
│  │   └─ 从 manifest + 系统默认读取配置，注入到各阶段           │
│  │                                                           │
│  └── Event Bus                                               │
│      └─ 发布 MemoryEvent（记忆写入/遗忘/巩固），供 Desktop App  │
│         订阅展示或日志系统记录                                 │
└──────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────┐
│  MemoryStore trait
│  └─ SqliteStore (rusqlite + FTS5 + sqlite-vss，唯一生产实现)       │
│     └─ storage/schema.rs + provider.rs + retrieval.rs modules        │
│     └─ 存储格式：memory/private.sqlite（WAL + FTS5 + vectors + edges）  │
│  └─ (未来) RemoteMemoryStore (云端分布式存储)                │
│  └─ InMemoryStore (SqliteStore::open_in_memory() 测试用 mock) │
└──────────────────────────────────────────────────────────────┘
```

**MemoryManager 核心方法：**

```rust
pub struct MemoryManager {
    store: Box<dyn MemoryStore>,
    rag_client: Option<Arc<RagClient>>,  // RAG 检索客户端（仅 manifest 声明 rag 时注入，None 则仅查 SQLite 记忆层）
    middlewares: Vec<Box<dyn MemoryMiddleware>>,
    config: MemoryConfig,
    event_bus: MemoryEventBus,
}

impl MemoryManager {
    /// 检索记忆（Retrieve 阶段）
    /// 内部调用 store.hybrid_search + 应用层 edges 多跳扩展 检索 SQLite 通道
    /// 若 rag_client 为 Some，并行查询 RAG 通道（RagClient.query，超时 5s 降级）
    /// 两条通道结果合并，按来源标注（Memory / RAG），转换为 MemoryContext
    pub async fn retrieve(&self, query: &str, context: &RetrieveContext) -> Result<Vec<MemoryContext>>;

    /// 注入记忆到上下文（Inject 阶段）
    /// 按 priority 排序，在 token 预算内从低到高裁剪
    /// 返回格式化字符串，供 Prompt Builder 插入上下文
    pub fn inject(&self, contexts: Vec<MemoryContext>, budget: &TokenBudget) -> String;

    /// 记录交互（Record 阶段，异步）
    /// 执行中间件链（pre_record → store_episode → post_record）
    /// 自动分类内容类型、工件性压缩
    pub async fn record(&self, episode: Episode) -> Result<()>;

    /// 即时巩固（Consolidate 阶段的即时部分）
    /// 检查本轮是否触发了 memory_store tool call
    /// 如果有,组装 Episode(knowledge_subtype) 落经历层（ADR-068:不直写沉淀层）
    pub fn consolidate_immediate(&self, store_call: Option<&MemoryStoreCall>) -> Result<()>;

    /// 离线巩固（Consolidate 阶段的离线部分，Phase 3）
    pub async fn consolidate_offline(&self) -> Result<()>;

    /// 遗忘扫描（Decay 阶段）
    pub fn decay(&self) -> Result<DecayScanResult>;

    /// 存储维护（Compact 阶段）
    pub fn compact(&self) -> Result<()>;
}
```

### 10.5 MemoryMiddleware trait（中间件接口）

中间件可以在记忆管线的 Record/Retrieve 阶段前后插入自定义逻辑，无需修改 Runtime 或 SQLite 后端代码。

```rust
pub trait MemoryMiddleware: Send + Sync {
    /// 中间件名称（用于日志和调试）
    fn name(&self) -> &str;

    /// 执行优先级（数字越小越先执行）
    fn priority(&self) -> i32;

    /// Record 阶段前处理（episode 写入前）
    /// 例如：情感标注、内容过滤、审计日志
    fn pre_record(&self, episode: &mut Episode, ctx: &MiddlewareContext) -> Result<()>;

    /// Record 阶段后处理（episode 写入后）
    /// 例如：触发关联索引更新、事件通知
    fn post_record(&self, episode: &Episode, ctx: &MiddlewareContext) -> Result<()>;

    /// Retrieve 阶段后处理（检索结果返回前）
    /// 例如：个性化重排序、合规过滤、结果增强
    fn post_retrieve(&self, results: &mut Vec<SearchResult>, ctx: &MiddlewareContext) -> Result<()>;
}
```

**中间件注册方式：**

```toml
# manifest.toml 中声明中间件（Phase 2+）
[memory.middlewares]
# 内置中间件（按名称引用）
emotion_tag = { priority = 10 }
audit_log = { priority = 100 }

# 自定义 WASM 中间件（Phase 3+）
custom_filter = { type = "wasm", path = "filters/content_filter.wasm", priority = 50 }
```

**中间件执行顺序：** 按 `priority` 从小到大排列，`pre_record` 正序执行，`post_record` 逆序执行（洋葱模型，类似 Tower middleware）。任一中间件返回 Err 时，该阶段中止并向上传播错误。

### 10.6 分阶段实现路线

| 阶段    | 内容                                                                                                                              | 说明                                                                  |
| ------- | --------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------- |
| Phase 1 | `MemoryStore` trait 定义 + `MemoryQuery` / `SearchResult` / `MemoryContext` 等数据类型 + `SqliteStore` 实现（基于 rusqlite） | trait 定义先行，SqliteStore 作为唯一实现，Runtime 改为通过 trait 调用 |
| Phase 1 | `MemoryManager` 基础结构 + 生命周期阶段触发                                                                                       | Manager 直接转发给 SqliteStore，不引入中间件机制                      |
| Phase 1 | `DecayConfig` 参数化                                                                                                              | 遗忘参数从硬编码改为可配置，通过 manifest 注入                        |
| Phase 2 | `MemoryMiddleware` trait + 注册机制 + 内置中间件（emotion_tag / audit_log）                                                       | 打开中间件扩展能力                                                    |
| Phase 3 | `InMemoryStore` mock 实现（基于 `SqliteStore::open_in_memory()`，用于测试）                                                       | 替代当前的集成测试方案                                                |
| Phase 3 | `RemoteMemoryStore` 探索（云端分布式存储）                                                                                        | 如果跨设备实时同步需求明确                                            |

### 10.7 设计决策

| 决策                           | 选择                           | 理由                                                                                                                                                                         |
| ------------------------------ | ------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 存储抽象方式                   | trait + impl                   | Rust 生态标准做法，零成本抽象（monomorphization），编译期检查                                                                                                                |
| 中间件模型                     | 洋葱模型（Tower 风格）         | 业界验证过的模式，支持前/后处理，错误传播自然                                                                                                                                |
| 配置注入                       | manifest.toml + 系统默认       | manifest 声明 Agent 级定制，系统默认兜底                                                                                                                                     |
| 事件通知                       | Event Bus（发布/订阅）         | Desktop App 和日志系统可订阅 MemoryEvent，不影响核心管线性能                                                                                                                 |
| Retrieve/Inject 拆分为两个阶段 | 是                             | Retrieve 关注"查什么"，Inject 关注"怎么放"，职责分离有利于未来 Inject 策略的独立演化（如 RAG 结果和本地记忆的混合排序）                                                      |
| RAG 双通道检索                 | 配置驱动 Opt-In                | RAG 通道仅当 manifest 声明 `type=rag` 时使能；MemoryManager 通过 `rag_client: Option<Arc<RagClient>>` 条件分支控制；无 RAG 声明的 Agent 行为零侵入（详见 00-prd.md §1.13.1） |
| 存储后端                       | rusqlite (SQLite 3 + FTS5 + sqlite-vss) | 嵌入式关系数据库，应用层 `nodes`/`edges`/`vectors` 表 + FTS5 虚表 + 应用层 RRF 融合 |
| 数据模型                       | LPG（Label + Property + Edge） | 替代关系型表结构，认知分层与 LPG Label 一一映射                                                                                                                              |
| 存储格式                       | `memory/private.sqlite`         | 单文件数据库，内含 WAL + 向量表 + FTS5 虚表                                                                                                                                    |

## 11. 质量评估框架（v3.7 新增）

记忆系统的质量不能只靠设计直觉——需要系统化的评估体系持续验证和校准。质量评估框架分为在线评估（Runtime 阶段）和离线基准（Benchmark 阶段）两个互补维度，结合可观测指标提供持续反馈。

### 11.1 在线评估（Runtime 阶段）

在线评估在每次检索后异步执行，不增加用户感知的延迟：

**RetrievalMetrics（每次检索后异步计算）**：

```rust
/// Metrics collected after each retrieval operation
/// Computed asynchronously to avoid impacting retrieval latency
pub struct RetrievalMetrics {
    pub result_count: usize,          // Number of results returned
    pub avg_score: f32,               // Average relevance score of results
    pub max_score: f32,               // Highest relevance score
    pub abstention_triggered: bool,   // Whether Abstention was triggered (§6.5)
    pub retrieval_level: u8,          // Degradation level (0-3, §6.1)
    pub graph_expand_nodes: usize,    // Number of nodes expanded via graph_expand
    pub hint_type: HintType,          // memory_hint.type used (s/f/r/i)
}
```

- **result_count + avg_score + max_score**：基础检索质量指标，连续低 avg_score 暗示 min_cosine 阈值需调整
- **abstention_triggered**：拒答率过高（>30%）可能说明 min_cosine 过严；拒答率过低（<5%）可能说明 min_cosine 过松
- **retrieval_level**：降级频率反映 SQLite 检索健康状况

**轻量 LLM Judge（Phase 3+，可选）**：

使用小型模型（如 qwen3:1.7b）评估检索结果与查询的相关性，作为在线评估的补充：

```
LLM Judge 流程（Phase 3+）：
  触发条件：采样率 10%（每 10 次检索评估 1 次）
  输入：查询文本 + Top-3 检索结果
  输出：每个结果的相关性评分（1-5 分）
  成本：qwen3:1.7b 约 50 tokens/次，可忽略
  用途：
    - 校准 RRF 分数与实际相关性的偏差
    - 识别 hybrid_search 系统性弱点（如特定 type 的检索质量差）
```

**用户隐式反馈**：

- Agent 后续输出是否引用了检索结果（引用率 = 引用了检索结果的回复数 / 触发了检索的回复数）
- 用户是否在同一话题追问（暗示检索结果不够完整）
- 用户是否直接否定 Agent 回复（暗示检索结果不准确）

### 11.2 离线基准（Benchmark 阶段）

**LongMemEval 5 维集成**：

LongMemEval 是当前 Agent 记忆系统最权威的评测基准，覆盖 5 个核心维度：

| 维度       | 代码 | 评估内容             | 与 ACowork 模块的对应    |
| ---------- | ---- | -------------------- | ---------------------------- |
| 信息提取   | IE   | 从对话中提取关键信息 | 即时提取（§4.1）             |
| 多会话推理 | MR   | 跨会话整合信息推理   | 关联扩散检索（§6）           |
| 时序推理   | TR   | 按时间顺序推理事件   | 经历层时间索引 + CDC history |
| 知识更新   | KU   | 处理信息更新和冲突   | 冲突处理三层信号（§6.4）     |
| 拒答       | Abs  | 信息不足时选择拒答   | Abstention 机制（§6.5）      |

**分阶段目标**：

| 阶段    | 综合目标 | IE   | MR   | TR   | KU   | Abs  | 说明                        |
| ------- | -------- | ---- | ---- | ---- | ---- | ---- | --------------------------- |
| Phase 2 | 65%+     | 70%+ | 60%+ | 55%+ | 60%+ | 60%+ | 精度优先，Abstention 是短板 |
| Phase 3 | 75%+     | 80%+ | 70%+ | 65%+ | 75%+ | 75%+ | 离线巩固 + LLM Judge 提升   |

**Phase 3 额外基准目标**：

- BEAM MDS < -0.12（多跳扩散检索的语义漂移控制）
- Accuracy@1M > 50%（大规模节点下的 Top-1 准确率）

### 11.3 可观测指标

**NRR（归一化检索相关性）**：

```
NRR = avg_score / max_possible_score

其中 max_possible_score 为完全匹配的理论最高分
NRR < 0.5 → 检索质量告警，需检查 embedding 模型或索引状态
NRR > 0.8 → 检索质量良好
```

**冲突处理准确率**：

```
冲突处理准确率 = 自动判定正确数 / 自动判定总数

"正确"定义：自动判定结果与后续 LLM 仲裁或用户确认一致
目标：Phase 2 > 85%，Phase 3 > 90%
告警：准确率 < 80% 时，回退所有启发式自动判定为 LLM 仲裁
```

**衰减参数校准指标**：

```
lambda 值 vs 用户反馈的"记忆过期率"：
  - 用户抱怨"记不住"→ lambda 可能过大（衰减太快）
  - 用户抱怨"老信息干扰"→ lambda 可能过小（衰减太慢）
  - 校准方式：收集用户反馈，与 Dormant 转化率交叉分析
  - 目标：Dormant 转化率与用户感知的"记忆过期率"偏差 < 15%
```

**指标聚合与告警**：

| 指标               | 采集频率     | 告警阈值            | 告警动作                       |
| ------------------ | ------------ | ------------------- | ------------------------------ |
| NRR                | 每次检索     | < 0.5 持续 10 次    | 检查 embedding 模型 + 索引状态 |
| Abstention 率      | 每次检索     | > 30% 或 < 5%       | 调整 min_cosine 阈值           |
| 冲突自动判定准确率 | 每次离线巩固 | < 80%               | 回退为 LLM 仲裁                |
| 降级频率           | 每次检索     | Level 2+ 占比 > 20% | 检查 SQLite 检索健康状态           |

### 11.4 质量门禁

Phase 2 交付前必须通过以下验证：

**LongMemEval-S（~115K tokens）验证**：

- 使用 LongMemEval-S 标准子集（约 115K tokens 上下文长度）运行完整 5 维评测
- 综合分数 >= 65%，各维度不低于 50%
- Abs 维度 >= 60%（拒答是最关键的差异化能力）
- 测试环境：单 Agent，SqliteStore 存储后端，embedding 模型 Ollama/Remote API（取决于 provider 配置）

**功能验证清单**：

| 功能              | 验证方法             | 通过标准                  |
| ----------------- | -------------------- | ------------------------- |
| Abstention 机制   | 人工构造低相关性查询 | 触发拒答且不产生幻觉      |
| 两层冲突检测      | 人工构造冲突场景     | 两层信号均能正确标记      |
| min_cosine 门控   | 调整阈值观察检索结果 | 阈值与过滤效果符合预期    |
| 检索权重动态调整  | 不同 hint.type 查询  | 权重和扩散参数正确切换    |
| 即时/离线巩固边界 | 对比即时和离线产出   | PendingNode 正确升级/降级 |

**Phase 3 质量门禁（在 Phase 2 基础上追加）**：

- LongMemEval 完整集（非 S 子集）综合 >= 75%
- BEAM MDS < -0.12
- Accuracy@1M > 50%
- 离线巩固能发现即时提取无法捕获的隐式关联（人工评估）

