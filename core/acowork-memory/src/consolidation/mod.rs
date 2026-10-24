//! Consolidation: the data structures *and* the episodic distiller
//! (ADR-068) of the memory system.
//!
//! These types define the data structures used by the consolidation pipeline:
//! - Instant extraction (memory_store tool calls)
//! - Offline consolidation (background upgrade of Pending nodes)
//! - Experience generalization (pattern extraction from repeated episodes)
//! - Triple extraction (LLM-driven knowledge extraction)
//! - Scheduling configuration
//!
//! [`distiller`] holds the promotion pipeline itself. It consumes
//! `dyn MemoryProvider`, so it lives with the trait rather than with any
//! single storage backend (relocated from `acowork-grafeo`, ADR-082 §4).
//!
//! All types use `u64` for node IDs (not grafeo_common::NodeId) to keep
//! this crate independent of the storage engine.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{AutobioCategory, ConflictSignal, KnowledgeSubType, NodeStatus, PrivacyLevel};

// ============================================================================
// Event-triggered promotion input (ADR-068 D8)
// ============================================================================

/// An event-triggered History milestone (ADR-068 §2.1 / §3.4.2 Step 3).
///
/// History is the one autobiographical category that is NOT promoted from an
/// episode cluster: per the two-axis matrix it is "event-triggered (no
/// episode input)". Milestones are asserted by the agent's runtime (specific
/// tool-call sequences, first successful deployment, major errors) and
/// delivered to the [`EpisodicDistiller::promote_event`] entry point.
///
/// A future `consolidation_event` MQTT topic (ADR-068 §3.4.3) is the
/// transport; this type is the in-process payload so the distiller stays
/// decoupled from the transport.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryMilestoneEvent {
    /// Stable milestone key, e.g. `"first_deployment"`. The promoted node
    /// key becomes `milestone_<slugified>` (ADR-068 Step 3 History row).
    pub key: String,
    /// Human-readable description, stored as the node `value`.
    pub value: String,
    /// When the milestone occurred.
    pub occurred_at: DateTime<Utc>,
    /// Confidence of the event source [0.0, 1.0].
    pub confidence: f32,
}

// ============================================================================
// Embedding function type alias
// ============================================================================

/// Shared embedding function type used across consolidation pipelines.
pub type EmbeddingFn = Arc<dyn for<'a> Fn(&'a str) -> Vec<f32> + Send + Sync>;

// ============================================================================
// LLM abstraction (migrated from grafeo::consolidation::triple_extraction)
// ============================================================================

/// A single message in the LLM conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    /// Role: "system", "user", or "assistant".
    pub role: String,
    /// Message content.
    pub content: String,
}

/// Response from the LLM abstraction.
#[derive(Debug, Clone, Default)]
pub struct LlmResponse {
    /// The text content of the assistant's reply.
    pub content: String,
    /// Token usage (if available).
    pub usage_tokens: Option<u64>,
    /// API finish reason (`"stop"`, `"length"`, `"tool_calls"`, ...), when the
    /// provider reports one. `Some("length")` means the reply was cut off by
    /// `max_tokens` — a JSON array in `content` is then guaranteed to be
    /// incomplete, which is a different diagnosis from "the model returned
    /// malformed JSON".
    pub finish_reason: Option<String>,
}

impl LlmResponse {
    /// Whether the provider reported that the reply hit the output token cap.
    pub fn truncated(&self) -> bool {
        self.finish_reason.as_deref() == Some("length")
    }
}

/// Trait for making LLM calls during triple extraction.
///
/// Implemented by the runtime layer using the active Provider.
/// This trait keeps the consolidation pipeline independent of
/// the provider ecosystem while still supporting LLM-driven consolidation.
#[async_trait::async_trait]
pub trait ConsolidationLlm: Send + Sync {
    /// Send a chat request and return the response text.
    async fn chat(&self, messages: Vec<LlmMessage>) -> std::result::Result<LlmResponse, String>;
}

// ============================================================================
// Instant extraction types (migrated from grafeo::consolidation::instant)
// ============================================================================

/// Input from LLM's `memory_store` tool call.
#[derive(Debug, Clone)]
pub struct MemoryStoreInput {
    /// Natural language content from LLM.
    pub content: String,
    /// Knowledge sub-type: Fact | Preference | Relation.
    ///
    /// Ignored when `autobiographical` is `Some` — autobiographical writes
    /// route to `AutobiographicalNode` regardless of `sub_type`.
    pub sub_type: KnowledgeSubType,
    /// Optional subject hint (defaults to "user").
    pub subject: Option<String>,
    /// Optional predicate hint.
    pub predicate: Option<String>,
    /// Optional object hint.
    pub object: Option<String>,
    /// LLM's confidence in this knowledge (default 0.7).
    pub confidence: Option<f32>,
    /// Source episode ID for traceability.
    pub source_episode_id: Option<u64>,
    /// Pre-computed embedding vector.
    pub embedding: Option<Vec<f32>>,
    /// Optional privacy level (design §7.1). Defaults to `Personal`.
    ///
    /// When `Some`, the pipeline stamps it on the created KnowledgeNode;
    /// when `None`, the conservative default `Personal` applies.
    pub privacy: Option<PrivacyLevel>,
    /// Optional importance score [0.0, 1.0] (design §3.1). Defaults to 0.5.
    pub importance: Option<f32>,
    /// Optional keywords provided by the LLM to aid retrieval (design §4.1).
    /// Persisted into node `metadata["keywords"]`.
    pub keywords: Option<Vec<String>>,
    /// Optional autobiographical path.
    ///
    /// When `Some`, the pipeline writes to `AutobiographicalNode` instead of
    /// `KnowledgeNode` / `ProceduralNode`. `sub_type`, `subject`, `predicate`,
    /// `object` are ignored in this case. Idempotent on `(aspect, key)` —
    /// re-emitting the same key updates the existing node in place.
    pub autobiographical: Option<AutobiographicalStoreInput>,
}

/// Subset of `MemoryStoreInput` that targets the autobiographical (self-
/// knowledge) layer.
///
/// The LLM populates this when the content is about *the Agent itself* —
///
/// identity, capabilities, limitations, self-preferences, milestones, or
/// long-term relationships — rather than about the user or the world. This
/// distinguishes "I tend to give conclusions first" (autobiographical
/// preference) from "user prefers concise replies" (knowledge preference).
#[derive(Debug, Clone)]
pub struct AutobiographicalStoreInput {
    /// Self-knowledge aspect (identity / capability / limitation /
    /// preference / history / relationship).
    pub aspect: AutobioCategory,
    /// Optional key for idempotent updates (e.g. "style", "name").
    ///
    /// When `None`, a stable key is derived from `aspect` + the first 8
    /// words of `content` (lower-cased, snake_cased).
    pub key: Option<String>,
    /// Provenance of this knowledge. Defaults to `"user_statement"`.
    ///
    /// Conventional values:
    /// - `"user_statement"` — user directly told the agent
    /// - `"important_event"` — significant interaction worth recording
    /// - `"self_evaluation"` — internal reflection (rare for instant path)
    pub source: Option<String>,
}

/// Action recommended by the conflict resolver.
///
/// Uses `u64` for node IDs to stay storage-engine-agnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictAction {
    /// Auto-resolve: new replaces old.
    AutoReplace {
        /// The existing node to be superseded.
        old_node_id: u64,
        /// New status for the old node (typically Dormant).
        new_status: NodeStatus,
    },
    /// Both kept, marked for user confirmation.
    MarkAmbiguous {
        /// Shared conflict group identifier.
        conflict_group_id: String,
    },
    /// Defer to LLM offline arbitration.
    DeferToLLM,
}

/// Detailed record of a single conflict resolution action.
#[derive(Debug, Clone)]
pub struct ConflictResolutionDetail {
    /// The existing node involved in the conflict.
    pub existing_node_id: u64,
    /// The resolution action taken.
    pub action: ConflictAction,
    /// The conflict signal that triggered the resolution.
    pub signal: ConflictSignal,
}

/// Result of processing a `memory_store` tool call.
#[derive(Debug, Clone)]
pub struct MemoryStoreResult {
    /// The ID of the newly created (or updated) knowledge node.
    pub node_id: u64,
    /// Detailed conflict resolution records.
    pub conflict_resolutions: Vec<ConflictResolutionDetail>,
}

// ============================================================================
// Generalization types (migrated from grafeo::consolidation::generalization)
// ============================================================================

/// Category of a behavior pattern - used for grouping and dedup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PatternCategory {
    /// Tool usage pattern (e.g., "use http_request for weather lookups").
    ToolUsage,
    /// User preference pattern (e.g., "user prefers concise output").
    UserPreference,
    /// Workflow pattern (e.g., "when asked for a report, first gather data, then format").
    Workflow,
    /// Error recovery pattern (e.g., "on API timeout, retry once").
    ErrorRecovery,
}

impl PatternCategory {
    /// Returns the string representation used in ProceduralNode metadata.
    pub fn as_str(&self) -> &'static str {
        match self {
            PatternCategory::ToolUsage => "ToolUsage",
            PatternCategory::UserPreference => "UserPreference",
            PatternCategory::Workflow => "Workflow",
            PatternCategory::ErrorRecovery => "ErrorRecovery",
        }
    }
}

impl std::str::FromStr for PatternCategory {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "ToolUsage" => Ok(PatternCategory::ToolUsage),
            "UserPreference" => Ok(PatternCategory::UserPreference),
            "Workflow" => Ok(PatternCategory::Workflow),
            "ErrorRecovery" => Ok(PatternCategory::ErrorRecovery),
            _ => Err(format!("unknown PatternCategory: {s}")),
        }
    }
}

/// A detected behavior pattern from episodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BehaviorPattern {
    /// Human-readable name for the pattern.
    pub name: String,
    /// Trigger condition description.
    pub trigger_condition: String,
    /// Action pattern description.
    pub action_pattern: String,
    /// Number of episodes this pattern was observed in.
    pub observation_count: usize,
    /// Confidence in the pattern [0.0, 1.0].
    pub confidence: f32,
    /// Pattern category (for grouping and dedup).
    #[serde(default = "default_pattern_category")]
    pub category: PatternCategory,
}

fn default_pattern_category() -> PatternCategory {
    PatternCategory::ToolUsage
}

/// Result of the generalization process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralizationResult {
    /// Detected patterns.
    pub patterns: Vec<BehaviorPattern>,
    /// Number of new ProceduralNodes created.
    pub nodes_created: usize,
    /// Number of existing ProceduralNodes boosted (confidence incremented).
    pub nodes_boosted: usize,
    /// Number of patterns deduplicated against existing nodes.
    pub patterns_deduplicated: usize,
    /// Timestamp of the generalization.
    pub generalized_at: DateTime<Utc>,
}

/// Configuration for the experience generalization process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralizationConfig {
    /// Minimum number of observations before a pattern is considered valid.
    /// Default: 3.
    pub min_observations: usize,
    /// Maximum number of unconsolidated episodes to scan per run.
    /// Default: 100.
    pub max_episodes_scan: usize,
    /// Confidence boost applied when a pattern reinforces an existing node.
    /// Default: 0.05.
    pub confidence_boost: f32,
    /// Maximum confidence for a ProceduralNode (cap after boosting).
    /// Default: 0.98.
    pub max_confidence: f32,
    /// Whether to use LLM for pattern discovery when available.
    /// Default: true.
    pub use_llm: bool,
}

impl Default for GeneralizationConfig {
    fn default() -> Self {
        Self {
            min_observations: 3,
            max_episodes_scan: 100,
            confidence_boost: 0.05,
            max_confidence: 0.98,
            use_llm: true,
        }
    }
}

// ============================================================================
// Offline consolidation types (migrated from grafeo::consolidation::offline)
// ============================================================================

/// Offline consolidation configuration.
#[derive(Debug, Clone)]
pub struct OfflineConsolidationConfig {
    /// Maximum number of pending nodes to process per batch.
    /// Default: 50.
    pub batch_size: usize,
    /// Minimum age (in hours) before a Pending node is eligible for
    /// offline processing. Default: 1.
    pub min_pending_age_hours: u64,
}

impl Default for OfflineConsolidationConfig {
    fn default() -> Self {
        Self {
            batch_size: 50,
            min_pending_age_hours: 1,
        }
    }
}

// ============================================================================
// Scheduler configuration (migrated from grafeo::consolidation::scheduler)
// ============================================================================

/// Configuration for the consolidation scheduler.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Idle timeout in seconds before automatic consolidation.
    /// Default: 1800 (30 minutes).
    pub idle_timeout_secs: u64,
    /// Minimum number of pending nodes before triggering consolidation.
    /// Default: 50.
    pub accumulation_threshold: usize,
    /// Batch size per consolidation run.
    /// Default: 50 (inherited from OfflineConsolidationConfig).
    pub batch_size: usize,
    /// Minimum age (in hours) before a Pending node is eligible.
    /// Default: 1 (inherited from OfflineConsolidationConfig).
    pub min_pending_age_hours: u64,
    /// Enable the EpisodicDistiller step in the background consolidation
    /// loop (ADR-068 M4). Default: false (OFF — opt-in).
    ///
    /// ADR-068 review revision: the distiller used to default to ON (M7),
    /// but the shipped decision is opt-in — the runtime resolves the real
    /// switch from the per-agent manifest `[memory.distiller].enabled`
    /// (`MemoryConfig::distiller_enabled`), defaulting to `false` when the
    /// section is absent. `SchedulerConfig::default()` mirrors that: OFF.
    pub distiller_enabled: bool,
    /// Distiller parameters used when `distiller_enabled` is true.
    /// `None` = use [`DistillerConfig::default`] (ADR-068 §3.4.1 defaults).
    pub distiller_config: Option<DistillerConfig>,
    /// Distiller auto-trigger interval in seconds (ADR-071 D1).
    /// The distiller may only fire when at least this much time has passed
    /// since the previous run (`last_distill_at`). Default: 3600 (1h).
    pub distiller_interval_secs: u64,
    /// Minimum unconsolidated-episode backlog required for the distiller to
    /// fire at an interval point (ADR-071 D1). Default: 50.
    pub distiller_accumulation: usize,
    /// Idle threshold (seconds) that — combined with a non-empty
    /// unconsolidated backlog — allows the distiller to fire at an interval
    /// point even when the backlog is below [`Self::distiller_accumulation`]
    /// (ADR-071 D1). Default: 1800 (30 min).
    pub distiller_idle_secs: u64,
    /// Enable episodic memory forgetting (pure time decay). Default: false
    /// (OFF — opt-in). When enabled, the background loop runs
    /// `run_episodic_decay_scan` at most once per `forgetting_interval_secs`.
    pub forgetting_enabled: bool,
    /// Episodic decay half-life in days (default 180).
    pub forgetting_half_life_days: u64,
    /// Retention threshold below which an episodic node becomes Dormant
    /// (default 0.1).
    pub forgetting_dormant_threshold: f32,
    /// Days a Dormant node is retained before archiving to the PurgeLog
    /// (default 90).
    pub forgetting_archive_days: u64,
    /// Minimum interval between episodic decay scans (seconds, default 3600).
    /// Guards against a full-table scan every poll tick.
    pub forgetting_interval_secs: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: 1800,
            accumulation_threshold: 50,
            batch_size: 50,
            min_pending_age_hours: 1,
            distiller_enabled: false,
            distiller_config: None,
            distiller_interval_secs: 3600,
            distiller_accumulation: 50,
            distiller_idle_secs: 1800,
            forgetting_enabled: crate::types::EpisodicDecayConfig::default().enabled,
            forgetting_half_life_days: crate::types::EpisodicDecayConfig::default().half_life_days,
            forgetting_dormant_threshold: crate::types::EpisodicDecayConfig::default()
                .dormant_threshold,
            forgetting_archive_days: crate::types::EpisodicDecayConfig::default().archive_days,
            forgetting_interval_secs: 3600,
        }
    }
}

// ============================================================================
// EpisodicDistiller types (ADR-068)
// ============================================================================
//
// Two-axis orthogonalization: LLM writes ONLY to the Episodic layer (with a
// `knowledge_subtype` classification). The semantic layer (Knowledge /
// Procedural / Autobiographical nodes) is produced exclusively by the offline
// `EpisodicDistiller`, driven by these shared types.
//
// Design: ADR-068 §3.4, §9.2. All types use `u64` for node IDs (not
// grafeo_common::NodeId) to keep this crate independent of the storage engine.

/// Autobiographical aspect recognized by the server-side LLM (ADR-068 Step 2a).
///
/// A subset of [`AutobioCategory`] — the aspects the distiller may promote
/// from Episodic evidence. `Identity`/`Capability` remain manifest-bootstrap
/// only (ADR-068 §2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AutobioAspect {
    /// Feedback about the agent's capability boundary.
    Limitation,
    /// Feedback about the agent's style/behavior (self-preference).
    Preference,
    /// Feedback about the agent's relationship with the user.
    Relationship,
    /// Significant events in the agent's trajectory.
    History,
}

impl AutobioAspect {
    pub fn as_str(&self) -> &'static str {
        match self {
            AutobioAspect::Limitation => "limitation",
            AutobioAspect::Preference => "preference",
            AutobioAspect::Relationship => "relationship",
            AutobioAspect::History => "history",
        }
    }
}

impl std::str::FromStr for AutobioAspect {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "limitation" => Ok(AutobioAspect::Limitation),
            "preference" => Ok(AutobioAspect::Preference),
            "relationship" => Ok(AutobioAspect::Relationship),
            "history" => Ok(AutobioAspect::History),
            _ => Err(format!("unknown AutobioAspect: {s}")),
        }
    }
}


/// What kind of semantic node a promotion produced (audit trail).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromotionKind {
    /// `KnowledgeNode{sub_type=Fact}`
    Fact,
    /// `KnowledgeNode{sub_type=Preference}`
    Preference,
    /// `KnowledgeNode{sub_type=Relation}`
    Relation,
    /// `ProceduralNode`
    Procedure,
    /// `AutobiographicalNode{category=Relationship}`
    AutobioRelationship,
    /// `AutobiographicalNode{category=History}`
    AutobioHistory,
}

/// Promotion outcome for a candidate cluster (ADR-068 Step 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromotionDecision {
    /// Node created and episodes marked consolidated.
    Promoted,
    /// Cluster will never be promoted (e.g. LLM judge rejected it).
    Skipped { reason: String },
    /// Not enough evidence yet — retry on a future distillation run.
    Deferred { reason: String },
}

/// Promotion provenance stamped on promoted semantic nodes (ADR-068 §3.6/§9.3).
///
/// Attached to Knowledge/Procedural/Autobiographical nodes created by the
/// `EpisodicDistiller` (or manifest bootstrap) — the audit trail connecting a
/// semantic node back to its episodic evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotionMetadata {
    /// When the promotion happened.
    pub promoted_at: DateTime<Utc>,
    /// Who/what performed the promotion:
    /// `"episodic_distiller"` | `"manifest_bootstrap"`.
    pub promoted_by: String,
    /// Episode node ids that formed the evidence (synonym of the node's
    /// `source_episode_ids` — duplicated for index-friendly retrieval).
    pub evidence_episode_ids: Vec<u64>,
    /// Time span in days between oldest and newest evidence episode.
    pub evidence_span_days: i64,
    /// LLM judge confidence [0.0, 1.0].
    pub llm_judge_confidence: f32,
    /// LLM judge's explanation of why promotion was warranted.
    pub llm_judge_reasoning: String,
}

/// One auditable promotion decision for one episode (ADR-068 §3.4.1).
#[derive(Debug, Clone, PartialEq)]
pub struct PromotionEvaluation {
    /// Episode node ids that form the evidence for this decision.
    pub source_episode_ids: Vec<u64>,
    /// Which semantic node kind this decision produced.
    pub promoted_kind: PromotionKind,
    /// Storage id of the written node (None when the episode was not
    /// consolidated — Deferred / below the importance floor).
    pub promoted_node_id: Option<u64>,
    /// LLM's stated reason (full audit trail). Empty on the zero-LLM
    /// projection path, which by definition has no reason to explain.
    pub llm_reasoning: String,
    /// LLM confidence [0.0, 1.0]. 1.0 on paths that never asked the LLM.
    pub llm_confidence: f32,
    /// Final outcome.
    pub decision: PromotionDecision,
}

/// Configuration for one `EpisodicDistiller` run (ADR-068 §3.4.1).
///
/// The distiller is *projection + merge*: it restates each episode as one
/// canonical sentence and asks the LLM only when an existing node looks like
/// the same fact. Every former evidence gate
/// (`fact_min_evidence`, `preference_min_evidence`, `procedure_min_evidence`,
/// `relation_min_evidence`, `autobio_min_evidence`, `autobio_min_span_days`,
/// `promotion_confidence_threshold`, `cluster_threshold`, `max_cluster_size`)
/// is gone: the writing LLM already judged an episode worth remembering, and a
/// second, weaker judgement on the same text only loses memories — see
/// `docs/plan/zh/memory-consolidation-normalization-plan.md` §2.
#[derive(Debug, Clone)]
pub struct DistillerConfig {
    /// Max episodes processed per distillation run. Default: 100.
    pub batch_size: usize,
    /// Existing semantic nodes recalled as merge candidates for one episode.
    /// The LLM sees the new statement plus these `k`, so the reply stays one
    /// small JSON object regardless of store size. Default: 5.
    pub merge_candidate_k: usize,
    /// Cosine similarity above which an existing node is offered to the LLM as
    /// a merge candidate. Deliberately loose: a false candidate costs one
    /// "distinct" verdict, a missed candidate costs a duplicate node that no
    /// later run will ever reunite. Default: 0.65.
    pub merge_recall_threshold: f32,
    /// Floor below which an episode is not projected at all. The single
    /// remaining volume-control knob, replacing the five evidence gates: if
    /// the semantic layer gets too dense this tightens in one place, and
    /// raising it back to 0.0 loses nothing (unprojected episodes stay in the
    /// episodic layer and are reconsidered every run — there is no tombstone).
    /// Default: 0.0 (project everything).
    pub min_importance: f32,
    /// Per-agent override for the merge system prompt (ADR-071 D7). `None` =
    /// the built-in `MERGE_SYSTEM_PROMPT` in `distiller` is used. Sourced from
    /// the package-level `prompts/distiller-merge.md` override.
    ///
    /// ADR-071 D7 defined two override slots (extraction + judge); the
    /// projection rewrite collapsed both into the single merge call, so one
    /// slot remains.
    pub merge_prompt_override: Option<String>,
}

impl Default for DistillerConfig {
    fn default() -> Self {
        Self {
            batch_size: 100,
            merge_candidate_k: 5,
            merge_recall_threshold: 0.65,
            min_importance: 0.0,
            merge_prompt_override: None,
        }
    }
}

/// Result of one distillation run with a full audit trail (ADR-068 §3.4.1).
#[derive(Debug, Clone, Default)]
pub struct DistillerResult {
    /// Episodes scanned in Step 1.
    pub episodes_scanned: usize,
    /// Knowledge nodes promoted (Fact).
    pub facts_promoted: usize,
    /// Knowledge nodes promoted (Preference).
    pub preferences_promoted: usize,
    /// Knowledge nodes promoted (Relation).
    pub relations_promoted: usize,
    /// Procedural nodes promoted.
    pub procedures_promoted: usize,
    /// Autobiographical nodes promoted (all aspects combined).
    pub autobio_promoted: usize,
    /// Episodes marked `consolidated = true` after promotion.
    pub episodes_marked_consolidated: usize,
    /// One entry per scanned episode — the audit trail.
    pub promotion_evaluations: Vec<PromotionEvaluation>,
    /// Stage-by-stage counters for this run. The promoted/deferred tallies
    /// above say what came out; the funnel says *where the episodes went*.
    /// Without it a run that silently produced nothing is indistinguishable
    /// from a run that had nothing to do.
    pub funnel: ConsolidationFunnel,
}

/// Per-run funnel counters (ADR-068 §3.4.2 observability).
///
/// Each stage counts episodes *entering* it, so a drop between two adjacent
/// stages localises where the pipeline lost them. This exists because
/// consolidation's failure modes are all silent: the original incident produced
/// 29 scheduler triggers, 24 failed LLM calls and zero user-visible memories,
/// with nothing in the API to distinguish it from an idle agent.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ConsolidationFunnel {
    /// Unconsolidated episodes read from the store.
    pub scanned: usize,
    /// Left out because the run hit [`DistillerConfig::batch_size`] — retried
    /// next run. Distinguishes "working through a queue" from "stuck".
    pub backlog_remaining: usize,
    /// Episodes below [`DistillerConfig::min_importance`] — left in the
    /// episodic layer, reconsidered every run (never tombstoned).
    pub below_importance: usize,
    /// Merge candidates recalled for an episode. Non-zero with `llm_calls` at
    /// zero means episodes are being projected without ever being deduplicated.
    pub candidates_recalled: usize,
    /// Episodes that took the zero-LLM projection path (no candidate at all).
    /// The share of the run that consolidation costs nothing in.
    pub projected: usize,
    /// LLM merge calls made — at most one per episode that had a candidate.
    pub llm_calls: usize,
    /// Merge calls whose reply could not be used (transport error, unparseable
    /// JSON, or truncated output). The episode is deferred, not lost.
    pub llm_calls_failed: usize,
    /// Of the failed calls, how many the provider reported as truncated
    /// (`finish_reason == "length"`). Separates "ask for less" from "the model
    /// is broken".
    pub llm_calls_truncated: usize,
    /// `no_merge` verdicts: the candidate list was a false positive. A high
    /// count relative to `llm_calls` says `merge_recall_threshold` is too loose.
    pub verdict_no_merge: usize,
    /// `merge` verdicts: the episode folded into an existing node.
    pub verdict_merged: usize,
    /// `contradicts` verdicts: the episode superseded an existing node.
    pub verdict_superseded: usize,
    /// Episodes marked `consolidated = true`.
    pub episodes_consolidated: usize,
    /// Episodes still eligible next run (LLM unavailable or failed, write
    /// error). The absence of a tombstone is the point: nothing here is lost.
    pub episodes_deferred: usize,
    /// Per-episode errors. One episode failing does not abort the run, so
    /// these are the only errors a distillation pass can carry.
    pub errors: Vec<String>,
}

impl ConsolidationFunnel {
    /// One-line summary for the `tracing` log. Ordered along the pipeline so
    /// the first number that goes to zero is the stage that broke.
    pub fn summary(&self) -> String {
        format!(
            "scanned={} backlog_left={} below_importance={} recalled={} projected={}              llm_calls={} llm_failed={} (truncated={})              no_merge={} merged={} superseded={} consolidated={} deferred={} errors={}",
            self.scanned,
            self.backlog_remaining,
            self.below_importance,
            self.candidates_recalled,
            self.projected,
            self.llm_calls,
            self.llm_calls_failed,
            self.llm_calls_truncated,
            self.verdict_no_merge,
            self.verdict_merged,
            self.verdict_superseded,
            self.episodes_consolidated,
            self.episodes_deferred,
            self.errors.len(),
        )
    }
}

// ============================================================================
// Promotion pipeline (ADR-068)
// ============================================================================

mod distiller;

pub use distiller::{DefaultEpisodicDistiller, EpisodicDistiller};
