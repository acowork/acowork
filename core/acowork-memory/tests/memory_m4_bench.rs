//! ADR-062 M4 (P2): before/after retrieval-quality benchmark.
//!
//! Quantifies the retrieval-quality delta of the D1 (Dormant exclusion) and
//! D2 (injection-path `min_score` fix) changes against a FIXED corpus + FIXED
//! query set with ground truth, so the numbers can be archived as the M5 gate
//! evidence (ADR-062 §5.2/§5.4).
//!
//! D2 was a fix on the per-turn auto-injection path, which ADR-101 decision 5
//! removed. The measurement is kept because it is about the *query shape*, not
//! about who issues it: "a no-filtering, all-labels query returns a hit". That
//! query is what the `memory_recall` tool sends today, so the bench drives
//! `MemoryQuery::deep_recall`.
//!
//! Metrics (per §5.2):
//!   1. Retrieval Precision@5 / Recall@5 / MRR  — via `evaluate_retrieval_quality`
//!   2. Dormant-garbage-in-context ratio        — fraction of retrieved results
//!      whose node status is Dormant (must be 0 after D1)
//!   3. deep-recall hit rate                   — fraction of tool-path queries
//!      that return non-empty (D2 min_score 0.3 → 0.0)
//!   4. confidence/importance distribution       — methodology shown; real data is
//!      collected via `memory_write_scores` telemetry in M3.6 (out of scope here)
//!
//! The corpus and query set are deterministic:
//!   - Corpus: 5 relevant Knowledge nodes (Active), 3 garbage nodes marked
//!     Dormant via `transition_to_dormant`, 2 distractor Active nodes.
//!   - Query set: 5 fixed queries, each with a ground-truth relevant node id.
//!
//! ADR-068 note: the `memory_store` tool only writes Episodes now, so the corpus
//! is seeded directly into the Knowledge (sediment) layer through the provider's
//! native store path — the same path the EpisodicDistiller uses on promotion.
//! This benchmark measures sediment-layer retrieval quality (D1/D2 gates) and
//! is intentionally independent of the LLM write chain.
//!
//! IMPORTANT: self-contained — uses an in-memory SQLite store, never touches
//! the running Gateway / Runtime / Desktop processes or their ports.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;

use acowork_core::EmbeddingProvider;
use acowork_eval::{EvalQuery, evaluate_retrieval_quality};

use acowork_memory::{
    KnowledgeSubType, MemoryManager, MemoryManagerConfig, MemoryProvider, MemoryQuery, NodeStatus,
    PrivacyLevel,
};


/// Deterministic embedding provider (same text → same 384-dim vector).
/// Mirrors the production fallback chain's deterministic behavior so retrieval
/// semantics are stable across runs.
struct DeterministicEmbedding;

#[async_trait::async_trait]
impl EmbeddingProvider for DeterministicEmbedding {
    fn name(&self) -> &str {
        "deterministic-m4-bench"
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, acowork_core::EmbeddingError> {
        Ok(acowork_memory::manager::procedural_embedding_fallback(text))
    }

    async fn embed_batch(
        &self,
        texts: &[&str],
    ) -> Result<Vec<Vec<f32>>, acowork_core::EmbeddingError> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.embed(t).await?);
        }
        Ok(out)
    }

    fn dimension(&self) -> usize {
        384
    }

    async fn is_available(&self) -> bool {
        true
    }
}

/// Shared harness: a real in-memory `GrafeoStore`, ready for sediment seeding
/// and `MemoryManager::retrieve`.
struct BenchE2e {
    store: Arc<acowork_memory::sqlite::SqliteStore>,
}

impl BenchE2e {
    fn new() -> Self {
        let store = Arc::new(
            acowork_memory::sqlite::SqliteStore::open_in_memory(
                acowork_memory::types::DEFAULT_EMBEDDING_DIM,
            )
            .expect("in-memory store"),
        );
        Self { store }
    }

    /// Seed a Knowledge node directly and return its node id.
    ///
    /// ADR-068 — see file header: the benchmark exercises sediment-layer
    /// retrieval quality, so it seeds Knowledge nodes through the provider's
    /// store path (the same path the EpisodicDistiller uses on promotion)
    /// instead of the LLM `memory_store` tool, which only writes Episodes.
    async fn store_knowledge(&self, content: &str, confidence: f32, importance: f32) -> u64 {
        let embedding = DeterministicEmbedding
            .embed(content)
            .await
            .expect("embed ok");
        let node = acowork_memory::KnowledgeNode {
            subject: "user".to_string(),
            predicate: String::new(),
            object: content.to_string(),
            sub_type: KnowledgeSubType::Fact,
            confidence,
            source_episode_id: None,
            source_episode_ids: Vec::new(),
            promotion_metadata: None,
            embedding: Some(embedding),
            status: NodeStatus::Active,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            metadata: HashMap::new(),
            privacy: PrivacyLevel::Personal,
            importance,
        };
        acowork_memory::MemoryProvider::store_knowledge(self.store.as_ref(), &node)
            .expect("store_knowledge ok")
    }
}

// ============================================================================
// Fixed corpus + query set
// ============================================================================

/// Fixed corpus: (key, content, confidence, importance).
/// - `A*` = ground-truth relevant nodes, kept Active.
/// - `D*` = garbage nodes that lexically overlap the queries, then marked Dormant.
/// - `N*` = distractor Active nodes (share partial words, not ground truth).
const CORPUS: &[(&str, &str, f32, f32)] = &[
    ("A1", "User prefers dark mode for the code editor", 0.9, 0.8),
    (
        "A2",
        "User lives in Shanghai near the Huangpu river",
        0.9,
        0.8,
    ),
    (
        "A3",
        "User works at Acme Corp as a backend engineer",
        0.9,
        0.8,
    ),
    (
        "A4",
        "User speaks Japanese and writes fluent code",
        0.9,
        0.8,
    ),
    ("A5", "User keeps two cats named Mochi and Tofu", 0.9, 0.8),
    (
        "D1",
        "User used to prefer dark mode in the terminal",
        0.9,
        0.1,
    ),
    ("D2", "Old note about the Shanghai office address", 0.9, 0.1),
    (
        "D3",
        "User previously worked at Acme Corp in sales",
        0.9,
        0.1,
    ),
    ("N1", "User prefers a light theme during winter", 0.9, 0.5),
    (
        "N2",
        "User reads books about backend architecture",
        0.9,
        0.5,
    ),
];

/// Fixed query set: (query text, ground-truth relevant corpus key).
const QUERIES: &[(&str, &str)] = &[
    ("dark mode editor", "A1"),
    ("Shanghai river home", "A2"),
    ("Acme backend engineer", "A3"),
    ("Japanese language code", "A4"),
    ("cats pets at home", "A5"),
];

/// Which corpus keys are garbage (to be marked Dormant).
const DORMANT_KEYS: &[&str] = &["D1", "D2", "D3"];
/// Which corpus keys must remain Active (relevant + distractor).
const ACTIVE_KEYS: &[&str] = &["A1", "A2", "A3", "A4", "A5", "N1", "N2"];

// ============================================================================
// Benchmark runner
// ============================================================================

/// Read one cutoff out of a `Vec<(k, value)>` the shared kernel produced.
fn at(values: &[(usize, f64)], k: usize) -> f32 {
    values.iter().find(|(i, _)| *i == k).map(|(_, v)| (*v) as f32).unwrap_or(0.0)
}

/// Per-state retrieval summary.
#[derive(Default)]
struct RunSummary {
    p5: f32,
    r5: f32,
    mrr: f32,
    dormant: f32,
    dr_hit: f32,
}

/// Run the full query set under one D1 state and one deep-recall `min_cosine`.
async fn run_state(
    e2e: &BenchE2e,
    relevant: &[EvalQuery],
    exclude_dormant: bool,
    deep_recall_min_cosine: Option<f32>,
) -> RunSummary {
    let mut cfg = MemoryManagerConfig::default();
    cfg.quality.exclude_dormant = exclude_dormant;
    let manager = MemoryManager::new(cfg);

    let mut per_query: Vec<Vec<u64>> = Vec::with_capacity(QUERIES.len());
    let mut dr_hits = 0usize;

    for (query_text, _rel_key) in QUERIES {
        let mut q = MemoryQuery::new(query_text.to_string());
        q.limit = 10;
        let result = manager
            .retrieve(&*e2e.store, &mut q, Some(&DeterministicEmbedding))
            .await
            .expect("retrieve ok");
        let ids: Vec<u64> = result.memories.iter().map(|m| m.node_id).collect();

        let mut dr = MemoryQuery::deep_recall(query_text.to_string(), None);
        dr.min_cosine = deep_recall_min_cosine;
        let dr_result = manager
            .retrieve(&*e2e.store, &mut dr, Some(&DeterministicEmbedding))
            .await
            .expect("deep_recall ok");
        if !dr_result.memories.is_empty() {
            dr_hits += 1;
        }
        per_query.push(ids);
    }

    let bench = evaluate_retrieval_quality(relevant, &per_query, &[5]);
    RunSummary {
        p5: at(&bench.precision_at_k, 5),
        r5: at(&bench.recall_at_k, 5),
        mrr: bench.mrr as f32,
        dormant: compute_dormant_ratio(e2e, &per_query),
        dr_hit: dr_hits as f32 / QUERIES.len() as f32,
    }
}

/// Fraction of retrieved results whose node status is Dormant, across queries.
fn compute_dormant_ratio(e2e: &BenchE2e, per_query: &[Vec<u64>]) -> f32 {
    let mut total = 0usize;
    let mut dormant = 0usize;
    for ids in per_query {
        for id in ids {
            total += 1;
            let status = e2e
                .store
                .get_node_status(*id)
                .ok()
                .flatten()
                .unwrap_or(NodeStatus::Active);
            if status == NodeStatus::Dormant {
                dormant += 1;
            }
        }
    }
    if total == 0 {
        0.0
    } else {
        dormant as f32 / total as f32
    }
}

// ============================================================================
// The M4 benchmark test
// ============================================================================

#[tokio::test]
async fn m4_benchmark_before_after() {
    let e2e = BenchE2e::new();

    // ── 1. Build the fixed corpus via the real write chain ──
    let mut ids_by_key: std::collections::HashMap<&str, u64> = std::collections::HashMap::new();
    for (key, content, confidence, importance) in CORPUS {
        let id = e2e.store_knowledge(content, *confidence, *importance).await;
        ids_by_key.insert(*key, id);
    }

    // ── 2. Mark garbage nodes Dormant (deterministic, bypasses decay timing) ──
    for key in DORMANT_KEYS {
        let id = ids_by_key[key];
        e2e.store
            .transition_to_dormant(id)
            .expect("transition ok");
        let status = e2e.store.get_node_status(id).ok().flatten().unwrap();
        assert_eq!(status, NodeStatus::Dormant, "{key} must be Dormant");
    }
    for key in ACTIVE_KEYS {
        let id = ids_by_key[key];
        let status = e2e.store.get_node_status(id).ok().flatten().unwrap();
        assert_eq!(status, NodeStatus::Active, "{key} must stay Active");
    }

    // Ground truth: one relevant node per query.
    let relevant: Vec<EvalQuery> = QUERIES
        .iter()
        .map(|(text, rel_key)| EvalQuery {
            query: text.to_string(),
            relevant_ids: vec![ids_by_key[*rel_key]],
        })
        .collect();

    // ── 3. BEFORE: D1 off (old behaviour) + deep-recall min_cosine 0.3 ──
    let before = run_state(&e2e, &relevant, false, Some(0.3)).await;

    // ── 4. AFTER: D1 on (default) + deep-recall min_cosine → quality.min_cosine ──
    let after = run_state(&e2e, &relevant, true, None).await;

    // ── 5. Report (before/after comparison table) ──
    println!("\n===== ADR-062 M4: before/after retrieval benchmark =====");
    println!("{:<34}{:>12}{:>12}", "metric", "before", "after");
    println!("{:-<58}", "");
    println!("{:<34}{:>12.4}{:>12.4}", "Precision@5", before.p5, after.p5);
    println!("{:<34}{:>12.4}{:>12.4}", "Recall@5", before.r5, after.r5);
    println!("{:<34}{:>12.4}{:>12.4}", "MRR", before.mrr, after.mrr);
    println!(
        "{:<34}{:>12.4}{:>12.4}",
        "Dormant garbage ratio", before.dormant, after.dormant
    );
    println!(
        "{:<34}{:>12.2}{:>12.2}",
        "deep-recall hit rate %",
        before.dr_hit * 100.0,
        after.dr_hit * 100.0
    );
    println!("{:-<58}", "");

    // ── 6. Gate assertions (M5 evidence, ADR-062 §5.2) ──
    // D1 guarantee: after must have zero Dormant garbage in context.
    assert!(
        after.dormant.abs() < 1e-6,
        "after D1: Dormant garbage ratio must be 0, got {}",
        after.dormant
    );
    // The benchmark must be non-vacuous: before D1, garbage is actually present.
    assert!(
        before.dormant > 0.0,
        "before D1: garbage ratio must be > 0 (corpus must contain retrievable Dormant nodes), got {}",
        before.dormant
    );
    // Precision must not regress after D1.
    assert!(
        after.p5 >= before.p5 - 1e-6,
        "after D1 Precision@5 must be >= before, got {after} vs {before}",
        after = after.p5,
        before = before.p5
    );
    // D2: the deep-recall hit rate must improve (min_cosine 0.3 was silently
    // filtering everything when it was read on the RRF score scale).
    assert!(
        after.dr_hit >= before.dr_hit,
        "deep-recall hit rate must not regress, got {:.2}% vs {:.2}%",
        after.dr_hit * 100.0,
        before.dr_hit * 100.0
    );
}

