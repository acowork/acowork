//! [`MemoryProvider`] implementation for the SQLite backend (ADR-082 D1).
//!
//! The Runtime only ever holds `Arc<dyn MemoryProvider>`, so this file is the
//! whole seam between it and the four memory feature chains — `memory_store`,
//! `memory_recall`, distillation (`run_episodic_decay_scan`'s sibling
//! pipelines) and forgetting.
//!
//! Layer-wide orchestration lives *above* this trait and is therefore reused
//! verbatim: dedup, Dormant exclusion, session and time-range filtering,
//! PageRank and time-decay re-ranking and graph expansion are all driven by
//! ([`crate::MemoryManager`]), which calls back into the node accessors
//! ([`get_node_status`](MemoryProvider::get_node_status),
//! [`get_node_session_id`](MemoryProvider::get_node_session_id),
//! [`get_node_created_at`](MemoryProvider::get_node_created_at),
//! [`get_node_content`](MemoryProvider::get_node_content)). That is why most
//! methods here are one-line delegations to the inherent `SqliteStore` API.
//!
//! Graph operations are no-ops by design (ADR-082 D4 — the layer is dead code;
//! see `run_gateway_fs_redline`-style evidence in ADR-082 §1.5).

use std::time::{Duration, Instant};

use acowork_core::error::{AcoworkError, Result as AcoworkResult};
use crate::MemoryProvider;
use crate::quality::MemoryQualityConfig;
use crate::types::{
    AutobiographicalNode, CollaborationSpan, DecayScanResult, Episode,
    EpisodicDecayConfig, KnowledgeNode, KnowledgeSubType, MemoryQuery, NodeStatus, ProceduralNode,
    ResultSource, SearchResult, StoreHealth, StoreStats,
};
use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

use crate::labels;
use crate::sqlite::{SqliteStore, ts_text};

/// Number of persisted indexes reported by [`MemoryProvider::stats`]: one FTS5
/// table per label plus the `vectors` primary key.
const INDEX_COUNT: usize = labels::ALL.len() + 1;

#[async_trait::async_trait]
impl MemoryProvider for SqliteStore {
    // ── Episodic layer ───────────────────────────────────────────────────

    fn store_episode(&self, episode: &Episode) -> AcoworkResult<u64> {
        Ok(SqliteStore::store_episode(self, episode)?)
    }

    /// Episodic recall: hybrid when an embedding is supplied, text-only
    /// otherwise.
    ///
    /// `MemoryQuery` filters (`time_range`, `exclude_session_id`, node types)
    /// are *not* applied here — `MemoryManager` owns them, and applies them
    /// after retrieval through the node accessors so every label is filtered
    /// identically (ADR-062 M5).
    fn search_episodes(&self, query: &MemoryQuery) -> AcoworkResult<Vec<SearchResult>> {
        let hits = match query.embedding.as_deref() {
            Some(embedding) if !embedding.is_empty() => {
                let (text_weight, vector_weight) = hint_weights(query);
                SqliteStore::hybrid_search_full(
                    self,
                    labels::EPISODIC,
                    &query.query_text,
                    embedding,
                    query.limit,
                    text_weight,
                    vector_weight,
                    query.min_cosine,
                )?
            }
            _ => self.text_search(labels::EPISODIC, &query.query_text, query.limit)?,
        };
        Ok(self.results_from_hits(labels::EPISODIC, hits)?)
    }

    fn mark_consolidated(&self, ids: &[u64]) -> AcoworkResult<()> {
        for id in ids {
            SqliteStore::mark_episode_consolidated(self, *id)?;
        }
        Ok(())
    }

    fn vector_search(
        &self,
        label: &str,
        query: &[f32],
        k: usize,
    ) -> AcoworkResult<Vec<(u64, f64)>> {
        Ok(SqliteStore::vector_search(self, label, query, k)?)
    }

    fn get_knowledge(&self, id: u64) -> AcoworkResult<Option<KnowledgeNode>> {
        Ok(SqliteStore::get_knowledge(self, id)?)
    }

    fn update_knowledge(&self, id: u64, node: &KnowledgeNode) -> AcoworkResult<()> {
        Ok(SqliteStore::update_knowledge(self, id, node)?)
    }

    fn cleanup_episodes(&self, older_than: Duration) -> AcoworkResult<u64> {
        let seconds = i64::try_from(older_than.as_secs()).unwrap_or(i64::MAX);
        let cutoff = ts_text(Utc::now() - TimeDelta::seconds(seconds));
        let conn = self.lock();
        let ids = crate::sqlite::query_ids(
            &conn,
            "SELECT id FROM nodes \
             WHERE label = ?1 AND json_extract(props, '$.consolidated') = 1 AND created_at < ?2",
            &[&labels::EPISODIC, &cutoff],
        )?;
        drop(conn);

        let mut removed = 0u64;
        for id in ids {
            if SqliteStore::delete_node(self, id as u64)? {
                removed += 1;
            }
        }
        Ok(removed)
    }

    fn get_episodes(&self, session_id: Option<&str>, limit: usize) -> AcoworkResult<Vec<Episode>> {
        Ok(match session_id {
            Some(sid) => SqliteStore::search_episodes_by_session(self, sid, limit)?,
            None => SqliteStore::list_all_episodes(self, limit)?,
        })
    }

    fn get_episodes_by_subtype(
        &self,
        subtype: Option<KnowledgeSubType>,
        limit: usize,
    ) -> AcoworkResult<Vec<(u64, Episode)>> {
        // Skipped episodes are excluded in SQL (sticky judge verdict, ADR-068
        // Step 4) but the subtype filter runs in Rust: `knowledge_subtype` is
        // nested inside the `props` JSON, and comparing serialized enum values
        // in SQL would silently break the moment the enum gains a variant.
        let ids = self.unconsolidated_episode_ids()?;

        let mut out = Vec::new();
        for id in ids {
            let Some(episode) = SqliteStore::get_episode(self, id as u64)? else {
                continue;
            };
            if subtype
                .clone()
                .is_some_and(|wanted| episode.knowledge_subtype != Some(wanted))
            {
                continue;
            }
            out.push((id as u64, episode));
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    fn count_unconsolidated_episodes(&self) -> AcoworkResult<usize> {
        Ok(SqliteStore::count_unconsolidated_episodes(self)?)
    }

    fn collaboration_span(&self) -> AcoworkResult<Option<CollaborationSpan>> {
        let (earliest, count) = self.episodic_span()?;
        if count == 0 {
            return Ok(None);
        }
        // An unparsable timestamp degrades to "no span" rather than an error:
        // the caller (Relationship auto-generation) treats absence as "not
        // enough history yet", which is the safe direction.
        let earliest_episode_at = earliest
            .and_then(|ts| DateTime::parse_from_rfc3339(&ts).ok())
            .map(|ts| ts.with_timezone(&Utc));
        Ok(
            earliest_episode_at.map(|earliest_episode_at| CollaborationSpan {
                earliest_episode_at,
                episode_count: count as u64,
            }),
        )
    }

    // ── Semantic layer ───────────────────────────────────────────────────

    fn store_knowledge(&self, node: &KnowledgeNode) -> AcoworkResult<u64> {
        Ok(SqliteStore::store_knowledge(self, node)?)
    }

    fn store_procedural(&self, node: &ProceduralNode) -> AcoworkResult<u64> {
        Ok(SqliteStore::store_procedural(self, node)?)
    }

    fn store_autobiographical(&self, node: &AutobiographicalNode) -> AcoworkResult<u64> {
        Ok(SqliteStore::store_autobiographical(self, node)?)
    }

    /// All-label retrieval.
    ///
    /// Labels are searched independently and then merged on the *same* score
    /// scale ([`SqliteStore::hybrid_search_full`] returns the RRF fused score
    /// for every label), so a cross-label top-k is meaningful. RRF is a
    /// rank-position signal — absolute values are not comparable across
    /// queries and must not be thresholded on. `MemoryManager` prefers the
    /// per-label `hybrid_search_full` path when it needs the label alongside
    /// each hit.
    fn hybrid_search(&self, query: &MemoryQuery) -> AcoworkResult<Vec<SearchResult>> {
        if query.embedding.is_none() && query.query_text.trim().is_empty() {
            return Ok(Vec::new());
        }
        let (text_weight, vector_weight) = hint_weights(query);
        let mut all: Vec<SearchResult> = Vec::new();
        for label in labels::ALL {
            let hits = match query.embedding.as_deref() {
                Some(embedding) if !embedding.is_empty() => SqliteStore::hybrid_search_full(
                    self,
                    label,
                    &query.query_text,
                    embedding,
                    query.limit,
                    text_weight,
                    vector_weight,
                    query.min_cosine,
                )?,
                _ => self.text_search(label, &query.query_text, query.limit)?,
            };
            all.extend(self.results_from_hits(label, hits)?);
        }
        all.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        all.truncate(query.limit);
        Ok(all)
    }

    // ── Forgetting ───────────────────────────────────────────────────────

    /// Pure time-decay scan over `Episodic` nodes only.
    ///
    /// Mirrors the grafeo semantics exactly: `retention = exp(-ln2 *
    /// age_days / half_life_days)`, Active → Dormant below
    /// `dormant_threshold`, Dormant → archived after `archive_days`. Semantic
    /// labels are never aged out — they are knowledge, not event records.
    ///
    /// The archive step copies the node into `purge_log` *before* deleting it,
    /// so forgetting is recoverable rather than destructive.
    fn run_episodic_decay_scan(
        &self,
        config: &EpisodicDecayConfig,
    ) -> AcoworkResult<DecayScanResult> {
        let mut result = DecayScanResult::default();
        if !config.enabled {
            return Ok(result);
        }
        let now = Utc::now();

        let rows = self.episodic_rows()?;

        for (id, status, props) in rows {
            let id = id as u64;
            let props: Value = serde_json::from_str(&props)?;

            if status == NodeStatus::Active.as_str() {
                let age_days = age_days(
                    props.get("created_at").or_else(|| props.get("timestamp")),
                    now,
                );
                if config.retention(age_days) < f64::from(config.dormant_threshold) {
                    self.transition_to_dormant(id)?;
                    result.to_dormant += 1;
                }
                continue;
            }

            if status == NodeStatus::Dormant.as_str() {
                let dormant_days = props
                    .get("dormant_since")
                    .and_then(Value::as_str)
                    .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
                    .map(|since| (now - since.with_timezone(&Utc)).num_days())
                    .unwrap_or(0);
                if dormant_days >= config.archive_days as i64 {
                    let importance = props
                        .get("importance")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0);
                    self.purge_episode(
                        id,
                        &format!(
                            "time_expired: dormant_days={dormant_days}, importance={importance:.4}"
                        ),
                    )?;
                    result.purged += 1;
                }
            }
        }

        Ok(result)
    }

    // ── Lifecycle ───────────────────────────────────────────────────────

    fn health_check(&self) -> AcoworkResult<StoreHealth> {
        let started = Instant::now();
        // A real read, not a constant: health must fail when the file is gone,
        // the lock is held by a dead thread or the schema is broken.
        let probe = self.probe();
        let latency_ms = started.elapsed().as_millis() as u64;
        match probe {
            Ok(_) => Ok(StoreHealth {
                is_healthy: true,
                latency_ms,
                error_count: 0,
                details: None,
            }),
            Err(e) => Ok(StoreHealth {
                is_healthy: false,
                latency_ms,
                error_count: 1,
                details: Some(e.to_string()),
            }),
        }
    }

    fn stats(&self) -> AcoworkResult<StoreStats> {
        let (episode_count, node_count, active_node_count, dormant_node_count) =
            self.label_counts()?;
        let storage_size_bytes = self.storage_size_bytes()?;

        Ok(StoreStats {
            episode_count,
            node_count,
            active_node_count,
            dormant_node_count,
            edge_count: 0,
            storage_size_bytes,
            index_count: INDEX_COUNT,
        })
    }

    fn close(&self) -> AcoworkResult<()> {
        Ok(SqliteStore::close(self)?)
    }

    // ── Hybrid retrieval (extended) ──────────────────────────────────────

    fn hybrid_search_full(
        &self,
        label: &str,
        query_text: &str,
        embedding: &[f32],
        k: usize,
        text_weight: f64,
        vector_weight: f64,
        min_cosine: Option<f32>,
    ) -> AcoworkResult<Vec<(u64, f64)>> {
        Ok(SqliteStore::hybrid_search_full(
            self,
            label,
            query_text,
            embedding,
            k,
            text_weight,
            vector_weight,
            min_cosine,
        )?)
    }

    /// Real per-source signals: delegates to the shared engine's detailed form
    /// instead of the trait's degraded default (ADR-095 D4).
    fn hybrid_search_detailed(
        &self,
        label: &str,
        lexical: &acowork_core::retrieval::LexicalQuery<'_>,
        embedding: &[f32],
        k: usize,
        policy: &acowork_core::retrieval::FusionPolicy,
    ) -> AcoworkResult<Vec<acowork_core::retrieval::FusedHit>> {
        Ok(SqliteStore::hybrid_search_detailed(
            self,
            label,
            lexical,
            Some(embedding),
            k,
            policy,
        )?)
    }

    fn text_search_with_filter(
        &self,
        label: &str,
        field: &str,
        query_text: &str,
        k: usize,
    ) -> AcoworkResult<Vec<(u64, f64)>> {
        Ok(SqliteStore::text_search_with_filter(
            self, label, field, query_text, k,
        )?)
    }

    // ── Node CRUD ────────────────────────────────────────────────────────

    fn get_all_procedural_nodes(&self) -> AcoworkResult<Vec<ProceduralNode>> {
        Ok(SqliteStore::get_all_procedural_nodes(self)?)
    }

    fn find_procedural_by_trigger(
        &self,
        trigger: &str,
        limit: usize,
    ) -> AcoworkResult<Vec<ProceduralNode>> {
        Ok(SqliteStore::find_procedural_by_trigger(
            self, trigger, limit,
        )?)
    }

    fn get_procedural(&self, node_id: u64) -> AcoworkResult<Option<ProceduralNode>> {
        Ok(SqliteStore::get_procedural(self, node_id)?)
    }

    fn update_procedural(&self, node: &ProceduralNode) -> AcoworkResult<()> {
        Ok(SqliteStore::update_procedural(self, node)?)
    }

    fn find_autobiographical_by_key(
        &self,
        key: &str,
    ) -> AcoworkResult<Option<AutobiographicalNode>> {
        Ok(SqliteStore::find_autobiographical_by_key(self, key)?)
    }

    fn find_autobiographical_by_category(
        &self,
        category: crate::AutobioCategory,
    ) -> AcoworkResult<Vec<AutobiographicalNode>> {
        Ok(SqliteStore::find_autobiographical_by_category(
            self, category,
        )?)
    }

    fn update_autobiographical(&self, node: &AutobiographicalNode) -> AcoworkResult<()> {
        Ok(SqliteStore::update_autobiographical(self, node)?)
    }

    // ── Retrieval pipeline support ───────────────────────────────────────

    fn get_node_content(&self, node_id: u64) -> AcoworkResult<Option<String>> {
        let Some((_, props)) = self.node_row(node_id)? else {
            return Ok(None);
        };
        Ok(Some(render_content(&props)))
    }

    fn get_node_session_id(&self, node_id: u64) -> AcoworkResult<Option<String>> {
        let Some((_, props)) = self.node_row(node_id)? else {
            return Ok(None);
        };
        Ok(props
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_string))
    }

    fn get_node_status(&self, node_id: u64) -> AcoworkResult<Option<NodeStatus>> {
        // The `status` column is authoritative: forgetting writes Dormant
        // there without touching `props`, so reading `props.status` would
        // report decayed episodes as Active forever.
        Ok(self
            .node_status_text(node_id)?
            .map(|status| status.parse().unwrap_or(NodeStatus::Active)))
    }

    fn get_node_created_at(&self, node_id: u64) -> AcoworkResult<Option<DateTime<Utc>>> {
        // Episodes store their timestamp in this column too, so `--since` /
        // `--until` on `memory_recall` filter episodic hits as well (the grafeo
        // path only read a `created_at` property, which episodes lack).
        let Some(ts) = self.node_created_at_text(node_id)? else {
            return Ok(None);
        };
        Ok(DateTime::parse_from_rfc3339(&ts)
            .ok()
            .map(|ts| ts.with_timezone(&Utc)))
    }

    fn apply_quality_config(&self, config: &MemoryQualityConfig) -> AcoworkResult<()> {
        let mut guard = self
            .quality
            .write()
            .map_err(|_| AcoworkError::Memory("sqlite quality config lock poisoned".to_string()))?;
        *guard = config.clone();
        Ok(())
    }

}

// ── Internal helpers ─────────────────────────────────────────────────────

impl SqliteStore {
    /// `(label, props)` for a node, or `None` when it does not exist.
    fn node_row(&self, node_id: u64) -> crate::sqlite::Result<Option<(String, Value)>> {
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT label, props FROM nodes WHERE id = ?1",
                params![node_id as i64],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        row.map(|(label, props)| Ok((label, serde_json::from_str(&props)?)))
            .transpose()
    }

    /// Materialize retrieval hits into [`SearchResult`]s with real content.
    fn results_from_hits(
        &self,
        label: &str,
        hits: Vec<(u64, f64)>,
    ) -> crate::sqlite::Result<Vec<SearchResult>> {
        let mut out = Vec::with_capacity(hits.len());
        for (node_id, score) in hits {
            let content = self
                .node_row(node_id)?
                .map(|(_, props)| render_content(&props))
                .unwrap_or_default();
            out.push(SearchResult {
                content,
                label: label.to_string(),
                score,
                source: ResultSource::DirectMatch,
                // The Runtime computes the injection budget itself; this field
                // is unused on the retrieval path (mirrors the grafeo store).
                context_tokens: 0,
                node_id,
                source_context: None,
            });
        }
        Ok(out)
    }

    /// `(id, status, props)` for every episodic node.
    fn episodic_rows(&self) -> crate::sqlite::Result<Vec<(i64, String, String)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT id, status, props FROM nodes WHERE label = ?1")?;
        let mapped = stmt.query_map(params![labels::EPISODIC], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        Ok(mapped.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Ids of unconsolidated, non-skipped episodes, oldest first.
    fn unconsolidated_episode_ids(&self) -> crate::sqlite::Result<Vec<i64>> {
        let conn = self.lock();
        crate::sqlite::query_ids(
            &conn,
            // Newest first — see `MemoryProvider::get_episodes_by_subtype` for
            // why the window must not start at the oldest rows.
            //
            // `knowledge_subtype IS NOT NULL` is required, not an optimisation:
            // the distiller only ever consolidates subtype-bearing episodes, so
            // a window that admits subtype-less rows spends part of its
            // `limit` on entries the caller immediately drops. That silently
            // shrinks the effective batch AND makes this scan disagree with
            // `count_unconsolidated_episodes`, so the reported backlog would
            // never drain to zero.
            "SELECT id FROM nodes WHERE label = ?1 AND json_extract(props, '$.consolidated') = 0 AND json_extract(props, '$.knowledge_subtype') IS NOT NULL ORDER BY created_at DESC",
            &[&labels::EPISODIC],
        )
    }

    /// Raw `status` column for a node.
    fn node_status_text(&self, node_id: u64) -> crate::sqlite::Result<Option<String>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT status FROM nodes WHERE id = ?1",
                params![node_id as i64],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    /// Raw `created_at` column for a node.
    fn node_created_at_text(&self, node_id: u64) -> crate::sqlite::Result<Option<String>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT created_at FROM nodes WHERE id = ?1",
                params![node_id as i64],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    /// `(earliest episode timestamp, total episode count)`.
    fn episodic_span(&self) -> crate::sqlite::Result<(Option<String>, i64)> {
        let conn = self.lock();
        Ok(conn.query_row(
            "SELECT MIN(created_at), COUNT(*) FROM nodes WHERE label = ?1",
            params![labels::EPISODIC],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    /// `(episodes, semantic nodes, active semantic, dormant semantic)`.
    fn label_counts(&self) -> crate::sqlite::Result<(u64, u64, u64, u64)> {
        let conn = self.lock();
        let count = |label: &str| -> crate::sqlite::Result<u64> {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM nodes WHERE label = ?1",
                params![label],
                |r| r.get::<_, i64>(0),
            )? as u64)
        };
        let status_count = |label: &str, status: &str| -> crate::sqlite::Result<u64> {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM nodes WHERE label = ?1 AND status = ?2",
                params![label, status],
                |r| r.get::<_, i64>(0),
            )? as u64)
        };

        let episode_count = count(labels::EPISODIC)?;
        let mut node_count = 0;
        let mut active = 0;
        let mut dormant = 0;
        for label in [
            labels::KNOWLEDGE,
            labels::PROCEDURAL,
            labels::AUTOBIOGRAPHICAL,
        ] {
            node_count += count(label)?;
            active += status_count(label, NodeStatus::Active.as_str())?;
            dormant += status_count(label, NodeStatus::Dormant.as_str())?;
        }
        Ok((episode_count, node_count, active, dormant))
    }

    /// `page_count * page_size` — the honest DB size for file and memory alike.
    pub(crate) fn storage_size_bytes(&self) -> crate::sqlite::Result<u64> {
        let conn = self.lock();
        let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        Ok((page_count.max(0) * page_size.max(0)) as u64)
    }

    /// Round-trip probe behind the health check.
    fn probe(&self) -> crate::sqlite::Result<i64> {
        let conn = self.lock();
        Ok(conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))?)
    }

    /// Mark an episode Dormant and record when it happened.
    ///
    /// `dormant_since` is written with `json_quote` — the value is a raw
    /// RFC3339 string, and `json()` would reject it as malformed JSON.
    ///
    /// Public because forgetting is a first-class operation: callers may
    /// dormant a single node without running a full decay scan (mirrors
    /// `GrafeoStore::transition_to_dormant`).
    pub fn transition_to_dormant(&self, node_id: u64) -> crate::sqlite::Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE nodes SET status = ?2, props = json_set(props, '$.dormant_since', json_quote(?3)) \
             WHERE id = ?1",
            params![
                node_id as i64,
                NodeStatus::Dormant.as_str(),
                ts_text(Utc::now())
            ],
        )?;
        Ok(())
    }

    /// Archive an episode into `purge_log`, then delete it.
    fn purge_episode(&self, node_id: u64, reason: &str) -> crate::sqlite::Result<()> {
        let (props, content, embedding) = {
            let conn = self.lock();
            let props: String = conn.query_row(
                "SELECT props FROM nodes WHERE id = ?1",
                params![node_id as i64],
                |r| r.get(0),
            )?;
            let content = serde_json::from_str::<Value>(&props)
                .ok()
                .and_then(|value| {
                    value
                        .get("content")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_default();
            let embedding: Option<Vec<u8>> = conn
                .query_row(
                    "SELECT embedding FROM vectors WHERE node_id = ?1",
                    params![node_id as i64],
                    |r| r.get(0),
                )
                .optional()?;
            (props, content, embedding)
        };

        let conn = self.lock();
        conn.execute(
            "INSERT INTO purge_log(node_id, label, props, content, embedding, reason, purged_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                node_id as i64,
                labels::EPISODIC,
                props,
                content,
                embedding,
                reason,
                ts_text(Utc::now())
            ],
        )?;
        drop(conn);

        SqliteStore::delete_node(self, node_id)?;
        Ok(())
    }
}

/// Age of a node in days, from its creation timestamp (0.0 when missing).
fn age_days(created_at: Option<&Value>, now: DateTime<Utc>) -> f64 {
    created_at
        .and_then(Value::as_str)
        .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
        .map(|ts| (now - ts.with_timezone(&Utc)).num_seconds() as f64 / 86_400.0)
        .unwrap_or(0.0)
}

/// Render a node's human-readable content.
///
/// Ported verbatim from the grafeo provider so distillation prompts, memory
/// injection and the `memory_recall` tool see byte-identical content after the
/// migration. Order matters: autobiographical is disambiguated by category,
/// procedural reads as a guideline, and knowledge is flattened to
/// `subject predicate object`.
pub(crate) fn render_content(props: &Value) -> String {
    let text = |key: &str| props.get(key).and_then(Value::as_str).unwrap_or("");
    let non_empty = |key: &str| {
        let value = text(key);
        (!value.is_empty()).then_some(value)
    };

    if let Some(category) = non_empty("category") {
        match (non_empty("key"), non_empty("value")) {
            (Some(key), Some(value)) => return format!("{category}: {key}: {value}"),
            _ => {
                if let Some(value) = non_empty("value") {
                    return format!("{category}: {value}");
                }
            }
        }
    }

    if let (Some(trigger), Some(action)) =
        (non_empty("trigger_condition"), non_empty("action_pattern"))
    {
        return format!("当 {trigger} 时，优先 {action}");
    }

    for key in ["content", "value"] {
        if let Some(value) = non_empty(key) {
            return value.to_string();
        }
    }

    if let (Some(subject), Some(predicate), Some(object)) = (
        non_empty("subject"),
        non_empty("predicate"),
        non_empty("object"),
    ) {
        return format!("{subject} {predicate} {object}");
    }

    if let Some(action) = non_empty("action_pattern") {
        return action.to_string();
    }

    for key in ["name", "key", "description"] {
        if let Some(value) = non_empty(key) {
            return value.to_string();
        }
    }

    String::new()
}

/// `(text_weight, vector_weight)` for a hint type.
///
/// Mirrors the private `acowork_memory::manager::hint_weights` table (ADR-051
/// C4) minus its graph weight, which has no meaning without a graph. The
/// `MemoryManager` retrieval path computes and passes these explicitly, so only
/// direct `MemoryQuery` callers depend on this copy.
fn hint_weights(query: &MemoryQuery) -> (f64, f64) {
    use crate::HintType;
    match query.hint_type {
        HintType::Semantic => (0.2, 0.8),
        HintType::Factual => (0.5, 0.5),
        HintType::Relational => (0.2, 0.6),
        HintType::Identity => (0.7, 0.3),
    }
}
