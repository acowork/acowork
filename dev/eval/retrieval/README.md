# Retrieval Evaluation Harness (ADR-095 D6 / design 28 §3.3)

Offline comparison of retrieval variants. **Never on a request path** — the
runners are integration tests and the shared kernel is a dev-dependency-only
crate (`core/acowork-eval`), so nothing here is linked into a shipped binary.
`core/acowork-eval/tests/shipping_redline.rs` fails the build if that stops
being true.

## Why this exists

ADR-095 D6 forbids changing fusion weights, cosine floors or the CJK term
strategy without measured evidence. Before this harness existed, the only way to
judge a retrieval change was to look at one query in the UI. The runner produces
the numbers that gate Phase 2 and Phase 4.

## Three harnesses, split along the binary boundary

ADR-100 made the memory engine and the Runtime separate binaries with separate
code paths, and the tests follow that line: each harness seeds and scores **only
the scope its own binary serves**, and a query whose ground truth lies entirely
in the other scope is skipped rather than counted — crediting an engine with rows
it was never given moves its numbers in the flattering direction.

| harness | scope | embeddings | gated |
|---|---|---|---|
| `core/acowork-memory/tests/retrieval_eval.rs` | memory | pre-generated, checked in | `--ignored` |
| `core/acowork-runtime/tests/retrieval_eval.rs` | conversation | pre-generated, checked in | `--ignored` |
| `core/acowork-runtime/tests/global_search_e2e.rs` | conversation, over the real HTTP path | the **live production model**, dimension read from `/health` | Tier 1/3 run by default; Tier 2 `--ignored` |

What the three share — metric math, dataset reading, the stand-in projection,
the report shape — lives in `core/acowork-eval`, which both crates pull in as a
`[dev-dependencies]` entry. Integration tests in sibling crates cannot import
each other, so without that kernel each side re-derives nDCG and "0.62" stops
being one number; with it, the two scope reports are readable side by side.

The offline harnesses must keep using checked-in vectors: two runs of the same
variant have to be comparable, and calling a live model would make every run
differ. The E2E harness exists precisely because that determinism cannot answer
ADR-095 §9, which asks about the real cosine space.

## Layout

```
dev/eval/retrieval/
  dataset.jsonl     # annotated queries (desensitized, hand-labeled) — 12 queries
  corpus.jsonl      # the fixture store: 22 memory rows + 16 conversation rows
  embeddings.json   # pre-generated vectors, keyed by corpus row index
  reports/          # generated (gitignored):
                    #   memory-report.{json,md}                   memory variants
                    #   conversation-report.{json,md}             conversation variants
                    #   conversation-live-model-report.{json,md}  live-model quality + latency
                    #   conversation-live-cosine.json             §9 threshold input
runner (memory)       # -> core/acowork-memory/tests/retrieval_eval.rs
runner (conversation) # -> core/acowork-runtime/tests/retrieval_eval.rs
e2e runner            # -> core/acowork-runtime/tests/global_search_e2e.rs
kernel                # -> core/acowork-eval/src/lib.rs (dev-dependency of both)
```

The corpus and embeddings are separate files so a run is **deterministic**:
the harness must not call a live embedding model, or two runs of the same
variant would not be comparable.

`embeddings.json` is **derived data** — `tokenize` + `project` applied to
`corpus.jsonl`, written by `acowork_eval::stand_in_embeddings_file`. Checking the
vectors in instead of computing them at read time is the seam where a production
model's vectors get dropped in later; the cost is that they can go stale, so
`EvalDataset::load` refuses a file the kernel's tokenizer does not reproduce. A
stale file is not cosmetic: a corpus embedded under one tokenizer and a query
embedded under another silently changes what the semantic weight is being
credited with, in both offline harnesses at once. After touching the tokenizer,
the stand-in dimension, or any corpus text, regenerate:

```bash
cd core
cargo test -p acowork-eval --test regenerate_stand_in_embeddings -- --ignored
```

## Running

```bash
cd core
cargo test -p acowork-memory --test retrieval_eval  -- --ignored --nocapture   # memory scope
cargo test -p acowork-runtime --test retrieval_eval -- --ignored --nocapture  # conversation scope
```

Each run writes `reports/{memory,conversation}-report.{json,md}`. Variants are a
fixed list in the harness (`enum Variant`), not CLI flags — a variant that can be
edited into existence is not a reproducible comparison.

Memory scope (the engine's own read path, which has no analyzer yet):

| variant | what changes |
|---------|--------------|
| `semantic_4to1_current` | today's behaviour: semantic-dominant, no cosine gate |
| `equal_no_gate` | equal weights, the shape the admin panel search uses |
| `semantic_4to1_floor` | the current weights with a cosine floor (`0.15`) |
| `semantic_2to1` | a milder tilt, to show which way the number moves before 4:1 is argued |

Conversation scope (the Runtime's fused global search):

| variant | what changes |
|---------|--------------|
| `baseline` | one query text drives both sources, equal weight |
| `dual_equal` | analyzer terms drive FTS, raw text drives cosine, equal weight |
| `dual_4to1` | the same dual query at `FusionPolicy::SEMANTIC_4TO1` |
| `dual_4to1_floor` | dual query at 4:1 with the conversation cosine floor |

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

All of them come from `core/acowork-eval/src/lib.rs` and are computed from **rank
position and id membership**, never from a `fused_score` — the fused score is a
rank signal, so averaging it would produce a number that moves when the corpus
size moves.

- `Recall@K`, `Precision@K`, `MRR`, `nDCG@K` — standard, over the annotated
  relevant set. Precision divides by `K`, not by the number of results actually
  returned, or a retriever that truncates the list would score better than one
  that fills it correctly. nDCG counts a hit at its true rank, so a relevant row
  found tenth is not credited as if it were first.
- `false_recall_rate` — fraction of `kind: "no_relevant"` queries that returned
  any hit. A variant that "finds more" by returning junk is not better. Reported
  as `n/a` when the scope has no such query, and the explanatory note is then
  suppressed rather than printed over an unmeasured column.
- `exact_term_retention` — **reported separately and never folded into nDCG**
  (D6): of the queries with `exact_terms`, how many still return a hit
  containing each exact term. This is the metric that catches an analyzer or a
  weight change destroying literal matches, which is the specific way hybrid
  retrieval regresses.
- `skipped` — queries whose ground truth lies entirely in the other scope. They
  are not averaged in, and the count is printed because a report that silently
  scored 4 of 12 queries would look like a report that scored 12.

## Dataset coverage matrix (D6 minimum)

`lang` × `kind` must cover: en / zh / mixed × short keyword / full sentence /
exact entity / semantics-only / no relevant result / duplicate-across-scopes.
A query that is not in the matrix cannot justify a production weight change.

## Adding data

`dataset.jsonl` and `corpus.jsonl` are joined by node id / session id, so adding
a query normally means adding corpus rows too. Keep content **desensitized**:
this directory is in a public docs tree. Editing any `content` string changes what
`embeddings.json` has to contain, so regenerate it afterwards (see Layout) — the
load-time check will fail until you do.

## Known limits

22 memory rows and 16 conversation rows over 12 queries is enough to rank
variants against each other and to expose structural defects; it is not enough to
justify a release gate. Each offline harness scores only its own scope, so the
memory report averages 9 queries and the conversation report 4 — read the
`skipped` column before comparing the two numbers. The cosine distributions in
`conversation-live-cosine.json` are truncated at the conversation baseline floor
(0.3), so the low tail is not observable, and both corpora are
synthetic-but-realistic rather than desensitized production transcripts. Before
§9 items 1 and 3 are decided on numbers, the corpus should be rebuilt from real
agent transcripts the way `memory_retrieval_bench.rs` does.
