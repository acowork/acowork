# ADR-062 M4: Retrieval Quality Before/After Benchmark Report

> **Chinese source of truth**: [ADR-062 M4](../zh/ADR-062-memory-quality-benchmark-report.md)
> **Terminology**: see [GLOSSARY.md](./GLOSSARY.md)

**Report date**: 2026-09
**Branch**: `bugfix/memory`
**Basis**: ADR-062 §5.2 (metrics and thresholds) / §5.4 (report archiving) / §6.4 (auto_inject
min_score)
**Harness**: `core/acowork-memory/tests/memory_m4_bench.rs`
**Probes**: `core/acowork-runtime/tests/memory_m4_probe.rs`, `memory_m4_tiediag.rs` (temporary, to
verify the score domain and MRR fluctuation; since removed — see the path note below)

> Path note: the numbers in this report were measured while memory was built into the Runtime, and
> these files then lived under `core/acowork-runtime/tests/`. After ADR-101 split the memory engine
> into its own service, the benchmarks that still exist (m4 / m5 / retrieval_eval / lifecycle /
> chains) moved with the logic to `core/acowork-memory/tests/`; the two temporary probes were
> deleted — they were one-off diagnostics whose conclusions are recorded below. The metric math
> (Precision@K / MRR / nDCG) behind every number here is the single definition now kept in the
> dev-only `core/acowork-eval`, so this report stays column-comparable with the moved harnesses.

---

## 1. Summary

A before/after retrieval-quality benchmark was run for the two changes in the ADR-062 P2 gate:

- **D1 (P0): exclude Dormant nodes from the retrieval path** (`MemoryQualityConfig.exclude_dormant`,
  default true)
- **D2 (P1): auto_inject's `min_score` changed from a hardcoded 0.3 to routing through
  `quality.min_score` (default 0.0)**

**Conclusion: D1's effect is significant and quantifiable (Precision@5 +41%, Dormant junk-in-context
ratio 0.3 → 0); D2's direction is correct but its hit-rate benefit cannot be quantified in this
environment (see §4's score-domain findings). The auto_inject gate passes (§6).**

| Metric | before (D1 off, min_score 0.3) | after (D1 on, min_score 0.0) | §5.2 threshold | Verdict |
|--------|----------------------------------|-----------------------------|-----------------|---------|
| Precision@5 | 0.5667 | **0.8000** | ≥0.5 and ≥20% improvement | ✅ met |
| Recall@5 | 1.0000 | 1.0000 | — (reference) | ✅ |
| MRR (deterministic pipeline) | 1.0000 | 1.0000 | — (reference) | ✅ |
| Dormant junk-in-context ratio | 0.3000 | **0.0000** | =0 | ✅ met |
| auto_inject hit rate | 100% | 100% | ≥60% | ✅ met* |

\* See §4: the identical before/after hit rate in this environment is **reasonable and consistent**
(the score domain is BM25, so `min_score=0.3` never filtered anything); it is **not** evidence that
D2 is ineffective.

---

## 2. Method

### 2.1 Fixed corpus (deterministic)

10 Knowledge nodes written through the real write chain (`MemoryStoreTool →
process_memory_store`) into an in-memory `GrafeoStore`:

- 5 ground-truth relevant nodes (A1–A5, Active, conf=0.9/imp=0.8)
- 3 junk nodes (D1–D3, set to Dormant via `transition_to_dormant` after writing, imp=0.1)
- 2 distractor nodes (N1–N2, Active, partial word overlap)

### 2.2 Fixed query set (5 queries, each with a ground-truth node id)

```
dark mode editor    → A1
Shanghai river home → A2
Acme backend engineer→ A3
Japanese language code→ A4
cats pets at home   → A5
```

### 2.3 Metrics

- **Precision@5 / Recall@5 / MRR**: reusing
  `grafeo::retrieval_metrics::evaluate_retrieval_quality`
- **Dormant junk ratio**: the share of `status == Dormant` in the top-10 results per query
- **auto_inject hit rate**: the share of the 5 queries whose `MemoryQuery::auto_inject` retrieval
  returns a non-empty result

### 2.4 Environment and determinism

- `DeterministicEmbedding` (`procedural_embedding_fallback`, 384 dims, same text → same vector), so
  retrieval semantics are reproducible across runs.
- **MRR is not reproducible under the default `graph_expand=true`** (root cause in §5), so the main
  table uses the deterministic `graph_expand=false` pipeline. P@5 / the Dormant ratio / auto_inject
  have identical values under both pipelines (membership-type metrics are immune to ranking
  randomness).

---

## 3. Before/After Results (identical across 3 runs)

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

- **D1's benefit**: Precision@5 improved **+41%** (0.567→0.800, meeting the ≥20% bar); the Dormant
  junk ratio went **0.3→0** (meeting the =0 bar). Recall is unharmed (stays 1.0), showing the Dormant
  exclusion did not over-tighten recall.
- **D2's benefit (this environment)**: the hit rate is identical before/after (see §4).

---

## 4. Key Finding 1: The score domain empirically (correcting ADR-062 §6.4's assumption)

### 4.1 Probe results

`memory_m4_probe.rs` measurements for a single node across `min_score` variants:

```
raw text search scores:                [(NodeId(0), 0.8630462173553426)]
hybrid scores (no min_score):          [(NodeId(0), 0.8630462173553426)]
hybrid scores (min_score=0.3):         [(NodeId(0), 0.8630462173553426)]
auto_inject min_score=Some(0.3) → 1 result, score 0.6437
auto_inject min_score=None    → 1 result, score 0.6437
```

### 4.2 The chain of facts

1. **Hybrid search degrades to a single source (text-only)**: `hybrid_search_full` internally calls
   `db.hybrid_search`, but with an empty vector index only the text source remains;
   `grafeo-engine fuse_results` **returns the raw BM25 score directly for a single source** (no RRF).
2. **The score domain is BM25 (~0.86), not the RRF assumed in ADR §6.4** (k=60 → max ~0.016).
3. Therefore **`min_score=0.3` filters nothing in the BM25 domain** — the identical 100% auto_inject
   hit rate before/after is reasonable and consistent, and is **not** evidence that D2 is useless.

### 4.3 Root cause: the write path bypasses vector index population

- `GrafeoStore::store_knowledge → store_node → db.create_node_with_props`
- `grafeo-engine create_node_with_props` **only auto-inserts into the text index, not the vector
  index**; `set_node_property` is what inserts into the vector index.
- Result: nodes written through the standard chain persist their `embedding` property but leave the
  **HNSW vector index empty** → the vector source does not participate → hybrid degrades to text-only.
- In production, `rebuild_embeddings` / `migrate_embedding_dimension` must run to populate the vector
  index.

### 4.4 Correction proposed to ADR-062

- ADR-062 §6.4 originally stated: "`min_score: Some(0.3)` sits in the RRF score domain
  (`1/(k+rank)`, k=60 → max ≈0.016) and would filter out almost all results."
- **Correction**: that claim only holds when the vector index participates in fusion (dual-source
  RRF). In an environment with an unpopulated vector index (text-only BM25 domain),
  `min_score=0.3` filters nothing.
- **D2's fix should still be kept**: it eliminates the hazard of "once the production vector index is
  populated, `min_score=0.3` silently filters everything out" (a defensive fix), but this benchmark
  environment cannot quantify its hit-rate benefit.
- **Where it went**: this "fused-score threshold" mechanism was later abolished wholesale by
  [ADR-082](./ADR-082-memory-storage-sqlite-vector-fts.md) in favour of each source applying its own
  gate in its own score domain before the rank fusion (`min_cosine`, absolute cosine domain). The
  §4.1 score-domain fact chain is a direct input to that decision.

---

## 5. Key Finding 2: MRR is not reproducible under the default pipeline (a production defect)

### 5.1 The phenomenon

- Under the initial harness (`graph_expand=true` default), MRR fluctuates across processes:
  0.6667 / 0.7667 / 0.8667 / 0.9 / 1.0.
- The scores of the same node pair (e.g. A1/D1) **swap across processes** (3.5987 ↔ 2.1558) — i.e.
  it is not merely a ranking tie, the **score itself depends on an unordered traversal**.

### 5.2 The decisive experiment

`memory_m4_tiediag.rs`, three configurations × 2 runs:

| Configuration | Same-pair scores across processes | Per-query rank |
|---------------|-----------------------------------|----------------|
| `graph_expand=true` (default) | swap (3.5987↔2.1558) | fluctuating (1/2/3) |
| `graph_expand=false` | stable | all rank=1, consistent across runs |
| `graph_expand=false, pagerank=0` | stable | all rank=1, consistent across runs |

### 5.3 Root cause

- `manager.rs` applies a **PageRank boost** (`apply_pagerank_boost`) to the retrieval results when
  `enable_graph_expand=true`.
- `compute_pagerank` (small graphs go through `CALL grafeo.pagerank`, falling back to
  `compute_pagerank_fallback` on failure) internally uses **HashMap/HashSet (RandomState random
  seed)** to build adjacency and scores, making the PageRank score non-deterministic across
  processes → near-tie nodes get random ordering.
- **This is a real non-determinism defect in production retrieval** (the same query returns a
  different ordering in a different process), unrelated to D1/D2.

### 5.4 Impact and recommendation

- For the M4 gate: membership-type metrics (P@5 / Dormant / auto_inject) are unaffected; the report
  measures MRR with the deterministic pipeline.
- **Follow-up**: make `compute_pagerank_fallback`'s HashMap/HashSet iteration deterministic (sort by
  NodeId), or add a deterministic secondary key (`node_id`) for ties in `manager.rs`'s final
  ordering. Schedulable as an ADR-062 follow-up or a standalone P-level item.

---

## 6. Gate Conclusion (the M5 Recommendation)

### 6.1 `auto_inject`: **open it (Option A)**

- All measurable §5.2 metrics pass: Precision@5 (+41% ≥20% and ≥0.5), Dormant junk =0, auto_inject
  hit rate ≥60% (100%).
- D1 (the Dormant exclusion) was the core reason `auto_inject` was originally closed ("junk in
  context"); that hole is now closed.
- D2 (min_score 0.3→0.0) landed with M2 as a defensive fix, removing the risk of silently killing
  all results once the vector index is enabled.

**Trigger strategy (Option A: first-turn, consistent with
[ADR-060 §6.3](../zh/ADR-060-prompt-cache-friendly-context-block-reorg.md#63-auto_inject_enabled-触发策略未来开启时))**:

- **At most once per session** — `retrieve_and_inject_memories()` runs on the session's first user
  message; on success `AgentLoop.memory_retrieved_for_session = true` is set, and later turns return
  early via that flag at `loop_memory.rs:93`.
- **Not per-turn (Option B)**: injecting into Block A every turn would invalidate both the Open
  128-token hash chain and the Anthropic prefix cache (rejected in ADR-060 §3.2); per-turn triggering
  would also require moving the injection out of Block A into the append / Block C-style path
  (ADR-060 §11 #2, not done) — an independent work item.
- **Not first-turn-plus-significant-change (Option C)**: the scenario is low probability — auto_inject
  is "warm-up context" at session start, and memories added mid-session are usually already covered
  by a later `memory_recall` tool call or the next session, so the incremental benefit is limited; the
  event channel (consolidation_bg → AgentLoop) is not implemented anyway (ADR-060 §11 #4).
- **Per-agent difference**: the trigger mode (`FirstTurn` / `FirstTurnPlusChange`) will eventually
  become a manifest config item (M5 default `FirstTurn`); different agent types can override it.

**M5 default change**: `MemoryManagerConfig.auto_inject_enabled: false → true` (`manager.rs:172`).
Zero config = first-turn trigger; an agent explicitly sets `auto_inject_enabled: false` in the
manifest `[memory.quality]` to disable it.

> ⚠️ **Subsequent rollback (2026-09)**: this default was reverted to `false` (per-agent opt-in, enabled
> explicitly via the manifest `[memory.quality].auto_inject_enabled = true`, triggering once on the
> first turn). Reason: it duplicated recall with the LLM's autonomous `memory_recall` — both paths use
> a user message as the query (auto_inject: all 4 labels / limit=5 / expand_hops=0 / hint=Identity;
> deep_recall: all 4 labels / limit=10 / expand_hops=2 / hint=Semantic), so core nodes necessarily
> overlap. Accompanying measure: the `memory_recall` tool description now carries an
> anti-duplicate-recall hint ("do NOT re-run the same query"), leaving the dedup judgement to the LLM.

> 📝 **Follow-up revision (2026-10)**: the `expand_hops` values quoted above have since been removed
> entirely as part of the dual-source query split — the field was never consumed by
> `MemoryManager::retrieve()` (dead code), and the backend migrated from the Grafeo graph store to
> SQLite (ADR-082). `memory_recall` now drives the vector source from `MemoryQuery.embedding_text`
> (the current turn's user message) and the BM25 source from `query_text` (LLM keywords); the
> "graph neighbors" phrasing was removed from the tool description. See
> `docs/design/en/05-memory.md` §Retrieval capabilities.

### 6.2 `keyword_index`: **open it (Option Y: write-time injection into `object`)**

**Fact baseline**:

- Keywords are currently persisted only as a `metadata["keywords"]` JSON array
  (`instant.rs:229-245`) and participate in no BM25 index.
- The BM25 index field whitelist is fixed (`grafeo.rs:200-221`): Knowledge's "content" (derived from
  subject/predicate/object), Procedural/Autobiographical "content", and the specific fields listed in
  `KNOWLEDGE_TEXT_FIELDS`.
- `MemoryQualityConfig.keyword_index: bool` is already defined (`quality.rs:163`), default `false`,
  mirrored in the manifest (`manifest.rs:389`), with zero implementation.

**Option Y decision**:

- **Write-time injection**: when `quality.keyword_index=true` and `input.keywords` is non-empty,
  append the keywords to the BM25 indexing surface of the Knowledge `object` field. `object` is
  already a derived field (the content index), so appending `format!(" Keywords: {kw1} {kw2} ...")`
  neither pollutes the user-visible content (the object derivation does not affect the
  subject/predicate/object display) nor needs any change to the retrieval path — once the text index
  hits the keywords, the existing `hybrid_search` path naturally brings the node back; no new hybrid
  source and no grafeo-engine version upgrade.
- **Zero retrieval-path change**; **reversibility**: gated on `quality.keyword_index`; when false the
  metadata-only status quo is preserved (identical to the M4 baseline), so switching is free.
- **Option Z (a separate property per keyword) rejected**: it would extend `KNOWLEDGE_TEXT_FIELDS` and
  leak the field-whitelist scan into every read path; the zero benefit does not outweigh the zero cost.
- **Option W (a keyword source in grafeo-engine) rejected**: it depends on external crate version
  syncing and crosses a boundary for an asymmetric benefit.

**M5 acceptance conditions**:

- Unit test: writing `keywords=["shanghai"]` → the BM25 `object` index can hit "shanghai"; with
  `keyword_index=false` the `object` does not contain "shanghai".
- Benchmark re-run: add a keyword-specific query set (queries that hit **only** via keywords) and
  compare P@5 / Recall@5 with the switch on vs off.
- The manifest `[memory.quality].keyword_index = true/false` takes effect in both directions.

> 📝 **2026-09 follow-up acceptance note**: the "hit only via keywords" acceptance condition implicitly
> assumes "BM25 uniqueness" — under the `hybrid` retrieval path, the `vector` branch reports
> cosine ≠ 0 for any query embedding (`DeterministicEmbedding` is a deterministic hash), so "relying
> on text alone to reject" cannot strictly hold. When `memory_m5_bench` uses `hit_rate` as a saturated
> metric, both before and after are 1.0; the metric that actually distinguishes the `keyword_index`
> effect is MRR (0.7917 → 0.9375). The assertion has been changed to `after.mrr > before.mrr`, keeping
> the p5/r5/mrr regression guards. The "unit test / `keyword_index=false`" part of the planned
> acceptance remains valid (it determines whether BM25 can reject a hit), but "the benchmark must show
> a hit@5 on/off difference" becomes MRR as the substantive measure — pending a "vector signal
> removal" isolation switch in the M5 harness with a real corpus.

### 6.2.1 The keyword quality gate (a mandatory M5 prerequisite)

**Problem**: the only source of keywords today is the LLM (the `memory_store` tool call arguments,
`memory_store.rs:271-276`), with zero cleaning, length limiting, dedup, or stopword filtering.
[05-memory.md §3.3](../../design/zh/05-memory.md) originally designed the Runtime to extract keywords
from `memory_hint.e` as the **deterministic primary source**, with the LLM optionally supplementing
("the LLM need not even fill it in"), but the v3.10 simplification (`05-memory.md:144,1151`) removed
the Runtime chain **without updating the design document** — this is an amplification risk for §6.2's
direct folding into `object` (garbage keywords → amplified into BM25 → retrieval pollution), the same
class of LLM anchoring problem ADR-062 §1 already warned about for `confidence`/`importance`.

**Decision (a lightweight write-time gate, Option A)**: add deterministic cleaning to the write
path, taking effect **whether or not `keyword_index` is on** (so `metadata["keywords"]` itself stays
clean and does not split).

- **Location**: the pure function `acowork_memory::keyword::sanitize(input: Vec<String>) ->
  Vec<String>`, called from both `memory_store.rs` (the LLM boundary) and `instant.rs` (a defensive
  fallback, idempotent).
- **Rules** (in order):
  1. `trim()` + length filter: `0 < len ≤ 30` (prevents whole-sentence pollution)
  2. lowercasing (matching the BM25 tokenizer's token case)
  3. character filter: must contain at least one ASCII alpha or CJK character (drops pure digits /
     pure punctuation)
  4. dedup (case-insensitive, on the lowercased string)
  5. stopword filter: ~30 built-in meaningless tokens (`the, a, user, fact, memory, note, info,
     data, thing, item, stuff, kind, type, way, something, anything, everything, one, two, three,
     yes, no, ok, okay, um, uh, hmm, oh, ah, wow`, empty strings from Chinese punctuation, etc.)
  6. count cap: ≤ 8 keywords per node (truncate, keeping the first 8)
- **Observability**: instrument the number of cleaned keywords and the per-rule trigger distribution
  (`memory_write_keyword_gate` structured event) for the later M3.6 threshold calibration.
- **Tool description updated in sync**: `memory_store` gains "Provide short lowercase tokens (≤30
  chars), avoid duplicates and common stopwords" — lowering the chance the LLM anchors on a bad
  default.

**Explicitly not done** (to avoid scope creep): frequency / globally common token blacklists (they
depend on cross-node statistics and need a separate P3); stemming / synonym merging (they need an NLP
library, out of M5's scope).

**Extended M5 acceptance conditions**: unit tests covering every rule's boundary (empty string,
overlong, pure digits, stopwords, duplicates, more than 8); a BM25 hit test proving a cleaned
keyword is findable in the `object` index; a regression test showing the existing e2e case
`test_memory_store_metadata_params_inmemory` still passes after cleaning; and **no new keyword switch
in the manifest `[memory.quality]`** (the cleaning is an always-on write constraint, not a
configurable parameter).

### 6.5 M5 Overall Steps

| Step | Changed files | Verification |
|------|---------------|--------------|
| 1 | `auto_inject_enabled` default → true | `manager.rs` unit test (default assertion) + the benchmark harness's hit rate |
| 2a | **the keyword write-time quality gate** (§6.2.1 prerequisite) | new `acowork-memory/src/keyword.rs` + `memory_store.rs` + `instant.rs` called in both directions; unit tests covering all 6 rule boundaries + tool description update + existing e2e regression |
| 2b | keyword_index write-time injection into `object` (depends on 2a) | `instant.rs`; unit test (object clean when off / contains `Keywords: …` when on) + benchmark |
| 3 | the manifest `[memory.quality].keyword_index` / `auto_inject_enabled` deserialization is already in place (no change) | the harness sets the values explicitly to verify the override |
| 4 | extend the benchmark: a keyword-specific query set + an auto_inject state sweep | `memory_m4_bench.rs`; renamed `memory_m5_bench` or with a keyword config switch appended |
| 5 | run the before/after comparison | output a table (Precision@5 / Recall@5 / MRR / Dormant / auto_inject hit / keyword hit) |

**Rollback path**: all changes are parameterized (`quality.auto_inject_enabled` /
`quality.keyword_index`); the rollback cost is changing a config value. There are no destructive logic
changes.

### 6.6 Design Principles (retained and added)

- **Retained**: the centralized `MemoryQualityConfig` parameters (ADR-062 §4.1), the Dormant exclusion
  (M1), and the min_score fix (M2 D2).
- **Added**: configurability of the trigger strategy (per-agent override), but M5 introduces no new
  dimension — only the two boolean gates `auto_inject_enabled` / `keyword_index` are kept.
- **Explicitly not done**: Option B (per-turn trigger + Block C append) and Option C
  (first-turn + change re-trigger) — see §6.1's rationale.

### 6.7 Consolidated Follow-up Backlog

The remaining items after M5, by priority and dependency:

| Priority | Item | Source | Effort |
|----------|------|--------|--------|
| **P1** | Fix the `compute_pagerank` non-determinism (`compute_pagerank_fallback` HashMap/HashSet → sort by NodeId) | M4 §5.4 | small (local change + regression test) |
| **P1** | Verify that the production vector index population path (`rebuild_embeddings` / startup migration) runs automatically after the write chain | M4 §4.3 | medium (production evidence) |
| **P2** | Delete the dead `hint_weights` code (`_text_weight`/`_vector_weight`/`_graph_weight` are dead at `manager.rs:318`) | ADR-062 §4.2 / M4 report §6.3 | small |
| **P2** | Parameterize `RRF k` (requires grafeo-engine support) | ADR-062 §4.2 | medium (depends on an upstream version) |
| **P2** | Decide whether to unify the generalization-path 0.8 and the offline 0.7 thresholds (an extension of the M3.6 calibration) | ADR-062 §4.2 | medium |
| **P3** | Move the auto_inject injection from Block A to Block C append (unlocks Option B) | ADR-060 §11 #2 | large (needs a Provider cache_control refactor) |
| **P3** | The `consolidation_bg → AgentLoop` notification channel (unlocks Option C) | ADR-060 §11 #4 | medium (event channel + notification mechanism) |
| **P3** | M3.6 threshold calibration (confidence/importance distribution variance + threshold reasonableness) | ADR-062 §6.6 | medium (needs instrumentation data first) |
| **P3** | The root fix for the score-domain finding: make `create_node_with_props` write to the vector index | M4 §4.3 | small (one place in the grafeo-engine wrapper) |

**Principle**: every item has a clear entry point (the source column) and an effort estimate, to avoid
a "backlog black hole".

---

## 7. Reproduction

```bash
cd core
cargo test -p acowork-memory --test memory_m4_bench   -- --nocapture   # the main benchmark
cargo test -p acowork-runtime --test memory_m4_probe    -- --nocapture   # the score-domain probe (file removed, see the path note above)
```

Self-contained, using an in-memory `GrafeoStore`; it does not touch any running Gateway / Runtime /
Desktop process or port.

---

## 8. M5 Execution Results (2026-09)

**Harness**: `core/acowork-memory/tests/memory_m5_bench.rs` (restored from the `15654af0` safe
snapshot; the `set_quality` helper was replaced by the trait method `apply_quality_config` because
`b117f901` reverted it).

### 8.1 The code change list (mapping to §6.5's steps)

| Step | Change | Verification |
|------|--------|--------------|
| 1 | `MemoryManagerConfig.auto_inject_enabled` default `false → true` (`manager.rs`); the first-turn trigger logic (`loop_memory.rs` `memory_retrieved_for_session`) already existed | the default assertion updated |
| 2a | new `acowork-memory/src/keyword.rs` (`sanitize` / `sanitize_with_stats`, 6 rules + 30 stopwords + cap 8); `memory_store.rs`'s LLM boundary and `instant.rs`'s persistence boundary both call it (idempotent); tool description updated; `memory_write_keyword_gate` instrumentation | 10 rule-boundary unit tests + e2e regression |
| 2b | `instant.rs`: with `quality.keyword_index=true` the cleaned keywords are folded into the BM25 `object` field (Option Y) | unit test (clean when off / contains `Keywords: …` when on) + bench |
| 3 | manifest `[memory.quality].auto_inject_enabled` added (both the draft and §6.5's claim that it was "already in place" were untrue) + `agent_core::init_memory_manager` injection; `keyword_index` was already mirrored | core manifest tests |
| 4/5 | `memory_m5_bench` before/after | see §8.2 |

### 8.2 Before/After Results (`enable_graph_expand=false`, the deterministic pipeline)

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

- **Option Y works**: with `keyword_index=false` BM25 cannot hit K* at all (content lexical
  isolation, keyword hit@5 = 0); with `true` the write-time injection into `object` makes BM25 hit
  everything (1.0).
- **No regression**: the after P@5 / R@5 ≥ the before values (the M4 baseline is not damaged).
- **The quality gate as a prerequisite**: all of K_CORPUS's keywords pass `sanitize`
  (`santorini/vacation/summer-2024`, `graphql/schema-stitching/federation`,
  `beekeeping/apiary/honey-extraction` are all clean tokens); the folded content is unpolluted.

### 8.3 Conclusions

- **`auto_inject`**: default off (per-agent opt-in), enabled explicitly via the manifest
  `[memory.quality].auto_inject_enabled=true` (triggering once on the first turn, ADR-060 §6.3). M5
  briefly defaulted it on, then reverted to opt-in because it duplicated recall with the LLM's
  autonomous `memory_recall` (see §6.1's note).
- **`keyword_index`**: default `false` (per-agent via `[memory.quality].keyword_index=true`) — the
  capability + quality gate landed, the M4 baseline is preserved, and the benchmark proves that
  enabling it takes keyword-specific retrieval from 0 hits to full hits.
- The quality gate (`sanitize`) is **always-on** and decoupled from `keyword_index`, guaranteeing
  that `metadata["keywords"]` itself is clean.

### 8.4 Reproduction

```bash
cd core
cargo test -p acowork-memory --test memory_m5_bench -- --nocapture   # M5 keyword before/after
cargo test -p acowork-memory --lib keyword                             # quality gate rule unit tests
```

