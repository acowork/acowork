//! Retrieval fusion layer (ADR-082 D5, upgraded by ADR-095 D3/D4).
//!
//! This is the **single** hybrid retrieval engine in the codebase. Three call
//! paths run through it: the `memory_recall` tool (via
//! ([`crate::MemoryProvider`]), the global-search memory scope
//! (`sqlite::admin`) and the global-search conversation scope
//! (`acowork-sqlite::conversation`). Before ADR-095 the latter two had degraded
//! copies of the same logic with different knobs (single query text, equal
//! weights, no per-source signals); converging them here is the reuse this
//! module exists to provide, not a new engine.
//!
//! The engine-era fusion API mixed three score domains into a single `score`
//! field (ADR-082 §1.4), which silently dropped every Chinese `memory_recall`
//! hit: a BM25 miss pushed the query onto the vector-only path, whose score
//! (`cos - 1`, non-positive) could never clear a `>= 0.0` gate. Fusion and
//! gating are therefore implemented here, so the score domains are structurally
//! separate:
//!
//! * **vector source** gates on `cos >= min_cosine` — the absolute cosine
//!   domain. `None` disables the gate; `memory_recall` passes `None` and relies
//!   on the result count (`k`) as the quality knob instead, because anisotropic
//!   embeddings make an absolute cosine floor unreliable as a relevance signal.
//! * **text source** has no fixed threshold: BM25 carries IDF and drifts with
//!   corpus statistics, so a lexical hit is independent evidence and survives
//!   even when its embedding sits far away. The vector gate never touches it
//!   (ADR-095 D3).
//! * **fusion** is weighted RRF (k = 60) over the union of the two ranked
//!   lists.
//!
//! [`SqliteStore::hybrid_search_detailed`] additionally keeps the per-source
//! signals (`matched_by`, in-source ranks, cosine) that ADR-095 D4 requires;
//! [`SqliteStore::hybrid_search_full`] is a thin compatibility wrapper that
//! projects them back to `(id, fused_score)` for callers that do not need them.
//!
//! The fused score is RRF (`Σ w / (60 + rank + 1)`). RRF encodes rank position
//! only and is not comparable across queries — do not threshold on its absolute
//! value, and do not present it as a similarity.

use std::collections::HashMap;

use acowork_core::retrieval::{FusedHit, FusionPolicy, LexicalPlan, LexicalQuery, MatchedBy};

use crate::sqlite::{Result, SqliteStore};

/// RRF rank constant (ADR-082 D5).
pub const RRF_K: f64 = 60.0;

impl SqliteStore {
    /// The shared hybrid retrieval engine (ADR-095 D3), returning each
    /// candidate with its in-source signals.
    ///
    /// * `lexical` drives the BM25/FTS source; `embedding` drives the cosine
    ///   source. They are **separate inputs by design** (D2): the
    ///   `memory_recall` tool feeds the current user message to the embedding
    ///   and LLM keywords to BM25, while global search feeds the raw query and
    ///   the analyzer's terms. [`LexicalQuery`] owns the "analyzer produced
    ///   nothing" fallback so no call site has to restate it. Passing `None`
    ///   for `embedding` degrades to a text-only result, which is the
    ///   documented behavior when the embedding provider is unreachable.
    /// * `policy` carries the per-source weights, the vector-only cosine gate
    ///   and the pool depth. Weights are rank-fusion weights, not probabilities.
    ///
    /// Order is `fused_rank` ascending. Each hit's `fused_rank` is recomputed
    /// here, so a caller that merges across labels must re-rank afterwards
    /// (see [`MemoryAdminService::semantic_search`](crate::MemoryAdminService::semantic_search)).
    pub fn hybrid_search_detailed(
        &self,
        label: &str,
        lexical: &LexicalQuery<'_>,
        embedding: Option<&[f32]>,
        k: usize,
        policy: &FusionPolicy,
    ) -> Result<Vec<FusedHit>> {
        if k == 0 {
            return Ok(Vec::new());
        }
        // Both rankings are fetched deeper than the final `k`: RRF is a function
        // of rank, so truncating a source before fusion would bake that source's
        // own ordering into the fused result.
        let pool = k.saturating_mul(policy.pool_factor.max(1));

        // Vector source: cosine-ranked, then gated. The gate is applied here and
        // nowhere else, so a lexical hit can never be dropped for having a far
        // embedding (ADR-095 D3).
        let floor = f64::from(policy.min_cosine.unwrap_or(-1.0_f32));
        let mut vector_hits: Vec<(u64, f64)> = Vec::new();
        if let Some(embedding) = embedding.filter(|e| !e.is_empty()) {
            for (id, cos) in self.vector_search(label, embedding, pool)? {
                if cos >= floor {
                    vector_hits.push((id, cos));
                }
            }
        }

        let text_ids: Vec<u64> = match lexical.lexical_plan() {
            LexicalPlan::Skip => Vec::new(),
            LexicalPlan::Terms(terms) => self
                .text_search_terms(label, terms, pool)?
                .into_iter()
                .map(|(id, _)| id)
                .collect(),
            LexicalPlan::Raw(raw) => self
                .text_search(label, raw, pool)?
                .into_iter()
                .map(|(id, _)| id)
                .collect(),
        };

        // ponytail: `(0.0, 0.0)` means "unspecified", not "mute both sources" —
        // the legacy trait signature cannot express the latter, and equal weight
        // is what ADR-082 D5 asks for anyway. New callers pass a `FusionPolicy`
        // and are unaffected.
        let (text_weight, vector_weight) =
            if policy.text_weight == 0.0 && policy.vector_weight == 0.0 {
                (1.0, 1.0)
            } else {
                (policy.text_weight, policy.vector_weight)
            };

        // Ranks are the positions inside each source's own list, captured before
        // fusion so they survive the merge. `cosine` is kept only for ids that
        // actually came out of the vector source.
        let mut text_rank: HashMap<u64, usize> = HashMap::with_capacity(text_ids.len());
        for (rank, id) in text_ids.iter().enumerate() {
            text_rank.entry(*id).or_insert(rank);
        }
        let mut vector_rank: HashMap<u64, usize> = HashMap::with_capacity(vector_hits.len());
        let mut cosine: HashMap<u64, f64> = HashMap::with_capacity(vector_hits.len());
        for (rank, (id, cos)) in vector_hits.iter().enumerate() {
            vector_rank.entry(*id).or_insert(rank);
            cosine.entry(*id).or_insert(*cos);
        }

        let mut fused: HashMap<u64, f64> = HashMap::new();
        for (id, rank) in &text_rank {
            *fused.entry(*id).or_default() += text_weight / (RRF_K + *rank as f64 + 1.0);
        }
        for (id, rank) in &vector_rank {
            *fused.entry(*id).or_default() += vector_weight / (RRF_K + *rank as f64 + 1.0);
        }

        let mut ordered: Vec<u64> = fused.keys().copied().collect();
        // Node id as tie-break keeps the order stable across runs (HashMap
        // iteration order is not deterministic).
        ordered.sort_by(|a, b| {
            fused[b]
                .partial_cmp(&fused[a])
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.cmp(b))
        });
        ordered.truncate(k);

        Ok(ordered
            .into_iter()
            .enumerate()
            .map(|(fused_rank, id)| {
                let v = vector_rank.get(&id).copied();
                let t = text_rank.get(&id).copied();
                let matched_by = match (v, t) {
                    (Some(_), Some(_)) => MatchedBy::Both,
                    (Some(_), None) => MatchedBy::Vector,
                    (None, Some(_)) => MatchedBy::Text,
                    // Unreachable: an id only enters `fused` through one of the
                    // two rank maps.
                    (None, None) => MatchedBy::Text,
                };
                FusedHit {
                    id,
                    matched_by,
                    vector_rank: v,
                    text_rank: t,
                    cosine: cosine.get(&id).copied(),
                    fused_score: fused[&id],
                    fused_rank,
                }
            })
            .collect())
    }

    /// Hybrid search over `label`: vector + text, gated per source, fused by RRF.
    ///
    /// Compatibility wrapper over [`Self::hybrid_search_detailed`] kept so
    /// callers that do not need per-source signals keep compiling (ADR-095
    /// Phase 1–2 rollout). Use the `_detailed` form when the hit needs
    /// `matched_by` / in-source rank / cosine.
    ///
    /// * `text_weight` / `vector_weight` scale each source's RRF contribution.
    ///   `(0.0, 0.0)` means "unspecified" and falls back to equal weight.
    /// * `min_cosine` is an absolute cosine floor applied to the **vector source
    ///   only**; `None` disables it. Callers that care about recall quality
    ///   should pass `None` and rely on `k` as the quality knob, since
    ///   anisotropic embeddings make an absolute cosine floor unreliable.
    ///
    /// Results come back ordered by fused rank. The returned score is the
    /// RRF fused score (`Σ w / (60 + rank + 1)`) — *not* the cosine. RRF is
    /// a rank-position signal, so absolute values are not comparable across
    /// queries and must not be thresholded on.
    #[allow(clippy::too_many_arguments)]
    pub fn hybrid_search_full(
        &self,
        label: &str,
        query: &str,
        embedding: &[f32],
        k: usize,
        text_weight: f64,
        vector_weight: f64,
        min_cosine: Option<f32>,
    ) -> Result<Vec<(u64, f64)>> {
        let policy = FusionPolicy {
            vector_weight,
            text_weight,
            min_cosine,
            pool_factor: 2,
        };
        Ok(self
            .hybrid_search_detailed(label, &LexicalQuery::raw_only(query), Some(embedding), k, &policy)?
            .into_iter()
            .map(|h| (h.id, h.fused_score))
            .collect())
    }

    /// Hybrid search with equal weights and no cosine floor.
    ///
    /// `text_prop` / `vec_prop` exist for `GrafeoStore` API parity and are
    /// ignored: the SQLite schema indexes one text column (`content`) and one
    /// vector column per label (ADR-082 D2/D3).
    pub fn hybrid_search_filtered(
        &self,
        label: &str,
        _text_prop: &str,
        _vec_prop: &str,
        query: &str,
        embedding: &[f32],
        k: usize,
    ) -> Result<Vec<(u64, f64)>> {
        self.hybrid_search_full(label, query, embedding, k, 1.0, 1.0, None)
    }

    /// Text search with `GrafeoStore` API parity; the field selector is ignored
    /// because only `content` is indexed.
    pub fn text_search_with_filter(
        &self,
        label: &str,
        _field: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<(u64, f64)>> {
        self.text_search(label, query, k)
    }
}
