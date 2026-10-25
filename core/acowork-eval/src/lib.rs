//! Offline retrieval-quality measurement kernel (ADR-095 D6 / design 28 §3.3).
//!
//! Pure math over ranked id lists: no store, no embedding, no async, no config.
//!
//! ## Why it is its own crate
//!
//! The two things that need it are measured from two crates that ADR-101 made
//! deliberately binary-isolated — the memory engine is benched from
//! `acowork-memory`, the conversation engine from `acowork-runtime` — and
//! integration tests cannot import each other either. Duplicating the metric
//! definitions, as both sides used to do, means the two gates are no longer
//! comparable: "nDCG 0.62" has to be the same number on both sides or a weight
//! change is being judged twice by two different yardsticks.
//!
//! Both already depend on `acowork-core`, so that would have been the cheapest
//! host — but this is measurement code, and putting it there would link a report
//! renderer and a stand-in tokenizer into every shipped binary and invite product
//! code to call them. So it is a leaf crate that the two harnesses pull in as a
//! **dev-dependency only**; `tests/shipping_redline.rs` fails the build if it ever
//! becomes a real dependency.
//!
//! ## Score-domain discipline
//!
//! Every metric here is computed from **rank position and id membership** —
//! never from a `fused_score`. See `acowork_core::retrieval` for why the fused score
//! is a rank signal and not a relevance measurement; averaging it across queries
//! would produce a number that moves when the corpus size moves. Cosine is only
//! ever used as a threshold by the engines under test, never folded in here.
//!
//! This is measurement code. It must never be called from a request path.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One annotated query: the text plus its ground-truth ids.
#[derive(Debug, Clone)]
pub struct EvalQuery {
    /// The query text (kept for reporting; the math only uses `relevant_ids`).
    pub query: String,
    /// Ground-truth ids.
    pub relevant_ids: Vec<u64>,
}

/// Precision@k = |relevant ∩ retrieved[:k]| / **k**.
///
/// The denominator is `k`, never the number of results actually returned. That
/// is the standard definition and it matters here: dividing by
/// `min(k, retrieved.len())` lets a retriever that returns one correct item
/// score a perfect 1.0 while one that fills the slot and is equally right on
/// rank 1 scores 0.2 — the metric would then reward truncating the result list,
/// the opposite of what a retrieval quality gate should do.
///
/// Generic over the item type on purpose: row ids and stable corpus tags are two
/// spellings of the same thing, and a harness forced to pick one would redefine
/// the metric locally — which is how two numbers both called "Precision@5" once
/// stopped meaning the same thing across the archived benchmarks.
pub fn precision_at_k<T: PartialEq>(retrieved: &[T], relevant: &[T], k: usize) -> f64 {
    if k == 0 || retrieved.is_empty() {
        return 0.0;
    }
    let hits = relevant_hits(retrieved, relevant, k).count();
    hits as f64 / k as f64
}

/// Recall@k = |relevant ∩ retrieved[:k]| / |relevant|.
///
/// An empty ground-truth set returns `1.0` by definition (nothing was expected,
/// nothing was missed). Callers that care about junk answers must not read this
/// as a success — that is what [`QualityTally`]'s `false_recall_rate` is for.
pub fn recall_at_k<T: PartialEq>(retrieved: &[T], relevant: &[T], k: usize) -> f64 {
    if relevant.is_empty() {
        return 1.0;
    }
    let hits = relevant_hits(retrieved, relevant, k).count();
    hits as f64 / relevant.len() as f64
}

/// Reciprocal rank of the first relevant hit in one result list (`0.0` if none).
pub fn reciprocal_rank<T: PartialEq>(retrieved: &[T], relevant: &[T]) -> f64 {
    retrieved
        .iter()
        .position(|item| relevant.contains(item))
        .map(|pos| 1.0 / (pos + 1) as f64)
        .unwrap_or(0.0)
}

/// Mean Reciprocal Rank over per-query `(retrieved, relevant)` pairs.
pub fn mean_reciprocal_rank<T: PartialEq>(pairs: &[(&[T], &[T])]) -> f64 {
    if pairs.is_empty() {
        return 0.0;
    }
    pairs
        .iter()
        .map(|(retrieved, relevant)| reciprocal_rank(retrieved, relevant))
        .sum::<f64>()
        / pairs.len() as f64
}

/// Normalised DCG@k (binary relevance) = `DCG@k / IDCG@k`, with
/// `DCG@k = Σ rel_i / log2(i + 1)` over the top k positions and `IDCG@k` the
/// same sum for the ideal ordering, i.e. all `min(|relevant|, k)` relevant items
/// first.
///
/// This is the metric to gate a small ground-truth corpus on. Precision@k has a
/// hard ceiling of `min(|relevant|, k) / k`, so on a corpus where each question
/// has one or two correct answers an absolute Precision@5 gate is unreachable by
/// construction, not by poor retrieval. nDCG divides by that same ideal, so 1.0
/// means "the ranking is as good as it could have been".
pub fn ndcg_at_k<T: PartialEq>(retrieved: &[T], relevant: &[T], k: usize) -> f64 {
    if k == 0 || relevant.is_empty() {
        return 0.0;
    }
    // Discount for a hit at 1-based rank `r`: 1 / log2(r + 1).
    let gain = |rank_one_based: usize| 1.0 / (rank_one_based as f64 + 1.0).log2();

    let dcg: f64 = relevant_hits(retrieved, relevant, k)
        .map(|(rank, _)| gain(rank))
        .sum();
    let idcg: f64 = (1..=relevant.len().min(k)).map(gain).sum();
    if idcg == 0.0 {
        return 0.0;
    }
    dcg / idcg
}

/// The ranked positions inside the top `k` whose item is relevant, as
/// `(1-based rank, item)`.
///
/// One definition of "hit" for all four metrics: the cutoff is applied to the
/// retrieved list, never to the ground-truth set, so `min(k, |relevant|)` appears
/// only in the ideal denominator.
///
/// The rank travels out of this helper rather than being recovered by the
/// caller's `enumerate`. Counting survivors after the filter would number them
/// 1..n and make a hit ranked tenth look first, which quietly turns nDCG into a
/// recall ratio — the one metric whose whole job is to notice late ranking.
fn relevant_hits<'a, T: PartialEq>(
    retrieved: &'a [T],
    relevant: &'a [T],
    k: usize,
) -> impl Iterator<Item = (usize, &'a T)> {
    retrieved[..k.min(retrieved.len())]
        .iter()
        .enumerate()
        .filter(move |(_, item)| relevant.contains(*item))
        .map(|(i, item)| (i + 1, item))
}

/// Whether the result set still carries every exact term (case-insensitive).
///
/// This is the D6 "exact-term retention" measure: it asks whether the literal
/// survived, independently of whether the hit ranked first — the specific way a
/// hybrid change regresses (an analyzer or a weight swap destroying literal
/// matches while the ranking metrics stay flat).
pub fn exact_terms_present(texts: &[String], terms: &[String]) -> bool {
    !terms.is_empty()
        && terms.iter().all(|t| {
            let needle = t.to_lowercase();
            texts.iter().any(|h| h.to_lowercase().contains(&needle))
        })
}

/// Per-query result row of [`QualityTally::record`], for the report's
/// `per_query` section.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct QueryScore {
    /// False for a `no_relevant` query: it is counted separately and never
    /// folded into the averages.
    pub scored: bool,
    pub retrieved: usize,
    pub relevant: usize,
    pub recall_at_k: f64,
    pub precision_at_k: f64,
    pub mrr: f64,
    pub ndcg_at_k: f64,
}

/// One variant's aggregate over a whole dataset.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VariantMetrics {
    /// Queries with a non-empty ground-truth set — the denominator of every
    /// average below.
    pub queries: usize,
    pub recall_at_k: f64,
    pub precision_at_k: f64,
    pub mrr: f64,
    pub ndcg_at_k: f64,
    /// Fraction of `no_relevant` queries that returned anything anyway. `None`
    /// when the dataset has no such query. A variant that "finds more" by
    /// returning junk is not better.
    pub false_recall_rate: Option<f64>,
    /// Fraction of queries with `exact_terms` whose result set still carries
    /// them all. `None` when the dataset has none. Reported separately and never
    /// folded into nDCG (D6).
    pub exact_term_retention: Option<f64>,
}

/// Streams a dataset through the metrics, one query at a time.
///
/// Streaming (rather than "collect every ranking, then average") is what lets a
/// harness print a per-query row and a per-variant aggregate from the same pass,
/// so the two can never disagree about which queries were counted.
#[derive(Debug, Default)]
pub struct QualityTally {
    k: usize,
    scored: usize,
    recall: f64,
    precision: f64,
    mrr: f64,
    ndcg: f64,
    no_relevant_total: usize,
    no_relevant_false: usize,
    exact_total: usize,
    exact_kept: usize,
}

impl QualityTally {
    /// Start a tally for the cutoff `k`.
    pub fn new(k: usize) -> Self {
        Self {
            k,
            ..Default::default()
        }
    }

    /// Score one query. An empty `relevant` set is routed to the false-recall
    /// counter instead of the averages, so it can never inflate them.
    pub fn record<T: PartialEq>(&mut self, retrieved: &[T], relevant: &[T]) -> QueryScore {
        let row = QueryScore {
            scored: !relevant.is_empty(),
            retrieved: retrieved.len(),
            relevant: relevant.len(),
            recall_at_k: recall_at_k(retrieved, relevant, self.k),
            precision_at_k: precision_at_k(retrieved, relevant, self.k),
            mrr: reciprocal_rank(retrieved, relevant),
            ndcg_at_k: ndcg_at_k(retrieved, relevant, self.k),
        };
        if relevant.is_empty() {
            self.no_relevant_total += 1;
            if !retrieved.is_empty() {
                self.no_relevant_false += 1;
            }
        } else {
            self.scored += 1;
            self.recall += row.recall_at_k;
            self.precision += row.precision_at_k;
            self.mrr += row.mrr;
            self.ndcg += row.ndcg_at_k;
        }
        row
    }

    /// Note the exact-term check for one query. Pass an empty `terms` for a
    /// query with no `exact_terms` annotation — it is then not counted at all.
    pub fn record_exact_check(&mut self, texts: &[String], terms: &[String]) -> bool {
        if terms.is_empty() {
            return false;
        }
        self.exact_total += 1;
        let kept = exact_terms_present(texts, terms);
        if kept {
            self.exact_kept += 1;
        }
        kept
    }

    /// Score one dataset query end to end: the ranking against its ground
    /// truth, the exact-term check, and the report row.
    ///
    /// Bundled into a single call on purpose. Split across two crates, the
    /// per-query row and the variant aggregate drift apart in exactly the way
    /// nobody notices — one side starts counting a query the other excludes.
    ///
    /// `analyzer_terms` is `Some` only for an engine that runs the ADR-095 query
    /// analyzer; pass `None` where no analyzer exists rather than a fake `0`, so
    /// "not applicable" and "produced no terms" stay distinguishable.
    pub fn record_query<T: PartialEq>(
        &mut self,
        query: &DatasetQuery,
        retrieved: &[T],
        relevant: &[T],
        retrieved_texts: &[String],
        analyzer_terms: Option<usize>,
    ) -> PerQueryReport {
        let row = self.record(retrieved, relevant);
        let kept = self.record_exact_check(retrieved_texts, &query.exact_terms);
        PerQueryReport {
            id: query.id.clone(),
            lang: query.lang.clone(),
            kind: query.kind.clone(),
            retrieved: row.retrieved,
            relevant: relevant.len(),
            recall_at_k: row.recall_at_k,
            mrr: row.mrr,
            exact_terms_kept: (!query.exact_terms.is_empty()).then_some(kept),
            analyzer_terms,
        }
    }

    /// Average over the scored queries.
    pub fn finish(&self) -> VariantMetrics {
        let n = self.scored.max(1) as f64;
        VariantMetrics {
            queries: self.scored,
            recall_at_k: self.recall / n,
            precision_at_k: self.precision / n,
            mrr: self.mrr / n,
            ndcg_at_k: self.ndcg / n,
            false_recall_rate: (self.no_relevant_total > 0)
                .then(|| self.no_relevant_false as f64 / self.no_relevant_total as f64),
            exact_term_retention: (self.exact_total > 0)
                .then(|| self.exact_kept as f64 / self.exact_total as f64),
        }
    }
}

/// Aggregate over a fixed query set at several cutoffs — the shape the
/// before/after benchmarks archive as gate evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkMetrics {
    /// Precision at each requested cutoff.
    pub precision_at_k: Vec<(usize, f64)>,
    /// Recall at each requested cutoff.
    pub recall_at_k: Vec<(usize, f64)>,
    /// nDCG at each requested cutoff.
    pub ndcg_at_k: Vec<(usize, f64)>,
    /// Mean reciprocal rank over all queries.
    pub mrr: f64,
    /// Queries evaluated.
    pub num_queries: usize,
}

/// Evaluate `queries` against `retrieval_results` (one ranked id list per query,
/// same order) at each cutoff in `k_values`.
pub fn evaluate_retrieval_quality(
    queries: &[EvalQuery],
    retrieval_results: &[Vec<u64>],
    k_values: &[usize],
) -> BenchmarkMetrics {
    let mut precisions = Vec::with_capacity(k_values.len());
    let mut recalls = Vec::with_capacity(k_values.len());
    let mut ndcgs = Vec::with_capacity(k_values.len());

    let n = queries.len().max(1) as f64;
    for &k in k_values {
        let (mut p, mut r, mut d) = (0.0, 0.0, 0.0);
        for (query, retrieved) in queries.iter().zip(retrieval_results.iter()) {
            p += precision_at_k(retrieved, &query.relevant_ids, k);
            r += recall_at_k(retrieved, &query.relevant_ids, k);
            d += ndcg_at_k(retrieved, &query.relevant_ids, k);
        }
        precisions.push((k, p / n));
        recalls.push((k, r / n));
        ndcgs.push((k, d / n));
    }

    let pairs: Vec<(&[u64], &[u64])> = queries
        .iter()
        .zip(retrieval_results.iter())
        .map(|(q, r)| (r.as_slice(), q.relevant_ids.as_slice()))
        .collect();

    BenchmarkMetrics {
        precision_at_k: precisions,
        recall_at_k: recalls,
        ndcg_at_k: ndcgs,
        mrr: mean_reciprocal_rank(&pairs),
        num_queries: queries.len(),
    }
}

// ── dataset ───────────────────────────────────────────────────────────────
//
// The annotated corpus under `dev/eval/retrieval/` is read by two harnesses in
// two crates. They must annotate-resolve and tokenize identically or the two
// reports stop being comparable, so the reading side lives here too.

/// One indexed row of `corpus.jsonl`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CorpusRow {
    /// `"memory"` or `"conversation"` — which engine owns the row.
    #[serde(rename = "type")]
    pub kind: String,
    /// Memory label (`knowledge` / `procedural` / …); empty for conversation rows.
    #[serde(default)]
    pub label: String,
    pub content: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub message_index: usize,
    #[serde(default)]
    pub role: String,
}

impl CorpusRow {
    /// The scope name this row belongs to, matching [`Annotation::scope`].
    pub fn scope(&self) -> &str {
        &self.kind
    }
}

/// One ground-truth pointer inside a query annotation.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Annotation {
    /// `"memory"` or `"conversation"`.
    pub scope: String,
    /// Memory rows are named by their exact content.
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub message_index: Option<usize>,
}

/// One annotated query of `dataset.jsonl`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct DatasetQuery {
    pub id: String,
    pub lang: String,
    /// Query shape (`full_sentence` / `short_keyword` / `exact_entity` / …).
    pub kind: String,
    pub query: String,
    pub relevant: Vec<Annotation>,
    /// Literals that must survive in the result set (ADR-095 D6 exact-term
    /// retention).
    #[serde(default)]
    pub exact_terms: Vec<String>,
}

impl DatasetQuery {
    /// The annotations naming a row of one scope. A harness scores only the
    /// queries its own engine can answer; the rest are skipped rather than
    /// counted as `no_relevant`, which would otherwise credit an engine for
    /// rows it was never given.
    pub fn annotations_for(&self, scope: &str) -> Vec<&Annotation> {
        self.relevant.iter().filter(|a| a.scope == scope).collect()
    }

    /// True when no annotation of this query belongs to `scope`.
    pub fn out_of_scope(&self, scope: &str) -> bool {
        !self.relevant.iter().any(|a| a.scope == scope)
    }
}

/// The whole evaluation dataset, plus the stand-in vectors.
#[derive(Debug, Clone)]
pub struct EvalDataset {
    pub corpus: Vec<CorpusRow>,
    pub queries: Vec<DatasetQuery>,
    /// Embedding dimension declared by `embeddings.json`.
    pub dim: usize,
    /// corpus row index -> vector
    pub vectors: std::collections::HashMap<usize, Vec<f32>>,
}

impl EvalDataset {
    /// The repo-level dataset directory, resolved from this crate's manifest so
    /// every harness finds the same files.
    pub fn dir() -> PathBuf {
        // `core/acowork-eval` -> repo root is two levels up.
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dev/eval/retrieval")
    }

    /// Load `corpus.jsonl` + `dataset.jsonl` + `embeddings.json`.
    ///
    /// A missing file panics instead of returning an empty dataset: an eval
    /// that silently reports "ok" while measuring nothing is worse than no eval
    /// at all, because it makes the D6 gate look satisfied.
    pub fn load(dir: &Path) -> Self {
        let corpus = Self::corpus_rows(dir);
        let queries = read_jsonl(&dir.join("dataset.jsonl"));
        let raw: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("embeddings.json"))
                .unwrap_or_else(|e| panic!("read embeddings.json: {e}")),
        )
        .expect("embeddings.json is valid json");
        let dim = raw["dim"].as_u64().expect("embeddings.json declares dim") as usize;
        let vectors = raw["vectors"]
            .as_object()
            .expect("embeddings.json has a vectors object")
            .iter()
            .map(|(k, v)| {
                (
                    k.parse::<usize>().expect("vector key is a row index"),
                    v.as_array()
                        .expect("vector is an array")
                        .iter()
                        .map(|x| x.as_f64().expect("vector component is a number") as f32)
                        .collect(),
                )
            })
            .collect();
        let ds = Self {
            corpus,
            queries,
            dim,
            vectors,
        };
        ds.assert_stand_in_is_current();
        ds
    }

    /// Fail if `embeddings.json` was not produced by the kernel's own
    /// tokenization.
    ///
    /// The file is the seam where a production-model vector set gets dropped in,
    /// so the vectors are data and the tokenizer is code — and those two can
    /// drift apart without anything noticing. That drift is not cosmetic: a
    /// corpus embedded under one tokenizer and a query embedded under another
    /// changes what the semantic weight is being credited with, in both
    /// harnesses, silently. Checking at load time covers every reader at once.
    fn assert_stand_in_is_current(&self) {
        let stale: Vec<usize> = self
            .corpus
            .iter()
            .enumerate()
            .filter(|(i, row)| {
                let want = project(&row.content, self.dim);
                match self.vectors.get(i) {
                    Some(got) => {
                        got.len() != want.len()
                            || got.iter().zip(&want).any(|(g, w)| (g - w).abs() > 1e-5)
                    }
                    None => true,
                }
            })
            .map(|(i, _)| i)
            .collect();
        assert!(
            stale.is_empty(),
            "{} of {} vectors in embeddings.json do not match the kernel's stand-in \
             projection (rows {stale:?}). Regenerate the checked-in file with \
             `cargo test -p acowork-eval --test regenerate_stand_in_embeddings -- --ignored` \
             so the corpus and the queries are embedded by one tokenizer.",
            stale.len(),
            self.corpus.len(),
        );
    }

    /// Load the repo-level dataset from [`EvalDataset::dir`].
    ///
    /// A missing dataset is a failure, not a skip, and says so with the gate it
    /// belongs to: an eval that silently reports "ok" while measuring nothing
    /// makes ADR-095 D6 look satisfied.
    pub fn load_repo() -> Self {
        let dir = Self::dir();
        assert!(
            dir.join("dataset.jsonl").exists(),
            "no dataset at {} — ADR-095 D6 requires these numbers to exist before \
             retrieval weights change",
            dir.display()
        );
        Self::load(&dir)
    }

    /// Read only `corpus.jsonl`, without touching `embeddings.json`.
    ///
    /// The regeneration path needs this: it writes the very file `load`
    /// validates, so it cannot be required to pass that validation first.
    pub fn corpus_rows(dir: &Path) -> Vec<CorpusRow> {
        read_jsonl(&dir.join("corpus.jsonl"))
    }

    /// Vector for one corpus row, or a hard error — a row without its embedding
    /// would silently drop it out of the semantic source.
    pub fn vector(&self, row: usize) -> &[f32] {
        self.vectors
            .get(&row)
            .unwrap_or_else(|| panic!("no embedding for corpus row {row}"))
    }
}

fn read_jsonl<T: for<'de> serde::Deserialize<'de>>(path: &Path) -> Vec<T> {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("valid jsonl row"))
        .collect()
}

/// Tokenize for the fixture's stand-in embedding: alphanumeric runs lowercased
/// into words, every CJK ideograph also emitted on its own.
///
/// The per-character CJK split is what keeps a Chinese query in the same space
/// as a Chinese corpus row; without it a whole sentence hashes to one bucket and
/// the semantic source degenerates to "identical sentence or nothing".
///
/// The character classes mirror `acowork_runtime::search::analyzer`, because the
/// stand-in's only job is to approximate the production lexer's token granularity
/// at zero cost. A divergence moves the semantic leg of the measurement without
/// anyone touching a fusion weight — the original version got this wrong, since
/// `char::is_alphanumeric` is true for Han characters: "网关端口19875" accumulated
/// one glued token, so the literal number matched nothing.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if is_cjk(ch) {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            out.push(ch.to_string());
        } else if ch.is_alphanumeric() || matches!(ch, '-' | '_') {
            cur.extend(ch.to_lowercase());
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The same ideograph ranges the query analyzer treats as per-character.
fn is_cjk(c: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&c)
        || ('\u{3400}'..='\u{4DBF}').contains(&c)
        || ('\u{3040}'..='\u{30FF}').contains(&c)
        || ('\u{AC00}'..='\u{D7AF}').contains(&c)
}

/// The hashed-token projection used as a stand-in embedding: bag-of-tokens
/// hashed into `dim` buckets, L2-normalized.
///
/// Deterministic and model-free, so two runs of one variant are byte-comparable
/// — but it is *not* a calibrated embedding space. Directional comparisons are
/// valid; absolute values are not.
pub fn project(text: &str, dim: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dim];
    for t in tokenize(text) {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in t.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        v[(h as usize) % dim] += 1.0;
    }
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n == 0.0 {
        v
    } else {
        v.iter().map(|x| x / n).collect()
    }
}

/// The stand-in embedding a harness queries with: [`project`] applied at the
/// corpus's own dimension, or `None` for text that projects to nothing (no
/// tokens), which the engines then see as "no semantic source".
///
/// Shared because a query that embeds differently from the corpus would quietly
/// change what the semantic weight is being credited with.
pub fn stand_in_embedding(text: &str, dim: usize) -> Option<Vec<f32>> {
    let v = project(text, dim);
    if v.iter().all(|x| *x == 0.0) {
        None
    } else {
        Some(v)
    }
}

/// The width the checked-in stand-in file is written at.
///
/// A constant rather than a per-file choice: changing it is a change to the
/// measurement, and it has to be visible as one.
pub const STAND_IN_DIM: usize = 64;

/// The full `embeddings.json` body for `corpus` at [`STAND_IN_DIM`].
///
/// The generator lives next to the tokenizer that defines the stand-in so the
/// file can always be rebuilt from the code that reads it — see
/// `tests/regenerate_stand_in_embeddings.rs`.
pub fn stand_in_embeddings_file(corpus: &[CorpusRow]) -> String {
    let vectors: serde_json::Map<String, serde_json::Value> = corpus
        .iter()
        .enumerate()
        .map(|(i, row)| {
            (
                i.to_string(),
                serde_json::json!(project(&row.content, STAND_IN_DIM)),
            )
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({
        "dim": STAND_IN_DIM,
        "note": "Deterministic hashed-token projection produced by \
                 acowork_eval::stand_in_embeddings_file — a stand-in for the \
                 production embedding model, not a calibrated embedding space. Regenerate with \
                 `cargo test -p acowork-eval --test regenerate_stand_in_embeddings -- --ignored`.",
        "vectors": vectors,
    }))
    .expect("serialize embeddings")
}

// ── report ──────────────────────────────────────────────────────────────────
//
// The document shape a harness writes to `dev/eval/retrieval/reports/`. It is
// shared for the same reason the metric math is: the two scope reports are read
// side by side when a knob is argued, and two spellings of the same column make
// that comparison a translation exercise rather than a reading.

/// One query's row of a variant's report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerQueryReport {
    pub id: String,
    pub lang: String,
    pub kind: String,
    pub retrieved: usize,
    pub relevant: usize,
    pub recall_at_k: f64,
    pub mrr: f64,
    /// `None` for a query with no `exact_terms` annotation — not "kept", not
    /// "lost", simply not measured, so it stays out of the retention average.
    pub exact_terms_kept: Option<bool>,
    /// Query-analyzer term count. `Some` only for the engine that runs the
    /// ADR-095 analyzer (the conversation scope); the memory harness reports
    /// `None` because no analyzer runs on its read path.
    pub analyzer_terms: Option<usize>,
}

/// One variant's aggregate plus its per-query rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariantReport {
    pub variant: String,
    pub metrics: VariantMetrics,
    pub per_query: Vec<PerQueryReport>,
    /// Queries whose ground truth lives entirely in the other scope. Recorded so
    /// a reader can tell a thin dataset from an engine that recalled nothing.
    pub skipped_out_of_scope: usize,
}

/// The whole report document, one per scope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalReport {
    pub generated_by: String,
    pub scope: String,
    pub k: usize,
    pub dataset: String,
    pub corpus_rows: usize,
    pub variants: Vec<VariantReport>,
    /// The caveat that must travel with the numbers.
    pub embedding_caveat: String,
}

/// The reading instruction for `false recall` on a fixture this size. It belongs
/// with the renderer, not with either harness, because it is the summary table
/// that produces the number.
const FALSE_RECALL_NOTE: &str = ">\n> `false recall` saturates at 1.0 on a fixture this size (the ungated vector\n> source fills K slots for every query). Read it as \"not yet measurable\", not\n> as a defect of the variant.\n\n";

impl EvalReport {
    /// The caveat every harness must ship, word for word — the stand-in space is
    /// the one thing a reader of these numbers cannot infer from them.
    pub const CAVEAT: &'static str = "embeddings.json is a deterministic hashed-token stand-in, not the \
                                      production embedding model. Directional comparisons are valid; \
                                      absolute values are not.";

    /// The headline comparison table. Everything after it is scope-specific and
    /// appended by the harness that owns the scope.
    pub fn render_summary(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "# Retrieval variant report — {} scope\n\n",
            self.scope
        ));
        s.push_str(&format!("- generated by `{}`\n", self.generated_by));
        s.push_str(&format!(
            "- dataset: {} · {} rows · K = {}\n",
            self.dataset, self.corpus_rows, self.k
        ));
        s.push_str(&format!("\n> **Caveat** {}\n", self.embedding_caveat));
        // Print the note only where the column actually carries a number. On a
        // scope whose dataset has no empty-ground-truth query the cell reads
        // `n/a`, and a paragraph explaining a saturated value would be
        // describing a value the report does not contain.
        if self
            .variants
            .iter()
            .any(|v| v.metrics.false_recall_rate.is_some())
        {
            s.push_str(FALSE_RECALL_NOTE);
        }
        s.push_str("| variant | queries | Recall@K | Precision@K | MRR | nDCG@K | false recall | exact-term retention | skipped |\n");
        s.push_str("|---|---|---|---|---|---|---|---|---|\n");
        for v in &self.variants {
            let m = &v.metrics;
            s.push_str(&format!(
                "| {} | {} | {:.3} | {:.3} | {:.3} | {:.3} | {} | {} | {} |\n",
                v.variant,
                m.queries,
                m.recall_at_k,
                m.precision_at_k,
                m.mrr,
                m.ndcg_at_k,
                m.false_recall_rate
                    .map_or("n/a".to_string(), |x| format!("{:.3}", x)),
                m.exact_term_retention
                    .map_or("n/a".to_string(), |x| format!("{:.3}", x)),
                v.skipped_out_of_scope,
            ));
        }
        s
    }

    /// Write `{scope}-report.{json,md}` into `dataset_dir/reports/` and return
    /// the two paths. `per_query_md` is the harness's own per-query section.
    pub fn write(&self, dataset_dir: &Path, per_query_md: &str) -> (PathBuf, PathBuf) {
        let out_dir = dataset_dir.join("reports");
        std::fs::create_dir_all(&out_dir).expect("reports dir");
        let json_path = out_dir.join(format!("{}-report.json", self.scope));
        std::fs::write(&json_path, serde_json::to_string_pretty(self).unwrap())
            .expect("write report json");
        let md_path = out_dir.join(format!("{}-report.md", self.scope));
        std::fs::write(
            &md_path,
            format!("{}\n{}", self.render_summary(), per_query_md),
        )
        .expect("write report md");
        (json_path, md_path)
    }
}

/// Render one `exact_terms_kept` cell the way both reports spell it.
pub fn kept_cell(kept: Option<bool>) -> String {
    kept.map_or("n/a".to_string(), |k| {
        if k {
            "kept".to_string()
        } else {
            "LOST".to_string()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(ids: &[u64]) -> Vec<u64> {
        ids.to_vec()
    }

    // ── precision@k ─────────────────────────────────────────────────────

    #[test]
    fn precision_is_one_when_every_slot_is_relevant() {
        assert_eq!(precision_at_k(&q(&[1, 2]), &q(&[1, 2]), 2), 1.0);
    }

    #[test]
    fn precision_counts_only_the_first_k_slots() {
        assert_eq!(precision_at_k(&q(&[1, 3, 4]), &q(&[1]), 2), 0.5);
    }

    #[test]
    fn precision_is_zero_for_an_empty_result_or_no_ground_truth() {
        assert_eq!(precision_at_k(&[], &q(&[1]), 5), 0.0);
        assert_eq!(precision_at_k(&q(&[1]), &[], 5), 0.0);
        assert_eq!(precision_at_k(&q(&[1]), &q(&[1]), 0), 0.0);
    }

    /// The denominator is `k`, never the number returned. A one-item answer that
    /// happens to be right must not score 1.0 at k=5 — that is what used to make
    /// an archived P@5 of 0.8 unreproducible, because the old retrieval returned
    /// almost nothing and was rewarded for it.
    #[test]
    fn precision_divides_by_k_not_by_result_count() {
        assert_eq!(precision_at_k(&q(&[1]), &q(&[1]), 5), 0.2);
        // k larger than the list still divides by k.
        assert_eq!(precision_at_k(&q(&[2, 1]), &q(&[1]), 5), 0.2);
    }

    // ── recall@k ────────────────────────────────────────────────────────

    #[test]
    fn recall_is_over_the_relevant_set_not_the_returned_set() {
        assert_eq!(recall_at_k(&q(&[1, 2, 3, 4, 5]), &q(&[1, 2]), 5), 1.0);
        assert_eq!(recall_at_k(&q(&[1, 9]), &q(&[1, 2, 3, 4]), 2), 0.25);
    }

    #[test]
    fn recall_of_an_empty_relevant_set_is_one_by_definition() {
        assert_eq!(recall_at_k(&q(&[7]), &[], 5), 1.0);
    }

    // ── MRR ─────────────────────────────────────────────────────────────

    #[test]
    fn mrr_is_one_for_a_first_rank_hit_and_halves_for_the_second() {
        assert_eq!(reciprocal_rank(&q(&[1, 2]), &q(&[1])), 1.0);
        assert_eq!(reciprocal_rank(&q(&[9, 1]), &q(&[1])), 0.5);
        assert_eq!(reciprocal_rank(&q(&[9, 8]), &q(&[1])), 0.0);
    }

    #[test]
    fn mean_reciprocal_rank_averages_across_queries() {
        let (first, first_gt, second, second_gt) = (q(&[1, 2]), q(&[1]), q(&[2, 1]), q(&[1]));
        let pairs: Vec<(&[u64], &[u64])> = vec![(&first, &first_gt), (&second, &second_gt)];
        assert_eq!(mean_reciprocal_rank(&pairs), 0.75);
        assert_eq!(mean_reciprocal_rank(&[] as &[(&[u64], &[u64])]), 0.0);
    }

    /// The metrics are generic over the ranked item because the two harnesses
    /// rank different spellings: the memory/conversation variant harness ranks
    /// store row ids, the end-to-end harness ranks corpus tags (ids it cannot
    /// know, since they depend on seeding order). One set of definitions has to
    /// serve both, or the two reports stop being comparable.
    #[test]
    fn metrics_are_not_tied_to_a_numeric_item_type() {
        let retrieved = ["C-GW-1", "C-GW-3", "C-UNREL-1"];
        let relevant = ["C-GW-3"];
        assert_eq!(recall_at_k(&retrieved, &relevant, 3), 1.0);
        assert_eq!(precision_at_k(&retrieved, &relevant, 3), 1.0 / 3.0);
        assert_eq!(reciprocal_rank(&retrieved, &relevant), 0.5);
        assert_eq!(
            ndcg_at_k(&retrieved, &relevant, 3),
            1.0 / (2.0_f64 + 1.0).log2()
        );
    }

    // ── nDCG@k ──────────────────────────────────────────────────────────

    #[test]
    fn ndcg_is_one_for_the_ideal_ranking() {
        assert_eq!(ndcg_at_k(&q(&[1, 2]), &q(&[1, 2]), 2), 1.0);
    }

    /// Binary relevance over two items inside a two-slot window is
    /// order-insensitive, so both orders must agree; the penalty only appears
    /// when a hit falls out of the window.
    #[test]
    fn ndcg_penalises_relevant_hits_ranked_late() {
        assert_eq!(ndcg_at_k(&q(&[1, 2]), &q(&[2, 1]), 2), 1.0);
        let early = ndcg_at_k(&q(&[1, 9, 8]), &q(&[1]), 3);
        let late = ndcg_at_k(&q(&[9, 8, 1]), &q(&[1]), 3);
        assert!(late < early, "{late} must score below {early}");
    }

    /// The reason the gates use nDCG instead of Precision@5: one relevant
    /// document still reaches 1.0 when it is ranked first.
    #[test]
    fn ndcg_is_not_capped_by_the_number_of_relevant_items() {
        assert_eq!(ndcg_at_k(&q(&[1, 2, 3, 4, 5]), &q(&[1]), 5), 1.0);
    }

    #[test]
    fn ndcg_is_zero_when_nothing_relevant_is_returned() {
        assert_eq!(ndcg_at_k(&q(&[7, 8]), &q(&[1]), 2), 0.0);
        assert_eq!(ndcg_at_k(&q(&[1]), &[], 2), 0.0);
    }

    /// A first-rank hit must not produce NaN through a `0 / 0` ideal discount.
    #[test]
    fn ndcg_is_never_nan() {
        for (retrieved, relevant, k) in [
            (q(&[1]), q(&[1]), 1usize),
            (q(&[1, 2, 3]), q(&[3]), 3),
            (q(&[]), q(&[1]), 5),
        ] {
            let v = ndcg_at_k(&retrieved, &relevant, k);
            assert!(v.is_finite(), "{retrieved:?}/{relevant:?}@{k} -> {v}");
        }
    }

    // ── exact terms ─────────────────────────────────────────────────────

    #[test]
    fn exact_term_check_is_case_insensitive_and_requires_all_terms() {
        let texts = q_text(&["Dark Mode Editor", "unrelated note"]);
        assert!(exact_terms_present(
            &texts,
            &["dark".to_string(), "editor".to_string()]
        ));
        assert!(!exact_terms_present(
            &texts,
            &["dark".to_string(), "tablet".to_string()]
        ));
        // No terms means "not annotated", which must never read as a pass.
        assert!(!exact_terms_present(&texts, &[]));
    }

    fn q_text(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // ── tally ───────────────────────────────────────────────────────────

    #[test]
    fn tally_averages_over_scored_queries_only() {
        let mut t = QualityTally::new(2);
        t.record(&q(&[1, 2]), &q(&[1, 2]));
        let junk = t.record(&q(&[7, 8]), &[]); // no ground truth: counted, not averaged
        assert!(!junk.scored);
        let m = t.finish();
        assert_eq!(m.queries, 1);
        assert_eq!(m.recall_at_k, 1.0);
        assert_eq!(m.false_recall_rate, Some(1.0));
        assert_eq!(m.exact_term_retention, None);
    }

    #[test]
    fn tally_tracks_exact_term_retention_separately() {
        let mut t = QualityTally::new(3);
        t.record(&q(&[1]), &q(&[1]));
        t.record_exact_check(
            &q_text(&["RRF fusion ranks by position"]),
            &["rrf".to_string()],
        );
        t.record(&q(&[2]), &q(&[2]));
        t.record_exact_check(&q_text(&["nothing literal here"]), &["bm25".to_string()]);
        let m = t.finish();
        assert_eq!(m.exact_term_retention, Some(0.5));
    }

    // ── batch evaluation ────────────────────────────────────────────────

    #[test]
    fn evaluate_reports_every_requested_cutoff() {
        let queries = [
            EvalQuery {
                query: "a".to_string(),
                relevant_ids: q(&[1]),
            },
            EvalQuery {
                query: "b".to_string(),
                relevant_ids: q(&[2, 3]),
            },
        ];
        let results = [q(&[1, 9]), q(&[9, 2, 3])];
        let m = evaluate_retrieval_quality(&queries, &results, &[1, 3]);
        assert_eq!(m.num_queries, 2);
        assert_eq!(m.precision_at_k[0], (1, 0.5)); // 1/2 at k=1, averaged
        assert_eq!(m.recall_at_k[1], (3, 1.0)); // both queries fully recalled
        // Query 1's single hit is at rank 1, so it reaches 1.0; query 2's two
        // hits are pushed to ranks 2 and 3 by the irrelevant item in front of
        // them, so it cannot. Taken as 1.0 this expectation would have been
        // evidence that nDCG ignores rank — which is the whole reason the gates
        // use it. The primitive is asserted by its own tests.
        let second = ndcg_at_k(&results[1], &queries[1].relevant_ids, 3);
        assert!(
            second < 1.0,
            "fixture must rank late to be meaningful: {second}"
        );
        assert_eq!(m.ndcg_at_k[1], (3, (1.0 + second) / 2.0));
        assert_eq!(m.mrr, (1.0 + 0.5) / 2.0);
    }

    #[test]
    fn projection_is_deterministic_and_unit_norm() {
        let a = project("Gateway MQTT broker 19875", 64);
        let b = project("Gateway MQTT broker 19875", 64);
        assert_eq!(a, b);
        let n = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-6, "norm was {n}");
        // Unrelated text shares no token, so the cosine must be low.
        let c = project("完全无关的中文句子", 64);
        let dot: f32 = a.iter().zip(&c).map(|(x, y)| x * y).sum();
        assert!(dot.abs() < 0.1, "dot was {dot}");
    }

    /// Locks the boundary, not just the presence of a token: a Latin run glued
    /// onto the ideographs before it would still satisfy `any(|x| x == "网")`.
    #[test]
    fn tokenizer_splits_cjk_per_character() {
        assert_eq!(
            tokenize("网关端口19875"),
            vec!["网", "关", "端", "口", "19875"]
        );
    }

    /// The analyzer keeps `-` and `_` inside a word so `hybrid_search` survives
    /// as one term; the stand-in has to agree or it is measuring a different
    /// lexer.
    #[test]
    fn tokenizer_keeps_in_word_connectors_like_the_analyzer() {
        assert_eq!(
            tokenize("hybrid_search pool-factor"),
            vec!["hybrid_search", "pool-factor"]
        );
    }

    #[test]
    fn scope_filter_decides_which_queries_a_harness_scores() {
        let q = DatasetQuery {
            id: "q".into(),
            lang: "en".into(),
            kind: "exact_entity".into(),
            query: "19876".into(),
            relevant: vec![
                Annotation {
                    scope: "memory".into(),
                    content: Some("x".into()),
                    session_id: None,
                    message_index: None,
                },
                Annotation {
                    scope: "conversation".into(),
                    content: None,
                    session_id: Some("s-1".into()),
                    message_index: Some(0),
                },
            ],
            exact_terms: vec![],
        };
        assert_eq!(q.annotations_for("memory").len(), 1);
        assert!(!q.out_of_scope("conversation"));
        assert!(q.out_of_scope("file"));
    }

    #[test]
    fn benchmark_metrics_survive_serde() {
        let m = evaluate_retrieval_quality(
            &[EvalQuery {
                query: "a".to_string(),
                relevant_ids: q(&[1]),
            }],
            &[q(&[1])],
            &[5],
        );
        let json = serde_json::to_string(&m).unwrap();
        let back: BenchmarkMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(back.num_queries, 1);
        assert_eq!(back.precision_at_k, m.precision_at_k);
    }

    // ── report layer ─────────────────────────────────────────────────────

    fn dataset_query(terms: Vec<String>) -> DatasetQuery {
        DatasetQuery {
            id: "q-1".into(),
            lang: "zh".into(),
            kind: "exact_entity".into(),
            query: "19876".into(),
            relevant: vec![Annotation {
                scope: "memory".into(),
                content: Some("x".into()),
                session_id: None,
                message_index: None,
            }],
            exact_terms: terms,
        }
    }

    #[test]
    fn record_query_emits_the_row_the_aggregate_was_computed_from() {
        let mut t = QualityTally::new(3);
        let row = t.record_query(
            &dataset_query(vec!["19876".to_string()]),
            &[1, 2],
            &[1],
            &["gateway listens on 19876".to_string()],
            None,
        );
        assert_eq!(
            (row.id.as_str(), row.retrieved, row.relevant),
            ("q-1", 2, 1)
        );
        assert_eq!(row.recall_at_k, 1.0);
        assert_eq!(row.exact_terms_kept, Some(true));
        // No analyzer on this path, so the column stays absent rather than 0.
        assert_eq!(row.analyzer_terms, None);
        assert_eq!(t.finish().queries, 1);
    }

    #[test]
    fn a_query_without_exact_terms_is_not_counted_as_retention() {
        let mut t = QualityTally::new(3);
        let row = t.record_query(
            &dataset_query(vec![]),
            &[1],
            &[1],
            &["x".to_string()],
            Some(2),
        );
        assert_eq!(row.exact_terms_kept, None);
        assert_eq!(row.analyzer_terms, Some(2));
        assert_eq!(t.finish().exact_term_retention, None);
    }

    #[test]
    fn summary_table_carries_every_column_both_scopes_are_read() {
        let report = EvalReport {
            generated_by: "tests".to_string(),
            scope: "memory".to_string(),
            k: 10,
            dataset: "7 queries".to_string(),
            corpus_rows: 20,
            variants: vec![VariantReport {
                variant: "semantic_4to1_current".to_string(),
                metrics: VariantMetrics {
                    queries: 7,
                    recall_at_k: 0.5,
                    precision_at_k: 0.2,
                    mrr: 0.33,
                    ndcg_at_k: 0.4,
                    false_recall_rate: Some(1.0),
                    exact_term_retention: None,
                },
                per_query: vec![],
                skipped_out_of_scope: 3,
            }],
            embedding_caveat: EvalReport::CAVEAT.to_string(),
        };
        let md = report.render_summary();
        assert!(
            md.contains("# Retrieval variant report — memory scope"),
            "{md}"
        );
        assert!(
            md.contains("| variant | queries | Recall@K | Precision@K | MRR | nDCG@K |"),
            "{md}"
        );
        assert!(
            md.contains(
                "semantic_4to1_current | 7 | 0.500 | 0.200 | 0.330 | 0.400 | 1.000 | n/a | 3 |"
            ),
            "{md}"
        );
        assert!(md.contains(EvalReport::CAVEAT), "{md}");
        // The saturation note travels with the number it explains.
        assert!(md.contains("`false recall` saturates at 1.0"), "{md}");
    }

    /// A scope whose dataset has no empty-ground-truth query reports `n/a` in
    /// that column, and must not carry a paragraph explaining a value that is
    /// not there.
    #[test]
    fn the_saturation_note_is_not_printed_over_an_unmeasured_column() {
        let mut report = EvalReport {
            generated_by: "tests".to_string(),
            scope: "conversation".to_string(),
            k: 5,
            dataset: "4 queries".to_string(),
            corpus_rows: 16,
            variants: vec![VariantReport {
                variant: "shipped".to_string(),
                metrics: VariantMetrics {
                    queries: 4,
                    false_recall_rate: None,
                    ..Default::default()
                },
                per_query: vec![],
                skipped_out_of_scope: 8,
            }],
            embedding_caveat: "live model, not a stand-in".to_string(),
        };
        assert!(
            !report.render_summary().contains("saturates at 1.0"),
            "{}",
            report.render_summary()
        );
        // And it comes back the moment one variant does carry the number.
        report.variants[0].metrics.false_recall_rate = Some(1.0);
        assert!(report.render_summary().contains("saturates at 1.0"));
    }

    #[test]
    fn kept_cell_distinguishes_unmeasured_from_lost() {
        assert_eq!(kept_cell(None), "n/a");
        assert_eq!(kept_cell(Some(true)), "kept");
        assert_eq!(kept_cell(Some(false)), "LOST");
    }
}

