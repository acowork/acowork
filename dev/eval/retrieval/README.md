# Retrieval Evaluation Harness (ADR-095 D6 / design 28 §3.3)

Offline comparison of retrieval variants. **Never on a request path** — nothing
here is compiled into a shipped binary except as an `#[ignore]` test.

## Why this exists

ADR-095 D6 forbids changing fusion weights, cosine floors or the CJK term
strategy without measured evidence. Before this harness existed, the only way to
judge a retrieval change was to look at one query in the UI. The runner produces
the numbers that gate Phase 2 and Phase 4.

## Two harnesses, because one number cannot be both reproducible and real

| harness | embeddings | answers | gated |
|---|---|---|---|
| `retrieval_eval.rs` | pre-generated, checked in | is the fusion arithmetic sound, and is a variant better than another on a fixed fixture | `--ignored` |
| `global_search_e2e.rs` | the **live production model**, read from `/health` | does the shipped path work end to end, and what do the numbers look like with the real embedding space | `--ignored` (Tier 2/3 run in CI without a model) |

The offline harness must keep using checked-in vectors: two runs of the same
variant have to be comparable, and calling a live model would make every run
differ. The E2E harness exists precisely because that determinism cannot answer
ADR-095 §9, which asks about the real cosine space.

## Layout

```
dev/eval/retrieval/
  dataset.jsonl     # annotated queries (desensitized, hand-labeled)
  corpus.jsonl      # the fixture store: memory nodes + conversation messages
  embeddings.json   # pre-generated vectors, keyed by content hash
  reports/          # generated (gitignored):
                    #   report.{json,md}              offline variants
                    #   e2e_live_model.{json,md}      live-model quality + latency
                    #   e2e_cosine_distribution.json  §9 threshold input
  runner            # -> core/acowork-runtime/tests/retrieval_eval.rs
  e2e runner        # -> core/acowork-runtime/tests/global_search_e2e.rs
```

The corpus and embeddings are separate files so a run is **deterministic**:
the harness must not call a live embedding model, or two runs of the same
variant would not be comparable.

## Running

```bash
cd core
cargo test -p acowork-runtime --test retrieval_eval -- --ignored --nocapture
```

Variants (CLI-selected inside the test; see `Variant`):

| # | variant | what changes |
|---|---------|--------------|
| 1 | `baseline` | single query drives both sources, equal weight (today's global search) |
| 2 | `dual_equal` | analyzer terms drive FTS, raw drives cosine, equal weight |
| 3 | `dual_4to1` | same dual query, `FusionPolicy::SEMANTIC_4TO1` |
| 4 | `floor_sweep` | variant 3 across a cosine-floor ladder (Phase 4) |

## Running the E2E harness

```bash
cd core
# Tier 1 (real HTTP server + real SQLite + real FTS) and Tier 3 (summary trust
# boundary): no external dependency, safe in CI.
cargo test -p acowork-runtime --test global_search_e2e

# Tier 2 (production embedding model): needs the local embedding service up.
cargo test -p acowork-runtime --test global_search_e2e -- --ignored --nocapture
```

Tier 2 **skips rather than fails** when nothing is listening on
`127.0.0.1:18080`. If the runtime binary cannot be relinked because the session
is running inside it, build the test into a target directory outside the repo:

```bash
CARGO_TARGET_DIR=/tmp/acowork-e2e-target cargo test -p acowork-runtime   --test global_search_e2e -- --ignored --nocapture
```

## Metrics

- `Recall@K`, `Precision@K`, `MRR`, `nDCG@K` — standard, over the annotated
  relevant set.
- `false_recall_rate` — fraction of `kind: "no_relevant"` queries that returned
  any hit. A variant that "finds more" by returning junk is not better.
- `exact_term_retention` — **reported separately and never folded into nDCG**
  (D6): of the queries with `exact_terms`, how many still return a hit
  containing each exact term. This is the metric that catches an analyzer or a
  weight change destroying literal matches, which is the specific way hybrid
  retrieval regresses.

## Dataset coverage matrix (D6 minimum)

`lang` × `kind` must cover: en / zh / mixed × short keyword / full sentence /
exact entity / semantics-only / no relevant result / duplicate-across-scopes.
A query that is not in the matrix cannot justify a production weight change.

## Adding data

`dataset.jsonl` and `corpus.jsonl` are joined by node id / session id, so adding
a query normally means adding corpus rows too. Keep content **desensitized**:
this directory is in a public docs tree.

## Known limits of the E2E corpus

37 documents and 11 queries is enough to rank variants against each other and to
expose structural defects; it is not enough to justify a release gate. The cosine
distributions in `e2e_cosine_distribution.json` are truncated at the conversation
baseline floor (0.3), so the low tail is not observable, and both corpora are
synthetic-but-realistic rather than desensitized production transcripts. Before
§9 items 1 and 3 are decided on numbers, the corpus should be rebuilt from real
agent transcripts the way `memory_retrieval_bench.rs` does.
