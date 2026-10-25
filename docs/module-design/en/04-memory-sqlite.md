# acowork-memory + acowork-sqlite — Agent Private Memory Engine and Memory Service

**Position**: Storage + retrieval + offline distillation of Agent-private Memory **plus the standalone service process**, described across two crates.

- `acowork-memory`: Memory traits / types / offline distiller / retrieval quality metrics — **plus** the SQLite storage engine (`src/sqlite/`) — **plus** the `acowork-memory` service process (`src/main.rs` + `src/service/`, ADR-101).
- `acowork-sqlite`: keeps only the two sidecar subsystems that still belong to the Runtime — the conversation vector index `ConversationStore` (ADR-081) and `SqliteSessionMetaStore` (ADR-082 §4 step 3); both hang off an already-open `SqliteStore`, and the crate re-exports that type.

**ADR-101 separated three things**:

1. **The storage engine moved up**: `SqliteStore` / schema / hybrid retrieval / admin implementations moved from `acowork-sqlite` into `acowork-memory::sqlite`. The reason is the service binary: the crate that hosts a binary must be able to link a concrete backend, and a backend crate must not depend back on the crate that defines the traits it implements. `acowork-sqlite::SqliteStore` survives as a re-export, so old call sites need no change.
2. **The process shape changed**: memory is no longer an in-process Runtime capability but a standalone `acowork-memory` process supervised by the Gateway (default `127.0.0.1:18084`, one process serving every instance). One DB per instance: `{data_dir}/instances/{instance_id}/memory.db` (`data_dir` defaults to `$HOME/.acowork/acowork-memory/`).
3. **The Runtime goes to zero**: the Runtime holds no `Arc<dyn MemoryProvider>` and never touches a memory DB; writes go only through the MCP tools `memory_store` / `memory_recall` (via the Gateway `memory_proxy`), panel reads only through the REST reverse proxy. `{workspace}/memory/private.sqlite` still exists as the Runtime's shared sessions / conversation-index file (ADR-082), but its memory rows were handed to the service once by Node and then DELETEd (ADR-101 Decision 10).

---

## 1. Goals & Boundaries

| Decision                    | Goal / Boundary                                                                |
| -------------------------- | ------------------------------------------------------------------------------ |
| Storage engine             | Single-file SQLite (rusqlite direct, no ORM, no Diesel). PRAGMA WAL + NORMAL + foreign_keys |
| Vector retrieval           | Same-database `vectors` table + application-layer cosine scan (ADR-082 C1)       |
| Full-text retrieval        | One FTS5 virtual table per Label, `tokenize='trigram'` (CJK substring match)   |
| Relation modelling         | Relations are `Knowledge(subtype=Relation)` nodes, not graph edges; the standalone `edges` table and the `graph_expand_*` / `create_memory_edge` APIs were removed by ADR-082 D4 (triples removed since ADR-057) |
| Embedding                  | The service owns its OpenAI-compatible client (`service/embedding.rs`); endpoints resolve from the Gateway global-resource snapshot (`service/resources.rs`); the DB stores vector bytes only |
| Lifecycle                  | Startup O(1) (no WAL replay, no BM25 rebuild) — ADR-082 §3                       |
| Process shape              | **Standalone binary** (ADR-101): one axum router serving both REST (panel + admin) and MCP (`POST /mcp`); spawn / port-file / restart owned by the Gateway supervisor |
| Schema evolution           | `PRAGMA user_version` + idempotent migration steps — ADR-082 D2                |
| Isolation                  | One DB per **Agent instance**: `{data_dir}/instances/{instance_id}/memory.db`, the id being the ADR-073 immutable install-time UUID; a request without a usable id is a 4xx — there is no cross-instance code path |

---

## 2. Crate Structure

### 2.1 `core/acowork-memory/`

```
crates/acowork-memory/
├── Cargo.toml                # lib + [[bin]] acowork-memory (ADR-101)
└── src/
    ├── lib.rs                  # Pub re-exports of trait/types/store/service
    ├── main.rs                 # Service entrypoint: clap Cli → MemoryService::run
    ├── provider.rs             # MemoryProvider trait (35+ methods)
    ├── store.rs                # MemoryStore trait (legacy, 16 methods)
    ├── manager.rs              # MemoryManager — Retrieve / Inject / Record three phases (the service uses retrieve only)
    ├── quality.rs              # MemoryQualityConfig + DedupQuality + ConsolidationQuality
    ├── admin.rs                # MemoryAdminService — node CRUD / list / rebuild
    ├── keyword.rs              # Keyword sanitize / length gates (ADR-062)
    ├── judge.rs                # LLM Judge sampling decision (ADR-068 §5)
    ├── types.rs                # Episode / KnowledgeNode / ProceduralNode
    │                           # AutobiographicalNode / MemoryQuery / SearchResult
    │                           # MemoryContext / DecayConfig / DecayScanResult ...
    ├── wire.rs                 # Panel wire format (shaped like the Runtime endpoints the service replaced, ADR-101)
    ├── consolidation/
    │   ├── mod.rs              # EpisodicDistiller trait + SchedulerConfig
    │   └── distiller.rs        # DefaultEpisodicDistiller (LLM distillation implementation)
    ├── retrieval_metrics.rs    # Retrieval quality metrics (Abstention / Conflict / Dedup evaluation)
    ├── sqlite/                 # ← ADR-101: the storage engine moved in from acowork-sqlite
    │   ├── mod.rs              # SqliteStore + factories + error type
    │   ├── schema.rs           # SCHEMA_SQL + SCHEMA_VERSION + MIGRATIONS + apply_migrations
    │   ├── provider.rs         # MemoryProvider / MemoryStore trait implementations
    │   ├── retrieval.rs        # hybrid_search / vector_search / text_search / RRF_K
    │   ├── admin.rs            # MemoryAdminService implementation
    │   ├── legacy_import.rs    # ADR-101 Decision 10: import_legacy_store / purge_memory_rows / has_sqlite_magic
    │   └── tests.rs            # Trait round-trip assertions (every field of every node type)
    └── service/                # ADR-101 service layer (additive on top of the lib)
        ├── mod.rs              # Exports Cli / MemoryService / MemoryServiceConfig / MemoryState
        ├── cli.rs              # Process arguments (port / data_dir / config / logging)
        ├── config.rs           # `{data_dir}/config.toml`: the authority for distill & forgetting scheduling
        ├── error.rs            # Service error → HTTP status (empty body) / JSON-RPC code mapping (MCP non-retryable rejection uses -32002)
        ├── server.rs           # axum router + boot (health / api / mcp assembly)
        ├── state.rs            # Per-instance store registry (lazy open + cached) + instance_id validation
        ├── health.rs           # `/health` probe (the supervisor's readiness check)
        ├── api/                # Panel REST: nodes / search / consolidation / rebuild / admin / import_legacy
        ├── mcp/                # `POST /mcp`: initialize / tools\_list / tools\_call (memory\_store, memory\_recall)
        ├── pipeline.rs         # Distillation + forgetting loops (one scheduler per instance)
        ├── llm.rs              # OpenAI-compatible chat client for distillation + candidate-chain resolution
        ├── embedding.rs        # Embedding client for retrieval and rebuild
        └── resources.rs        # Pulls the provider / embedding snapshot from Gateway `GET /api/global-resources`
```

> `session_meta.rs` was removed from this crate (ADR-101): the authoritative definitions of
> `SessionMeta` and the `SessionMetaStore` trait live in `acowork-core::session_meta`, and this
> crate only re-exported them — so the Runtime no longer has to depend on the memory crate just to
> reach session metadata.

**One-liner per module**:

- `provider.rs` / `store.rs`: define traits (implementations live in this crate's `sqlite/`).
- `manager.rs`: intermediate layer between the caller and the Store; orchestrates the three-phase
  memory lifecycle. The service side uses only its **retrieval orchestration**
  (`service/mcp/tools.rs` runs `retrieve()` with `MemoryManagerConfig::default()`: retrieval
  quality parameters are service-wide now, and the per-agent config that lived inside the old
  Runtime session handle has no counterpart on this side of the boundary).
- `consolidation/distiller.rs`: 6-step offline distillation pipeline (ADR-068 §3.4), reads/writes via the `MemoryProvider` interface; scheduled by `service/pipeline.rs`.
- `retrieval_metrics.rs`: retrieval quality metrics (see §6).
- `quality.rs` / `keyword.rs` / `judge.rs`: distillation-side gating config.
- `sqlite/legacy_import.rs`: the single write entrypoint for the one-off legacy-store handoff (ADR-101 Decision 10).
- `service/`: the process, the routers, the authenticated instance namespace, the schedulers, and how external dependencies (LLM / embedding) resolve.

### 2.2 `core/acowork-sqlite/`

After ADR-101 this crate keeps **only the two sidecar subsystems that still belong to the Runtime** (the storage engine moved up to `acowork-memory::sqlite`):

```
crates/acowork-sqlite/
├── Cargo.toml                # depends on acowork-memory (uses its traits) — never the reverse
└── src/
    ├── lib.rs                  # re-export acowork_memory::sqlite::{SqliteStore, Error, ...}
    ├── conversation.rs         # ConversationStore (ADR-082 §4 step 2, reusing nodes/vectors/FTS)
    └── session_meta.rs         # SqliteSessionMetaStore (ADR-082 §4 step 3, sessions table)

tests/
├── memory_chains_e2e.rs        # write/read / distiller / dedup end-to-end
├── shared_store_e2e.rs         # multiple instances sharing one DB (label boundaries)
├── session_meta.rs             # sessions table CRUD
└── store_isolation.rs          # same DB different label isolation (privacy boundary)
```

Both sidecars wrap an already-open store handle (`ConversationStore::from_store(Arc<SqliteStore>)`, `SqliteSessionMetaStore::new(Arc<SqliteStore>)`), so the Runtime-side conversation index and session metadata still share one `{workspace}/memory/private.sqlite`. The memory rows are gone from it — they were uploaded once through Node and then DELETEd from this file (ADR-101 Decision 10).

---

## 3. SQLite Schema (ADR-082 D1)

A single database with 6 physical tables and 6 virtual tables. All DDL in `schema.rs::SCHEMA_SQL` is idempotent; re-running on every `open` does not corrupt data.

`SCHEMA_SQL` is frozen at the v0 baseline shape: columns/tables introduced by migrations **never** get backfilled into that DDL (explicit convention at the top of `schema.rs`). A fresh database starts at `user_version = 0` and runs the whole migration chain, ending with exactly the shape an upgraded database has.

### 3.1 Physical tables

| Table          | Role                                                                                |
| -------------- | ----------------------------------------------------------------------------------- |
| `nodes`        | One row per memory node; `label` distinguishes memory type; `props` JSON holds other fields |
| `vectors`      | Node embedding (f32 BLOB), FK `node_id` → `nodes.id` ON DELETE CASCADE              |
| `meta`         | Library-level metadata (currently only `embedding_dim`); reopen reads from DB to avoid stale caller dimension |
| `conversation_state` | Per-session indexer watermark (`next_line` / `byte_offset`), committed in the same transaction as the indexed row (design 27 §4) |
| `purge_log`    | Forgetting decay terminal table (archive first, then delete; field-encoded reason like `decay/purge/expiry`) |
| `sessions`     | Session metadata (ADR-082 §4 step 3, replacing `conversations/meta/*.json` sidecar)  |

### 3.2 Virtual tables (FTS5, trigram tokenizer)

| Virtual table           | Purpose                                              |
| ----------------------- | ---------------------------------------------------- |
| `fts_episodic`          | Episodic-layer `content` full-text index (CJK substring match) |
| `fts_knowledge`         | Consolidated Knowledge `content` full-text index      |
| `fts_procedural`        | Consolidated Procedural `content` full-text index     |
| `fts_autobiographical`  | Consolidated Autobiographical `content` full-text index |
| `fts_conversation`      | Conversation index (ADR-082 §4 step 2, reuses same machinery) |
| `fts_sessions`          | Session metadata full-text index (title/agent_id/workspace) |

**Why trigram**: under `unicode61`, CJK text collapses into a single token; cross-word matches miss. `trigram` slices into 3-character windows, aligning substring matches with human expectations (ADR-082 D3).

### 3.3 `nodes` row contract

- `id INTEGER PRIMARY KEY` — in-DB primary key; Runtime/distiller cross-node association uses this ID.

> ⚠️ **id domain**: historically the docs mixed `node_id: String` / `entity_id: u64`. **Current contract**:
> `nodes.id` is `INTEGER` (u64 externally); the `id` field in the node's JSON serialization is `Option<u64>` at the trait layer (serde projection). Runtime business logic uniformly uses `u64`. **String node IDs are no longer used** (any historical String reference predates it; should be cleaned up in code review).

- `label TEXT` — one of `{Episodic, Knowledge, Procedural, Autobiographical, Session, Conversation}`.
- `status TEXT` — node lifecycle state `{Active, Dormant, Archived}`; default `Active`.
- `props JSON NOT NULL` — `serde_json` serialization of the label-specific node struct (every field except `id` / `embedding`).
- `created_at` / `updated_at` — ISO8601 strings; default is empty string (not NULL).

### 3.4 `vectors` row contract

```
node_id   INTEGER PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE
dim       INTEGER NOT NULL        -- current in-library vector dimension
embedding BLOB NOT NULL           -- dim × f32 little-endian
```

A dimension mismatch on write errors out — silent score corruption is never acceptable; a stale caller dimension is overridden by the stored `meta.embedding_dim` (see `SqliteStore::from_connection` priority).

### 3.5 `conversation_state` row contract

```
session_id   TEXT PRIMARY KEY
next_line    INTEGER NOT NULL DEFAULT 0   -- next physical line to index
byte_offset  INTEGER NOT NULL DEFAULT 0   -- matching byte offset
```

The watermark commits in the same transaction as the indexed row, so a crash neither re-indexes nor skips anything. It lets the sweep skip untouched logs by file size alone and brings reopening down to O(#sessions) instead of a full-table pass (the design 27 §4 iron rule: no whole-session reads on request paths).

### 3.6 `purge_log` row contract

Forgetting is the only path that destroys node data — archive first, then delete (`SqliteStore::purge_expired`):

```
id        INTEGER PRIMARY KEY
node_id   INTEGER NOT NULL
label     TEXT NOT NULL
props     JSON NOT NULL         -- full row JSON, recovery = re-INSERT
content   TEXT NOT NULL DEFAULT ''
reason    TEXT NOT NULL DEFAULT '' -- {decay, manual_purge, conflict_lost, ...}
purged_at TEXT NOT NULL
```

---

## 4. Schema Evolution (ADR-082 D2)

### 4.1 The trinity

```rust
pub const SCHEMA_VERSION: i64 = 5;                           // current version
pub(crate) const MIGRATIONS: &[(i64, &str)] = &[...];        // [(target, sql)]
pub(crate) fn apply_migrations(conn: &mut Connection) -> Result<()>;
```

### 4.2 Upgrade protocol

`PRAGMA user_version` is the single source of truth for the schema version. `apply_migrations` flow:

1. Read stored value from `PRAGMA user_version`.
2. For each `(target, sql)`, execute only when `stored < target`.
3. After all steps run, write `PRAGMA user_version = SCHEMA_VERSION` inside a single transaction.

**Idempotency**: comes from the `user_version` gate (`stored < target` is the only guard) plus a single transaction — **not** from self-guarding SQL. `ALTER TABLE ... ADD COLUMN` has no `IF NOT EXISTS` form, so a migration step that ran twice would error outright; the version gate is the sole protection against that.

### 4.3 Adding a column (developer reference)

1. Add a new entry `(target = SCHEMA_VERSION + 1, sql)` to `MIGRATIONS`, with `ALTER TABLE ... ADD COLUMN`.
2. Do **not** add the column to the initial DDL in `SCHEMA_SQL` — the base DDL describes the pre-v1 shape and new columns belong to `MIGRATIONS` only (explicit convention in `schema.rs`). A fresh DB starts at `user_version = 0` and runs the whole chain, ending with the same shape as an upgraded one; `fresh_and_upgraded_databases_share_one_sessions_shape` pins that.
3. Make the new columns nullable with no default, so a pre-existing row reads as `NULL` = "no data yet" rather than a fabricated zero.
4. Add the field to the node struct with `#[serde(default)]` so old databases still deserialize.
5. Run `tests::round_trip_*` to assert the new field round-trips, and add the column to the `DROP COLUMN` list in `fresh_and_upgraded_*`.
6. Open PR, bump `SCHEMA_VERSION`.

### 4.4 Migration history

| target | Contents | Origin |
|--------|----------|--------|
| 1 | `sessions.version` / `sessions.corrupted` | ADR-082 D2 |
| 2 | `sessions.user_id` / `sessions.visibility` | ADR-076 §Decision 4 (session ownership) |
| 3 | `sessions.perf_tps_sum` / `perf_tps_samples` / `perf_ttft_ms_sum` / `perf_ttft_ms_samples` | Session-level LLM output speed and first-token latency |
| 4 | `sessions.chat_session_id` | Design 27 §8.2 (a group-execution session's owning Chat session) |
| 5 | `sessions.last_compaction_summary` + the new `conversation_state` table | Design 27 §4 (O(1) handoff read + indexer watermark) |

v3 stores **sum + count**, not a folded mean: each figure divides by its own sample count. A call can be measurable for TTFT only (the Provider reported no completion count), and a shared denominator would systematically inflate the speed mean.

> `acowork-memory::sqlite::schema::SCHEMA_VERSION` is independent of the trait-layer semantic version — it is the SQLite in-library `user_version`, decoupled from the `acowork-memory` crate version.

---

## 5. SqliteStore — MemoryProvider Implementation

> The type is authoritatively defined in `acowork-memory::sqlite` (ADR-101);
> `acowork_sqlite::SqliteStore` is a re-export of the same type. The factories and trait
> implementations in this section all refer to the former.

### 5.1 Factories

```rust
// Main path (embedding provided by caller)
SqliteStore::open(path: impl AsRef<Path>, embedding_dim: usize) -> Result<Self>

// In-memory (tests)
SqliteStore::open_in_memory(embedding_dim: usize) -> Result<Self>

// Dimension-agnostic open (subsystems sharing the workspace .sqlite file that
// never touch vectors — SessionMeta / Conversation)
SqliteStore::open_dim_agnostic(path: impl AsRef<Path>) -> Result<Self>
```

Why `open_dim_agnostic` exists: subsystems sharing the workspace `.sqlite` file (SessionMeta, Conversation) must not race to write `meta.embedding_dim`; if they `open(path, 768)` first, the later `open(path, 384)` memory subsystem would inherit the wrong 768 (see the `meta`-priority logic in `lib.rs::from_connection`).

### 5.2 Concurrency model

- Single `Connection` with outer `Mutex`.
- ADR-082 workload premise: **one agent per DB, zero query concurrency**; serial access is sufficient and simpler.
- ADR-101 keeps the shape but raises the requester count: one service process serves every instance,
  so different instances run truly in parallel (one `Arc<SqliteStore>` each, held in
  `service/state.rs`'s `RwLock<HashMap<…>>`), while panel reads, MCP calls and the distill loop hitting
  the *same* instance serialize on that store's mutex. The single-connection-per-DB choice
  deliberately did not change — the traffic that got worse is per-instance, not global.
- `Connection: Send`, so `SqliteStore: Send + Sync` (statically asserted).
- If high concurrency arises in the future → swap to a pool, **without** changing the trait interface.

### 5.3 Trait method → implementation map

| `MemoryProvider` method family                  | `acowork-memory::sqlite` implementation location |
| ----------------------------------------------- | ----------------------------------------- |
| Episodic CRUD / retrieval                         | `provider.rs::episodic_*`                 |
| Consolidated CRUD / retrieval                    | `provider.rs::consolidated_*`             |
| Hybrid retrieval / RRF fusion                    | `retrieval.rs` + `provider.rs`            |
| Node CRUD                                         | `provider.rs::node_*`                     |
| Forgetting decay / archive                       | `provider.rs::decay_*` / `purge_*`        |
| Health check / stats                            | `provider.rs::health_*`                   |
| `MemoryAdminService` (node list / rebuild)        | `admin.rs`                                |

### 6. Retrieval Metrics (`acowork-memory::retrieval_metrics.rs`)

`RetrievalMetrics` collects offline evaluation and online metrics:

| Metric        | Meaning                                                                  |
| ------------- | ------------------------------------------------------------------------ |
| Abstention    | Refuse to answer when retrieval is unreliable (paired with ADR-082 P6 `AbstentionConfig`) |
| Conflict      | Conflict-arbitration accuracy (paired with ADR-068 Step 4 Judge)          |
| Dedup         | De-duplication of high-similarity nodes with the same `(subject, predicate)` (ADR-062 D2) |
| LongMemEval   | LongMemEval subset regression metrics                                    |

**Decoupled from the trait**: collection points live as hooks inside `MemoryManager`, not in the trait; swapping storage backends does not affect metrics.

---

## 7. Key Invariants of `SqliteStore`

- **Symmetric round-trip**: nodes round-trip through `serde_json` field-for-field equal (asserted by `tests::tests::round_trip_*`).
- **Dimension consistency**: `vectors.dim` matches `meta.embedding_dim`; reopen honors the stored value.
- **FK cascades**: `vectors` / `purge_log` FKs are all `ON DELETE CASCADE`; deleting a node cleans up resources.
- **Status projection**: `status` never goes into `props`, always in `nodes.status` column for cheap queries.
- **WAL safety**: `PRAGMA synchronous = NORMAL` + WAL; crash-recovery safe (see ADR-082 D2).

---

## 8. Error Type

```rust
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sqlite: {0}")]     Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]       Json(#[from] serde_json::Error),
    #[error("io: {0}")]         Io(#[from] std::io::Error),
    #[error("{0}")]             Memory(String),  // invariant violation
}
```

`From<Error> for AcoworkError::Memory` is implemented; `?` propagates directly inside trait implementations.

---

## 9. Boundaries with Neighbouring Modules

| Boundary                | ↔                                                                                                          | Notes                                                                                  |
| ----------------------- | --------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------- |
| Runtime ↔ memory         | ↔ | **Zero boundary** (ADR-101): the Runtime never `use acowork_memory::*` and holds no store handle; writes go only through the MCP tools `memory_store` / `memory_recall` |
| Desktop ↔ panel data    | ↔ | Only via the Gateway `memory_proxy` (panel paths `/api/agents/{id}/memory/*` + short paths `/api/memory/*`); the Gateway never opens an instance DB (ADR-009 §5) |
| Gateway ↔ service process | ↔ | The supervisor owns lifecycle only: spawn / port-file / `/health` probe / restart, and injects the MCP into the global resources (ADR-101 Decisions 1, 3) |
| Service ↔ embedding      | ↔ | The service owns its OpenAI-compatible embedding client (`service/embedding.rs`); endpoints resolve from the Gateway global-resource snapshot; the DB stores vector bytes only |
| Storage ↔ service data dir | ↔ | The service writes `{data_dir}/instances/{instance_id}/memory.db` (+ WAL/SHM); neither the Runtime nor the Gateway reads that directory directly |
| Runtime ↔ workspace DB   | ↔ | `{workspace}/memory/private.sqlite` is still where the Runtime's conversation index + session meta live (`open_dim_agnostic`), but it no longer contains memory rows |
| Cloning                  | ↔ | The `acowork-node` clone path still copies the `private.sqlite` + `*.wal` + `*.shm` triplet and calls `purge_conversation_state()` (ADR-082 D5); but since ADR-101 memory no longer travels with the workspace directory, so **a clone inherits no service-side memory** (see the ADR-101 §5 risk table) |
| Legacy-store handoff     | ↔ | Initiated by `acowork-node` only: read `{workspace}/memory/private.sqlite` → `POST /api/memory/admin/import?instance_id=` → delete the local rows only after a real import (ADR-101 Decision 10) |
| InboxAgent tests     | ↔ | Integration tests use `SqliteStore::open_in_memory()`, same trait / same path as production |

---

## 10. Deprecated Capabilities (Do Not Reintroduce)

These capabilities existed in the `acowork-grafeo` era but have been removed after ADR-082 implementation. **New code must not call them**:

- ❌ `grafeo-engine` crates (grafeo / grafeo-common / grafeo-core) — removed from workspace.
- ❌ `grafeo-engine` native LPG / GQL / HNSW / BM25 / PageRank / CDC / community-detection APIs.
- ❌ `MemoryProvider::graph_expand_*` / `create_memory_edge` / `apply_pagerank_boost` methods (removed by ADR-082 D4).
- ❌ `.grafeo` single-file storage — historical working-tree residue (`memory/private.grafeo*`) can be physically deleted.
- ❌ `meta/*.json` sidecar files — SessionMeta data has migrated to the `sessions` table.
- ❌ `conversation_index.grafeo` / standalone `conversation_index.sqlite` — all unified into the `nodes` table.

ADR-101 (memory-as-a-service) retired another batch — these must not be reintroduced either:

- ❌ Memory code paths inside the Runtime (`agent/loop_memory.rs`, the built-in `memory_store` /
  `memory_recall` tools, the `Arc<dyn MemoryProvider>` field, `init_memory_store` /
  `retrieve_and_inject_memories`).
- ❌ The Runtime running its own distill / forgetting loops (scheduling now lives in exactly one
  place: `service/pipeline.rs`).
- ❌ The Gateway reading instance DBs directly (`instances/*/memory.db`) — panel data goes through
  `memory_proxy` only.
- ❌ The Runtime-side `POST …/memory/consolidate` (folded into the service's `POST /memory/distill`).
- ❌ A memory scope inside `acowork-runtime::http::global_search` (the panel's global search now
  calls `/api/memory/search` directly).
- ❌ The five memory-subset switches in `PackageOptions` (`include_episodes` /
  `include_private_knowledge` / `include_procedural` / `include_autobiographical` /
  `include_public_knowledge`) — packaging always excluded the whole `memory/` directory, so the
  toggles never had an effect (ADR-101 Decision 11).
- ❌ The `edges` table and any `edge_*` method on `SqliteStore` — that table is not in the schema;
  don't JOIN against it from the historical doc wording (relations are
  `Knowledge(subtype=Relation)` nodes, see §1).

