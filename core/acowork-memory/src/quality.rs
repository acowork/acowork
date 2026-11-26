//! MemoryQualityConfig — centralized memory quality parameters (ADR-062 D2).
//!
//! Before ADR-062, quality thresholds that affect memory behaviour were
//! scattered across the retrieval and write paths as hardcoded constants
//! (see ADR-062 §2.4). This module centralizes the "effective but hardcoded"
//! parameters so they can be tuned per agent (via the `.bot` manifest
//! `[memory.quality]` section) and calibrated from measured distributions
//! (ADR-062 §6.6 / M3.6).
//!
//! Invariant: every field's default MUST equal the current in-code behaviour
//! so that "zero configuration = current behaviour" (ADR-062 §4.2). Changing
//! a default here changes behaviour globally — only re-calibrate defaults
//! from real distribution data (M3.6).

use serde::{Deserialize, Serialize};


/// Dedup thresholds (ADR-062 §4.1).
///
/// Cosine-similarity above which two nodes are considered duplicates
/// (see the dedup path in `acowork-sqlite`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DedupQuality {
    /// Knowledge dedup threshold. Default `0.95`.
    pub knowledge_threshold: f32,
    /// Procedural dedup threshold (lower — procedures are more specific).
    /// Default `0.90`.
    pub procedure_threshold: f32,
}

impl Default for DedupQuality {
    fn default() -> Self {
        Self {
            knowledge_threshold: 0.95,
            procedure_threshold: 0.90,
        }
    }
}

/// Consolidation confidence gates (ADR-062 §4.1).
///
/// Covers the three separate confidence lines that existed as hardcoded
/// constants across the write path (ADR-062 §4.2): the instant-extraction
/// "directly Active" line (`instant.rs`), the offline "Pending → Active" and
/// "→ Dormant" lines (`offline.rs`), and the Pending-age gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConsolidationQuality {
    /// Instant extraction: confidence ≥ this → created directly Active.
    /// Default `0.85`.
    pub direct_active_threshold: f32,
    /// Offline consolidation: Pending + confidence ≥ this → upgraded to Active.
    /// Default `0.7`.
    pub pending_upgrade_threshold: f32,
    /// Experience generalization (ProceduralNode path): Pending → Active
    /// confidence line. Default `0.8`.
    ///
    /// ADR-062 §4.2: whether this should equal `pending_upgrade_threshold`
    /// (0.7) is an open question — different node types may legitimately use
    /// different confidence lines (procedural patterns cost more to get
    /// wrong). Parameterized separately first; unify only after M3.6
    /// calibration data decides.
    pub generalization_active_threshold: f32,
    /// Offline consolidation: Pending + confidence < this → marked Dormant.
    /// Default `0.3`.
    pub dormant_confidence: f32,
    /// Minimum age (hours) before a Pending node is eligible for offline
    /// consolidation. Default `1`.
    pub min_pending_age_hours: u64,
}

impl Default for ConsolidationQuality {
    fn default() -> Self {
        Self {
            direct_active_threshold: 0.85,
            pending_upgrade_threshold: 0.7,
            generalization_active_threshold: 0.8,
            dormant_confidence: 0.3,
            min_pending_age_hours: 1,
        }
    }
}

/// Centralized memory quality configuration (ADR-062 D2).
///
/// Embedded in [`crate::MemoryManagerConfig`] and pushed down to the
/// `MemoryProvider` via `MemoryProvider::apply_quality_config` so both the
/// memory layer (retrieval) and the storage engine (write path) read from a
/// single source of truth.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryQualityConfig {
    /// Exclude Dormant nodes from retrieval results (ADR-062 D1). Default `true`.
    pub exclude_dormant: bool,
    /// Minimum cosine similarity for a retrieval result to survive filtering
    /// (ADR-062 §4.2). Applied to the *absolute* cosine similarity recovered
    /// from the vector index (`cos = 1 - distance`), NOT to the fused RRF
    /// score: RRF scores encode rank position only, carry no relevance
    /// magnitude, and are not comparable across queries nor between a
    /// single-source and a dual-source result set. Default `0.3` (≈72°)
    /// drops clearly unrelated candidates while keeping literal /
    /// exact-match hits.
    ///
    /// ponytail: an absolute floor is provider-dependent — real sentence
    /// embeddings are anisotropic (unrelated pairs still sit at cos 0.5–0.9,
    /// measured on the live store: even `0.6` filtered nothing), while the
    /// procedural fallback embedding is near-orthogonal for any two texts
    /// (cos ≈ 0, so the floor drops everything). Upgrade path: score the
    /// query against a corpus-mean baseline (centered cosine / per-query
    /// z-score) instead of an absolute cut.
    pub min_cosine: f32,
    /// Dedup thresholds.
    pub dedup: DedupQuality,
    /// Consolidation confidence gates.
    pub consolidation: ConsolidationQuality,
    /// Whether sanitized `keywords` are folded into the BM25-indexed
    /// `object` field at write time (ADR-062 §6.2 Plan Y, M5 step 2b).
    /// Default `false` — per-agent opt-in via manifest
    /// `[memory.quality].keyword_index = true`. The keyword quality gate
    /// (ADR-062 §6.2.1) runs regardless of this toggle.
    pub keyword_index: bool,
}

impl Default for MemoryQualityConfig {
    fn default() -> Self {
        Self {
            exclude_dormant: true,
            min_cosine: 0.3,
            dedup: DedupQuality::default(),
            consolidation: ConsolidationQuality::default(),
            keyword_index: false,
        }
    }
}

/// Convert an agent manifest `[memory.quality]` section (ADR-062 D2) into the
/// memory-layer config, merging each present override onto the defaults.
///
/// "Zero configuration = current behaviour": a manifest with no quality section
/// (or a section where every field is `None`) yields [`MemoryQualityConfig::default`].
impl From<acowork_core::manifest::ManifestMemoryQuality> for MemoryQualityConfig {
    fn from(m: acowork_core::manifest::ManifestMemoryQuality) -> Self {
        let mut c = MemoryQualityConfig::default();
        if let Some(v) = m.exclude_dormant {
            c.exclude_dormant = v;
        }
        if let Some(v) = m.min_cosine {
            c.min_cosine = v;
        }
        if let Some(v) = m.keyword_index {
            c.keyword_index = v;
        }
        if let Some(d) = m.dedup {
            if let Some(v) = d.knowledge_threshold {
                c.dedup.knowledge_threshold = v;
            }
            if let Some(v) = d.procedure_threshold {
                c.dedup.procedure_threshold = v;
            }
        }
        if let Some(cf) = m.consolidation {
            if let Some(v) = cf.direct_active_threshold {
                c.consolidation.direct_active_threshold = v;
            }
            if let Some(v) = cf.pending_upgrade_threshold {
                c.consolidation.pending_upgrade_threshold = v;
            }
            if let Some(v) = cf.generalization_active_threshold {
                c.consolidation.generalization_active_threshold = v;
            }
            if let Some(v) = cf.dormant_confidence {
                c.consolidation.dormant_confidence = v;
            }
            if let Some(v) = cf.min_pending_age_hours {
                c.consolidation.min_pending_age_hours = v;
            }
        }
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::manifest::{
        ManifestConsolidationQuality, ManifestDedupQuality, ManifestMemoryQuality,
    };

    #[test]
    fn from_empty_manifest_is_default() {
        let q = MemoryQualityConfig::from(ManifestMemoryQuality::default());
        assert_eq!(q, MemoryQualityConfig::default());
    }

    #[test]
    fn from_manifest_merges_present_fields_only() {
        let m = ManifestMemoryQuality {
            exclude_dormant: Some(false),
            dedup: Some(ManifestDedupQuality {
                knowledge_threshold: Some(0.9),
                ..Default::default()
            }),
            consolidation: Some(ManifestConsolidationQuality {
                dormant_confidence: Some(0.2),
                ..Default::default()
            }),
            ..Default::default()
        };
        let q = MemoryQualityConfig::from(m);
        assert!(!q.exclude_dormant);
        assert_eq!(q.min_cosine, 0.3, "unspecified field keeps default");
        assert_eq!(q.dedup.knowledge_threshold, 0.9);
        assert_eq!(q.dedup.procedure_threshold, 0.90, "unspecified nested keeps default");
        assert_eq!(q.consolidation.dormant_confidence, 0.2);
        assert_eq!(q.consolidation.pending_upgrade_threshold, 0.7);
    }

    #[test]
    fn defaults_match_current_behavior() {
        // "Zero configuration = current behaviour" (ADR-062 §4.2) — every
        // default MUST mirror the pre-ADR-062 hardcoded constants.
        let q = MemoryQualityConfig::default();
        assert!(q.exclude_dormant, "D1 default on");
        assert_eq!(q.min_cosine, 0.3);
        assert_eq!(q.dedup.knowledge_threshold, 0.95);
        assert_eq!(q.dedup.procedure_threshold, 0.90);
        assert_eq!(q.consolidation.direct_active_threshold, 0.85);
        assert_eq!(q.consolidation.pending_upgrade_threshold, 0.7);
        assert_eq!(q.consolidation.generalization_active_threshold, 0.8);
        assert_eq!(q.consolidation.dormant_confidence, 0.3);
        assert_eq!(q.consolidation.min_pending_age_hours, 1);
        // ADR-062 M5: keyword fold is per-agent opt-in via manifest
        // `[memory.quality].keyword_index = true` — default stays false
        // (zero-config = M4 metadata-only behaviour).
        assert!(!q.keyword_index, "keyword_index defaults to off; opt-in per agent");
    }

    #[test]
    fn serde_roundtrip_with_missing_fields() {
        // Partial `[memory.quality]` TOML section: missing fields fall back
        // to defaults via `#[serde(default)]`.
        let json = serde_json::json!({ "exclude_dormant": false });
        let q: MemoryQualityConfig = serde_json::from_value(json).unwrap();
        assert!(!q.exclude_dormant);
        assert_eq!(q.min_cosine, 0.3, "unspecified field keeps default");
        assert_eq!(q.dedup.knowledge_threshold, 0.95);

        // Partial nested section also merges with defaults.
        let json = serde_json::json!({
            "dedup": { "knowledge_threshold": 0.5 }
        });
        let q: MemoryQualityConfig = serde_json::from_value(json).unwrap();
        assert_eq!(q.dedup.knowledge_threshold, 0.5);
        assert_eq!(
            q.dedup.procedure_threshold, 0.90,
            "unspecified nested field keeps default"
        );
    }
}
