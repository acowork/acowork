//! [`MemoryAdminService`] implementation for the SQLite backend (ADR-082 D1).
//!
//! Serves the Desktop Memory panel and the admin HTTP endpoints: paged node
//! listing, single-node detail, semantic search, raw CRUD, statistics and
//! embedding-dimension migration.
//!
//! Everything here reads the generic row shape (`nodes.label` + `nodes.props`)
//! rather than the typed structs, because the admin surface must be able to
//! show a node the typed API cannot deserialize — a record written by an older
//! agent, or one created through [`create_node`](MemoryAdminService::create_node)
//! with an arbitrary property map. A typed path would make those nodes
//! invisible in the panel, which is exactly when an operator needs to see them.

use std::collections::HashMap;

use acowork_core::error::{AcoworkError, Result as AcoworkResult};
use acowork_memory::MemoryAdminService;
use acowork_memory::admin::{
    AdminListNodesOutput, AdminListNodesParams, AdminNodeDetail, AdminNodeRecord, AdminStats,
    RebuildScope, RebuildStats,
};
use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::{OptionalExtension, params};
use serde_json::{Map, Value};

use crate::{SqliteStore, labels, ts_text};

/// Refuse an unfiltered list scan past this many nodes (mirrors the grafeo
/// guard). An unfiltered scan materializes every node to render its content,
/// so without a ceiling one panel request could load the whole store.
const MAX_UNFILTERED_MEMORY_SCAN: u64 = 10_000;

/// Hard page-size ceiling, matching the wire contract.
const MAX_PAGE_SIZE: u32 = 100;

/// The row set an embedding rebuild touches.
///
/// Wider than the memory browser's [`labels::ALL`]: a model swap changes
/// the width of *every* vector in the store, and `vector_search` silently
/// skips rows of a foreign width, so leaving `ConversationMessage` out
/// would strand the whole conversation index - it is append-only by line
/// watermark and never re-embeds an indexed row, so nothing else would
/// ever heal those vectors. Shared by the scoped migration and the scope
/// pre-count so the two can never select different row sets.
const MIGRATION_LABELS: &[&str] = &[
    labels::EPISODIC,
    labels::KNOWLEDGE,
    labels::PROCEDURAL,
    labels::AUTOBIOGRAPHICAL,
    crate::conversation::LABEL,
];

/// One `nodes` row, in the shape the admin surface needs.
struct AdminRow {
    id: u64,
    label: String,
    status: String,
    props: Value,
    created_at: String,
}

impl MemoryAdminService for SqliteStore {
    fn list_nodes(&self, params: &AdminListNodesParams) -> AdminListNodesOutput {
        let page = params.page.max(1);
        let size = params.size.clamp(1, MAX_PAGE_SIZE);
        let cutoff = time_range_cutoff(&params.time_range);

        // Only an unfiltered scan is unbounded (every row gets rendered), so it
        // is the only one worth refusing.
        let has_filter = !params.keyword.trim().is_empty()
            || !params.node_type.is_empty()
            || !params.sub_type.is_empty()
            || !matches!(params.time_range.as_str(), "" | "all");
        if !has_filter
            && let Ok(total) = self.total_node_count()
            && total > MAX_UNFILTERED_MEMORY_SCAN
        {
            tracing::warn!(
                total,
                limit = MAX_UNFILTERED_MEMORY_SCAN,
                "memory list: refused unfiltered scan of oversized store"
            );
            return AdminListNodesOutput {
                total,
                page,
                size,
                nodes: Vec::new(),
                rejected_unfiltered: Some(total),
            };
        }

        let rows = match self.all_rows() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "memory list: query failed");
                return empty_page(page, size);
            }
        };
        paginate(rows, params, page, size, cutoff)
    }

    fn semantic_search(
        &self,
        query_text: &str,
        embedding: Option<&[f32]>,
        mode: &str,
        limit: usize,
    ) -> Vec<AdminNodeRecord> {
        let limit = limit.clamp(1, MAX_PAGE_SIZE as usize);
        if query_text.is_empty() && embedding.is_none() {
            return Vec::new();
        }

        // Labels are disjoint, so node ids cannot collide across them; the
        // merge is a plain score sort.
        let mut scored: Vec<(f64, u64)> = Vec::new();
        for label in labels::ALL {
            let hits = match (mode, embedding) {
                ("vector", Some(embedding)) => self.vector_search(label, embedding, limit),
                ("hybrid", Some(embedding)) => SqliteStore::hybrid_search_full(
                    self, label, query_text, embedding, limit, 1.0, 1.0, None,
                ),
                // "keyword" / "" / unknown mode, or no embedding available:
                // BM25 text search is the documented fallback.
                _ => self.text_search(label, query_text, limit),
            };
            match hits {
                Ok(hits) => scored.extend(hits.into_iter().map(|(id, score)| (score, id))),
                Err(e) => {
                    tracing::warn!(label, error = %e, "memory semantic_search: label search failed");
                }
            }
        }
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        });
        scored.truncate(limit);

        scored
            .into_iter()
            .filter_map(|(_, id)| self.admin_row(id).ok().flatten())
            .map(|row| record_from_row(&row))
            .collect()
    }

    fn get_node(&self, node_id: u64) -> AdminNodeDetail {
        let row = match self.admin_row(node_id) {
            Ok(Some(row)) => row,
            Ok(None) => return missing_detail(node_id, "Node not found"),
            Err(e) => {
                tracing::warn!(node_id, error = %e, "memory get_node failed");
                return missing_detail(node_id, "Memory store read failed");
            }
        };
        let record = record_from_row(&row);
        let properties = row
            .props
            .as_object()
            .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        AdminNodeDetail {
            node_id,
            found: true,
            node_type: record.node_type,
            sub_type: record.sub_type,
            content: record.content,
            confidence: record.confidence,
            importance: record.importance,
            decay_score: record.decay_score,
            created_at: record.created_at,
            last_accessed_at: record.last_accessed_at,
            access_count: record.access_count,
            status: record.status,
            properties,
            message: String::new(),
        }
    }

    fn create_node(&self, label: &str, properties: &HashMap<String, Value>) -> AcoworkResult<u64> {
        let mut props = Map::new();
        for (key, value) in properties {
            // `embedding` is projected into the `vectors` table (see `encode`),
            // so it must not also be duplicated inside `props`.
            if key != "id" && key != "embedding" {
                props.insert(key.clone(), value.clone());
            }
        }
        let props = Value::Object(props);

        let status = props
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_else(|| acowork_memory::types::NodeStatus::Active.as_str())
            .to_string();
        let created_at = timestamp_field(&props).unwrap_or_else(Utc::now);
        let embedding = properties
            .get("embedding")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_f64)
                    .map(|v| v as f32)
                    .collect::<Vec<f32>>()
            })
            .filter(|v| !v.is_empty());

        Ok(self.insert_row(
            label,
            &status,
            &serde_json::to_string(&props)?,
            &crate::provider::render_content(&props),
            &ts_text(created_at),
            &ts_text(Utc::now()),
            embedding.as_deref(),
        )?)
    }

    fn update_node(&self, node_id: u64, properties: &HashMap<String, Value>) -> AcoworkResult<()> {
        let Some(row) = self.admin_row(node_id)? else {
            return Err(AcoworkError::Memory(format!("node {node_id} not found")));
        };

        // Merge, never replace: the admin endpoint patches a few properties and
        // must not drop the fields it did not send.
        let mut props = row.props.as_object().cloned().unwrap_or_default();
        for (key, value) in properties {
            if key != "id" && key != "embedding" {
                props.insert(key.clone(), value.clone());
            }
        }
        let props = Value::Object(props);

        let status = props
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_else(|| acowork_memory::types::NodeStatus::Active.as_str())
            .to_string();
        let embedding = self.embedding_of(node_id)?;

        self.update_row(
            node_id,
            &row.label,
            &status,
            &serde_json::to_string(&props)?,
            &crate::provider::render_content(&props),
            &row.created_at,
            &ts_text(Utc::now()),
            embedding.as_deref(),
        )?;
        Ok(())
    }

    fn delete_node(&self, node_id: u64) -> bool {
        match SqliteStore::delete_node(self, node_id) {
            Ok(deleted) => deleted,
            Err(e) => {
                tracing::warn!(node_id, error = %e, "Failed to delete memory node");
                false
            }
        }
    }

    fn get_stats(&self) -> AdminStats {
        let by_type: HashMap<String, u64> = labels::ALL
            .iter()
            .filter_map(|label| {
                self.node_count_by_label(label)
                    .ok()
                    .map(|count| ((*label).to_string(), count))
            })
            .collect();
        let total_nodes = by_type.values().sum();

        let mut by_status = HashMap::new();
        if let Ok(counts) = self.status_counts() {
            by_status = counts;
        }
        if let Ok(purged) = self.purged_count() {
            by_status.insert("purged".to_string(), purged);
        }

        let nodes_with_embedding = SqliteStore::count_nodes_with_embedding(self).unwrap_or(0);
        let stored_dim = SqliteStore::embedding_dim(self) as u64;
        // A store that has not recorded a dimension yet holds no vectors either,
        // and `<> 0 bytes` would match every row in it. Treat "no dimension" as
        // "nothing to be stale about".
        let vectors_of_other_dim = if stored_dim == 0 {
            0
        } else {
            self.count_vectors_of_other_dim(stored_dim as usize).unwrap_or(0)
        };

        // The desktop Memory panel shows its health banner on exactly these two
        // conditions, so log them with an explicit grep target for support runs.
        if total_nodes > 0 && nodes_with_embedding < total_nodes {
            tracing::warn!(
                target: "memory_diag",
                total_nodes,
                nodes_with_embedding,
                missing = total_nodes - nodes_with_embedding,
                stored_dim,
                "memory_store: detected nodes without vector embeddings"
            );
        }
        if vectors_of_other_dim > 0 {
            tracing::warn!(
                target: "memory_diag",
                vectors_of_other_dim,
                stored_dim,
                "memory_store: detected vectors at a foreign width, invisible to search"
            );
        }

        AdminStats {
            total_nodes,
            storage_bytes: self.storage_size_bytes().unwrap_or(0),
            by_type,
            by_status,
            index_health: derive_index_health(total_nodes, nodes_with_embedding, vectors_of_other_dim)
                .to_string(),
            stored_dim,
            nodes_with_embedding,
            vectors_of_other_dim,
            // `PRAGMA user_version` is a 32-bit integer SQLite preserves
            // across connections; bump it from `schema.rs` whenever the
            // table layout changes. 0 is the implicit value before any
            // migration sets it — not an error, just "untouched".
            schema_version: read_user_version(&self.lock()),
        }
    }

    fn embedding_dim(&self) -> usize {
        SqliteStore::embedding_dim(self)
    }

    fn count_nodes_with_embedding(&self) -> u64 {
        SqliteStore::count_nodes_with_embedding(self).unwrap_or(0)
    }

    fn migrate_embedding_dimension(
        &self,
        embed_fn: &(dyn Fn(&str) -> Option<Vec<f32>> + Send + Sync),
        new_dim: usize,
    ) -> AcoworkResult<RebuildStats> {
        self.migrate_embedding_dimension_with_progress(embed_fn, new_dim, None)
    }

    /// The trait's default implementation drops the progress callback on the
    /// floor, which left the desktop's model-switch UI showing "rebuilding,
    /// 0 of 0" for the entire run - the work was real, only the numbers were
    /// missing. SQLite knows the row count before it starts, so report it.
    fn migrate_embedding_dimension_with_progress(
        &self,
        embed_fn: &(dyn Fn(&str) -> Option<Vec<f32>> + Send + Sync),
        new_dim: usize,
        progress: Option<&dyn Fn(u64, u64)>,
    ) -> AcoworkResult<RebuildStats> {
        self.migrate_embedding_dimension_scoped(embed_fn, new_dim, RebuildScope::All, progress)
    }

    /// The scope decides only *which rows are selected*; the re-embed loop and
    /// the dimension-declaration gate are shared with the full migration, so a
    /// repair cannot drift from the semantics of a rebuild.
    ///
    /// `Repair` selects the rows `vector_search` cannot see - no vector, or a
    /// vector at a width other than `new_dim` - which is what the memory
    /// panel's health banner counts. A full scan of a 5000-row store to heal
    /// two foreign-width rows is 5000 embed calls spent on 2 rows of work.
    fn migrate_embedding_dimension_scoped(
        &self,
        embed_fn: &(dyn Fn(&str) -> Option<Vec<f32>> + Send + Sync),
        new_dim: usize,
        scope: RebuildScope,
        progress: Option<&dyn Fn(u64, u64)>,
    ) -> AcoworkResult<RebuildStats> {
        let rows = match scope {
            RebuildScope::All => self.rows_with_labels(MIGRATION_LABELS)?,
            RebuildScope::Repair => {
                self.rows_with_foreign_or_missing_vector(MIGRATION_LABELS, new_dim)?
            }
        };
        self.reembed_rows(&rows, embed_fn, new_dim, progress)
    }

    /// Exact per-scope counts, built from the same SQL predicates the
    /// scoped migration selects with — the number the panel promises
    /// before the click and the `total` the progress bar reports after it
    /// are literally one query, so they cannot drift apart.
    fn rebuild_scope_count(&self, scope: RebuildScope, dim: usize) -> u64 {
        match scope {
            RebuildScope::All => self.count_rows_with_labels(MIGRATION_LABELS).unwrap_or(0),
            RebuildScope::Repair => {
                self.count_rows_foreign_or_missing_vector(MIGRATION_LABELS, dim)
                    .unwrap_or(0)
            }
        }
    }
}

// ── Internal helpers ─────────────────────────────────────────────────────

/// The WHERE fragment that defines the repair scope: rows under `labels`
/// whose vector is missing or stored at a width other than the
/// placeholder-bound `dim` - exactly the rows `vector_search` skips.
///
/// One fragment, two consumers ([`SqliteStore::rows_with_foreign_or_missing_vector`]
/// and [`SqliteStore::count_rows_foreign_or_missing_vector`]): the rebuild
/// and its pre-count are the same SQL by construction, so the number the
/// panel promises can never disagree with the rows the rebuild processes.
fn repair_scope_where(labels_len: usize) -> String {
    let placeholders: Vec<String> = (1..=labels_len).map(|i| format!("?{i}")).collect();
    format!(
        "n.label IN ({}) AND (v.node_id IS NULL OR length(v.embedding) <> ?{})",
        placeholders.join(", "),
        labels_len + 1,
    )
}

impl SqliteStore {
    /// Re-embed `rows` and apply the dimension-declaration gate.
    ///
    /// Shared by every scope so the work and the safety check are identical
    /// whatever set of rows was selected.
    fn reembed_rows(
        &self,
        rows: &[AdminRow],
        embed_fn: &(dyn Fn(&str) -> Option<Vec<f32>> + Send + Sync),
        new_dim: usize,
        progress: Option<&dyn Fn(u64, u64)>,
    ) -> AcoworkResult<RebuildStats> {
        let mut stats = RebuildStats::default();
        let total = rows.len() as u64;
        for row in rows {
            stats.total_scanned += 1;
            // Reported before the work, not after: a row the embedder skips must
            // still move the bar, otherwise a store whose model rejects most of
            // its content shows a progress bar stuck at zero and then done.
            if let Some(p) = progress {
                p(stats.total_scanned, total);
            }
            let content = crate::provider::render_content(&row.props);
            if content.trim().is_empty() {
                stats.skipped_no_content += 1;
                continue;
            }
            let Some(embedding) = embed_fn(&content) else {
                stats.skipped_no_embedding += 1;
                continue;
            };
            if embedding.len() != new_dim {
                // A model that returns the wrong width would poison the vector
                // index for every future query; skip it and count the error.
                tracing::warn!(
                    node_id = row.id,
                    expected = new_dim,
                    got = embedding.len(),
                    "embedding migration: dimension mismatch, skipping node"
                );
                stats.errors += 1;
                continue;
            }
            match self.set_embedding_of(row.id, &embedding) {
                Ok(()) => stats.rebuilt += 1,
                Err(e) => {
                    tracing::warn!(node_id = row.id, error = %e, "embedding migration: write failed");
                    stats.errors += 1;
                }
            }
        }

        // Only declare the store to be at `new_dim` when nothing was left behind
        // at another width. `vector_search` skips foreign-width rows silently, so
        // flipping the recorded dimension over a half-migrated store makes the
        // store claim a width it does not have - which is what let the memory
        // panel report "no rebuild needed" while 294 conversation vectors were
        // still 512-wide and invisible to retrieval.
        //
        // Measured from the rows themselves rather than from the counters: a node
        // the embedder returned nothing *for* only matters if it is holding an old
        // vector, and a node with no vector at all is simply not yet embedded.
        let stale = self.count_vectors_of_other_dim(new_dim)?;
        if stale == 0 {
            self.set_embedding_dim(new_dim)?;
        } else {
            tracing::warn!(
                new_dim,
                rebuilt = stats.rebuilt,
                stale,
                "embedding migration: store left at its previous dimension - \
                 {stale} rows are still at another width, so a rebuild is still required"
            );
        }
        Ok(stats)
    }

    /// Rows under `labels` whose vector is missing or stored at a width other
    /// than `dim` - the rows `vector_search` skips, and the set the memory
    /// panel's stale/missing-embedding banners count.
    fn rows_with_foreign_or_missing_vector(
        &self,
        labels: &[&str],
        dim: usize,
    ) -> crate::Result<Vec<AdminRow>> {
        let conn = self.lock();
        let sql = format!(
            "SELECT n.id, n.label, n.status, n.props, n.created_at \
             FROM nodes n LEFT JOIN vectors v ON v.node_id = n.id \
             WHERE {}",
            repair_scope_where(labels.len()),
        );
        let bytes = dim.saturating_mul(4) as i64;
        let mut stmt = conn.prepare(&sql)?;
        let mut args: Vec<&dyn rusqlite::types::ToSql> = labels
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();
        args.push(&bytes);
        let mapped = stmt.query_map(args.as_slice(), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut rows = Vec::new();
        for row in mapped {
            let (id, label, status, props, created_at) = row?;
            rows.push(AdminRow {
                id: id as u64,
                label,
                status,
                props: serde_json::from_str(&props)?,
                created_at,
            });
        }
        Ok(rows)
    }

    /// Count of [`Self::rows_with_foreign_or_missing_vector`], without
    /// materializing the rows. Same predicate by construction - both build
    /// their SQL from [`repair_scope_where`].
    fn count_rows_foreign_or_missing_vector(
        &self,
        labels: &[&str],
        dim: usize,
    ) -> crate::Result<u64> {
        let conn = self.lock();
        let sql = format!(
            "SELECT COUNT(*) FROM nodes n LEFT JOIN vectors v ON v.node_id = n.id WHERE {}",
            repair_scope_where(labels.len()),
        );
        let bytes = dim.saturating_mul(4) as i64;
        let mut stmt = conn.prepare(&sql)?;
        let mut args: Vec<&dyn rusqlite::types::ToSql> = labels
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();
        args.push(&bytes);
        let n: i64 = stmt.query_row(args.as_slice(), |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Count of [`Self::rows_with_labels`], without materializing the rows.
    fn count_rows_with_labels(&self, labels: &[&str]) -> crate::Result<u64> {
        let conn = self.lock();
        let placeholders: Vec<String> = (1..=labels.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT COUNT(*) FROM nodes WHERE label IN ({})",
            placeholders.join(", ")
        );
        let mut stmt = conn.prepare(&sql)?;
        let args: Vec<&dyn rusqlite::types::ToSql> = labels
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();
        let n: i64 = stmt.query_row(args.as_slice(), |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Rows in `vectors` whose blob is not `dim` floats wide.
    ///
    /// The migration uses this to decide whether the store may *claim* the new
    /// dimension: `vector_search` ignores rows of a foreign width without
    /// reporting it, so any row left here is a row retrieval will never see.
    fn count_vectors_of_other_dim(&self, dim: usize) -> crate::Result<u64> {
        let conn = self.lock();
        let bytes = dim.saturating_mul(4) as i64;
        let n: u64 = conn.query_row(
            "SELECT COUNT(*) FROM vectors WHERE length(embedding) <> ?1",
            params![{ bytes }],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// Every memory row, in whatever order SQLite returns them.
    ///
    /// Deliberately narrower than [`Self::rows_with_labels`]: this is the
    /// memory-browser scope, and `ConversationMessage` rows are excluded so the
    /// panel does not fill with chat transcripts. The embedding migration needs
    /// a wider scope and asks for it explicitly.
    fn all_rows(&self) -> crate::Result<Vec<AdminRow>> {
        self.rows_with_labels(&[
            labels::EPISODIC,
            labels::KNOWLEDGE,
            labels::PROCEDURAL,
            labels::AUTOBIOGRAPHICAL,
        ])
    }

    /// Every row carrying one of `labels`.
    fn rows_with_labels(&self, labels: &[&str]) -> crate::Result<Vec<AdminRow>> {
        let conn = self.lock();
        let placeholders: Vec<String> = (1..=labels.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT id, label, status, props, created_at FROM nodes WHERE label IN ({})",
            placeholders.join(", ")
        );
        let mut stmt = conn.prepare(&sql)?;
        let mapped = stmt.query_map(
            rusqlite::params_from_iter(labels.iter().map(|s| s.to_string())),
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            },
        )?;
        let mut rows = Vec::new();
        for row in mapped {
            let (id, label, status, props, created_at) = row?;
            rows.push(AdminRow {
                id: id as u64,
                label,
                status,
                props: serde_json::from_str(&props)?,
                created_at,
            });
        }
        Ok(rows)
    }

    fn admin_row(&self, node_id: u64) -> crate::Result<Option<AdminRow>> {
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT id, label, status, props, created_at FROM nodes WHERE id = ?1",
                params![node_id as i64],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(id, label, status, props, created_at)| {
            Ok(AdminRow {
                id: id as u64,
                label,
                status,
                props: serde_json::from_str(&props)?,
                created_at,
            })
        })
        .transpose()
    }

    fn total_node_count(&self) -> crate::Result<u64> {
        let conn = self.lock();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE label IN (?1, ?2, ?3, ?4)",
            params![
                labels::EPISODIC,
                labels::KNOWLEDGE,
                labels::PROCEDURAL,
                labels::AUTOBIOGRAPHICAL
            ],
            |r| r.get::<_, i64>(0),
        )? as u64)
    }

    /// `status -> count` across every memory label.
    fn status_counts(&self) -> crate::Result<HashMap<String, u64>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT status, COUNT(*) FROM nodes WHERE label IN (?1, ?2, ?3, ?4) GROUP BY status",
        )?;
        let mapped = stmt.query_map(
            params![
                labels::EPISODIC,
                labels::KNOWLEDGE,
                labels::PROCEDURAL,
                labels::AUTOBIOGRAPHICAL
            ],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )?;
        let mut out = HashMap::new();
        for row in mapped {
            let (status, count) = row?;
            out.insert(status, count as u64);
        }
        Ok(out)
    }

    fn purged_count(&self) -> crate::Result<u64> {
        let conn = self.lock();
        Ok(conn.query_row("SELECT COUNT(*) FROM purge_log", [], |r| r.get::<_, i64>(0))? as u64)
    }

    fn embedding_of(&self, node_id: u64) -> crate::Result<Option<Vec<f32>>> {
        self.load_embedding(node_id)
    }

    fn set_embedding_of(&self, node_id: u64, embedding: &[f32]) -> crate::Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO vectors(node_id, dim, embedding) VALUES (?1, ?2, ?3) \
             ON CONFLICT(node_id) DO UPDATE SET dim = excluded.dim, embedding = excluded.embedding",
            params![
                node_id as i64,
                embedding.len() as i64,
                crate::embedding_to_blob(embedding)
            ],
        )?;
        Ok(())
    }
}

fn paginate(
    rows: Vec<AdminRow>,
    params: &AdminListNodesParams,
    page: u32,
    size: u32,
    cutoff: Option<i64>,
) -> AdminListNodesOutput {
    let keyword = params.keyword.trim().to_lowercase();

    let mut entries: Vec<AdminNodeRecord> = rows
        .into_iter()
        .filter(|row| params.node_type.is_empty() || params.node_type == row.label)
        .filter_map(|row| {
            let record = record_from_row(&row);
            if !keyword.is_empty() && !record.content.to_lowercase().contains(&keyword) {
                return None;
            }
            if !params.sub_type.is_empty() {
                let wanted = params.sub_type.to_lowercase();
                match record.sub_type.as_deref() {
                    Some(found) if found.to_lowercase() == wanted => {}
                    _ => return None,
                }
            }
            if let Some(cutoff) = cutoff
                && record.created_at < cutoff
            {
                return None;
            }
            Some(record)
        })
        .collect();

    // Most recent first, node id as a stable tiebreaker so paging never
    // repeats or drops a row.
    entries.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.node_id.cmp(&a.node_id))
    });

    let total = entries.len() as u64;
    let start = u64::from(page - 1) * u64::from(size);
    let nodes: Vec<AdminNodeRecord> = entries
        .into_iter()
        .skip(start as usize)
        .take(size as usize)
        .collect();

    AdminListNodesOutput {
        total,
        page,
        size,
        nodes,
        rejected_unfiltered: None,
    }
}

/// `""` / `"all"` → no cutoff; `"1h"` / `"1d"` / `"7d"` / `"30d"` → epoch
/// seconds. Unknown values are ignored (and logged) rather than treated as
/// "no filter", so a typo cannot silently widen the scan.
fn time_range_cutoff(range: &str) -> Option<i64> {
    let now = Utc::now();
    let window = match range {
        "" | "all" => return None,
        "1h" => TimeDelta::hours(1),
        "1d" => TimeDelta::days(1),
        "7d" => TimeDelta::days(7),
        "30d" => TimeDelta::days(30),
        other => {
            tracing::warn!(
                time_range = other,
                "memory list: unknown time_range, ignoring filter"
            );
            return None;
        }
    };
    Some((now - window).timestamp())
}

fn record_from_row(row: &AdminRow) -> AdminNodeRecord {
    let props = &row.props;
    AdminNodeRecord {
        node_id: row.id,
        node_type: row.label.clone(),
        sub_type: extract_sub_type(&row.label, props),
        content: crate::provider::render_content(props),
        confidence: f64_prop(props, "confidence"),
        importance: f64_prop(props, "importance"),
        decay_score: f64_prop(props, "decay_score"),
        created_at: parse_timestamp(&row.created_at)
            .or_else(|| timestamp_field(props))
            .map(|ts| ts.timestamp())
            .unwrap_or(0),
        last_accessed_at: prop_timestamp(props, "last_accessed_at")
            .map(|ts| ts.timestamp())
            .unwrap_or(0),
        access_count: props
            .get("access_count")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(u64::from(u32::MAX)) as u32,
        status: if row.status.is_empty() {
            "Active".to_string()
        } else {
            row.status.clone()
        },
    }
}

fn empty_page(page: u32, size: u32) -> AdminListNodesOutput {
    AdminListNodesOutput {
        total: 0,
        page,
        size,
        nodes: Vec::new(),
        rejected_unfiltered: None,
    }
}

fn missing_detail(node_id: u64, message: &str) -> AdminNodeDetail {
    AdminNodeDetail {
        node_id,
        found: false,
        node_type: String::new(),
        sub_type: None,
        content: String::new(),
        confidence: 0.0,
        importance: 0.0,
        decay_score: 0.0,
        created_at: 0,
        last_accessed_at: 0,
        access_count: 0,
        status: String::new(),
        properties: HashMap::new(),
        message: message.to_string(),
    }
}

/// Sub-classification property per label, mirroring the grafeo admin surface.
fn extract_sub_type(label: &str, props: &Value) -> Option<String> {
    let property = match label {
        labels::KNOWLEDGE => "sub_type",
        labels::AUTOBIOGRAPHICAL => "category",
        labels::EPISODIC => "knowledge_subtype",
        _ => return None,
    };
    props
        .get(property)
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// A timestamp property, which admin-created nodes may carry as an RFC3339
/// string where typed nodes store a real timestamp.
fn prop_timestamp(props: &Value, key: &str) -> Option<DateTime<Utc>> {
    let value = props.get(key)?;
    value.as_str().and_then(parse_timestamp).or_else(|| {
        value
            .as_i64()
            .and_then(|secs| DateTime::from_timestamp(secs, 0))
    })
}

/// First of `created_at` / `timestamp` that parses — episodic nodes carry
/// `timestamp`, every other type `created_at`.
fn timestamp_field(props: &Value) -> Option<DateTime<Utc>> {
    ["created_at", "timestamp"]
        .iter()
        .find_map(|key| prop_timestamp(props, key))
}

fn parse_timestamp(text: &str) -> Option<DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|ts| ts.with_timezone(&Utc))
}

fn f64_prop(props: &Value, key: &str) -> f64 {
    props.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

/// Read `PRAGMA user_version` from a held SQLite connection. SQLite stores
/// this as a 32-bit signed integer but the engine bumps it as a u32
/// counter, so we widen to `u64` to match `AdminStats::schema_version`.
///
/// Failures (no DB, pragma rejected) are swallowed and reported as 0 —
/// `user_version` is observability metadata, not a correctness invariant,
/// and a stuck read must not prevent the stats endpoint from returning.
/// One-word summary of what the two health counters below say about the store.
///
/// Was hardcoded to `"healthy"`, which made the field a constant rather than a
/// report: a store whose recorded dimension had been moved over vectors still
/// at the previous width - the exact state a model swap can leave - read back
/// as healthy, because nothing in the string was computed from anything.
/// Foreign width wins over missing embeddings: those rows exist and are
/// silently excluded from search, which is the harder failure to notice.
fn derive_index_health(total_nodes: u64, nodes_with_embedding: u64, stale: u64) -> &'static str {
    if stale > 0 {
        "stale_vectors"
    } else if total_nodes > 0 && nodes_with_embedding < total_nodes {
        "missing_embeddings"
    } else {
        "healthy"
    }
}

fn read_user_version(conn: &rusqlite::Connection) -> u64 {
    conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
        .ok()
        .and_then(|v| u64::try_from(v).ok())
        .unwrap_or(0)
}
