# acowork-memory + acowork-sqlite — Agent 私有记忆引擎与记忆服务

**定位**：Agent 私有 Memory 的存储 + 检索 + 离线蒸馏 + **独立服务进程** 双 crate 描述。

- `acowork-memory`：记忆 trait / 类型 / 离线蒸馏 / 检索质量指标 **加上** SQLite 存储引擎（`src/sqlite/`）**加上** `acowork-memory` 服务进程（`src/main.rs` + `src/service/`，ADR-101）。
- `acowork-sqlite`：仅保留仍属 Runtime 的两个旁路子系统——会话向量索引 `ConversationStore`（ADR-081）与 `SqliteSessionMetaStore`（ADR-082 §4 step 3）；两者都挂在已打开的 `SqliteStore` 上，并 re-export 该类型。

**ADR-101 拆分了三件事**：

1. **存储引擎上移**：`SqliteStore` / schema / hybrid retrieval / admin 实现从 `acowork-sqlite` 搬进 `acowork-memory::sqlite`。理由在服务二进制：二进制所在的 crate 必须能 link 一个具体后端，而后端 crate 不能反过来依赖它实现的 trait 所在 crate。`acowork-sqlite::SqliteStore` 作为 re-export 保留，旧调用方无需修改。
2. **进程形态改变**：记忆不再是 Runtime 进程内能力，而是 Gateway supervisor 托管的独立进程 `acowork-memory`（默认 `127.0.0.1:18084`，一个进程服务所有实例）。每实例一库 `{data_dir}/instances/{instance_id}/memory.db`（`data_dir` 默认 `$HOME/.acowork/acowork-memory/`）。
3. **Runtime 归零**：Runtime 不持 `Arc<dyn MemoryProvider>`，也不碰记忆库；写记忆只通过 MCP `memory_store` / `memory_recall`（经 Gateway `memory_proxy`），面板读只通过 REST 反代。`{workspace}/memory/private.sqlite` 仍作为 Runtime 侧 sessions / 会话索引的共享文件存在（ADR-082），但其中的记忆行已由 Node 一次性交接给服务并 DELETE（ADR-101 决策 10）。

---

## 1. 设计目标与边界

| 维度              | 目标 / 边界                                                                          |
| ----------------- | ------------------------------------------------------------------------------------ |
| 存储引擎          | 单文件 SQLite（rusqlite 直连，无 ORM，无 Diesel）。PRAGMA WAL + NORMAL + foreign_keys |
| 向量检索          | 同库 `vectors` 表 + 应用层余弦扫描；当前实现是全量精确扫描，详见 ADR-082 C1         |
| 全文检索          | 每 Label 一张 FTS5 虚表，`tokenize='trigram'`（CJK 子串匹配）                          |
| 关系表达          | 关系是 `Knowledge(subtype=Relation)` 节点，不是图边；独立的 `edges` 表与 `graph_expand_*` / `create_memory_edge` API 已随 ADR-082 D4（triples-removed 起于 ADR-057）下线 |
| Embedding          | 服务自持 OpenAI-compatible 客户端（`service/embedding.rs`），端点从 Gateway 全局资源快照解析（`service/resources.rs`）；库只存向量字节 |
| 生命周期          | 启动 O(1)（无 WAL replay、无 BM25 rebuild），详见 ADR-082 §3                         |
| 进程形态           | **独立二进制**（ADR-101）：axum 一路由同时供 REST（面板 + admin）与 MCP（`POST /mcp`）；由 Gateway supervisor spawn / 写 port-file / 重启 |
| Schema 演进       | `PRAGMA user_version` + 幂等迁移步，详见 ADR-082 D2                                  |
| 隔离              | 按 **Agent 实例** 一库：`{data_dir}/instances/{instance_id}/memory.db`，id 是 ADR-073 的安装期不可变 UUID；请求不带可用 id 即 4xx，不存在跨实例路径 |

---

## 2. Crate 结构

### 2.1 `core/acowork-memory/`

```
crates/acowork-memory/
├── Cargo.toml                # lib + [[bin]] acowork-memory（ADR-101）
└── src/
    ├── lib.rs                  # Pub re-exports of trait/types/store/service
    ├── main.rs                 # 服务进程入口：clap Cli → MemoryService::run
    ├── provider.rs             # MemoryProvider trait (35+ methods)
    ├── store.rs                # MemoryStore trait (legacy, 16 methods)
    ├── manager.rs              # MemoryManager — Retrieve / Inject / Record 三阶段（服务只用 retrieve）
    ├── quality.rs              # MemoryQualityConfig + DedupQuality + ConsolidationQuality
    ├── admin.rs                # MemoryAdminService — 节点 CRUD / 列表 / 重建
    ├── keyword.rs              # 关键词 sanitize / 长度门禁 (ADR-062)
    ├── judge.rs                # LLM Judge 采样决策 (ADR-068 §5)
    ├── types.rs                # Episode / KnowledgeNode / ProceduralNode
    │                           # AutobiographicalNode / MemoryQuery / SearchResult
    │                           # MemoryContext / DecayConfig / DecayScanResult ...
    ├── wire.rs                 # 面板 wire 格式（服务与它取代的 Runtime 端点同形，ADR-101）
    ├── consolidation/
    │   ├── mod.rs              # EpisodicDistiller trait + SchedulerConfig
    │   └── distiller.rs        # DefaultEpisodicDistiller（LLM 蒸馏实现）
    ├── retrieval_metrics.rs    # 检索质量指标（Abstention / 冲突 / 去重维度评估）
    ├── sqlite/                 # ← ADR-101：存储引擎从 acowork-sqlite 搬进来
    │   ├── mod.rs              # SqliteStore + 工厂 + 错误类型
    │   ├── schema.rs           # SCHEMA_SQL + SCHEMA_VERSION + MIGRATIONS + apply_migrations
    │   ├── provider.rs         # MemoryProvider / MemoryStore trait 实现
    │   ├── retrieval.rs        # hybrid_search / vector_search / text_search / RRF_K
    │   ├── admin.rs            # MemoryAdminService 实现
    │   ├── legacy_import.rs    # ADR-101 决策 10：import_legacy_store / purge_memory_rows / has_sqlite_magic
    │   └── tests.rs            # trait round-trip 断言（每类节点字段必现）
    └── service/                # ADR-101 服务层（lib 之上的 additive）
        ├── mod.rs              # 导出 Cli / MemoryService / MemoryServiceConfig / MemoryState
        ├── cli.rs              # 进程参数（端口 / data_dir / config / 日志）
        ├── config.rs           # `{data_dir}/config.toml`：蒸馏与遗忘调度的权威来源
        ├── error.rs            # 服务错误 → HTTP 状态码（空体）/ JSON-RPC code 映射（MCP 拒权重试用 -32002）
        ├── server.rs           # axum 路由器 + 启动（health / api / mcp 拼装）
        ├── state.rs            # 每实例 store 注册表（懒开 + 缓住）+ instance_id 校验
        ├── health.rs           # `/health` 探针（supervisor 就绪判定）
        ├── api/                # 面板 REST：nodes / search / consolidation / rebuild / admin / import_legacy
        ├── mcp/                # `POST /mcp`：initialize / tools\_list / tools\_call（memory\_store、memory\_recall）
        ├── pipeline.rs         # 蒸馏 + 遗忘后台循环（每实例一个调度器）
        ├── llm.rs              # 蒸馏用的 OpenAI-compatible chat 客户端 + 候选链解析
        ├── embedding.rs        # 检索与重建用的 embedding 客户端
        └── resources.rs        # 从 Gateway `GET /api/global-resources` 拉 provider / embedding 快照
```

> `session_meta.rs` 已从本 crate 移除（ADR-101）：类型与 `SessionMetaStore` trait 的
> 权威定义在 `acowork-core::session_meta`，本 crate 只在旧路径上 re-export，以免
> Runtime 为了拿到会话元数据而依赖记忆 crate。

**模块职责一句话**：

- `provider.rs` / `store.rs`：定义 trait（实现侧在本 crate 的 `sqlite/`）。
- `manager.rs`：调用方与 Store 之间的中间层，编排三阶段记忆生命周期。服务侧只用它的
  **检索编排**（`service/mcp/tools.rs` 用 `MemoryManagerConfig::default()` 跑 `retrieve()`：
  检索质量参数现在是服务级的，旧 Runtime 会话里的 per-agent 配置已无对应物）。
- `consolidation/distiller.rs`：离线蒸馏 6 步流水线（ADR-068 §3.4），按 `MemoryProvider` 接口做写读；由 `service/pipeline.rs` 调度。
- `retrieval_metrics.rs`：检索质量指标（见 §6）。
- `quality.rs` / `keyword.rs` / `judge.rs`：蒸馏侧门控配置。
- `sqlite/legacy_import.rs`：遗留库一次性交接的唯一写入入口（ADR-101 决策 10）。
- `service/`：进程、路由、鉴权后的实例命名空间、调度与外部依赖（LLM / embedding）解析。

### 2.2 `core/acowork-sqlite/`

ADR-101 后本 crate 只剩 **两个仍属 Runtime 的旁路子系统**（存储引擎已上移到 `acowork-memory::sqlite`）：

```
crates/acowork-sqlite/
├── Cargo.toml                # depends on acowork-memory (uses its traits) — never the reverse
└── src/
    ├── lib.rs                  # re-export acowork_memory::sqlite::{SqliteStore, Error, ...}
    ├── conversation.rs         # ConversationStore（ADR-082 §4 step 2，复用 nodes/vectors/FTS）
    └── session_meta.rs         # SqliteSessionMetaStore（ADR-082 §4 step 3，sessions 表）

tests/
├── memory_chains_e2e.rs        # 写读 / 蒸馏 / 去重 端到端
├── shared_store_e2e.rs         # 多实例共享同库（kafka 风格多 label 边界）
├── session_meta.rs             # sessions 表 CRUD
└── store_isolation.rs          # 同库不同 label 隔离（privacy 边界）
```

两个旁路都以共享句柄包已打开的 store（`ConversationStore::from_store(Arc<SqliteStore>)`、`SqliteSessionMetaStore::new(Arc<SqliteStore>)`），所以 Runtime 侧的会话索引与 session 元数据仍共享同一个 `{workspace}/memory/private.sqlite`。记忆行不在里面了——它们经 Node 一次性上传后从本文件 DELETE（ADR-101 决策 10）。

---

## 3. SQLite Schema（ADR-082 D1）

单库 6 张物理表 + 6 张虚表，所有 DDL 在 `schema.rs::SCHEMA_SQL` 内幂等；任何次 `open` 重跑不破坏数据。

`SCHEMA_SQL` 冻结在 v0 基线形状：迁移新增的列/表**绝不**回填进这段 DDL（`schema.rs` 顶部有显式约定），fresh DB 从 `user_version = 0` 起把整条迁移链跑完，形状与升级库一致。

### 3.1 物理表

| 表名           | 作用                                                                                     |
| -------------- | ---------------------------------------------------------------------------------------- |
| `nodes`        | 全部记忆节点的统一行（一行一节点），`label` 区分记忆类型，`props` JSON 存其余字段             |
| `vectors`      | 节点 embedding（f32 BLOB），FK `node_id` → `nodes.id` ON DELETE CASCADE                  |
| `meta`         | 库级元数据（当前仅 `embedding_dim`）；reopen 以库内值为准，避免调用方维度陈旧             |
| `conversation_state` | 每会话的索引水位（`next_line` / `byte_offset`），与被索引行同事务更新（design 27 §4）    |
| `purge_log`    | 遗忘衰减最终目的表（先归档再删除，单字段 `decay/purge/expiry` 可区分原因）                  |
| `sessions`     | Session 元数据（ADR-082 §4 step 3，替代 `conversations/meta/*.json` 边车文件）             |

### 3.2 虚表（FTS5，trigram 分词）

| 虚表                  | 用途                                              |
| --------------------- | ------------------------------------------------- |
| `fts_episodic`        | 经历层 `content` 全文索引（CJK 子串匹配）         |
| `fts_knowledge`       | 沉淀层 Knowledge `content` 全文索引               |
| `fts_procedural`      | 沉淀层 Procedural `content` 全文索引              |
| `fts_autobiographical`| 沉淀层 Autobiographical `content` 全文索引        |
| `fts_conversation`    | 对话索引（ADR-082 §4 step 2，复用同套机制）       |
| `fts_sessions`        | Session 元数据全文索引（title/agent_id/workspace） |

**trigram 选择理由**：CJK 在 `unicode61` 下会被折叠成一个 token，跨词匹配不到；`trigram` 切成 3 字符窗口，子串匹配对齐人类预期（ADR-082 D3）。

### 3.3 `nodes` 行契约

- `id INTEGER PRIMARY KEY` —— 库内主键；Runtime/蒸馏端跨节点关联走该 ID。

> ⚠️ **id 域**：历史上文档存在 `node_id: String` / `entity_id: u64` 混用。**当前 contract**：
> 库内 `nodes.id` 是 `INTEGER`（u64 对外呈现）；节点 JSON 序列化的 `id` 字段在 trait 层是 `Option<u64>`（serde 投影），Runtime 业务逻辑统一按 `u64` 处理。**不再使用 String 节点 ID**（任何历史 String 引用都是 ADR-082 之前的遗留，应在代码 review 时清理）。

- `label TEXT` —— `{Episodic, Knowledge, Procedural, Autobiographical, Session, Conversation}` 之一。
- `status TEXT` —— 节点生命周期状态 `{Active, Dormant, Archived}`；默认 `Active`。
- `props JSON NOT NULL` —— 该 label 节点结构体的 `serde_json` 序列化（除 `id` / `embedding` 外所有字段）。
- `created_at` / `updated_at` —— ISO8601 字符串，`Default::default()` 是空串（不是 NULL）。

### 3.4 `vectors` 行契约

```
node_id   INTEGER PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE
dim       INTEGER NOT NULL        -- 当前库内向量维度
embedding BLOB NOT NULL           -- dim × f32 little-endian
```

写入时维度校验失败 → 报错，不静默破坏相似度分数；调用方陈旧维度被库内值覆盖（`SqliteStore::from_connection` 优先 `meta.embedding_dim`）。

### 3.5 `conversation_state` 行契约

```
session_id   TEXT PRIMARY KEY
next_line    INTEGER NOT NULL DEFAULT 0   -- 下一条待索引的物理行号
byte_offset  INTEGER NOT NULL DEFAULT 0   -- 对应字节偏移
```

水位与被索引行在同一事务里提交，所以崩溃后不会重复索引，也不会漏索引。它让 sweep 能靠文件大小跳过未变更的日志，把重启成本从全表扫描降到 O(#sessions)（design 27 §4 的「请求路径不做整会话读取」铁律）。

### 3.6 `purge_log` 行契约

遗忘是唯一销毁节点数据的路径，先归档再删除（`SqliteStore::purge_expired`）：

```
id        INTEGER PRIMARY KEY
node_id   INTEGER NOT NULL
label     TEXT NOT NULL
props     JSON NOT NULL         -- 整行 JSON，恢复 = 重新 INSERT
content   TEXT NOT NULL DEFAULT ''
reason    TEXT NOT NULL DEFAULT '' -- {decay, manual_purge, conflict_lost, ...}
purged_at TEXT NOT NULL
```

---

## 4. Schema 演进（ADR-082 D2）

### 4.1 三件套

```rust
pub const SCHEMA_VERSION: i64 = 5;                           // 当前版本
pub(crate) const MIGRATIONS: &[(i64, &str)] = &[...];        // [(target, sql)]
pub(crate) fn apply_migrations(conn: &mut Connection) -> Result<()>;
```

### 4.2 升级协议

`PRAGMA user_version` 是 schema-version 的唯一真相。`apply_migrations` 流程：

1. `PRAGMA user_version` 读出 stored 值；
2. 对每个 `(target, sql)`，`stored < target` 才执行；
3. 全部执行后写 `PRAGMA user_version = SCHEMA_VERSION`，整体一个 transaction。

**幂等保证**：幂等来自 `user_version` 版本门（`stored < target` 才执行）+ 单 transaction，而**不是**来自 SQL 自身的守卫。`ALTER TABLE ... ADD COLUMN` 没有 `IF NOT EXISTS` 形式，重复执行必然报错——所以迁移步绝不能被跑两次，版本门是唯一的保护。

### 4.3 加字段流程（开发者参考）

1. 在 `MIGRATIONS` 加新条目 `(target = SCHEMA_VERSION + 1, sql)`，SQL 为 `ALTER TABLE ... ADD COLUMN`；
2. **不要**把该列加进 `SCHEMA_SQL` 的初始 DDL——基表 DDL 只描述 v1 之前的形状，新列一律走 `MIGRATIONS`（见 `schema.rs` 中的显式约定）。fresh DB 从 `user_version = 0` 起，会把整条链跑完，形状与升级库一致，这一点由 `fresh_and_upgraded_databases_share_one_sessions_shape` 钉住；
3. 新列一律 nullable 且无默认值，让旧行读成 `NULL` = "尚无该数据"，而不是伪造一个 0；
4. 节点结构体加 `#[serde(default)]` 字段（保证旧库读不报错）；
5. 跑 `tests::round_trip_*` 断言新字段 round-trip 不丢，并在 `fresh_and_upgraded_*` 的 `DROP COLUMN` 清单里补上新列；
6. 提 PR，bump `SCHEMA_VERSION`。

### 4.4 迁移史

| target | 内容 | 来源 |
|--------|------|------|
| 1 | `sessions.version` / `sessions.corrupted` | ADR-082 D2 |
| 2 | `sessions.user_id` / `sessions.visibility` | ADR-076 §决策 4（session 归属） |
| 3 | `sessions.perf_tps_sum` / `perf_tps_samples` / `perf_ttft_ms_sum` / `perf_ttft_ms_samples` | 会话级 LLM 输出速度与首字延迟 |
| 4 | `sessions.chat_session_id` | 设计 27 §8.2（群组执行会话归属到某个 Chat 会话） |
| 5 | `sessions.last_compaction_summary` + 新表 `conversation_state` | 设计 27 §4（O(1) 交接读 + 索引水位） |

v3 存 **sum + count** 而非折叠好的均值：两个指标各自统计自己的样本数。一次调用可能只测得出 TTFT（Provider 未上报 completion 数），共用一个分母会把速度均值系统性抬高。

> `acowork-memory::sqlite::schema::SCHEMA_VERSION` 与 trait 层语义版本独立 —— 它是 SQLite 库内 `user_version`，与 `acowork-memory` crate 版本号无耦合。

---

## 5. SqliteStore —— MemoryProvider 实现

> 类型的权威定义在 `acowork-memory::sqlite`（ADR-101）；`acowork_sqlite::SqliteStore` 是同
> 一个类型的 re-export。本节里的工厂与 trait 实现均指前者。

### 5.1 工厂函数

```rust
// 主路径（embedding 由调用方传入）
SqliteStore::open(path: impl AsRef<Path>, embedding_dim: usize) -> Result<Self>

// 内存版（测试）
SqliteStore::open_in_memory(embedding_dim: usize) -> Result<Self>

// 不写 embedding_dim 的开口（SessionMeta / Conversation 等共享库文件、不碰向量的子系统）
SqliteStore::open_dim_agnostic(path: impl AsRef<Path>) -> Result<Self>
```

`open_dim_agnostic` 存在的原因：共享 workspace `.sqlite` 文件的子系统（SessionMeta、Conversation）不应抢着写 `meta.embedding_dim`；如果它们先 `open(path, 768)` 落了维度，再来 `open(path, 384)` 的真记忆子系统会沿用错误的 768（详见 `lib.rs::from_connection` 的 `meta` 优先逻辑）。

### 5.2 并发模型

- 单 `Connection`，外层 `Mutex` 保护。
- ADR-082 工作负载前提：**一 agent 一库，零查询并发**；串行访问充分且简单。
- ADR-101 保留了这层形状，但把请求方数量抬高了：一个服务进程要带所有实例，所以**不同实例之间真正
  并行**（各自一个 `Arc<SqliteStore>`，存在 `service/state.rs` 的 `RwLock<HashMap<…>>` 里），而打在
  **同一**实例上的面板读、MCP 调用与蒸馏循环在该 store 的 mutex 上串行。一库一连接的选择是故意不改的
  ——变忙的是单实例内的争抢，不是全局。
- `Connection: Send`，`SqliteStore: Send + Sync`（静态断言）。
- 高并发场景（如果将来发生）→ 升级路径：换连接池，**不改** trait 接口。

### 5.3 trait 实现的对应关系

| `MemoryProvider` 方法族                  | `acowork-memory::sqlite` 实现位置       |
| ---------------------------------------- | ------------------------------- |
| 经历层 CRUD / 检索                       | `provider.rs::episodic_*`       |
| 沉淀层 CRUD / 检索                       | `provider.rs::consolidated_*`   |
| 混合检索 / RRF 融合                      | `retrieval.rs` + `provider.rs`  |
| 节点 CRUD                        | `provider.rs::node_*`           |
| 遗忘衰减 / 归档                          | `provider.rs::decay_*` / `purge_*` |
| 健康检查 / 统计                          | `provider.rs::health_*`         |
| `MemoryAdminService`（节点列表/重建）    | `admin.rs`                        |

### 6. 检索指标（acowork-memory::retrieval_metrics.rs）

`RetrievalMetrics` 收集离线评估与在线指标：

| 维度          | 含义                                                                 |
| ------------- | -------------------------------------------------------------------- |
| Abstention    | 检索不可靠时拒答（与 ADR-082 P6 的 `AbstentionConfig` 配对）          |
| Conflict      | 冲突仲裁准确率（与 ADR-068 Step 4 Judge 配对）                         |
| Dedup         | 同 `(subject, predicate)` 高相似节点的去重（ADR-062 D2）               |
| LongMemEval   | LongMemEval 子集回归指标                                            |

**与 trait 解耦**：指标采集点放在 `MemoryManager` 内的 hook，不进 trait；切换存储后端不影响指标。

---

## 7. SqliteStore 关键不变量

- **写读对称**：节点 `serde_json` round-trip 后字段全等（`tests::tests::round_trip_*` 断言）。
- **维度一致**：`vectors.dim` 与 `meta.embedding_dim` 一致；reopen 以库内值为准。
- **FK 级联**：`vectors` / `purge_log` FK 全 `ON DELETE CASCADE`，节点删除时资源随之清理。
- **Status 投影**：`status` 不进 `props`，总是 `nodes.status` 列，便于过滤。
- **WAL 不丢**：`PRAGMA synchronous = NORMAL` + WAL，崩溃恢复安全（详见 ADR-082 D2）。

---

## 8. 错误类型

```rust
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sqlite: {0}")]     Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]       Json(#[from] serde_json::Error),
    #[error("io: {0}")]         Io(#[from] std::io::Error),
    #[error("{0}")]             Memory(String),  // 不变量违反
}
```

`From<Error> for AcoworkError::Memory` 已实现，`?` 在 trait 实现内直接传播。

---

## 9. 与相邻模块的边界

| 边界                    |     | 说明                                                                                  |
| ---------------------- | --- | ------------------------------------------------------------------------------------- |
| Runtime ↔ 记忆           | ↔   | **零边界**（ADR-101）：Runtime 不 `use acowork_memory::*`，也不持任何 store 句柄；写记忆只走 MCP `memory_store` / `memory_recall`            |
| Desktop ↔ 面板数据      | ↔   | 只经 Gateway `memory_proxy`（面板路径 `/api/agents/{id}/memory/*` + 短路径 `/api/memory/*`）；Gateway 不打开实例库（ADR-009 §5） |
| Gateway ↔ 服务进程      | ↔   | supervisor 只管生命周期：spawn / port-file / `/health` 探针 / 重启，并将 MCP 注入全局资源（ADR-101 决策 1、3）                    |
| 服务 ↔ Embedding         | ↔   | 服务自持 OpenAI-compatible embedding 客户端（`service/embedding.rs`），端点从 Gateway 全局资源快照解析；库只存向量字节              |
| Storage ↔ 服务数据目录  |     | 服务写 `{data_dir}/instances/{instance_id}/memory.db`（含 WAL/SHM）；Runtime 与 Gateway 都不直读该目录                              |
| Runtime ↔ Workspace 库   |     | `{workspace}/memory/private.sqlite` 仍是 Runtime 的会话索引 + session 元数据所在（`open_dim_agnostic`），但已不含记忆行                  |
| Cloning                  | ↔   | `acowork-node` 的 clone 仍拷 `private.sqlite` + `*.wal` + `*.shm` 三件套并 `purge_conversation_state()`（ADR-082 D5）；但 ADR-101 之后记忆不再随 workspace 目录走，**clone 不继承服务侧记忆**（见 ADR-101 §5 风险表） |
| 遗留库交接               | ↔   | 仅 `acowork-node` 发起：读 `{workspace}/memory/private.sqlite` → `POST /api/memory/admin/import?instance_id=` → 导入成功才本地 DELETE（ADR-101 决策 10） |
| InboxAgent 测试         | ↔   | 集成测试用 `SqliteStore::open_in_memory()`，与生产代码同 trait、同路径                  |

---

## 10. 已下线能力（避免重新引入）

下面这些能力在 `acowork-grafeo` 时代存在，但 ADR-082 实施后已下线。**新代码不应再调用**：

- ❌ `grafeo-engine` crates（grafeo/gradfo-common/gradfo-core）—— crate 已从 workspace 删除。
- ❌ `grafeo-engine` 原生 LPG / GQL / HNSW / BM25 / PageRank / CDC / 社区检测 API。
- ❌ `MemoryProvider` 的 `graph_expand_*` / `create_memory_edge` / `apply_pagerank_boost` 方法（ADR-082 D4 删除）。
- ❌ `.grafeo` 单文件存储 —— 历史工作树残留（`memory/private.grafeo*`）可物理删除。
- ❌ `meta/*.json` 边车文件 —— SessionMeta 数据已迁移进 `sessions` 表。
- ❌ `conversation_index.grafeo` / 独立 `conversation_index.sqlite` —— 全部进 `nodes` 统一表。

ADR-101（记忆服务化）又下线了一批 —— 它们同样不应被重新引入：

- ❌ Runtime 内的记忆代码路径（`agent/loop_memory.rs`、`memory_store` / `memory_recall` 内置 tool、
  `Arc<dyn MemoryProvider>` 字段、`init_memory_store` / `retrieve_and_inject_memories`）。
- ❌ Runtime 自己跑蒸馏 / 遗忘后台循环（现在只有 `service/pipeline.rs` 一处调度）。
- ❌ Gateway 直接读实例记忆库（`instances/*/memory.db`）——面板数据只走 `memory_proxy`。
- ❌ Runtime 侧 `POST …/memory/consolidate`（已并成服务侧 `POST /memory/distill`）。
- ❌ `acowork-runtime::http::global_search` 里的 memory scope（面板全局搜索直连 `/api/memory/search`）。
- ❌ `PackageOptions` 的 5 个记忆子集开关（`include_episodes` / `include_private_knowledge` /
  `include_procedural` / `include_autobiographical` / `include_public_knowledge`）——打包一直排除
  整个 `memory/` 目录，开关从未生效（ADR-101 决策 11）。
- ❌ `edges` 表与 `SqliteStore` 上的 `edge_*` 方法 —— 目标里没有这张表，别按文档历史写法去 JOIN
  （关系走 `Knowledge(subtype=Relation)` 节点，见 §1）。
