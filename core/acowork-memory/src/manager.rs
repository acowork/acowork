//! MemoryManager - retrieval and write-side lifecycle over a `MemoryProvider`.
//!
//! 1. Retrieve - hybrid search, dedup, decay, rank (the `memory_recall` read path)
//! 2. Record  - persist distilled episodes (via `record_distilled`)
//!
//! The former "Inject" phase — formatting a retrieved block for the system
//! prompt, with its three per-category token budgets — died with ADR-100
//! decision 5. Recall is a tool call now: the Agent asks, the tool handler in
//! `crate::service::mcp::tools` formats the answer, and the result lands in the
//! conversation as a tool result rather than a prompt section.
//!
//! Conversation turns are NOT automatically recorded as episodic memory
//! (the former `record_turn` / `ConversationRecord` channel was removed —
//! dead code, see docs/memory-write-entrypoints.md).
//!
//! ADR-051 P2: Moved from acowork-runtime to acowork-memory.
//! Error type changed from RuntimeError::Tool to AcoworkError::Memory.
//! EmbeddingProvider trait imported from acowork-core.
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::{
    labels, Episode, HintType, MemoryProvider, MemoryQuery,
};
use crate::consolidation::EmbeddingFn;
use crate::quality::MemoryQualityConfig;

use acowork_core::EmbeddingProvider;
use crate::types::{DistilledEpisode, EpisodicDecayConfig, NodeStatus};
use acowork_core::error::{AcoworkError, Result};
use acowork_core::retrieval::{FusedHit, LexicalQuery, MatchedBy};

// ---------------------------------------------------------------------------
// Procedural embedding fallback (ADR-057 P0 ProceduralNode embedding required)
// ---------------------------------------------------------------------------
//
// All three ProceduralNode creation paths must supply a non-None embedding.
// When a real `EmbeddingFn` is available we use it; otherwise we fall back to a
// deterministic, dependency-free hash embedding of dimension 384 (matches
// `HnswConfig::default().vector_dim`) so the node remains indexable from the
// moment it is written.
//
// This is intentionally a pure function with no external dependencies so it
// can live in `acowork-memory` (storage-agnostic).

/// Default embedding dimension. Must match `HnswConfig::default().vector_dim`.
pub const PROCEDURAL_FALLBACK_DIM: usize = 384;

/// Deterministic, dependency-free fallback embedding for procedural nodes when
/// no `EmbeddingFn` is available. The result is not semantically meaningful,
/// but it is stable, non-zero, and dimensionally compatible with the HNSW
/// index so the stored `ProceduralNode` is immediately vector-searchable.
pub fn procedural_embedding_fallback(text: &str) -> Vec<f32> {
    let mut out = vec![0.0f32; PROCEDURAL_FALLBACK_DIM];
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    let h1 = hasher.finish();
    for (i, slot) in out.iter_mut().enumerate() {
        let mut h = DefaultHasher::new();
        (h1.wrapping_add(i as u64)).hash(&mut h);
        let v = h.finish();
        *slot = ((v % 2000) as f32 - 1000.0) / 1000.0;
    }
    out
}

/// Resolve the procedural embedding using the provided `EmbeddingFn` or fall
/// back to [`procedural_embedding_fallback`]. Never returns `None` — the
/// `embedding` field on a persisted `ProceduralNode` must be `Some(_)` per
/// ADR-057 P0.
pub fn procedural_embedding_for(
    text: &str,
    embedding_fn: Option<&EmbeddingFn>,
) -> Vec<f32> {
    match embedding_fn {
        Some(f) => f(text),
        None => procedural_embedding_fallback(text),
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for MemoryManager.
#[derive(Debug, Clone)]
pub struct MemoryManagerConfig {
    /// Default number of results to retrieve (default: 10).
    pub default_k: usize,
    /// Centralized memory quality parameters (ADR-062 D2).
    ///
    /// Source of truth for the retrieval-side quality knobs that were
    /// previously separate `MemoryManagerConfig` fields:
    /// - `quality.min_cosine` (absolute cosine floor on the retrieval gate;
    ///   replaces the old `min_score`, which thresholded the fused rank score)
    /// - `quality.exclude_dormant` (Dormant retrieval exclusion, D1)
    ///
    /// The write-path thresholds (dedup / consolidation) are pushed down to
    /// the `MemoryProvider` via `MemoryProvider::apply_quality_config` so
    /// engine internals read from the same config (ADR-062 §4.1). `Default`
    /// mirrors current behaviour ("zero configuration = current behaviour").
    pub quality: MemoryQualityConfig,
    /// Retrieval-side episodic time decay (memory forgetting). Default:
    /// disabled. When enabled, Episodic search scores are multiplied by the
    /// half-life retention factor before ranking.
    ///
    /// The same type is used by the background scan (`EpisodicDecayConfig`),
    /// so scan-side and retrieval-side semantics can never drift apart.
    pub forgetting: EpisodicDecayConfig,
    // ADR-100 decision 5: per-turn auto-injection is gone — recall is driven by
    // the agent calling the memory tools. That also removed the three injection
    // token budgets (`max_inject_tokens` / `max_autobio_core_tokens` /
    // `max_autobio_history_tokens`) and `record_async` (the knob of the deleted
    // `record_turn` path): nothing read them once their consumer was gone.
    // The `auto_inject_enabled` switch went the same way, and so did G9's
    // `abstention_prompt`: no caller ever set `abstention_enabled`, and
    // abstention guidance is a prompt-layer concern the Runtime already owns
    // through its `prompts/abstention.md` package asset.
}

impl Default for MemoryManagerConfig {
    fn default() -> Self {
        Self {
            default_k: 10,
            quality: MemoryQualityConfig::default(),
            forgetting: EpisodicDecayConfig::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Result of a memory retrieval operation.
#[derive(Debug)]
pub struct RetrievalResult {
    /// Retrieved memories sorted by relevance (highest first).
    pub memories: Vec<RetrievedMemory>,
}

/// One retrieval candidate between the per-label search and the final
/// [`RetrievedMemory`]: the fused score plus the in-source signals that must
/// survive dedup, Dormant exclusion, session/time filtering and decay.
///
/// This replaced a `(f64, String, String)` tuple. The tuple is the reason
/// ADR-095 D4's signals were historically dropped at the fusion boundary — a
/// positional record with four fields of different meanings is easy to thread
/// and impossible to read.
#[derive(Debug, Clone)]
struct RetrievalCandidate {
    /// Weighted RRF fused score. In-source rank signal, not a similarity.
    score: f64,
    label: String,
    /// `"hybrid"` | `"text"` — the legacy provenance string, kept for
    /// compatibility. [`Self::matched_by`] is its precise replacement.
    source: String,
    matched_by: Option<MatchedBy>,
    vector_rank: Option<usize>,
    text_rank: Option<usize>,
}

/// A single retrieved memory with relevance metadata.
#[derive(Debug, Clone)]
pub struct RetrievedMemory {
    /// Formatted content text.
    pub content: String,
    /// Node label (Knowledge, Episodic, Procedural, Autobiographical) or
    /// RAG source label (e.g., "RAG:enterprise_knowledge").
    pub label: String,
    /// Relevance score.
    pub score: f64,
    /// Retrieval source: "vector" | "text" | "graph" | "hybrid" | "rag".
    pub source: String,
    /// Grafeo node ID (for tracing). 0 for RAG results.
    pub node_id: u64,
    /// Source URL (for RAG results, describing where the chunk came from).
    pub source_url: Option<String>,
    /// Chunk ID within the source document (for RAG results).
    pub chunk_id: Option<String>,
    /// Which retrieval source(s) matched this node (ADR-095 D4).
    ///
    /// `None` means the provider could not say (a default-impl
    /// `hybrid_search_detailed`), **not** "matched nothing". RAG-channel
    /// results always carry `None` — they never went through the fusion engine.
    pub matched_by: Option<acowork_core::retrieval::MatchedBy>,
    /// Rank inside the vector source's candidate pool. In-source signal only.
    pub vector_rank: Option<usize>,
    /// Rank inside the lexical source's candidate pool. In-source signal only.
    pub text_rank: Option<usize>,
}

// ---------------------------------------------------------------------------
// MemoryManager
// ---------------------------------------------------------------------------

/// Retrieval and record orchestration over a [`MemoryProvider`].
///
/// ADR-051 C4: RAG channel removed from MemoryManager — RAG retrieval is
/// issued by the caller against its own provider and merged by score.
/// ADR-100 decision 5: the injection/formatting phase is removed too; the
/// recall tool handler owns the output format.
pub struct MemoryManager {
    config: MemoryManagerConfig,
}

impl MemoryManager {
    /// Create a new MemoryManager with the given configuration.
    pub fn new(config: MemoryManagerConfig) -> Self {
        Self { config }
    }

    /// Read the manager's configuration (e.g. to check feature switches).
    pub fn config(&self) -> &MemoryManagerConfig {
        &self.config
    }

    /// Retrieve relevant memories for the current query.
    ///
    /// If `query.embedding` is `None` and `embedding_provider` is `Some`,
    /// generates the embedding automatically with a 200ms timeout before
    /// proceeding to hybrid search. On timeout or failure, falls back to
    /// text-only search (graceful degradation).
    ///
    /// Pipeline: (auto-embed) → hybrid_search → dedup → merge & rank.
    ///
    /// RAG is not a channel here (ADR-051 C4): whoever needs enterprise
    /// knowledge queries its own `RagProvider` and merges the results with
    /// these by score, so every entry below is a memory node.
    pub async fn retrieve(
        &self,
        provider: &dyn MemoryProvider,
        query: &mut MemoryQuery,
        embedding_provider: Option<&dyn EmbeddingProvider>,
    ) -> Result<RetrievalResult> {
        // ── Auto-generate embedding if needed ──
        // Timeout is handled by FallbackEmbeddingProvider internally
        // (200ms per attempt, then fallback to next provider).
        // A missing provider used to degrade every retrieval to BM25 with no
        // trace in the logs at all. Name it, so a `source="text"` line in
        // "Memory search completed" always has a visible cause.
        if query.embedding.is_none() && embedding_provider.is_none() {
            tracing::info!(
                "No embedding provider on the memory session handle;                  retrieval will use text (BM25) only"
            );
        }
        if query.embedding.is_none()
            && let Some(emb_prov) = embedding_provider {
                // Vector source prefers `embedding_text` (full user message)
                // over `query_text` (LLM keywords) when the caller set it —
                // full sentences embed far better than keyword bags.
                let embed_src = query
                    .embedding_text
                    .as_deref()
                    .filter(|t| !t.trim().is_empty())
                    .unwrap_or(&query.query_text);
                match emb_prov.embed(embed_src).await {
                    Ok(vec) => {
                        tracing::info!(
                            dim = vec.len(),
                            provider = emb_prov.name(),
                            "Auto-generated query embedding"
                        );
                        query.embedding = Some(vec);
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "Embedding generation failed, falling back to text search"
                        );
                    }
                }
            }

        let k = if query.limit > 0 {
            query.limit
        } else {
            self.config.default_k
        };
        let hint_type = query.hint_type;
        let (vector_weight, text_weight) = hint_weights(hint_type);

        // G10 (design §6.6): search ALL 4 labels regardless of hint type.
        // The design explicitly states that "searching all 4 labels is the
        // safer choice" — even Identity queries (e.g. `auto_inject`) must
        // reach Knowledge / Procedural layers. Narrowing per hint type was
        // removed (would require an explicit design decision to re-add).
        let search_labels: Vec<&str> = vec![
            labels::EPISODIC,
            labels::KNOWLEDGE,
            labels::PROCEDURAL,
            labels::AUTOBIOGRAPHICAL,
        ];

        // Run hybrid search on each label.
        //
        // Quality knob is `k` (result count), not `min_cosine`. The absolute
        // cosine floor is unreliable as a relevance signal: real sentence
        // embeddings are anisotropic (unrelated pairs sit at cos 0.5–0.9;
        // measured on the live store, even 0.6 filtered nothing). The gate
        // only affected the vector source anyway, so passing `None` is the
        // honest choice — quality is controlled downstream by `k` and by
        // the eventual LLM-judge / z-score follow-up tracked separately.
        // One shared fusion engine for every caller (ADR-095 D3): the hint
        // weights become a `FusionPolicy` and the detailed form keeps the
        // per-source signals instead of discarding them after fusion.
        //
        // `min_cosine: None` is deliberate and unchanged — see the note above
        // on anisotropic embeddings.
        let fusion_policy = acowork_core::retrieval::FusionPolicy {
            vector_weight,
            text_weight,
            min_cosine: None,
            pool_factor: 2,
        };

        let mut all_results: Vec<(u64, RetrievalCandidate)> = Vec::new();

        for label in &search_labels {
            let search_result: Result<Vec<FusedHit>> = if let Some(ref embedding) = query.embedding
            {
                provider
                    .hybrid_search_detailed(
                        label,
                        // This path has no analyzer: `query_text` is already the
                        // lexical query (for `memory_recall` it is the
                        // LLM-extracted keywords), so `raw_only` is the honest
                        // representation — not a fallback.
                        &LexicalQuery::raw_only(&query.query_text),
                        embedding,
                        k,
                        &fusion_policy,
                    )
                    .map_err(|e| AcoworkError::Memory(format!("Hybrid search failed: {e}")))
            } else {
                // Fallback to text search when no embedding is available. Every
                // hit is lexical evidence and there is no cosine to report.
                provider
                    .text_search_with_filter(
                        label,
                        "content",
                        &query.query_text,
                        k,
                    )
                    .map(|hits| {
                        hits.into_iter()
                            .enumerate()
                            .map(|(rank, (id, score))| FusedHit {
                                id,
                                matched_by: MatchedBy::Text,
                                vector_rank: None,
                                text_rank: Some(rank),
                                cosine: None,
                                fused_score: score,
                                fused_rank: rank,
                            })
                            .collect()
                    })
                    .map_err(|e| AcoworkError::Memory(format!("Text search failed: {e}")))
            };

            match search_result {
                Ok(results) => {
                    let source = if query.embedding.is_some() {
                        "hybrid"
                    } else {
                        "text"
                    };
                    tracing::info!(
                        label,
                        result_count = results.len(),
                        source,
                        "Memory search completed (before dedup + exclude)"
                    );
                    for hit in results {
                        all_results.push((
                            hit.id,
                            RetrievalCandidate {
                                score: hit.fused_score,
                                label: label.to_string(),
                                source: source.to_string(),
                                matched_by: Some(hit.matched_by),
                                vector_rank: hit.vector_rank,
                                text_rank: hit.text_rank,
                            },
                        ));
                    }
                }
                Err(e) => {
                    // Log and continue — partial results are better than no results.
                    tracing::warn!("Search failed for label {}: {}", label, e);
                }
            }
        }

        // ADR-062 D1: Exclude Dormant nodes from the final result set.
        // Dormant nodes are decayed below threshold but retained (design
        // §5.2) — they must NOT appear in retrieval results.
        //
        // Pending nodes intentionally remain retrievable: they are
        // low-confidence but searchable, and are naturally down-ranked by
        // confidence in the final ordering.
        if self.config.quality.exclude_dormant && !all_results.is_empty() {
            let before = all_results.len();
            // Resolve status once per unique node to avoid repeated lookups.
            let mut status_cache: HashMap<u64, Option<NodeStatus>> = HashMap::new();
            for (id, _) in &all_results {
                status_cache
                    .entry(*id)
                    .or_insert_with(|| provider.get_node_status(*id).ok().flatten());
            }
            all_results.retain(|(id, _)| {
                status_cache
                    .get(id)
                    .and_then(|s| s.clone())
                    .is_none_or(|s| s != NodeStatus::Dormant)
            });
            tracing::debug!(
                before,
                after = all_results.len(),
                "Excluded Dormant nodes from retrieval results"
            );
        }

        // Deduplicate by node_id, keeping the highest-scoring candidate and
        // **its own** signals. Carrying the signals of the winning candidate is
        // the point: a node that surfaced from both sources must not be
        // reported as the text-only copy of itself just because the other label
        // scored marginally lower.
        let mut best_by_id: HashMap<u64, RetrievalCandidate> = HashMap::new();
        for (id, cand) in all_results {
            best_by_id
                .entry(id)
                .and_modify(|existing| {
                    if cand.score > existing.score {
                        *existing = cand.clone();
                    }
                })
                .or_insert(cand);
        }

        // Post-filter: exclude nodes belonging to the current session.
        // Prevents re-injecting compaction summaries that are already in
        // the conversation context window.
        if let Some(ref exclude_sid) = query.filters.exclude_session_id {
            let before = best_by_id.len();
            best_by_id.retain(|node_id, _| {
                match provider.get_node_session_id(*node_id) {
                    Ok(Some(sid)) => sid != *exclude_sid,
                    _ => true, // No session_id or error -> keep
                }
            });
            tracing::debug!(
                before,
                after = best_by_id.len(),
                exclude_session_id = %exclude_sid,
                "Excluded current-session nodes from retrieval"
            );
        }

        // Post-filter: apply the time-range window (created_at in [since, until]).
        // Drives the `memory_recall` tool's `since`/`until` parameters, which
        // were previously validated but never applied (ADR-062 M5).
        if let Some((since, until)) = query.filters.time_range {
            let before = best_by_id.len();
            best_by_id.retain(|node_id, _| {
                match provider.get_node_created_at(*node_id) {
                    Ok(Some(ts)) => ts >= since && ts <= until,
                    // No timestamp or error -> keep (defensive; mirrors the
                    // exclude_session_id filter's keep-on-unknown policy).
                    _ => true,
                }
            });
            tracing::debug!(
                before,
                after = best_by_id.len(),
                since = %since,
                until = %until,
                "Applied time-range filter to retrieval"
            );
        }

        // Episodic time-decay (memory forgetting, ADR-057 §5.3 redesign):
        // multiply the final score of Episodic nodes by the half-life
        // retention factor `exp(-ln2 × age_days / half_life_days)`.
        // Semantic-layer nodes (Knowledge / Procedural / Autobiographical)
        // are never decayed — they are durable knowledge, not event records.
        // Pure scoring logic lives in `apply_retrieval_decay` (unit-tested);
        // missing timestamps keep the current score.
        best_by_id = apply_retrieval_decay(best_by_id, &self.config.forgetting, |node_id| {
            match provider.get_node_created_at(node_id) {
                Ok(Some(ts)) => Some((chrono::Utc::now() - ts).num_seconds() as f64 / 86400.0),
                // No timestamp or error -> keep current score (defensive).
                _ => None,
            }
        });

        // Build RetrievedMemory list, sorted by score descending.
        let mut memories: Vec<RetrievedMemory> = Vec::new();
        for (node_id, cand) in best_by_id {
            let RetrievalCandidate {
                score,
                label,
                source,
                matched_by,
                vector_rank,
                text_rank,
            } = cand;
            let content = match provider.get_node_content(node_id) {
                Ok(Some(c)) => c,
                Ok(None) => String::new(),
                Err(e) => {
                    tracing::warn!(node_id, error = %e, "Failed to extract node content");
                    String::new()
                }
            };
            memories.push(RetrievedMemory {
                content,
                label,
                score,
                source,
                node_id,
                source_url: None,
                chunk_id: None,
                matched_by,
                vector_rank,
                text_rank,
            });
        }

        memories.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Limit to k results.
        let result_count = memories.len().min(k);
        memories.truncate(result_count);

        // The fused score is a rank signal, not a similarity (ADR-095), so the
        // only aggregate worth logging here is how much came back.
        tracing::debug!("Retrieved {} memories", result_count);

        Ok(RetrievalResult { memories })
    }
    /// Record a distilled/compacted episode into the episodic layer.
    ///
    /// ADR-057 → triples removal (this PR): only the natural-language
    /// summary is persisted. Triple-based knowledge extraction was removed
    /// because compact-model output quality was too low (see
    /// [`parse_compact_output_strict`]’s gate rationale). Knowledge-layer
    /// updates flow through the `memory_store` tool / procedural creation
    /// paths instead (see `docs/memory-write-entrypoints.md`).
    ///
    /// Embedding of the summary is best-effort (D1 — failure degrades to
    /// `None`, episode still stored, vector recall drops for that node).
    pub async fn record_distilled(
        &self,
        provider: &dyn MemoryProvider,
        episode: &DistilledEpisode,
        embedding_provider: Option<&dyn EmbeddingProvider>,
    ) -> Result<()> {
        let summary_embedding = match embedding_provider {
            Some(prov) => prov.embed(&episode.summary).await.ok(),
            None => None,
        };

        let mut metadata = std::collections::HashMap::new();
        metadata.insert(
            "source_session_id".to_string(),
            serde_json::Value::String(episode.source_session_id.clone()),
        );

        let ep = Episode {
            session_id: episode.session_id.clone(),
            turn_index: 0,
            role: "distilled".to_string(),
            content: episode.summary.clone(),
            embedding: summary_embedding,
            timestamp: chrono::Utc::now(),
            consolidated: false,
            metadata,
            importance: 0.7,
            knowledge_subtype: None,
            normalized: None,
        };
        provider.store_episode(&ep)?;

        tracing::debug!(
            session_id = %episode.session_id,
            summary_len = episode.summary.len(),
            "Recorded distilled episode (summary-only, no triple landing)"
        );

        Ok(())
    }

    // ── ADR-051 P3: High-level semantic methods ─────────────────────────
    //
    // These methods encapsulate all direct provider CRUD operations. Callers —
    // the memory service's tool handlers — go through them, never raw provider
    // methods.
    //
    // ADR-100 decision 5 removed `retrieve_and_inject` together with the
    // Runtime's per-turn auto-injection path: a recall is now a tool call the
    // agent makes, and its handler composes `retrieve` with its own formatting.

    /// Activate ProceduralNodes that were retrieved and matched the context.
    ///
    /// For each retrieved memory with label "Procedural", increments the
    /// `activation_count`. This tracks how often a procedure is actually
    /// used, feeding into confidence boosting (ADR-057 P1) and future
    /// activation-based retrieval ranking. Called by the recall tool so the
    /// signal survives the removal of the auto-injection path (ADR-100).
    pub(crate) fn activate_procedural_nodes(
        &self,
        provider: &dyn MemoryProvider,
        memories: &[RetrievedMemory],
    ) {
        for memory in memories {
            if memory.label != labels::PROCEDURAL || memory.node_id == 0 {
                continue;
            }

            if let Some(mut node) = provider.get_procedural(memory.node_id).ok().flatten() {
                node.activation_count = node.activation_count.saturating_add(1);
                node.updated_at = chrono::Utc::now();
                if let Err(e) = provider.update_procedural(&node) {
                    tracing::debug!(
                        node_id = memory.node_id,
                        error = %e,
                        "Failed to increment activation_count (non-fatal)"
                    );
                }
            }
        }
    }
}

/// Retrieval-side episodic time-decay (ADR-057 §5.3 redesign).
///
/// Multiplies the final score of Episodic nodes by the half-life
/// retention factor `exp(-ln2 × age_days / half_life_days)` (see
/// [`EpisodicDecayConfig::retention`], clamped to [0, 1]). Semantic-layer
/// nodes (Knowledge / Procedural / Autobiographical) are never decayed —
/// they are durable knowledge, not event records.
///
/// `age_days(node_id)` returns the node's age in days; `None` (missing
/// timestamp or read error) keeps the current score, mirroring the other
/// post-filters' keep-on-unknown policy.
///
/// Kept as a pure module-level function (no provider I/O) so the scoring
/// branch is directly unit-testable.
fn apply_retrieval_decay(
    mut best_by_id: HashMap<u64, RetrievalCandidate>,
    config: &EpisodicDecayConfig,
    mut age_days: impl FnMut(u64) -> Option<f64>,
) -> HashMap<u64, RetrievalCandidate> {
    if !config.enabled || best_by_id.is_empty() {
        return best_by_id;
    }
    for (node_id, cand) in best_by_id.iter_mut() {
        let (score, label) = (&mut cand.score, &cand.label);
        if label != labels::EPISODIC {
            continue;
        }
        let age_days = match age_days(*node_id) {
            Some(age) => age,
            // No timestamp or error -> keep current score (defensive).
            None => continue,
        };
        // Same half-life formula as the scan-side engine
        // (`EpisodicDecayConfig::retention`): a zero half-life disables
        // decay, a future timestamp never amplifies the score.
        let retention = config.retention(age_days);
        *score *= retention;
        tracing::debug!(
            node_id,
            age_days = format!("{age_days:.1}"),
            retention = format!("{retention:.3}"),
            "Episodic retrieval time-decay applied"
        );
    }
    best_by_id
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Get hybrid search weights based on hint type.
///
/// Returns `(vector_weight, text_weight)`.
fn hint_weights(hint_type: HintType) -> (f64, f64) {
    match hint_type {
        HintType::Semantic => (0.8, 0.2),
        HintType::Factual => (0.5, 0.5),
        HintType::Relational => (0.6, 0.2),
        HintType::Identity => (0.3, 0.7),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Tests (pure logic - no GrafeoStore dependency)
// GrafeoStore-dependent integration tests remain in acowork-runtime.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    /// Decay operates on the post-dedup candidate map; tests only care about
    /// `score` and `label`, so build the rest here rather than repeating it.
    fn cand(score: f64, label: &str) -> RetrievalCandidate {
        RetrievalCandidate {
            score,
            label: label.to_string(),
            source: "hybrid".into(),
            matched_by: None,
            vector_rank: None,
            text_rank: None,
        }
    }

    use super::*;

    #[test]
    fn test_retrieval_decay_half_life_episodic_only() {
        // Episodic at exactly one half-life → score halved; fresh Episodic
        // ≈ full score; Knowledge never decayed (durable sediment layer).
        let cfg = EpisodicDecayConfig {
            enabled: true,
            half_life_days: 180,
            dormant_threshold: 0.1,
            archive_days: 90,
        };
        let mut best: HashMap<u64, RetrievalCandidate> = HashMap::new();
        best.insert(1, cand(1.0, labels::EPISODIC));
        best.insert(2, cand(1.0, labels::EPISODIC));
        best.insert(3, cand(1.0, labels::KNOWLEDGE));

        let out = apply_retrieval_decay(best, &cfg, |id| match id {
            1 => Some(180.0),
            2 => Some(0.5),
            _ => Some(999.0),
        });

        assert!(
            (out[&1].score - 0.5).abs() < 1e-6,
            "half-life Episodic node must be halved, got {}",
            out[&1].score
        );
        assert!(
            out[&2].score > 0.99,
            "fresh Episodic node must keep ~full score, got {}",
            out[&2].score
        );
        assert_eq!(
            out[&3].score, 1.0,
            "Knowledge must never be decayed by retrieval-side forgetting"
        );
    }

    #[test]
    fn test_retrieval_decay_disabled_or_unknown_age_keeps_scores() {
        // enabled = false (default) → no decay at all.
        let cfg_off = EpisodicDecayConfig::default();
        let mut best: HashMap<u64, RetrievalCandidate> = HashMap::new();
        best.insert(1, cand(0.8, labels::EPISODIC));
        let out = apply_retrieval_decay(best, &cfg_off, |_| Some(100000.0));
        assert_eq!(out[&1].score, 0.8, "disabled config must be a no-op");

        // enabled with missing/error timestamp → keep current score.
        let cfg_on = EpisodicDecayConfig {
            enabled: true,
            ..Default::default()
        };
        let mut best: HashMap<u64, RetrievalCandidate> = HashMap::new();
        best.insert(1, cand(0.8, labels::EPISODIC));
        let out = apply_retrieval_decay(best, &cfg_on, |_| None);
        assert_eq!(out[&1].score, 0.8, "unknown age must keep current score");
    }

    #[test]
    fn test_retrieval_decay_zero_half_life_and_future_clamp() {
        // Zero half-life disables decay even for ancient nodes.
        let cfg_zero = EpisodicDecayConfig {
            enabled: true,
            half_life_days: 0,
            ..Default::default()
        };
        let mut best: HashMap<u64, RetrievalCandidate> = HashMap::new();
        best.insert(1, cand(0.9, labels::EPISODIC));
        let out = apply_retrieval_decay(best, &cfg_zero, |_| Some(100000.0));
        assert_eq!(out[&1].score, 0.9, "zero half-life must disable decay");

        // Future timestamp (negative age) → retention clamped to [0, 1]:
        // the score must never be amplified above its original value.
        let cfg = EpisodicDecayConfig {
            enabled: true,
            half_life_days: 180,
            ..Default::default()
        };
        let mut best: HashMap<u64, RetrievalCandidate> = HashMap::new();
        best.insert(1, cand(0.5, labels::EPISODIC));
        let out = apply_retrieval_decay(best, &cfg, |_| Some(-50.0));
        assert!(
            out[&1].score <= 0.5 + 1e-9 && out[&1].score > 0.0,
            "future timestamp must not amplify score, got {}",
            out[&1].score
        );
    }

    #[test]
    fn test_config_defaults() {
        let config = MemoryManagerConfig::default();
        assert_eq!(config.default_k, 10);
        assert_eq!(config.quality.min_cosine, 0.3);
        assert!(config.quality.exclude_dormant, "D1 Dormant exclusion on by default");
    }

    #[test]
    fn test_manager_new() {
        let config = MemoryManagerConfig::default();
        let manager = MemoryManager::new(config.clone());
        assert_eq!(manager.config.default_k, config.default_k);
    }

    // ── ADR-057 P0: ProceduralNode embedding is mandatory ────────────────
    //
    // Verifies that the procedural embedding fallback is deterministic and
    // dimension-compatible with HNSW, and that `procedural_embedding_for`
    // prefers a real `EmbeddingFn` when supplied. These are used by all
    // ProceduralNode creation paths (distill, generalization, manual).

    #[test]
    fn procedural_fallback_embedding_is_deterministic_and_dim_384() {
        let a = procedural_embedding_fallback("hello world");
        let b = procedural_embedding_fallback("hello world");
        let c = procedural_embedding_fallback("another text");
        assert_eq!(a, b, "same text must hash identically");
        assert_ne!(a, c, "different text must hash differently");
        assert_eq!(
            a.len(),
            PROCEDURAL_FALLBACK_DIM,
            "fallback dim must match HnswConfig::default().vector_dim (384)"
        );
    }

    #[test]
    fn procedural_embedding_for_prefers_real_fn_over_fallback() {
        let marker: EmbeddingFn = std::sync::Arc::new(|_text: &str| vec![42.0f32; 8]);
        let v = procedural_embedding_for("any text", Some(&marker));
        assert_eq!(v, vec![42.0f32; 8], "real fn must win over fallback");

        let v = procedural_embedding_for("any text", None);
        assert_eq!(
            v.len(),
            PROCEDURAL_FALLBACK_DIM,
            "fallback used when no fn provided"
        );
    }
}
