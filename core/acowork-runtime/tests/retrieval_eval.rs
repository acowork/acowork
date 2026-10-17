//! Retrieval variant comparison harness (ADR-095 D6, design 28 §3.3).
//!
//! ## What this answers
//!
//! "If we change the query analyzer, the fusion weights, or the cosine floor,
//! which measured quality moves, and by how much?" ADR-095 D6 makes a Phase-2
//! weight change conditional on that being answered, so this file is the gate,
//! not documentation.
//!
//! ## Relationship to `memory_retrieval_bench.rs`
//!
//! Deliberately not merged. That benchmark asserts **absolute** quality gates on
//! a store shaped like real agent memory; this one compares **variants** on a
//! hand-annotated query set. The metric math is duplicated (textbook
//! definitions) rather than extracted into a test-only crate: two integration
//! tests cannot import each other, and a crate for `nDCG` alone would be an
//! abstraction with one real consumer.
//!
//! ## Determinism
//!
//! Embeddings come from `dev/eval/retrieval/embeddings.json` and are never
//! fetched from a live model, so two runs of one variant are byte-comparable.
//! The checked-in file is a hashed-token projection: a **stand-in** that makes
//! lexical/semantic agreement reproducible, but is not a calibrated embedding
//! space. Regenerate it with the production model before a weight change is
//! justified by these numbers — `--ignored --nocapture regenerate` prints the
//! vectors a generator needs.
//!
//! ## Running
//!
//! ```bash
//! cd core
//! cargo test -p acowork-runtime --test retrieval_eval -- --ignored --nocapture
//! ```
//!
//! Reports land in `dev/eval/retrieval/reports/`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use acowork_core::retrieval::{FusionPolicy, LexicalQuery};

// ── fixtures ──────────────────────────────────────────────────────────────

fn eval_dir() -> PathBuf {
    // `core/acowork-runtime` -> repo root is two levels up.
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dev/eval/retrieval")
}

#[derive(serde::Deserialize, Clone)]
struct CorpusRow {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    label: String,
    content: String,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    message_index: usize,
    #[serde(default)]
    role: String,
}

#[derive(Debug, serde::Deserialize)]
struct Relevant {
    scope: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    message_index: Option<usize>,
}

#[derive(Debug, serde::Deserialize)]
struct Query {
    id: String,
    lang: String,
    kind: String,
    query: String,
    relevant: Vec<Relevant>,
    #[serde(default)]
    exact_terms: Vec<String>,
}

fn read_jsonl<T: for<'de> serde::Deserialize<'de>>(path: &Path) -> Vec<T> {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("valid jsonl row"))
        .collect()
}

/// The built fixture store plus the id maps annotations resolve through.
struct Fixture {
    /// The shared engine. `Arc` because `ConversationStore` borrows the same
    /// store the memory scopes search, exactly as production wires it.
    store: std::sync::Arc<acowork_sqlite::SqliteStore>,
    /// node id -> indexed text. Built from the corpus rather than read back
    /// through the store, so the harness depends on no private accessor.
    text_by_id: HashMap<u64, String>,
    /// corpus row index -> node id (both scopes share this namespace map)
    row_ids: HashMap<usize, u64>,
    /// annotation key -> node id
    memory_by_content: HashMap<String, u64>,
    conversation_by_key: HashMap<(String, usize), u64>,
    dim: usize,
}

fn build_fixture(dir: &Path) -> Fixture {
    let corpus: Vec<CorpusRow> = read_jsonl(&dir.join("corpus.jsonl"));
    let raw: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("embeddings.json")).expect("embeddings.json"),
    )
    .expect("embeddings json");
    let dim = raw["dim"].as_u64().unwrap() as usize;
    let vectors: HashMap<usize, Vec<f32>> = raw["vectors"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| {
            (
                k.parse::<usize>().expect("vector key is a row index"),
                v.as_array()
                    .unwrap()
                    .iter()
                    .map(|x| x.as_f64().unwrap() as f32)
                    .collect(),
            )
        })
        .collect();

    let store = std::sync::Arc::new(
        acowork_sqlite::SqliteStore::open_in_memory(dim).expect("open store"),
    );
    let conv = acowork_sqlite::ConversationStore::from_store(store.clone()).expect("conv store");

    let mut row_ids = HashMap::new();
    let mut text_by_id = HashMap::new();
    let mut memory_by_content = HashMap::new();
    let mut conversation_by_key = HashMap::new();

    for (i, row) in corpus.iter().enumerate() {
        let vector = vectors
            .get(&i)
            .unwrap_or_else(|| panic!("no embedding for corpus row {i}"));
        let id = match row.kind.as_str() {
            "memory" => insert_memory(&store, &row.label, &row.content, vector),
            "conversation" => conv
                .index_message(&row.session_id, row.message_index, &row.role, &row.content, vector)
                .expect("index message"),
            other => panic!("unknown corpus kind {other}"),
        };
        row_ids.insert(i, id);
        text_by_id.insert(id, row.content.clone());
        match row.kind.as_str() {
            "memory" => {
                memory_by_content.insert(row.content.clone(), id);
            }
            _ => {
                conversation_by_key.insert((row.session_id.clone(), row.message_index), id);
            }
        }
    }

    Fixture { store, row_ids, text_by_id, memory_by_content, conversation_by_key, dim }
}

fn insert_memory(store: &std::sync::Arc<acowork_sqlite::SqliteStore>, label: &str, content: &str, vector: &[f32]) -> u64 {
    use acowork_memory::labels;
    let now = chrono::Utc::now();
    match label {
        labels::KNOWLEDGE => {
            // `KnowledgeNode` is a (subject, predicate, object) triple and the
            // engine dedups on (subject, predicate), so the fixture spreads its
            // rows across distinct predicates — otherwise every row after the
            // first would overwrite the previous one and the corpus would
            // silently collapse to a single node.
            let node = acowork_memory::types::KnowledgeNode {
                subject: "eval".to_string(),
                predicate: format!("note-{}", sha16(content)),
                object: content.to_string(),
                sub_type: acowork_memory::KnowledgeSubType::Fact,
                confidence: 0.9,
                source_episode_id: None,
                source_episode_ids: vec![],
                promotion_metadata: None,
                embedding: Some(vector.to_vec()),
                status: acowork_memory::NodeStatus::Active,
                created_at: now,
                updated_at: now,
                metadata: std::collections::HashMap::new(),
                privacy: acowork_memory::PrivacyLevel::Personal,
                importance: 0.7,
            };
            store.store_knowledge(&node).expect("store knowledge")
        }
        labels::PROCEDURAL => {
            let node = acowork_memory::types::ProceduralNode {
                id: None,
                name: format!("proc-{}", sha16(content)),
                // Indexed text is `name trigger action_pattern`, so the content
                // goes in `action_pattern` to stay searchable.
                trigger_condition: "eval".to_string(),
                action_pattern: content.to_string(),
                success_count: 0,
                fail_count: 0,
                confidence: 0.9,
                activation_count: 0,
                source_skill: None,
                learned_from: "user_feedback".to_string(),
                embedding: vector.to_vec(),
                status: acowork_memory::NodeStatus::Active,
                created_at: now,
                updated_at: now,
                source_episode_ids: vec![],
                promotion_metadata: None,
                metadata: std::collections::HashMap::new(),
            };
            store.store_procedural(&node).expect("store procedural")
        }
        // episodic + autobiographical both live in the episode table.
        _ => {
            let ep = acowork_memory::types::Episode {
                session_id: format!("eval-{label}-{}", sha16(content)),
                turn_index: 0,
                role: "user".to_string(),
                content: content.to_string(),
                embedding: Some(vector.to_vec()),
                timestamp: now,
                consolidated: false,
                metadata: std::collections::HashMap::new(),
                importance: 0.7,
                knowledge_subtype: None,
                normalized: None,
            };
            store.store_episode(&ep).expect("store episode")
        }
    }
}

/// Short stable content id, used only to make fixture keys unique.
fn sha16(text: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    text.hash(&mut h);
    format!("{:016x}", h.finish())
}

impl Fixture {
    /// Resolve one annotation to the node id it names. A miss is a dataset
    /// error and must fail loudly: silently skipping it would score a query as
    /// "no relevant results" and quietly flatter every variant.
    fn resolve(&self, r: &Relevant) -> u64 {
        match r.scope.as_str() {
            "memory" => *self
                .memory_by_content
                .get(r.content.as_deref().unwrap_or_default())
                .unwrap_or_else(|| panic!("annotation names no memory row: {:?}", r.content)),
            "conversation" => *self
                .conversation_by_key
                .get(&(
                    r.session_id.clone().unwrap_or_default(),
                    r.message_index.unwrap_or_default(),
                ))
                .unwrap_or_else(|| panic!("annotation names no conversation row: {:?}", r)),
            other => panic!("unknown annotation scope {other}"),
        }
    }
}

// ── variants ──────────────────────────────────────────────────────────────

/// A retrieval configuration under test. Each field is one of the knobs ADR-095
/// authorizes only with measurement behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Variant {
    /// Today's global search: one query text drives both sources, equal weight.
    Baseline,
    /// Analyzer terms drive FTS, raw text drives cosine, equal weight.
    DualEqual,
    /// The same dual query at `SEMANTIC_4TO1` — the ADR's initial memory policy.
    DualSemantic4To1,
    /// Dual query at 4:1 with the conversation cosine floor.
    DualSemantic4To1WithFloor,
}

impl Variant {
    fn name(self) -> &'static str {
        match self {
            Variant::Baseline => "baseline",
            Variant::DualEqual => "dual_equal",
            Variant::DualSemantic4To1 => "dual_4to1",
            Variant::DualSemantic4To1WithFloor => "dual_4to1_floor",
        }
    }

    fn uses_analyzer(self) -> bool {
        !matches!(self, Variant::Baseline)
    }

    fn policy(self) -> FusionPolicy {
        match self {
            Variant::Baseline | Variant::DualEqual => FusionPolicy::EQUAL_NO_GATE,
            Variant::DualSemantic4To1 => FusionPolicy::SEMANTIC_4TO1,
            Variant::DualSemantic4To1WithFloor => FusionPolicy::CONVERSATION_BASELINE,
        }
    }
}

// ── metrics ───────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, serde::Serialize)]
struct Metrics {
    /// Queries scored (excludes `no_relevant`, which measure false recall).
    queries: usize,
    recall_at_k: f64,
    precision_at_k: f64,
    mrr: f64,
    ndcg_at_k: f64,
    /// Fraction of `no_relevant` queries that returned any hit. A variant that
    /// "finds more" by returning junk is not better.
    false_recall_rate: Option<f64>,
    /// Exact-term retention, reported separately and never folded into nDCG
    /// (ADR-095 D6): this is the metric that catches an analyzer or weight
    /// change destroying literal matches — the specific way hybrid retrieval
    /// regresses.
    exact_term_retention: Option<f64>,
}

const K: usize = 10;

/// Recall@K over the annotated relevant set.
fn recall_at_k(retrieved: &[u64], relevant: &std::collections::HashSet<u64>) -> f64 {
    if relevant.is_empty() {
        return 0.0;
    }
    let hits = retrieved.iter().take(K).filter(|id| relevant.contains(id)).count();
    hits as f64 / relevant.len() as f64
}

/// Precision@K with denominator `K` — not `min(K, returned)`, which would score
/// a retriever that returns one correct item as perfect.
fn precision_at_k(retrieved: &[u64], relevant: &std::collections::HashSet<u64>) -> f64 {
    if relevant.is_empty() {
        return 0.0;
    }
    let hits = retrieved.iter().take(K).filter(|id| relevant.contains(id)).count();
    hits as f64 / K as f64
}

/// Reciprocal rank of the first relevant hit.
fn reciprocal_rank(retrieved: &[u64], relevant: &std::collections::HashSet<u64>) -> f64 {
    retrieved
        .iter()
        .take(K)
        .position(|id| relevant.contains(id))
        .map_or(0.0, |p| 1.0 / (p as f64 + 1.0))
}

/// Standard nDCG position discount for a 0-based index: `log2(i + 2)`.
///
/// The `+ 2` matters: `log2(i + 1)` gives rank 0 a discount of `log2(1) = 0`,
/// which divides to infinity and turns the first relevant hit into NaN — the
/// exact case the metric exists to reward.
fn discount(rank: usize) -> f64 {
    (rank as f64 + 2.0).log2()
}

/// nDCG@K with binary relevance.
fn ndcg_at_k(retrieved: &[u64], relevant: &std::collections::HashSet<u64>) -> f64 {
    if relevant.is_empty() {
        return 0.0;
    }
    let dcg: f64 = retrieved
        .iter()
        .take(K)
        .enumerate()
        .map(|(i, id)| {
            if relevant.contains(id) {
                1.0 / discount(i)
            } else {
                0.0
            }
        })
        .sum();
    // Ideal order: all relevant items first, capped at K.
    let ideal = relevant.len().min(K);
    let idcg: f64 = (0..ideal).map(|i| 1.0 / discount(i)).sum();
    if idcg == 0.0 {
        0.0
    } else {
        dcg / idcg
    }
}

/// Whether a returned node's text contains every exact term (case-insensitive).
///
/// This is the D6 "文本精确命中保留率" measure: it asks whether the *result set*
/// still carries the literal, independent of whether it ranked first.
fn exact_terms_present(texts: &[String], terms: &[String]) -> bool {
    terms.iter().all(|t| {
        let needle = t.to_lowercase();
        texts.iter().any(|h| h.to_lowercase().contains(&needle))
    })
}

// ── runner ────────────────────────────────────────────────────────────────

/// Run one variant over the whole dataset.
fn run_variant(fx: &Fixture, queries: &[Query], variant: Variant, analyzer_cfg: &acowork_runtime::search::analyzer::AnalyzerConfig) -> VariantReport {
    let mut acc = Metrics::default();
    let mut scored = 0usize;
    let mut no_relevant_total = 0usize;
    let mut no_relevant_false = 0usize;
    let mut exact_total = 0usize;
    let mut exact_kept = 0usize;
    let mut per_query = Vec::new();

    for q in queries {
        let dual = acowork_runtime::search::analyzer::analyze(&q.query, analyzer_cfg);
        let terms: &[String] = if variant.uses_analyzer() { &dual.lexical_terms } else { &[] };

        // Memory scope: the four labels are disjoint, so ids cannot collide and
        // the merge is a fused-score sort — the same shape the production admin
        // path uses.
        let mut memory_hits: Vec<(u64, f64)> = Vec::new();
        for label in acowork_memory::labels::ALL {
            let embedding = query_embedding(fx, &q.query);
            let lexical = LexicalQuery { raw: &q.query, terms };
            let hits = fx
                .store
                .hybrid_search_detailed(label, &lexical, embedding.as_deref(), K, &variant.policy())
                .expect("hybrid search");
            memory_hits.extend(hits.into_iter().map(|h| (h.id, h.fused_score)));
        }
        memory_hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let embedding = query_embedding(fx, &q.query);
        let lexical = LexicalQuery { raw: &q.query, terms };
        let conv_hits = fx
            .conv_store()
            .search_detailed(&lexical, embedding.as_deref(), K, &variant.policy())
            .expect("conversation search");

        let memory_ids: Vec<u64> = memory_hits.iter().map(|(id, _)| *id).collect();
        let conv_ids: Vec<u64> = conv_hits.iter().map(|(_, fused)| fused.id).collect();
        let retrieved: Vec<u64> = memory_ids.iter().chain(conv_ids.iter()).copied().collect();

        let relevant: std::collections::HashSet<u64> = q.relevant.iter().map(|r| fx.resolve(r)).collect();

        if relevant.is_empty() {
            no_relevant_total += 1;
            if !retrieved.is_empty() {
                no_relevant_false += 1;
            }
        } else {
            scored += 1;
            acc.recall_at_k += recall_at_k(&retrieved, &relevant);
            acc.precision_at_k += precision_at_k(&retrieved, &relevant);
            acc.mrr += reciprocal_rank(&retrieved, &relevant);
            acc.ndcg_at_k += ndcg_at_k(&retrieved, &relevant);
        }

        if !q.exact_terms.is_empty() {
            exact_total += 1;
            let texts = fx.texts_for(&retrieved);
            if exact_terms_present(&texts, &q.exact_terms) {
                exact_kept += 1;
            }
        }

        per_query.push(PerQuery {
            id: q.id.clone(),
            lang: q.lang.clone(),
            kind: q.kind.clone(),
            analyzer_terms: terms.len(),
            retrieved: retrieved.len(),
            relevant: relevant.len(),
            recall_at_k: recall_at_k(&retrieved, &relevant),
            mrr: reciprocal_rank(&retrieved, &relevant),
            exact_terms_kept: if q.exact_terms.is_empty() {
                None
            } else {
                Some(exact_terms_present(&fx.texts_for(&retrieved), &q.exact_terms))
            },
        });
    }

    acc.queries = scored;
    let n = scored.max(1) as f64;
    acc.recall_at_k /= n;
    acc.precision_at_k /= n;
    acc.mrr /= n;
    acc.ndcg_at_k /= n;
    // NOTE: on a fixture this small the vector source (with no cosine floor)
    // fills K slots for every query, so this ratio saturates at 1.0 for *all*
    // variants and cannot discriminate between them. It becomes meaningful only
    // against a corpus large enough that "returned nothing" is reachable.
    // Read 1.0 here as "not yet measurable", never as "this variant
    // hallucinates results".
    acc.false_recall_rate = if no_relevant_total > 0 {
        Some(no_relevant_false as f64 / no_relevant_total as f64)
    } else {
        None
    };
    acc.exact_term_retention = if exact_total > 0 {
        Some(exact_kept as f64 / exact_total as f64)
    } else {
        None
    };

    VariantReport {
        variant: variant.name().to_string(),
        metrics: acc,
        per_query,
    }
}

fn query_embedding(fx: &Fixture, text: &str) -> Option<Vec<f32>> {
    // The fixture's stand-in model is the same projection used to build the
    // corpus, so a query embeds in the identical space.
    let v = fx.project(text);
    if v.iter().all(|x| *x == 0.0) {
        None
    } else {
        Some(v)
    }
}

#[derive(serde::Serialize)]
struct PerQuery {
    id: String,
    lang: String,
    kind: String,
    analyzer_terms: usize,
    retrieved: usize,
    relevant: usize,
    recall_at_k: f64,
    mrr: f64,
    exact_terms_kept: Option<bool>,
}

#[derive(serde::Serialize)]
struct VariantReport {
    variant: String,
    metrics: Metrics,
    per_query: Vec<PerQuery>,
}

#[derive(serde::Serialize)]
struct Report {
    generated_by: String,
    k: usize,
    dataset: String,
    corpus_rows: usize,
    variants: Vec<VariantReport>,
    /// The caveat that must travel with the numbers.
    embedding_caveat: String,
}

impl Fixture {
    fn conv_store(&self) -> acowork_sqlite::ConversationStore {
        acowork_sqlite::ConversationStore::from_store(self.store.clone()).expect("conv store")
    }

    /// Re-implement the fixture's token projection so queries embed in the same
    /// space as the corpus without a model call.
    fn project(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; self.dim];
        for t in tokenize(text) {
            let mut h: u64 = 0xcbf29ce484222325;
            for b in t.as_bytes() {
                h ^= *b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            v[(h as usize) % self.dim] += 1.0;
        }
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if n == 0.0 {
            v
        } else {
            v.iter().map(|x| x / n).collect()
        }
    }

    fn texts_for(&self, ids: &[u64]) -> Vec<String> {
        ids.iter().filter_map(|id| self.text_by_id.get(id).cloned()).collect()
    }
}

fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            cur.extend(ch.to_lowercase());
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        if is_cjk(ch) {
            out.push(ch.to_string());
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn is_cjk(ch: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&ch)
}

// ── the test ──────────────────────────────────────────────────────────────

/// Ignored by design: this is an evaluation, not a CI assertion. It has no
/// pass/fail threshold because ADR-095 D6 says the thresholds get agreed at
/// review time, from the first real numbers this produces.
#[test]
#[ignore]
fn compare_retrieval_variants() {
    let dir = eval_dir();
    // A missing dataset is a failure, not a skip: an eval that silently
    // reports "ok" while measuring nothing is worse than no eval at all,
    // because it makes the D6 gate look satisfied.
    assert!(
        dir.join("dataset.jsonl").exists(),
        "no dataset at {} — ADR-095 D6 requires these numbers to exist before          retrieval weights change",
        dir.display()
    );
    let fx = build_fixture(&dir);
    let queries: Vec<Query> = read_jsonl(&dir.join("dataset.jsonl"));
    let analyzer_cfg = acowork_runtime::search::policy::SearchPolicyConfig::default().analyzer_config();

    let variants = [
        Variant::Baseline,
        Variant::DualEqual,
        Variant::DualSemantic4To1,
        Variant::DualSemantic4To1WithFloor,
    ];

    let reports: Vec<VariantReport> =
        variants.iter().map(|v| run_variant(&fx, &queries, *v, &analyzer_cfg)).collect();

    let report = Report {
        generated_by: "core/acowork-runtime/tests/retrieval_eval.rs".to_string(),
        k: K,
        dataset: format!("{} queries", queries.len()),
        corpus_rows: fx.row_ids.len(),
        variants: reports,
        embedding_caveat: "embeddings.json is a deterministic hashed-token stand-in, not the production embedding model. Directional comparisons are valid; absolute values are not.".to_string(),
    };

    let out_dir = dir.join("reports");
    std::fs::create_dir_all(&out_dir).expect("reports dir");
    let json_path = out_dir.join("report.json");
    std::fs::write(&json_path, serde_json::to_string_pretty(&report).unwrap()).unwrap();

    let md = render_markdown(&report);
    let md_path = out_dir.join("report.md");
    std::fs::write(&md_path, &md).unwrap();

    println!("{md}");
    println!("\nwrote {} and {}", json_path.display(), md_path.display());
}

fn render_markdown(r: &Report) -> String {
    let mut s = String::new();
    s.push_str("# Retrieval variant report\n\n");
    s.push_str(&format!("- generated by `{}`\n", r.generated_by));
    s.push_str(&format!("- dataset: {} · corpus rows: {} · K = {}\n", r.dataset, r.corpus_rows, r.k));
    s.push_str(&format!("\n> **Caveat** {embedding_caveat}\n", embedding_caveat = r.embedding_caveat));
    s.push_str(
        ">\n> `false recall` saturates at 1.0 on a fixture this size (the ungated vector\n> source fills K slots for every query). Read it as \"not yet measurable\", not\n> as a defect of the variant.\n\n",
    );
    s.push_str("| variant | Recall@K | Precision@K | MRR | nDCG@K | false recall | exact-term retention |\n");
    s.push_str("|---|---|---|---|---|---|---|\n");
    for v in &r.variants {
        let m = &v.metrics;
        s.push_str(&format!(
            "| {} | {:.3} | {:.3} | {:.3} | {:.3} | {} | {} |\n",
            v.variant,
            m.recall_at_k,
            m.precision_at_k,
            m.mrr,
            m.ndcg_at_k,
            m.false_recall_rate.map_or("n/a".to_string(), |x| format!("{:.3}", x)),
            m.exact_term_retention.map_or("n/a".to_string(), |x| format!("{:.3}", x)),
        ));
    }
    s.push_str("\n## Per query\n\n");
    if let Some(v) = r
        .variants
        .iter()
        .find(|v| v.variant != "baseline")
        .or_else(|| r.variants.first())
    {
        // Term counts come from an analyzer variant: the baseline is *defined*
        // to have zero terms, so printing it would report "0 terms" for every
        // query and hide the one signal this column exists for.
        s.push_str("| id | lang | kind | relevant | analyzer terms |\n|---|---|---|---|---|\n");
        for q in &v.per_query {
            s.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                q.id, q.lang, q.kind, q.relevant, q.analyzer_terms
            ));
        }
    }
    s
}

// ── harness self-checks ───────────────────────────────────────────────────
//
// The eval is `#[ignore]`d, but these run in CI. A metric definition that is
// wrong produces numbers nobody can trust, and the D6 gate rests on them — so
// the harness itself is the thing under test here.

#[cfg(test)]
mod metric_tests {
    use super::*;

    fn set(ids: &[u64]) -> std::collections::HashSet<u64> {
        ids.iter().copied().collect()
    }

    #[test]
    fn perfect_ranking_scores_one() {
        let relevant = set(&[1, 2]);
        let retrieved = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
        assert_eq!(recall_at_k(&retrieved, &relevant), 1.0);
        assert!((ndcg_at_k(&retrieved, &relevant) - 1.0).abs() < 1e-12);
        assert_eq!(reciprocal_rank(&retrieved, &relevant), 1.0);
    }

    #[test]
    fn ndcg_is_never_nan_for_a_first_rank_hit() {
        // Regression: the position discount used log2(i+1), so rank 0 divided
        // by zero and the best possible result became NaN.
        let relevant = set(&[7]);
        let n = ndcg_at_k(&[7, 1, 2, 3], &relevant);
        assert!(n.is_finite(), "nDCG was {n}");
        assert!((n - 1.0).abs() < 1e-12);
    }

    #[test]
    fn ndcg_penalizes_relevant_hits_ranked_late() {
        let relevant = set(&[9]);
        let early = ndcg_at_k(&[9, 1, 2, 3], &relevant);
        let late = ndcg_at_k(&[1, 2, 3, 9], &relevant);
        assert!(early > late, "early {early} should beat late {late}");
    }

    #[test]
    fn precision_divides_by_k_not_by_returned_count() {
        // Returning one correct item is 1/K precision, not 1.0. Dividing by
        // `min(k, returned)` is the flaw the archived benchmarks had: it let a
        // one-item retriever look perfect.
        let relevant = set(&[1]);
        assert!((precision_at_k(&[1], &relevant) - 1.0 / K as f64).abs() < 1e-12);
    }

    #[test]
    fn recall_over_the_relevant_set_not_the_returned_set() {
        let relevant = set(&[1, 2, 3, 4]);
        assert!((recall_at_k(&[1, 2, 5], &relevant) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn empty_relevant_set_scores_zero_rather_than_one() {
        // `no_relevant` queries are measured by false-recall, not by these.
        let none = set(&[]);
        assert_eq!(recall_at_k(&[1, 2], &none), 0.0);
        assert_eq!(ndcg_at_k(&[1, 2], &none), 0.0);
        assert_eq!(reciprocal_rank(&[1, 2], &none), 0.0);
    }

    #[test]
    fn exact_term_retention_is_case_insensitive_and_requires_all_terms() {
        let texts = vec!["Gateway MQTT broker on port 19875".to_string()];
        assert!(exact_terms_present(&texts, &["mqtt".to_string(), "19875".to_string()]));
        assert!(!exact_terms_present(&texts, &["mqtt".to_string(), "kafka".to_string()]));
        // An empty term list must not claim retention it did not measure.
        assert!(exact_terms_present(&texts, &[]));
    }

    #[test]
    fn variants_differ_in_at_least_one_knob() {
        // Guards against a variant being added without wiring its policy, which
        // would silently duplicate a row in the report.
        let all = [
            Variant::Baseline,
            Variant::DualEqual,
            Variant::DualSemantic4To1,
            Variant::DualSemantic4To1WithFloor,
        ];
        let names: Vec<&str> = all.iter().map(|v| v.name()).collect();
        assert_eq!(names.len(), names.iter().collect::<HashSet<_>>().len());
        let policies: Vec<(f64, f64)> = all
            .iter()
            .map(|v| (v.policy().vector_weight, v.policy().text_weight))
            .collect();
        assert_ne!(policies[0], policies[2], "baseline and 4:1 must differ");
        assert!(
            all.iter().map(|v| v.uses_analyzer()).collect::<Vec<_>>().windows(2).any(|w| w[0] != w[1]),
            "some variant must exercise the analyzer"
        );
    }
}
