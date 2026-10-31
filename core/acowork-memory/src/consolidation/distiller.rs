//! EpisodicDistiller - projection + merge consolidation (ADR-068).
//!
//! Two-axis orthogonalization (ADR-068): the LLM writes ONLY to the Episodic
//! layer (via the `memory_store` tool, tagging episodes with a
//! `knowledge_subtype` and an optional `normalized` restatement). The semantic
//! layer (Knowledge / Procedural nodes) is produced EXCLUSIVELY by this
//! offline distiller.
//!
//! ```text
//! Step 1  scan unconsolidated episodes with a knowledge_subtype (newest first)
//! Step 2  per episode:
//!           project  text = normalized.unwrap_or(content)
//!           recall   vector neighbours above `merge_recall_threshold`
//!           no candidates  -> write the node            (zero LLM calls)
//!           candidates     -> ask the LLM once:
//!                              no_merge    -> write a new node
//!                              merge       -> fold into the existing node
//!                              contradicts -> new statement wins, old retired
//! Step 3  mark the episode consolidated
//! Step 4  emit DistillerResult with a per-stage funnel
//! ```
//!
//! There is deliberately no evidence gate, no confidence threshold, no
//! clustering pass and no skip tombstone. The writing LLM already decided each
//! episode was worth remembering; re-judging that offline, with less context
//! and a weaker prompt, only loses memories. Volume is controlled by
//! `DistillerConfig::min_importance` and by decay, not by promotion gates -
//! see `docs/plan/zh/memory-consolidation-normalization-plan.md` section 2.
//!
//! This module is the only producer of semantic-layer nodes (ADR-068 R3).
//! It is deliberately decoupled from the runtime: it consumes
//! `dyn MemoryProvider` and `dyn ConsolidationLlm` (both from
//! `acowork_memory`), plus an optional `EmbeddingFn`.

use std::collections::HashMap;

use crate::consolidation::{
    ConsolidationLlm, DistillerConfig, DistillerResult, EmbeddingFn, HistoryMilestoneEvent,
    LlmMessage, PromotionDecision, PromotionEvaluation, PromotionKind, PromotionMetadata,
};
use crate::types::{
    labels, AutobioCategory, AutobiographicalNode, Episode, KnowledgeNode, KnowledgeSubType,
    NodeStatus, PrivacyLevel, ProceduralNode,
};
use crate::MemoryProvider;
use chrono::Utc;
use serde::Deserialize;

use acowork_core::error::{AcoworkError, Result};

// ============================================================================
// Trait
// ============================================================================

/// Offline distiller that promotes classified Episodes to semantic nodes.
///
/// ADR-068 section 3.4: `run` performs one consolidation pass over
/// unconsolidated episodes and returns a [`DistillerResult`] with a full audit
/// trail.
#[async_trait::async_trait]
pub trait EpisodicDistiller: Send + Sync {
    /// Run one consolidation pass.
    ///
    /// * `provider` - the memory backend (read candidates, write/update
    ///   semantic nodes, mark episodes consolidated).
    /// * `llm` - server-side LLM for the merge decision. When `None`, episodes
    ///   with no recalled candidate still consolidate (projection needs no
    ///   LLM); episodes *with* a candidate are deferred to a later run rather
    ///   than guessed into a merge or a duplicate.
    /// * `embedding_fn` - text embedding for candidate recall. When `None`,
    ///   nothing can be recalled, so every episode projects as a new node.
    /// * `config` - scan window, recall width/threshold, importance floor.
    async fn run(
        &self,
        provider: &dyn MemoryProvider,
        llm: Option<&dyn ConsolidationLlm>,
        embedding_fn: Option<&EmbeddingFn>,
        config: &DistillerConfig,
    ) -> Result<DistillerResult>;

    /// 30-day collaboration-span Relationship promotion (ADR-068 M8).
    ///
    /// Relationship is a *runtime-observed* autobiographical category: it is
    /// not bootstrapped from the manifest and not produced by the legacy
    /// offline consolidation. This method is the single producer — it runs
    /// rule-based (no LLM judge) against the provider's collaboration span:
    ///
    /// * no episodes yet, or span < 30 days → `Ok(None)` (not yet eligible);
    /// * a Relationship node already exists → `Ok(None)` (idempotent);
    /// * otherwise creates `AutobiographicalNode{category=Relationship,
    ///   key="collaboration_span"}` and returns its audit evaluation.
    ///
    /// ADR-068 M8 moved the old `auto_generate_relationship_nodes` offline
    /// step here so the category has one producer with a full audit trail.
    async fn promote_autobio_relationship(
        &self,
        provider: &dyn MemoryProvider,
    ) -> Result<Option<PromotionEvaluation>> {
        let Some(span) = provider.collaboration_span()? else {
            return Ok(None);
        };
        let span_days = (Utc::now() - span.earliest_episode_at).num_days();
        if span_days < RELATIONSHIP_MIN_SPAN_DAYS {
            return Ok(None);
        }
        // Idempotency: Relationship nodes are created once per collaboration.
        let existing = provider
            .find_autobiographical_by_category(AutobioCategory::Relationship)
            ?;
        if !existing.is_empty() {
            return Ok(None);
        }

        let now = Utc::now();
        let value = format!(
            "collaborated {} days ({} episodes recorded)",
            span_days, span.episode_count
        );
        let node = AutobiographicalNode {
            id: None,
            category: AutobioCategory::Relationship,
            key: "collaboration_span".to_string(),
            value,
            confidence: 0.9,
            source_episode_id: None,
            source_episode_ids: Vec::new(),
            promotion_metadata: Some(PromotionMetadata {
                promoted_at: now,
                promoted_by: "episodic_distiller".to_string(),
                evidence_episode_ids: Vec::new(),
                evidence_span_days: span_days,
                llm_judge_confidence: 1.0,
                llm_judge_reasoning: format!(
                    "collaboration span {span_days}d >= {}d (ADR-068 M8 rule)",
                    RELATIONSHIP_MIN_SPAN_DAYS
                ),
            }),
            embedding: None,
            status: NodeStatus::Active,
            created_at: now,
            updated_at: now,
            // Derived from collaboration episodes — internal derivation.
            source: "self_evaluation".to_string(),
            metadata: std::collections::HashMap::new(),
        };
        let node_id = provider.store_autobiographical(&node)?;
        Ok(Some(PromotionEvaluation {
            source_episode_ids: Vec::new(),
            promoted_kind: PromotionKind::AutobioRelationship,
            promoted_node_id: Some(node_id),
            llm_reasoning: "collaboration span rule (ADR-068 M8)".to_string(),
            llm_confidence: 1.0,
            decision: PromotionDecision::Promoted,
        }))
    }
    /// Promote one event-triggered History milestone (ADR-068 D8).
    ///
    /// History milestones are NOT episode-clustered — the event itself is the
    /// evidence. This entry point consumes an in-process
    /// [`HistoryMilestoneEvent`] (the future `consolidation_event` MQTT topic
    /// is the transport; the distiller stays transport-agnostic).
    ///
    /// Idempotent: an existing `category=History` node with the same
    /// `milestone_<slug>` key suppresses re-promotion. Returns `None` in that
    /// case, otherwise creates the node and returns its audit evaluation.
    async fn promote_event(
        &self,
        event: &HistoryMilestoneEvent,
        provider: &dyn MemoryProvider,
    ) -> Result<Option<PromotionEvaluation>> {
        let slug = slugify_milestone_key(&event.key);
        let node_key = format!("milestone_{slug}");
        // Idempotency: one node per milestone.
        if provider
            .find_autobiographical_by_key(&node_key)
            ?
            .is_some()
        {
            return Ok(None);
        }

        let now = Utc::now();
        let node = AutobiographicalNode {
            id: None,
            category: AutobioCategory::History,
            key: node_key,
            value: event.value.clone(),
            confidence: event.confidence,
            source_episode_id: None,
            source_episode_ids: Vec::new(),
            promotion_metadata: Some(PromotionMetadata {
                promoted_at: now,
                promoted_by: "episodic_distiller".to_string(),
                evidence_episode_ids: Vec::new(),
                evidence_span_days: 0,
                llm_judge_confidence: event.confidence,
                llm_judge_reasoning: format!(
                    "event-triggered History milestone '{}' occurred at {} (ADR-068 D8)",
                    event.key,
                    event.occurred_at.to_rfc3339()
                ),
            }),
            embedding: None,
            status: NodeStatus::Active,
            created_at: now,
            updated_at: now,
            // External milestone assertion — not a user statement.
            source: "important_event".to_string(),
            metadata: std::collections::HashMap::new(),
        };
        let node_id = provider.store_autobiographical(&node)?;
        Ok(Some(PromotionEvaluation {
            source_episode_ids: Vec::new(),
            promoted_kind: PromotionKind::AutobioHistory,
            promoted_node_id: Some(node_id),
            llm_reasoning: format!("event-triggered milestone '{}'", event.key),
            llm_confidence: event.confidence,
            decision: PromotionDecision::Promoted,
        }))
    }
}

/// Minimum collaboration span (days) before a Relationship node is promoted.
/// Per design §3.3: collaboration > 30 days → Relationship node.
const RELATIONSHIP_MIN_SPAN_DAYS: i64 = 30;

/// Slugify a milestone key for the History node key (`milestone_<slug>`).
fn slugify_milestone_key(key: &str) -> String {
    let slug: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = slug.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "event".to_string()
    } else {
        trimmed
    }
}

// ============================================================================
// Default implementation
// ============================================================================

/// Reference implementation of [`EpisodicDistiller`].
///
/// Stateless and cheap to clone — holds no per-run state.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultEpisodicDistiller;

// ---------------------------------------------------------------------------
// Merge verdict - the distiller's only LLM touchpoint
// ---------------------------------------------------------------------------

/// System prompt for the single merge decision. Deliberately short: it sees one
/// new statement and a handful of neighbours, not a batch.
const MERGE_SYSTEM_PROMPT: &str = r#"You are consolidating one new memory statement against the existing statements that look most similar to it.

Decide exactly one action:
- "merge": an existing statement says the same thing as the new one. Fold them into one sentence.
- "contradicts": an existing statement is outdated or wrong given the new one. The new statement wins.
- "no_merge": the new statement is genuinely different from every candidate.

Rules:
- Never state a fact that appears in neither the new statement nor the candidate you acted on.
- "statement" is required for merge and contradicts: one sentence, third person, free of session-specific detail, and true whenever it is later read.
- "target_id" is required for merge and contradicts, and must be one of the candidate ids given.
- When unsure whether two statements are the same fact, answer "no_merge". Keeping two nodes is recoverable; merging two different facts is not.

Reply with a single JSON object and nothing else:
{"action":"merge"|"contradicts"|"no_merge","target_id":<candidate id or null>,"statement":"<one sentence or null>","reasoning":"<one short clause>"}
"#;

/// What the LLM decided about one new statement against its neighbours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MergeAction {
    /// No candidate states the same thing - store as a new node.
    NoMerge,
    /// A candidate states the same thing - fold into it.
    Merge,
    /// A candidate is outdated or wrong - the new statement wins.
    Contradicts,
}

#[derive(Debug, Clone, Deserialize)]
struct MergeVerdict {
    action: MergeAction,
    #[serde(default)]
    target_id: Option<u64>,
    #[serde(default)]
    statement: Option<String>,
    #[serde(default)]
    reasoning: String,
}

/// One existing statement recalled as a possible merge target.
#[derive(Debug, Clone)]
struct Candidate {
    id: u64,
    text: String,
}

// ---------------------------------------------------------------------------
// Run orchestration
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl EpisodicDistiller for DefaultEpisodicDistiller {
    async fn run(
        &self,
        provider: &dyn MemoryProvider,
        llm: Option<&dyn ConsolidationLlm>,
        embedding_fn: Option<&EmbeddingFn>,
        config: &DistillerConfig,
    ) -> Result<DistillerResult> {
        let mut result = DistillerResult::default();

        // ---- Step 1: scan unconsolidated episodes -------------------------
        // ADR-068 3.4.2: only episodes with knowledge_subtype.is_some()
        // participate. Pure dialogue fragments (subtype == None) stay in the
        // episodic layer forever.
        let raw = provider.get_episodes_by_subtype(None, config.batch_size)?;
        let candidates: Vec<(u64, Episode)> = raw
            .into_iter()
            .filter(|(_, ep)| ep.knowledge_subtype.is_some())
            .collect();
        result.episodes_scanned = candidates.len();
        result.funnel.scanned = candidates.len();
        result.funnel.backlog_remaining = provider
            .count_unconsolidated_episodes()
            .unwrap_or(0)
            .saturating_sub(candidates.len());

        // ---- Step 2: consolidate one episode at a time --------------------
        // Per-episode, never per-batch: one episode's LLM call or write
        // failing must cost exactly that episode, which stays unconsolidated
        // and is retried next run. This is the structural fix for the incident
        // where a single truncated reply discarded a batch of 100.
        for (episode_id, episode) in &candidates {
            match consolidate_one(provider, llm, embedding_fn, config, *episode_id, episode).await
            {
                Ok(outcome) => outcome.record(&mut result),
                Err(e) => {
                    tracing::warn!(
                        episode_id,
                        error = %e,
                        "distiller: episode consolidation failed, deferring"
                    );
                    result.funnel.episodes_deferred += 1;
                    result.funnel.errors.push(format!("episode {episode_id}: {e}"));
                }
            }
        }

        Ok(result)
    }
}

/// How one episode's consolidation turned out.
struct Outcome {
    verdict: Verdict,
    kind: PromotionKind,
    episode_id: u64,
    node_id: Option<u64>,
    reasoning: String,
    confidence: f32,
    /// Whether an LLM call was made for this episode.
    llm_called: bool,
    /// Whether that call failed, and whether it failed by truncation.
    llm_failed: bool,
    truncated: bool,
    /// Candidates recalled for this episode (funnel accounting).
    candidates: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// No candidates at all - written straight from the episode. No LLM.
    Projected,
    /// LLM judged the episode distinct from every candidate - new node.
    NoMerge,
    /// LLM folded the episode into an existing node.
    Merged,
    /// LLM retired a contradicting node in favour of this episode.
    Superseded,
    /// Below `DistillerConfig::min_importance` - left in the episodic layer.
    BelowImportance,
    /// Not consolidated this run; still eligible next run.
    Deferred,
}

impl Outcome {
    fn record(self, result: &mut DistillerResult) {
        let funnel = &mut result.funnel;
        funnel.candidates_recalled += self.candidates;
        if self.llm_called {
            funnel.llm_calls += 1;
            if self.llm_failed {
                funnel.llm_calls_failed += 1;
                if self.truncated {
                    funnel.llm_calls_truncated += 1;
                }
            }
        }
        match self.verdict {
            Verdict::Projected => funnel.projected += 1,
            Verdict::NoMerge => funnel.verdict_no_merge += 1,
            Verdict::Merged => funnel.verdict_merged += 1,
            Verdict::Superseded => funnel.verdict_superseded += 1,
            Verdict::BelowImportance => funnel.below_importance += 1,
            Verdict::Deferred => funnel.episodes_deferred += 1,
        }

        let consolidated = matches!(
            self.verdict,
            Verdict::Projected | Verdict::NoMerge | Verdict::Merged | Verdict::Superseded
        );
        if consolidated {
            funnel.episodes_consolidated += 1;
            result.episodes_marked_consolidated += 1;
            match self.kind {
                PromotionKind::Fact => result.facts_promoted += 1,
                PromotionKind::Preference => result.preferences_promoted += 1,
                PromotionKind::Relation => result.relations_promoted += 1,
                PromotionKind::Procedure => result.procedures_promoted += 1,
                _ => result.autobio_promoted += 1,
            }
        }

        result.promotion_evaluations.push(PromotionEvaluation {
            source_episode_ids: vec![self.episode_id],
            promoted_kind: self.kind,
            promoted_node_id: self.node_id,
            llm_reasoning: self.reasoning.clone(),
            llm_confidence: self.confidence,
            decision: if consolidated {
                PromotionDecision::Promoted
            } else if self.verdict == Verdict::BelowImportance {
                PromotionDecision::Skipped {
                    reason: "below min_importance".to_string(),
                }
            } else {
                PromotionDecision::Deferred {
                    reason: self.reasoning.clone(),
                }
            },
        });
    }
}

/// Consolidate exactly one episode. See the module doc for the pipeline.
async fn consolidate_one(
    provider: &dyn MemoryProvider,
    llm: Option<&dyn ConsolidationLlm>,
    embedding_fn: Option<&EmbeddingFn>,
    config: &DistillerConfig,
    episode_id: u64,
    episode: &Episode,
) -> Result<Outcome> {
    let Some(subtype) = episode.knowledge_subtype.clone() else {
        // Not a consolidation candidate; stays in the episodic layer.
        return Ok(deferred(episode_id, PromotionKind::Fact, 0, String::new()));
    };
    let kind = promotion_kind_for(&subtype);

    if f64::from(episode.importance) < f64::from(config.min_importance) {
        return Ok(Outcome {
            verdict: Verdict::BelowImportance,
            kind,
            episode_id,
            node_id: None,
            reasoning: String::new(),
            confidence: 1.0,
            llm_called: false,
            llm_failed: false,
            truncated: false,
            candidates: 0,
        });
    }

    // ---- project ----------------------------------------------------------
    let text = canonical_text(episode);
    // Reuse the write-time vector when there is one. The write path embeds
    // `statement()` too, so a stored episode vector and a freshly embedded
    // statement are the same key - re-embedding a 262-episode backlog would
    // cost 262 calls for vectors the store already holds.
    let embedding = match episode.embedding.as_deref() {
        Some(v) if !v.is_empty() => Some(v.to_vec()),
        _ => embedding_fn.map(|f| f(&text)),
    };
    let label = semantic_label(&subtype);
    let recalled = recall(provider, label, embedding.as_ref(), &subtype, config)?;
    let n = recalled.len();

    if n == 0 {
        // Pure projection: the writing LLM already decided this was worth
        // keeping and nothing similar exists yet, so there is nothing to
        // judge. This path costs no LLM tokens at all.
        let src = Source {
            episode_id,
            text: &text,
            embedding: embedding.as_ref(),
            importance: episode.importance,
        };
        let node_id = write_new_node(provider, &subtype, &src)?;
        provider.mark_consolidated(&[episode_id])?;
        return Ok(Outcome {
            verdict: Verdict::Projected,
            kind,
            episode_id,
            node_id: Some(node_id),
            reasoning: String::new(),
            confidence: 1.0,
            llm_called: false,
            llm_failed: false,
            truncated: false,
            candidates: 0,
        });
    }

    // ---- merge / conflict -------------------------------------------------
    let Some(llm) = llm else {
        // Candidates exist but nothing can judge them. Guessing is wrong in
        // both directions: merging two different facts is unrecoverable, and
        // duplicating silently undoes the work the merge exists to prevent.
        // Defer - the episode stays eligible for a run that has a model.
        return Ok(deferred(
            episode_id,
            kind,
            n,
            "candidates recalled but no LLM available to judge them".to_string(),
        ));
    };

    let verdict = match ask_merge(llm, &text, &recalled, config).await {
        Ok(v) => v,
        Err(failure) => {
            return Ok(Outcome {
                verdict: Verdict::Deferred,
                kind,
                episode_id,
                node_id: None,
                reasoning: failure.reason,
                confidence: 0.0,
                llm_called: true,
                llm_failed: true,
                truncated: failure.truncated,
                candidates: n,
            });
        }
    };

    let (target_id, action) = match verdict.action {
        MergeAction::NoMerge => {
            let src = Source {
                episode_id,
                text: &text,
                embedding: embedding.as_ref(),
                importance: episode.importance,
            };
            let node_id = write_new_node(provider, &subtype, &src)?;
            provider.mark_consolidated(&[episode_id])?;
            return Ok(Outcome {
                verdict: Verdict::NoMerge,
                kind,
                episode_id,
                node_id: Some(node_id),
                reasoning: verdict.reasoning,
                confidence: 1.0,
                llm_called: true,
                llm_failed: false,
                truncated: false,
                candidates: n,
            });
        }
        MergeAction::Merge | MergeAction::Contradicts => {
            let Some(id) = verdict.target_id else {
                return Ok(deferred_llm(
                    episode_id,
                    kind,
                    n,
                    format!("{:?} verdict without target_id", verdict.action),
                ));
            };
            // A target outside the recalled set is a hallucinated id; acting on
            // it would rewrite an unrelated node.
            if !recalled.iter().any(|c| c.id == id) {
                return Ok(deferred_llm(
                    episode_id,
                    kind,
                    n,
                    format!("target_id {id} is not a recalled candidate"),
                ));
            }
            (id, verdict.action)
        }
    };

    let merged_text = verdict
        .statement
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(text.as_str())
        .to_string();
    let merged_embedding = if merged_text == text {
        embedding.clone()
    } else {
        let Some(embed) = embedding_fn else {
            return Ok(deferred_llm(
                episode_id,
                kind,
                n,
                "merge rewrote the statement but no embedding function is available".to_string(),
            ));
        };
        Some(embed(&merged_text))
    };

    let src = Source {
        episode_id,
        text: &merged_text,
        embedding: merged_embedding.as_ref(),
        importance: episode.importance,
    };
    let node_id = apply_merge(
        provider,
        &subtype,
        target_id,
        &src,
        action == MergeAction::Contradicts,
    )?;
    provider.mark_consolidated(&[episode_id])?;

    Ok(Outcome {
        verdict: if action == MergeAction::Contradicts {
            Verdict::Superseded
        } else {
            Verdict::Merged
        },
        kind,
        episode_id,
        node_id: Some(node_id),
        reasoning: verdict.reasoning,
        confidence: 1.0,
        llm_called: true,
        llm_failed: false,
        truncated: false,
        candidates: n,
    })
}

/// An LLM call that produced nothing usable, and whether it was truncated
/// (separates "ask for less" from "the model is broken").
struct LlmFailure {
    reason: String,
    truncated: bool,
}

fn deferred(episode_id: u64, kind: PromotionKind, candidates: usize, reason: String) -> Outcome {
    Outcome {
        verdict: Verdict::Deferred,
        kind,
        episode_id,
        node_id: None,
        reasoning: reason,
        confidence: 0.0,
        llm_called: false,
        llm_failed: false,
        truncated: false,
        candidates,
    }
}

fn deferred_llm(episode_id: u64, kind: PromotionKind, candidates: usize, reason: String) -> Outcome {
    Outcome {
        llm_called: true,
        llm_failed: true,
        ..deferred(episode_id, kind, candidates, reason)
    }
}

// ---------------------------------------------------------------------------
// Projection helpers
// ---------------------------------------------------------------------------

/// The statement to consolidate: the LLM's own canonical restatement when it
/// supplied one, otherwise the raw episode content.
///
/// Falling back to `content` is what keeps the rewrite backward compatible -
/// every episode written before the `normalized` field existed still projects.
///
/// The fallback itself lives on [`Episode::statement()`], because the write
/// path embeds the same string; two copies would drift apart and make the
/// stored vector and the lookup vector disagree about what an episode says.
fn canonical_text(episode: &Episode) -> String {
    episode.statement().to_string()
}

/// Which semantic label a subtype projects into.
fn semantic_label(subtype: &KnowledgeSubType) -> &'static str {
    match subtype {
        KnowledgeSubType::Procedure => labels::PROCEDURAL,
        _ => labels::KNOWLEDGE,
    }
}

fn promotion_kind_for(subtype: &KnowledgeSubType) -> PromotionKind {
    match subtype {
        KnowledgeSubType::Fact => PromotionKind::Fact,
        KnowledgeSubType::Preference => PromotionKind::Preference,
        KnowledgeSubType::Relation => PromotionKind::Relation,
        KnowledgeSubType::Procedure => PromotionKind::Procedure,
    }
}

/// Short deterministic identifier used for a projected node's `predicate` /
/// `name`. Truncated so a long sentence cannot bloat the dedup key.
fn slug(text: &str) -> String {
    // Runs of punctuation collapse to one separator, so "a, b" and "a b" share
    // a dedup key. Without this the key is punctuation-sensitive in a way that
    // splits the same statement across two nodes.
    let mut raw = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_alphanumeric() {
            raw.extend(c.to_lowercase());
        } else if !raw.ends_with('_') {
            raw.push('_');
        }
    }
    let slim = raw.trim_matches('_');
    let out: String = slim.chars().take(48).collect();
    if out.is_empty() {
        "statement".to_string()
    } else {
        out
    }
}

fn projection_metadata(episode_ids: &[u64]) -> PromotionMetadata {
    PromotionMetadata {
        promoted_at: Utc::now(),
        promoted_by: "episodic_distiller".to_string(),
        evidence_episode_ids: episode_ids.to_vec(),
        evidence_span_days: 0,
        llm_judge_confidence: 1.0,
        llm_judge_reasoning: String::new(),
    }
}

/// Everything both node writers need about the episode a write derives from.
///
/// Grouped because a projection and a merge take the same five inputs, and
/// passing them positionally made `apply_merge` an eight-argument function.
struct Source<'a> {
    episode_id: u64,
    text: &'a str,
    embedding: Option<&'a Vec<f32>>,
    importance: f32,
}

/// Write a fresh semantic node from one projected episode.
///
/// `store_knowledge` collapses an exact `(subject, predicate)` duplicate when
/// it has no embeddings to compare - for a projected node that means two
/// episodes whose text differs only in punctuation fold into one row. That is
/// the desired outcome (they state the same thing), and the store unions the
/// two episodes' provenance rather than dropping the second one.
fn write_new_node(
    provider: &dyn MemoryProvider,
    subtype: &KnowledgeSubType,
    src: &Source<'_>,
) -> Result<u64> {
    let Source {
        episode_id,
        text,
        embedding,
        importance,
    } = *src;
    let now = Utc::now();
    let meta = projection_metadata(&[episode_id]);
    if subtype == &KnowledgeSubType::Procedure {
        provider.store_procedural(&ProceduralNode {
            id: None,
            name: slug(text),
            trigger_condition: String::new(),
            action_pattern: text.to_string(),
            success_count: 0,
            fail_count: 0,
            confidence: 0.8,
            activation_count: 0,
            source_skill: None,
            learned_from: "offline_consolidation".to_string(),
            embedding: embedding.cloned().unwrap_or_default(),
            status: NodeStatus::Active,
            created_at: now,
            updated_at: now,
            source_episode_ids: vec![episode_id],
            promotion_metadata: Some(meta),
            metadata: HashMap::new(),
        })
    } else {
        provider.store_knowledge(&KnowledgeNode {
            subject: "user".to_string(),
            predicate: slug(text),
            object: text.to_string(),
            sub_type: subtype.clone(),
            confidence: 0.8,
            source_episode_id: Some(episode_id),
            source_episode_ids: vec![episode_id],
            promotion_metadata: Some(meta),
            embedding: embedding.cloned(),
            status: NodeStatus::Active,
            created_at: now,
            updated_at: now,
            metadata: HashMap::new(),
            privacy: PrivacyLevel::Personal,
            importance,
        })
    }
}

/// Fold one episode into an existing node, or retire that node in favour of it.
///
/// `supersede` keeps the displaced statement under
/// `metadata["superseded_statements"]` instead of dropping it: a wrong
/// "contradicts" verdict should stay recoverable by a human reading the node.
fn apply_merge(
    provider: &dyn MemoryProvider,
    subtype: &KnowledgeSubType,
    target_id: u64,
    src: &Source<'_>,
    supersede: bool,
) -> Result<u64> {
    let Source {
        episode_id,
        text,
        embedding,
        importance,
    } = *src;
    let now = Utc::now();
    if subtype == &KnowledgeSubType::Procedure {
        let Some(mut node) = provider.get_procedural(target_id)? else {
            return Err(AcoworkError::Memory(format!(
                "merge target {target_id} is not a procedural node"
            )));
        };
        let previous = std::mem::replace(&mut node.action_pattern, text.to_string());
        if supersede {
            record_superseded(&mut node.metadata, &previous);
        }
        if !node.source_episode_ids.contains(&episode_id) {
            node.source_episode_ids.push(episode_id);
        }
        node.promotion_metadata = Some(projection_metadata(&node.source_episode_ids));
        if let Some(emb) = embedding {
            node.embedding = emb.clone();
        }
        node.updated_at = now;
        node.id = Some(target_id);
        provider.update_procedural(&node)?;
        return Ok(target_id);
    }

    let Some(mut node) = provider.get_knowledge(target_id)? else {
        return Err(AcoworkError::Memory(format!(
            "merge target {target_id} is not a knowledge node"
        )));
    };
    let previous = std::mem::replace(&mut node.object, text.to_string());
    if supersede {
        record_superseded(&mut node.metadata, &previous);
    }
    if !node.source_episode_ids.contains(&episode_id) {
        node.source_episode_ids.push(episode_id);
    }
    node.source_episode_id = node.source_episode_ids.first().copied();
    node.promotion_metadata = Some(projection_metadata(&node.source_episode_ids));
    node.importance = node.importance.max(importance);
    node.confidence = node.confidence.max(0.8);
    if embedding.is_some() {
        node.embedding = embedding.cloned();
    }
    node.updated_at = now;
    provider.update_knowledge(target_id, &node)?;
    Ok(target_id)
}

fn record_superseded(metadata: &mut HashMap<String, serde_json::Value>, previous: &str) {
    let mut list = match metadata.get("superseded_statements") {
        Some(serde_json::Value::Array(v)) => v.clone(),
        _ => Vec::new(),
    };
    list.push(serde_json::Value::String(previous.to_string()));
    metadata.insert("superseded_statements".to_string(), list.into());
}

// ---------------------------------------------------------------------------
// Candidate recall
// ---------------------------------------------------------------------------

/// Existing semantic nodes similar enough to be possible merge targets.
///
/// Without an embedding function nothing can be recalled, so every episode
/// projects as a new node. That is a real degradation (the semantic layer gets
/// denser and nothing dedups it) and it is visible in the funnel: `llm_calls`
/// stays at zero while `projected` climbs.
fn recall(
    provider: &dyn MemoryProvider,
    label: &str,
    embedding: Option<&Vec<f32>>,
    subtype: &KnowledgeSubType,
    config: &DistillerConfig,
) -> Result<Vec<Candidate>> {
    let Some(emb) = embedding else {
        return Ok(Vec::new());
    };
    // Over-fetch by 4x: the Knowledge label mixes Fact, Preference and Relation
    // nodes and the subtype filter can only run after the vector scan, so
    // asking for exactly `k` can return far fewer once foreign subtypes drop.
    let hits = provider.vector_search(label, emb, config.merge_candidate_k.saturating_mul(4))?;
    let mut out = Vec::new();
    for (id, cosine) in hits {
        if cosine < f64::from(config.merge_recall_threshold) {
            continue;
        }
        let text = if label == labels::KNOWLEDGE {
            provider
                .get_knowledge(id)?
                .filter(|n| &n.sub_type == subtype)
                .map(|n| n.object)
        } else {
            provider.get_procedural(id)?.map(|n| n.action_pattern)
        };
        if let Some(text) = text {
            out.push(Candidate { id, text });
            if out.len() >= config.merge_candidate_k {
                break;
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// The merge call
// ---------------------------------------------------------------------------

async fn ask_merge(
    llm: &dyn ConsolidationLlm,
    text: &str,
    candidates: &[Candidate],
    config: &DistillerConfig,
) -> std::result::Result<MergeVerdict, LlmFailure> {
    let system = config
        .merge_prompt_override
        .as_deref()
        .unwrap_or(MERGE_SYSTEM_PROMPT);
    let user = serde_json::to_string(&serde_json::json!({
        "new_statement": text,
        "candidates": candidates
            .iter()
            .map(|c| serde_json::json!({ "id": c.id, "statement": c.text }))
            .collect::<Vec<_>>(),
    }))
    .map_err(|e| LlmFailure {
        reason: format!("cannot build merge request: {e}"),
        truncated: false,
    })?;

    let reply = llm
        .chat(vec![
            LlmMessage {
                role: "system".to_string(),
                content: system.to_string(),
            },
            LlmMessage {
                role: "user".to_string(),
                content: user,
            },
        ])
        .await
        .map_err(|e| LlmFailure {
            reason: e,
            truncated: false,
        })?;

    if reply.truncated() {
        return Err(LlmFailure {
            reason: "reply truncated (finish_reason=length)".to_string(),
            truncated: true,
        });
    }
    parse_verdict(&reply.content).map_err(|reason| LlmFailure {
        reason,
        truncated: false,
    })
}

fn parse_verdict(content: &str) -> std::result::Result<MergeVerdict, String> {
    let value = parse_json_object(content)?;
    serde_json::from_value(value).map_err(|e| format!("verdict schema violation: {e}"))
}

/// Extract the first JSON object from a reply, tolerating prose and fences.
fn parse_json_object(content: &str) -> std::result::Result<serde_json::Value, String> {
    let trimmed = content.trim();
    let stripped = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|s| s.trim_start())
        .unwrap_or(trimmed)
        .trim_end_matches("```")
        .trim();
    serde_json::from_str(stripped).map_err(|e| format!("invalid JSON: {e}"))
}
