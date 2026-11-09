//! `SqliteSessionMetaStore` — SQLite-backed implementation of the
//! [`acowork_core::session_meta::SessionMetaStore`] trait (ADR-082 §4 step 3).
//!
//! Lives next to the `SqliteStore` memory and `ConversationStore` index
//! tables inside the same `.sqlite` file. The schema lives in
//! [`schema::SCHEMA_SQL`] (the `sessions` table and `fts_sessions` virtual
//! table); this module only knows how to map a [`acowork_core::session_meta::SessionMeta`]
//! row to the columns.
//!
//! # Concurrency
//!
//! `SqliteStore` already serialises writes through a `Mutex<Connection>`.
//! `upsert` and friends go through that lock exactly like the memory
//! methods; there is no second mutex layer.
//!
//! # Sort key
//!
//! `last_active_at` is persisted as INTEGER epoch-ms so
//! `ORDER BY last_active_at DESC` is a numeric scan (no parse on every
//! comparison), and the helper [`acowork_core::session_meta::last_active_at_ms`] converts
//! the RFC3339 string both ways.
//!
//! # Derived index
//!
//! Every write mirrors `session_id` / `title` / `agent_id` / `workspace_id`
//! into `fts_sessions`. [`search`](SessionMetaStore::search) itself still uses
//! `LIKE %x%` (see the note at the bottom of this file) — the FTS table is kept
//! in sync so the upgrade to `MATCH` is a query change, not a backfill.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rusqlite::{Connection, params};

use acowork_core::error::{AcoworkError, Result as AcoworkResult};
use acowork_core::session_meta::{
    IntentBinding, ModelUsageEntry, SessionMeta, SessionMetaStore, SessionVisibility, TokenStatsRow,
    last_active_at_ms, split_model_usage_key,
};

use crate::{Error as SqliteError, Result as SqliteResult, SqliteStore};

/// Per-session `"{provider}|{model}"` breakdown, the decoded form of the
/// `sessions.model_usage` JSON column. Source of truth for `token_stats`.
pub(crate) type ModelUsageMap = BTreeMap<String, ModelUsageEntry>;

/// SQLite implementation of `SessionMetaStore`.
///
/// Cheap to clone — holds an `Arc<SqliteStore>` plus the read-side helpers
/// the trait needs. The store itself owns the connection lock.
#[derive(Clone)]
pub struct SqliteSessionMetaStore {
    store: Arc<SqliteStore>,
}

impl SqliteSessionMetaStore {
    /// Wrap an existing [`SqliteStore`] so callers can hold an
    /// `Arc<dyn SessionMetaStore>` alongside an `Arc<dyn MemoryProvider>`
    /// without a second lock surface.
    pub fn new(store: Arc<SqliteStore>) -> Self {
        Self { store }
    }
}

// ── row ↔ struct ──────────────────────────────────────────────────────

fn created_at_ms(meta: &SessionMeta) -> i64 {
    chrono::DateTime::parse_from_rfc3339(&meta.created_at)
        .map(|dt| dt.with_timezone(&chrono::Utc).timestamp_millis())
        .unwrap_or(0)
}

fn row_to_meta(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionMeta> {
    let created_ms: i64 = row.get("created_at")?;
    let last_ms: i64 = row.get("last_active_at")?;
    let created_at = ms_to_rfc3339(created_ms);
    let last_active_at = ms_to_rfc3339(last_ms);

    let version: i64 = row.get("version")?;
    let corrupted: i64 = row.get("corrupted")?;
    // ADR-076 §决策 4: `NULL` = pre-accounts / local mode.
    let initiator: Option<String> = row.get("initiator")?;
    let visibility_text: Option<String> = row.get("visibility")?;
    let visibility = visibility_text
        .as_deref()
        .and_then(SessionVisibility::from_db_str);
    // Design 27 §8.2: Chat session binding for `Members` visibility.
    let chat_session_id: Option<String> = row.get("chat_session_id")?;
    // ADR-076 §决策 4 (rev.): read-only human awareness. A NULL column (any
    // row written before the migration) reads as an empty list — "no extra
    // readers", the pre-feature behaviour.
    let aware_users_json: Option<String> = row.get("aware_users")?;
    let aware_users: Vec<String> = aware_users_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    // Plan D2: the human interveners. Same NULL tolerance as `aware_users`
    // above - a row written before the column existed grants no intervention.
    let interveners_json: Option<String> = row.get("interveners")?;
    let interveners: Vec<String> = interveners_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();

    let title: Option<String> = row.get("title")?;
    let workspace_id: Option<String> = row.get("workspace_id")?;
    let model: Option<String> = row.get("model")?;
    let provider: Option<String> = row.get("provider")?;
    let account_id: Option<String> = row.get("account_id")?;
    let reasoning_effort: Option<String> = row.get("reasoning_effort")?;
    let temperature: Option<f64> = row.get("temperature")?;
    let context_window: Option<i64> = row.get("context_window")?;
    let todos_json: Option<String> = row.get("todos")?;
    // Per-session tool narrowing. Corrupt JSON reads as "inherit the agent
    // layer" (an empty policy) rather than failing the whole listing — the
    // column is user state written by one endpoint, and a hand-edited row
    // must not make a session unreadable (same lenience as `visibility`).
    let tool_policy_json: Option<String> = row.get("tool_policy")?;
    let message_count: i64 = row.get("message_count")?;
    let llm_call_counter: Option<i64> = row.get("llm_call_counter")?;
    let model_ratio: Option<f64> = row.get("model_ratio")?;
    let last_compaction_offset: Option<i64> = row.get("last_compaction_offset")?;
    // Per-model breakdown (token statistics). An empty/absent map reads as
    // `None` so a session that never made an LLM call is byte-equivalent in
    // the API shape to a pre-feature row.
    let model_usage_json: Option<String> = row.get("model_usage")?;
    let model_usage_map = parse_model_usage(model_usage_json.as_deref());
    let model_usage = (!model_usage_map.is_empty()).then_some(model_usage_map);

    let token_last_input: i64 = row.get("token_last_input")?;
    let token_last_output: i64 = row.get("token_last_output")?;
    let token_total_input: i64 = row.get("token_total_input")?;
    let token_total_output: i64 = row.get("token_total_output")?;
    let token_last_cache_read: i64 = row.get("token_last_cache_read")?;
    let token_last_cache_write: i64 = row.get("token_last_cache_write")?;
    let token_total_cache_read: i64 = row.get("token_total_cache_read")?;
    let token_total_cache_write: i64 = row.get("token_total_cache_write")?;

    let tokens = if token_last_input == 0
        && token_last_output == 0
        && token_total_input == 0
        && token_total_output == 0
        && token_last_cache_read == 0
        && token_last_cache_write == 0
        && token_total_cache_read == 0
        && token_total_cache_write == 0
    {
        None
    } else {
        Some(acowork_core::session_meta::SessionTokens {
            last_input: token_last_input.max(0) as u64,
            last_output: token_last_output.max(0) as u64,
            total_input: token_total_input.max(0) as u64,
            total_output: token_total_output.max(0) as u64,
            last_cache_read: token_last_cache_read.max(0) as u64,
            last_cache_write: token_last_cache_write.max(0) as u64,
            total_cache_read: token_total_cache_read.max(0) as u64,
            total_cache_write: token_total_cache_write.max(0) as u64,
        })
    };

    let todos = match todos_json.as_deref() {
        Some(s) => serde_json::from_str::<Vec<acowork_core::session_meta::TodoItem>>(s).ok(),
        None => None,
    };

    // `NULL` and "an empty policy" mean the same thing — inherit the agent
    // layer — so a decoded-but-empty object is normalised to `None` and the
    // store never carries two spellings of one state.
    let tool_policy = tool_policy_json
        .as_deref()
        .and_then(|s| {
            serde_json::from_str::<acowork_core::session_meta::SessionToolPolicy>(s).ok()
        })
        .filter(|p| !p.is_empty());

    // A row that never produced a measurement reads as `None` (frontend
    // shows "—") rather than a fabricated 0 — the same "宁可 miss 也不估计"
    // rule the token counters follow. The two figures keep separate sample
    // counts, so a speed sum with zero counts is not a mean of anything.
    let perf_tps_sum: Option<f64> = row.get("perf_tps_sum")?;
    let perf_tps_samples: Option<i64> = row.get("perf_tps_samples")?;
    let perf_ttft_ms_sum: Option<f64> = row.get("perf_ttft_ms_sum")?;
    let perf_ttft_ms_samples: Option<i64> = row.get("perf_ttft_ms_samples")?;
    let perf = match (
        perf_tps_sum,
        perf_tps_samples,
        perf_ttft_ms_sum,
        perf_ttft_ms_samples,
    ) {
        (None, None, None, None) => None,
        (tps_sum, tps_n, ttft_sum, ttft_n) => {
            let perf = acowork_core::session_meta::SessionPerf {
                tps_sum: tps_sum.unwrap_or(0.0).max(0.0),
                tps_samples: tps_n.unwrap_or(0).clamp(0, u32::MAX as i64) as u32,
                ttft_ms_sum: ttft_sum.unwrap_or(0.0).max(0.0),
                ttft_ms_samples: ttft_n.unwrap_or(0).clamp(0, u32::MAX as i64) as u32,
            };
            // An all-NULL row and an empty accumulator mean the same thing.
            if perf.is_empty() { None } else { Some(perf) }
        }
    };

    Ok(SessionMeta {
        version: version.max(0) as u32,
        session_id: row.get("session_id")?,
        agent_id: row.get("agent_id")?,
        created_at,
        initiator,
        visibility,
        chat_session_id,
        aware_users,
        interveners,
        title,
        workspace_id,
        model,
        provider,
        account_id,
        reasoning_effort,
        temperature: temperature.map(|t| t as f32),
        context_window: context_window.map(|c| c.max(0) as u64),
        todos,
        tool_policy,
        message_count: message_count.max(0) as u64,
        last_active_at,
        tokens,
        model_usage,
        llm_call_counter: llm_call_counter.map(|c| c.max(0) as u32),
        perf,
        model_ratio,
        last_compaction_offset: last_compaction_offset.map(|o| o.max(0) as u64),
        corrupted: corrupted != 0,
    })
}

pub(crate) fn ms_to_rfc3339(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| "1970-01-01T00:00:00.000Z".to_string())
}

/// Convert the SQLite crate's local error into the workspace-wide
/// `AcoworkError`. Centralised so the trait impl stays one-line per call.
pub(crate) fn to_acowork(e: SqliteError) -> AcoworkError {
    AcoworkError::Memory(e.to_string())
}

pub(crate) fn to_acowork_sqlite(e: rusqlite::Error) -> AcoworkError {
    AcoworkError::Memory(format!("sqlite: {e}"))
}

// ── trait impl ────────────────────────────────────────────────────────

impl SessionMetaStore for SqliteSessionMetaStore {
    fn get(&self, session_id: &str) -> AcoworkResult<Option<SessionMeta>> {
        let conn = self.store.lock();
        match conn.query_row(SELECT_ALL_COLUMNS, params![session_id], row_to_meta) {
            Ok(row) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(to_acowork_sqlite(e)),
        }
    }

    fn upsert(&self, meta: &SessionMeta) -> AcoworkResult<()> {
        // Single transaction: the row replace and the `token_stats` diff
        // commit together, so a crash between the two can never let the
        // aggregate drift from the breakdowns. The diff is recomputed
        // against the PERSISTED old row, which makes the maintenance
        // idempotent — a lost or replayed delta is corrected by the next
        // upsert's diff, and `rebuild_token_stats` remains the repair path.
        let mut conn = self.store.lock();
        let result: SqliteResult<()> = (|| {
            let tx = conn.transaction()?;
            let old_usage: Option<String> = tx
                .query_row(
                    "SELECT model_usage FROM sessions WHERE session_id = ?1",
                    params![meta.session_id],
                    |r| r.get(0),
                )
                .unwrap_or(None);
            upsert_row(&tx, meta)?;
            tx.execute(
                "DELETE FROM fts_sessions WHERE session_id = ?1",
                params![meta.session_id],
            )?;
            tx.execute(
                "INSERT INTO fts_sessions(session_id, title, agent_id, workspace_id) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    meta.session_id,
                    meta.title.as_deref().unwrap_or(""),
                    meta.agent_id,
                    meta.workspace_id.as_deref().unwrap_or(""),
                ],
            )?;
            apply_stats_delta(
                &tx,
                &parse_model_usage(old_usage.as_deref()),
                &meta.model_usage.clone().unwrap_or_default(),
            )?;
            tx.commit()?;
            Ok(())
        })();
        result.map_err(to_acowork)
    }

    fn delete(&self, session_id: &str) -> AcoworkResult<()> {
        let mut conn = self.store.lock();
        let result: SqliteResult<()> = (|| {
            let tx = conn.transaction()?;
            // Subtract the deleted row's breakdown from the aggregate in the
            // same transaction — symmetric with `upsert`, so pruned sessions
            // stop weighing on the model-share panel.
            let old_usage: Option<String> = tx
                .query_row(
                    "SELECT model_usage FROM sessions WHERE session_id = ?1",
                    params![session_id],
                    |r| r.get(0),
                )
                .unwrap_or(None);
            tx.execute(
                "DELETE FROM sessions WHERE session_id = ?1",
                params![session_id],
            )?;
            tx.execute(
                "DELETE FROM fts_sessions WHERE session_id = ?1",
                params![session_id],
            )?;
            apply_stats_delta(&tx, &parse_model_usage(old_usage.as_deref()), &ModelUsageMap::new())?;
            tx.commit()?;
            Ok(())
        })();
        result.map_err(to_acowork)
    }

    fn list_recent(&self, limit: usize) -> AcoworkResult<Vec<SessionMeta>> {
        let conn = self.store.lock();
        let sql = format!(
            "{SELECT_ALL_COLUMNS_PREFIX} ORDER BY last_active_at DESC LIMIT ?1"
        );
        let mut stmt = conn.prepare(&sql).map_err(to_acowork_sqlite)?;
        let rows = stmt
            .query_map(params![limit as i64], row_to_meta)
            .map_err(to_acowork_sqlite)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(to_acowork_sqlite)?);
        }
        Ok(out)
    }

    fn find_latest(&self) -> AcoworkResult<Option<SessionMeta>> {
        let conn = self.store.lock();
        let sql = format!("{SELECT_ALL_COLUMNS_PREFIX} ORDER BY last_active_at DESC LIMIT 1");
        let mut stmt = conn.prepare(&sql).map_err(to_acowork_sqlite)?;
        let mut rows = stmt
            .query_map([], row_to_meta)
            .map_err(to_acowork_sqlite)?;
        match rows.next() {
            Some(Ok(row)) => Ok(Some(row)),
            Some(Err(e)) => Err(to_acowork_sqlite(e)),
            None => Ok(None),
        }
    }

    fn search(&self, query: &str, limit: usize) -> AcoworkResult<Vec<SessionMeta>> {
        if query.trim().is_empty() {
            // Match the JSON backend: empty query degenerates to list_recent.
            // We can't call `self.list_recent` while still holding the lock
            // here — `list_recent` re-locks and the Mutex is not reentrant.
            let conn = self.store.lock();
            let sql = format!(
                "{SELECT_ALL_COLUMNS_PREFIX} ORDER BY last_active_at DESC LIMIT ?1"
            );
            let mut stmt = conn.prepare(&sql).map_err(to_acowork_sqlite)?;
            let rows = stmt
                .query_map(params![limit as i64], row_to_meta)
                .map_err(to_acowork_sqlite)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(to_acowork_sqlite)?);
            }
            return Ok(out);
        }
        let conn = self.store.lock();
        let needle = format!("%{}%", query);
        let sql = format!(
            "{SELECT_ALL_COLUMNS_PREFIX} \
             WHERE title LIKE ?1 COLLATE NOCASE \
                OR agent_id LIKE ?1 COLLATE NOCASE \
                OR workspace_id LIKE ?1 COLLATE NOCASE \
             ORDER BY last_active_at DESC LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql).map_err(to_acowork_sqlite)?;
        let rows = stmt
            .query_map(params![needle, limit as i64], row_to_meta)
            .map_err(to_acowork_sqlite)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(to_acowork_sqlite)?);
        }
        Ok(out)
    }

    fn prune_to(&self, max_sessions: usize) -> AcoworkResult<Vec<String>> {
        let mut conn = self.store.lock();
        let result: SqliteResult<Vec<String>> = (|| {
            let tx = conn.transaction()?;
            // ADR-076 §决策 4: the cap is **per owner**, not global — one
            // account's busy week must not evict another account's history.
            // `COALESCE(initiator, '')` puts every ownerless (pre-accounts /
            // local-mode) row in a single bucket, which is exactly the
            // pre-ADR-076 behaviour for those rows.
            let mut stmt = tx.prepare(
                "SELECT session_id FROM sessions \
                 WHERE session_id NOT IN ( \
                       SELECT session_id FROM ( \
                         SELECT session_id, \
                                ROW_NUMBER() OVER ( \
                                  PARTITION BY COALESCE(initiator, '') \
                                  ORDER BY last_active_at DESC \
                                ) AS rn \
                         FROM sessions \
                       ) WHERE rn <= ?1 \
                     )",
            )?;
            let victims: Vec<String> = stmt
                .query_map(params![max_sessions as i64], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(stmt);
            if !victims.is_empty() {
                let placeholders = std::iter::repeat_n("?", victims.len())
                    .collect::<Vec<_>>()
                    .join(",");
                // Read the victims' breakdowns BEFORE deleting, so the
                // aggregate subtraction happens in the same transaction
                // (symmetric with `delete`).
                let usage_sql =
                    format!("SELECT model_usage FROM sessions WHERE session_id IN ({placeholders})");
                let params_vec: Vec<&dyn rusqlite::ToSql> =
                    victims.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
                let mut usage_stmt = tx.prepare(&usage_sql)?;
                let usages: Vec<Option<String>> = usage_stmt
                    .query_map(params_vec.as_slice(), |r| r.get::<_, Option<String>>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                drop(usage_stmt);
                let sql = format!("DELETE FROM sessions WHERE session_id IN ({placeholders})");
                tx.execute(&sql, params_vec.as_slice())?;
                let fts_sql =
                    format!("DELETE FROM fts_sessions WHERE session_id IN ({placeholders})");
                tx.execute(&fts_sql, params_vec.as_slice())?;
                for usage in &usages {
                    apply_stats_delta(&tx, &parse_model_usage(usage.as_deref()), &ModelUsageMap::new())?;
                }
            }
            tx.commit()?;
            Ok(victims)
        })();
        result.map_err(to_acowork)
    }

    fn token_stats(&self) -> AcoworkResult<Vec<TokenStatsRow>> {
        // O(#models) — one row per (provider, model) pair, never a session
        // scan. This is the whole point of maintaining the aggregate.
        let conn = self.store.lock();
        let mut stmt = conn
            .prepare(
                "SELECT provider, model, total_input, total_output, \n                 total_cache_read, total_cache_write, total_cost \n                 FROM token_stats ORDER BY (total_input + total_output) DESC",
            )
            .map_err(to_acowork_sqlite)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(TokenStatsRow {
                    provider: r.get(0)?,
                    model: r.get(1)?,
                    total_input: r.get::<_, i64>(2)?.max(0) as u64,
                    total_output: r.get::<_, i64>(3)?.max(0) as u64,
                    total_cache_read: r.get::<_, i64>(4)?.max(0) as u64,
                    total_cache_write: r.get::<_, i64>(5)?.max(0) as u64,
                    total_cost: r.get(6)?,
                })
            })
            .map_err(to_acowork_sqlite)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(to_acowork_sqlite)
    }

    fn rebuild_token_stats(&self) -> AcoworkResult<()> {
        // Repair path (disposable derived index, ADR-082 pattern): drop the
        // aggregate and re-derive it from the per-session breakdowns. Cost
        // follows the same poison rule as the incremental diff — any entry
        // without a known price makes the row's cost NULL. The Chat ledger
        // contributes through the same fold (schema v9): the aggregate's
        // source of truth is `sessions.model_usage` UNION
        // `chat_token_usage.model_usage`, matching `token_stats_consistent`.
        let mut conn = self.store.lock();
        let result: SqliteResult<()> = (|| {
            let tx = conn.transaction()?;
            tx.execute("DELETE FROM token_stats", [])?;
            let mut stmt =
                tx.prepare("SELECT model_usage FROM sessions WHERE model_usage IS NOT NULL \n                 UNION ALL \n                 SELECT model_usage FROM chat_token_usage WHERE model_usage IS NOT NULL")?;
            let jsons: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(stmt);
            /// (provider, model) -> (in, out, cache_read, cache_write, cost)
            /// fold; cost is `None` once any contribution is unpriced.
            type Fold = BTreeMap<(String, String), (i64, i64, i64, i64, Option<f64>)>;
            let mut acc: Fold = BTreeMap::new();
            for json in &jsons {
                for (key, entry) in parse_model_usage(Some(json.as_str())) {
                    let (p, m) = split_model_usage_key(&key);
                    let slot = acc
                        .entry((p.to_string(), m.to_string()))
                        .or_insert((0, 0, 0, 0, Some(0.0)));
                    slot.0 = slot.0.saturating_add(token_to_i64(entry.input));
                    slot.1 = slot.1.saturating_add(token_to_i64(entry.output));
                    slot.2 = slot.2.saturating_add(token_to_i64(entry.cache_read));
                    slot.3 = slot.3.saturating_add(token_to_i64(entry.cache_write));
                    slot.4 = match (slot.4, entry.cost) {
                        (Some(sum), Some(c)) => Some(sum + c),
                        _ => None,
                    };
                }
            }
            for ((p, m), (i, o, cr, cw, cost)) in acc {
                tx.execute(
                    "INSERT INTO token_stats (provider, model, total_input, total_output, \n                     total_cache_read, total_cache_write, total_cost) \n                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![p, m, i, o, cr, cw, cost],
                )?;
            }
            tx.commit()?;
            Ok(())
        })();
        result.map_err(to_acowork)
    }

    fn token_stats_consistent(&self) -> AcoworkResult<bool> {
        // One pass over each side, no row transport: the aggregate's column
        // sums vs a `json_each` fold of every session's AND every Chat
        // ledger row's `model_usage` breakdown (schema v9 — the ledger
        // feeds the aggregate through the same diff as sessions, so it
        // must count on the detail side too, or the check would fail on
        // the first router turn). Cheap enough to run on every token-stats
        // read.
        //
        // Cost is not compared token-style: the aggregate poisons a row's
        // cost to NULL when any contribution is unpriced, and that NULL
        // stays valid until `rebuild_token_stats` — so NULL aggregate cost
        // counts as consistent, and a priced aggregate must match Σ details.
        let conn = self.store.lock();
        /// One row of eleven scalars: the aggregate's four token sums, its
        /// cost fold, the same five folded off the per-session breakdowns,
        /// plus the aggregate row count.
        type Sums = (
            i64,
            i64,
            i64,
            i64,
            Option<f64>,
            i64,
            i64,
            i64,
            i64,
            Option<f64>,
            i64,
        );
        let (agg_i, agg_o, agg_cr, agg_cw, agg_cost, det_i, det_o, det_cr, det_cw, det_cost, agg_rows): Sums = conn
            .query_row(
                r#"SELECT
                    (SELECT COALESCE(SUM(total_input), 0)  FROM token_stats),
                    (SELECT COALESCE(SUM(total_output), 0) FROM token_stats),
                    (SELECT COALESCE(SUM(total_cache_read), 0)  FROM token_stats),
                    (SELECT COALESCE(SUM(total_cache_write), 0) FROM token_stats),
                    (SELECT SUM(total_cost) FROM token_stats),
                    (SELECT COALESCE(SUM(json_extract(j.value, '$.input')), 0)
                     FROM (SELECT model_usage FROM sessions WHERE model_usage IS NOT NULL
                           UNION ALL
                           SELECT model_usage FROM chat_token_usage WHERE model_usage IS NOT NULL) u,
                          json_each(u.model_usage) j),
                    (SELECT COALESCE(SUM(json_extract(j.value, '$.output')), 0)
                     FROM (SELECT model_usage FROM sessions WHERE model_usage IS NOT NULL
                           UNION ALL
                           SELECT model_usage FROM chat_token_usage WHERE model_usage IS NOT NULL) u,
                          json_each(u.model_usage) j),
                    (SELECT COALESCE(SUM(json_extract(j.value, '$.cache_read')), 0)
                     FROM (SELECT model_usage FROM sessions WHERE model_usage IS NOT NULL
                           UNION ALL
                           SELECT model_usage FROM chat_token_usage WHERE model_usage IS NOT NULL) u,
                          json_each(u.model_usage) j),
                    (SELECT COALESCE(SUM(json_extract(j.value, '$.cache_write')), 0)
                     FROM (SELECT model_usage FROM sessions WHERE model_usage IS NOT NULL
                           UNION ALL
                           SELECT model_usage FROM chat_token_usage WHERE model_usage IS NOT NULL) u,
                          json_each(u.model_usage) j),
                    (SELECT SUM(json_extract(j.value, '$.cost'))
                     FROM (SELECT model_usage FROM sessions WHERE model_usage IS NOT NULL
                           UNION ALL
                           SELECT model_usage FROM chat_token_usage WHERE model_usage IS NOT NULL) u,
                          json_each(u.model_usage) j),
                    (SELECT COUNT(*) FROM token_stats)"#,
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                        r.get(10)?,
                    ))
                },
            )
            .map_err(to_acowork_sqlite)?;
        let tokens_ok =
            agg_i == det_i && agg_o == det_o && agg_cr == det_cr && agg_cw == det_cw;
        // Empty aggregate with empty details is consistent; empty aggregate
        // over non-empty details only slips past the token equality when
        // every detail contribution is zero — guard the cost fold anyway.
        let cost_ok = match agg_cost {
            None => agg_rows > 0 || det_i + det_o + det_cr + det_cw == 0,
            Some(a) => {
                let d = det_cost.unwrap_or(0.0);
                (a - d).abs() <= (a.abs() + d.abs()) * 1e-6
            }
        };
        Ok(tokens_ok && cost_ok)
    }

    // ── intent routing (ADR-106 D11) ──────────────────────────────────
    //
    // `upsert_row` deliberately does not list `intent_key` / `intent_closed` in
    // its `ON CONFLICT DO UPDATE SET`, so these statements are the only writers
    // and a session's own meta write is never in their way.

    fn intent_lookup(&self, key: &str) -> AcoworkResult<Option<String>> {
        let conn = self.store.lock();
        // Newest holder wins: a reopened task can bind a second session to the
        // same context, and the older one stays on disk as history.
        match conn.query_row(
            "SELECT session_id FROM sessions \n             WHERE intent_key = ?1 AND intent_closed = 0 \n             ORDER BY last_active_at DESC LIMIT 1",
            params![key],
            |r| r.get::<_, String>(0),
        ) {
            Ok(sid) => Ok(Some(sid)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(to_acowork_sqlite(e)),
        }
    }

    fn intent_bind(&self, session_id: &str, key: &str) -> AcoworkResult<()> {
        let conn = self.store.lock();
        conn.execute(
            "UPDATE sessions SET intent_key = ?2 WHERE session_id = ?1",
            params![session_id, key],
        )
        .map_err(to_acowork_sqlite)?;
        Ok(())
    }

    fn intent_close(&self, key: &str) -> AcoworkResult<Vec<String>> {
        let conn = self.store.lock();
        // Read the ids first, then mark: the caller closes exactly the sessions
        // it reports, so a row that changed under the statement can never be
        // announced as closed while it still reads open.
        let mut stmt = conn
            .prepare(
                "SELECT session_id FROM sessions \n                 WHERE intent_key = ?1 AND intent_closed = 0",
            )
            .map_err(to_acowork_sqlite)?;
        let ids: Vec<String> = stmt
            .query_map(params![key], |r| r.get::<_, String>(0))
            .map_err(to_acowork_sqlite)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(to_acowork_sqlite)?;
        drop(stmt);
        if ids.is_empty() {
            return Ok(ids);
        }
        conn.execute(
            "UPDATE sessions SET intent_closed = 1 WHERE intent_key = ?1",
            params![key],
        )
        .map_err(to_acowork_sqlite)?;
        Ok(ids)
    }

    fn intent_reopen(&self, session_id: &str) -> AcoworkResult<()> {
        let conn = self.store.lock();
        conn.execute(
            "UPDATE sessions SET intent_closed = 0 WHERE session_id = ?1",
            params![session_id],
        )
        .map_err(to_acowork_sqlite)?;
        Ok(())
    }

    fn intent_bindings(&self) -> AcoworkResult<Vec<IntentBinding>> {
        let conn = self.store.lock();
        // Only bound rows are of interest: every unbound session is invisible to
        // the routing index, so this stays proportional to task/conv contexts,
        // not to the session count (ADR-096).
        let mut stmt = conn
            .prepare(
                "SELECT session_id, intent_key, intent_closed FROM sessions \n                 WHERE intent_key IS NOT NULL",
            )
            .map_err(to_acowork_sqlite)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(IntentBinding {
                    session_id: r.get(0)?,
                    intent_key: r.get(1)?,
                    closed: r.get::<_, i64>(2)? != 0,
                })
            })
            .map_err(to_acowork_sqlite)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(to_acowork_sqlite)
    }
}

// ── SQL building blocks ───────────────────────────────────────────────

const SELECT_ALL_COLUMNS: &str = "SELECT \
    session_id, agent_id, created_at, last_active_at, \
    version, corrupted, initiator, visibility, \
    title, workspace_id, model, provider, account_id, reasoning_effort, \
    temperature, context_window, todos, message_count, \
    llm_call_counter, model_ratio, last_compaction_offset, \
    token_last_input, token_last_output, token_total_input, token_total_output, \
    token_last_cache_read, token_last_cache_write, token_total_cache_read, token_total_cache_write, \
    perf_tps_sum, perf_tps_samples, perf_ttft_ms_sum, perf_ttft_ms_samples, chat_session_id, aware_users, model_usage, tool_policy, interveners \
FROM sessions WHERE session_id = ?1";

const SELECT_ALL_COLUMNS_PREFIX: &str = "SELECT \
    session_id, agent_id, created_at, last_active_at, \
    version, corrupted, initiator, visibility, \
    title, workspace_id, model, provider, account_id, reasoning_effort, \
    temperature, context_window, todos, message_count, \
    llm_call_counter, model_ratio, last_compaction_offset, \
    token_last_input, token_last_output, token_total_input, token_total_output, \
    token_last_cache_read, token_last_cache_write, token_total_cache_read, token_total_cache_write, \
    perf_tps_sum, perf_tps_samples, perf_ttft_ms_sum, perf_ttft_ms_samples, chat_session_id, aware_users, model_usage, tool_policy, interveners \
FROM sessions";

/// Clamp a `u64` token counter into SQLite's `i64` column.
///
/// ponytail: the `sessions` token columns are `INTEGER` (i64), so a counter
/// above `i64::MAX` would wrap negative through `as i64` and then be clamped to
/// 0 on read — silent data loss. Saturating at the storage ceiling keeps the
/// value monotone instead. Ceiling: ~9.2e18 tokens; the upgrade path is a
/// `TEXT` column, not worth it for counters that move in the thousands.
pub(crate) fn token_to_i64(v: u64) -> i64 {
    v.min(i64::MAX as u64) as i64
}

fn upsert_row(
    conn: &rusqlite::Connection,
    meta: &SessionMeta,
) -> rusqlite::Result<()> {
    let tokens = meta.tokens.clone().unwrap_or_default();
    let todos_json = meta
        .todos
        .as_ref()
        .map(|t| serde_json::to_string(t).unwrap_or_else(|_| "[]".to_string()));
    // An empty policy (or none at all) writes NULL: "inherit the agent
    // layer" has exactly one on-disk spelling, so clearing the last switch
    // returns the row to the state a never-toggled session is in.
    let tool_policy_json = meta
        .tool_policy
        .as_ref()
        .filter(|p| !p.is_empty())
        .map(|p| serde_json::to_string(p).unwrap_or_else(|_| "null".to_string()));

    // ADR-076 §决策 4 (rev.): the awareness list is stored as a JSON array; an
    // empty list writes NULL so the column matches "no extra readers".
    let aware_users_json = if meta.aware_users.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&meta.aware_users).unwrap_or_else(|_| "[]".to_string()))
    };
    // Same shape for the intervention roster: empty writes NULL, so an
    // untouched row stays identical to a pre-feature one.
    let interveners_json = if meta.interveners.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&meta.interveners).unwrap_or_else(|_| "[]".to_string()))
    };
    conn.execute(
        "INSERT INTO sessions( \
            session_id, agent_id, created_at, last_active_at, \
            title, workspace_id, model, provider, account_id, reasoning_effort, \
            temperature, context_window, todos, message_count, \
            llm_call_counter, model_ratio, last_compaction_offset, \
            token_last_input, token_last_output, token_total_input, token_total_output, \
            token_last_cache_read, token_last_cache_write, token_total_cache_read, token_total_cache_write, \
            perf_tps_sum, perf_tps_samples, perf_ttft_ms_sum, perf_ttft_ms_samples, \
            version, corrupted, initiator, visibility, chat_session_id, aware_users, model_usage, tool_policy, interveners \
         ) VALUES ( \
            ?1, ?2, ?3, ?4, \
            ?5, ?6, ?7, ?8, ?9, ?10, \
            ?11, ?12, ?13, ?14, \
            ?15, ?16, ?17, \
            ?18, ?19, ?20, ?21, \
            ?22, ?23, ?24, ?25, \
            ?26, ?27, ?28, ?29, \
            ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38 \
         ) \
         ON CONFLICT(session_id) DO UPDATE SET \
            agent_id = excluded.agent_id, \
            created_at = excluded.created_at, \
            last_active_at = excluded.last_active_at, \
            title = excluded.title, \
            workspace_id = excluded.workspace_id, \
            model = excluded.model, \
            provider = excluded.provider, \
            account_id = excluded.account_id, \
            reasoning_effort = excluded.reasoning_effort, \
            temperature = excluded.temperature, \
            context_window = excluded.context_window, \
            todos = excluded.todos, \
            message_count = excluded.message_count, \
            llm_call_counter = excluded.llm_call_counter, \
            model_ratio = excluded.model_ratio, \
            last_compaction_offset = excluded.last_compaction_offset, \
            token_last_input = excluded.token_last_input, \
            token_last_output = excluded.token_last_output, \
            token_total_input = excluded.token_total_input, \
            token_total_output = excluded.token_total_output, \
            token_last_cache_read = excluded.token_last_cache_read, \
            token_last_cache_write = excluded.token_last_cache_write, \
            token_total_cache_read = excluded.token_total_cache_read, \
            token_total_cache_write = excluded.token_total_cache_write, \
            perf_tps_sum = excluded.perf_tps_sum, \
            perf_tps_samples = excluded.perf_tps_samples, \
            perf_ttft_ms_sum = excluded.perf_ttft_ms_sum, \
            perf_ttft_ms_samples = excluded.perf_ttft_ms_samples, \
            version = excluded.version, \
            corrupted = excluded.corrupted, \
            initiator = excluded.initiator, \
            visibility = excluded.visibility, \
            chat_session_id = excluded.chat_session_id, \
            aware_users = excluded.aware_users, \
            model_usage = excluded.model_usage, \
            tool_policy = excluded.tool_policy, \
            interveners = excluded.interveners",
        params![
            meta.session_id,
            meta.agent_id,
            created_at_ms(meta),
            last_active_at_ms(meta),
            meta.title,
            meta.workspace_id,
            meta.model,
            meta.provider,
            meta.account_id,
            meta.reasoning_effort,
            meta.temperature,
            meta.context_window.map(|c| c as i64),
            todos_json,
            meta.message_count as i64,
            meta.llm_call_counter.map(|c| c as i64),
            meta.model_ratio,
            meta.last_compaction_offset.map(|o| o as i64),
            token_to_i64(tokens.last_input),
            token_to_i64(tokens.last_output),
            token_to_i64(tokens.total_input),
            token_to_i64(tokens.total_output),
            token_to_i64(tokens.last_cache_read),
            token_to_i64(tokens.last_cache_write),
            token_to_i64(tokens.total_cache_read),
            token_to_i64(tokens.total_cache_write),
            // Perf is stored as sum + count per figure. `None` writes four
            // NULLs, which `row_to_meta` reads back as `None` — the "absent,
            // not zero" round-trip the token counters follow.
            meta.perf.as_ref().map(|p| p.tps_sum),
            meta.perf.as_ref().map(|p| p.tps_samples as i64),
            meta.perf.as_ref().map(|p| p.ttft_ms_sum),
            meta.perf.as_ref().map(|p| p.ttft_ms_samples as i64),
            meta.version as i64,
            meta.corrupted as i64,
            meta.initiator,
            meta.visibility.map(|v| v.as_str()),
            meta.chat_session_id,
            aware_users_json,
            serialize_model_usage(meta.model_usage.as_ref()),
            tool_policy_json,
            interveners_json,
        ],
    )?;
    Ok(())
}

// ── token statistics helpers ────────────────────────────────────────────

/// Decode the `model_usage` JSON column. Corrupt JSON reads as "no
/// breakdown": the column is derived-adjacent state and a hand-edited row
/// must not fail the whole listing (same lenience as `visibility`).
pub(crate) fn parse_model_usage(json: Option<&str>) -> ModelUsageMap {
    json.and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default()
}

/// Encode a breakdown. The empty map is stored as NULL so a session that
/// never made an LLM call is indistinguishable from a pre-feature row.
pub(crate) fn serialize_model_usage(map: Option<&ModelUsageMap>) -> Option<String> {
    map.filter(|m| !m.is_empty())
        .and_then(|m| serde_json::to_string(m).ok())
}

/// Fold one row's breakdown change (`delta = new − old`, per key) into the
/// agent-scoped `token_stats` aggregate.
///
/// The diff is computed against the PERSISTED old breakdown, not against
/// "what this caller thinks was there before" — which is what makes the
/// maintenance idempotent and self-healing: a lost delta (crash between two
/// writes) or a replayed one (retry) is corrected by the next upsert, and
/// the aggregate always equals Σ(breakdowns) after any successful write.
///
/// Cost follows the "宁可 miss 也不估计" poison rule: a delta whose cost
/// cannot be computed (either side lacks a price) sets the aggregate row's
/// `total_cost` to NULL, where it stays until [`Self::rebuild_token_stats`]
/// re-derives from rows that individually know their cost. A fabricated 0
/// would read as "free".
pub(crate) fn apply_stats_delta(
    conn: &Connection,
    old: &ModelUsageMap,
    new: &ModelUsageMap,
) -> rusqlite::Result<()> {
    let keys: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    for key in keys {
        let o = old.get(key);
        let n = new.get(key);
        let delta = |get: fn(&ModelUsageEntry) -> u64| {
            token_to_i64(n.map(get).unwrap_or(0)) - token_to_i64(o.map(get).unwrap_or(0))
        };
        let (d_in, d_out) = (delta(|e| e.input), delta(|e| e.output));
        let (d_cr, d_cw) = (delta(|e| e.cache_read), delta(|e| e.cache_write));
        // Skip no-op keys: re-applying a zero delta would poison the cost
        // row even where nothing changed.
        let same_cost = o.map(|e| e.cost) == n.map(|e| e.cost);
        if d_in == 0 && d_out == 0 && d_cr == 0 && d_cw == 0 && same_cost {
            continue;
        }
        // A cost delta is only computable when both sides carry a price (or
        // the key is brand new and the previous aggregate never saw it, or
        // a priced key is removed — delete/prune must subtract the known
        // cost, not poison the row that other priced sessions still share).
        let d_cost: Option<f64> = match (o.map(|e| e.cost), n.map(|e| e.cost)) {
            (Some(Some(oc)), Some(Some(nc))) => Some(nc - oc),
            (None, Some(Some(nc))) => Some(nc),
            (Some(Some(oc)), None) => Some(-oc),
            _ => None,
        };
        let (provider, model) = split_model_usage_key(key);
        conn.execute(
            "INSERT INTO token_stats (provider, model, total_input, total_output, \n             total_cache_read, total_cache_write, total_cost) \n             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \n             ON CONFLICT(provider, model) DO UPDATE SET \n                total_input = total_input + excluded.total_input, \n                total_output = total_output + excluded.total_output, \n                total_cache_read = total_cache_read + excluded.total_cache_read, \n                total_cache_write = total_cache_write + excluded.total_cache_write, \n                total_cost = CASE \n                    WHEN token_stats.total_cost IS NULL OR excluded.total_cost IS NULL THEN NULL \n                    ELSE token_stats.total_cost + excluded.total_cost \n                END",
            params![provider, model, d_in, d_out, d_cr, d_cw, d_cost],
        )?;
    }
    Ok(())
}

// ponytail: search uses `LIKE %x%` rather than FTS5 because session titles
// are short and the dataset is small (a workspace has dozens, not millions).
// The index on `last_active_at` is what keeps the dashboard fast; trigram
// over a few hundred rows is overhead, not win. Upgrade path: switch to
// `fts_sessions MATCH ?` once a workspace exceeds ~10k sessions.