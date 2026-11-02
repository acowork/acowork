//! `SqliteChatUsageStore` — the Chat Router token ledger (design 29 /
//! ADR-099 follow-up).
//!
//! One aggregate row per Chat session in the `chat_token_usage` table
//! (schema v9): cumulative token counters and the `"{provider}|{model}"`
//! breakdown, in exactly the shape `sessions` uses. A Chat session is NOT
//! an execution session — this row carries no conversation state, only
//! spend attributed to the chat. The breakdown feeds the agent-scoped
//! `token_stats` aggregate through the same same-transaction diff as
//! session rows (`apply_stats_delta`), so the model-share panel accounts
//! for router spend without the sessions table gaining a fake-session
//! entity.
//!
//! The fold rules are the shared ADR-027/066 primitives in
//! `acowork_core::session_meta` (`SessionTokens::folded_with_call`,
//! `fold_model_usage_entry`) — the single owner that keeps
//! "Σ detail == totals" true across sessions, session detail, and this
//! ledger.
//!
//! # Concurrency
//!
//! Same as [`crate::SqliteSessionMetaStore`]: writes go through the one
//! `Mutex<Connection>` the shared `SqliteStore` owns. `record_call` is a
//! single transaction (read-modify-write + aggregate diff), so two router
//! turns of different chats can never interleave a half-folded row.

use std::sync::Arc;

use acowork_core::error::Result as AcoworkResult;
use acowork_core::providers::traits::UsageInfo;
use acowork_core::session_meta::{
    fold_model_usage_entry, model_usage_key, SessionTokens, SessionTotals,
};
use rusqlite::{params, Connection};
use serde::Serialize;

use crate::session_meta::{
    apply_stats_delta, ms_to_rfc3339, parse_model_usage, serialize_model_usage, to_acowork,
    to_acowork_sqlite, token_to_i64, ModelUsageMap,
};
use crate::SqliteStore;

/// One decoded `chat_token_usage` row (read model for listings / panel).
///
/// Serialized straight onto `GET /chat-usage/{chat_id}` (plan §4.6 phase
/// two): the per-chat usage view reads one row per agent participant.
#[derive(Debug, Clone, Serialize)]
pub struct ChatUsageRow {
    pub chat_session_id: String,
    /// Session-kind string stamped by the router (`user_group` /
    /// `project_group` / `user_agent`); `None` when no call recorded a
    /// kind (the envelope carried none — `record_call` never persists
    /// the empty string as a value).
    pub chat_kind: Option<String>,
    /// RFC3339, derived from the epoch-ms column.
    pub created_at: String,
    pub last_active_at: String,
    pub llm_call_count: u64,
    pub tokens: SessionTokens,
    /// Tokens the router's projected history window (Block B) cost on the
    /// most recent turn — the occupancy of the `chat_router.history_max_tokens`
    /// budget.
    ///
    /// `None` = not measured (a row written before schema v10, or a turn
    /// that never projected a history window). The UI renders a dash rather
    /// than a fabricated 0 — "宁可 miss 也不估计" (ADR-027).
    ///
    /// This is deliberately NOT the provider-reported prompt count: that
    /// number also carries the system prompt and the tool schemas, so it
    /// routinely exceeds the history budget and cannot be compared against
    /// it.
    pub history_tokens: Option<u64>,
    /// Per-`(provider, model)` breakdown; `None` when empty (a row that
    /// recorded only zero-delta calls reads identical to a fresh one).
    pub model_usage: Option<ModelUsageMap>,
}

/// SQLite implementation of the Chat usage ledger. Cheap to clone — holds
/// an `Arc<SqliteStore>`; the store owns the connection lock.
#[derive(Clone)]
pub struct SqliteChatUsageStore {
    store: Arc<SqliteStore>,
}

/// One model call to fold into a chat's ledger row.
///
/// A parameter object rather than a positional list: the call carries
/// seven independent facts, and a positional list that long makes
/// "which one is the cost / which one is the history occupancy" a
/// reading exercise at every call site. Named fields keep the meaning
/// attached to the value.
#[derive(Debug, Clone)]
pub struct ChatUsageCall<'a> {
    /// The chat the call belongs to.
    pub chat_session_id: &'a str,
    /// The router's Session-kind string. `None` (and the empty string,
    /// normalized away by the caller) records "kind not known" — the
    /// column must not distinguish "unrecorded" from "recorded but
    /// blank", so a blank is stored as NULL, same as a fresh row.
    pub chat_kind: Option<&'a str>,
    /// The provider's token accounting for this call.
    pub usage: &'a UsageInfo,
    /// Provider label (the `"{provider}|{model}"` breakdown key).
    pub provider_id: &'a str,
    /// Model id (the breakdown key's model half).
    pub model_id: &'a str,
    /// The call's price delta. `None` = no price known — never "free"
    /// (ADR-027 poison rule).
    pub cost_delta: Option<f64>,
    /// Tokens the router's projected history window (Block B) cost on
    /// this turn (`None` = not measured). A SNAPSHOT, not a cumulative
    /// counter: it overwrites on every call, because the window is
    /// refilled per turn and only the latest occupancy is meaningful
    /// against the budget.
    pub history_tokens: Option<u64>,
}

impl SqliteChatUsageStore {
    /// Wrap the already-open workspace store (the same
    /// `memory/private.sqlite` that carries `sessions` / `token_stats`),
    /// so the ledger and the aggregate diff commit on one connection.
    pub fn new(store: Arc<SqliteStore>) -> Self {
        Self { store }
    }

    /// Fold one successful router model call into the chat's ledger row
    /// and diff the breakdown into `token_stats`, in a single transaction.
    ///
    /// The diff is computed against the PERSISTED old breakdown — the same
    /// idempotent self-healing contract as `SessionMetaStore::upsert`: a
    /// lost or replayed delta is corrected by the next call's diff, and
    /// `rebuild_token_stats` remains the repair path.
    ///
    /// The per-call facts live in [`ChatUsageCall`] — see that struct for
    /// the meaning of each field (`chat_kind`, `cost_delta`,
    /// `history_tokens`).
    pub fn record_call(&self, call: &ChatUsageCall<'_>) -> AcoworkResult<()> {
        let ChatUsageCall {
            chat_session_id,
            chat_kind,
            usage,
            provider_id,
            model_id,
            cost_delta,
            history_tokens,
        } = *call;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let key = model_usage_key(Some(provider_id), Some(model_id));
        let mut conn = self.store.lock();
        let result: crate::Result<()> = (|| {
            let tx = conn.transaction()?;
            let (prev, old_map) = read_fold_state(&tx, chat_session_id)?;
            let tokens = prev.folded_with_call(usage);
            let mut new_map = old_map.clone();
            fold_model_usage_entry(&mut new_map, &key, usage, cost_delta);

            tx.execute(
                "INSERT INTO chat_token_usage ( \n                     chat_session_id, chat_kind, created_at, last_active_at, llm_call_count, \n                     token_last_input, token_last_output, token_total_input, token_total_output, \n                     token_last_cache_read, token_last_cache_write, token_total_cache_read, token_total_cache_write, \n                     model_usage, history_tokens \n                 ) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) \n                 ON CONFLICT(chat_session_id) DO UPDATE SET \n                     chat_kind = excluded.chat_kind, \n                     last_active_at = excluded.last_active_at, \n                     llm_call_count = chat_token_usage.llm_call_count + 1, \n                     token_last_input = excluded.token_last_input, \n                     token_last_output = excluded.token_last_output, \n                     token_total_input = excluded.token_total_input, \n                     token_total_output = excluded.token_total_output, \n                     token_last_cache_read = excluded.token_last_cache_read, \n                     token_last_cache_write = excluded.token_last_cache_write, \n                     token_total_cache_read = excluded.token_total_cache_read, \n                     token_total_cache_write = excluded.token_total_cache_write, \n                     model_usage = excluded.model_usage, \n                     history_tokens = COALESCE(excluded.history_tokens, chat_token_usage.history_tokens)",
                params![
                    chat_session_id,
                    chat_kind,
                    now_ms,
                    now_ms,
                    token_to_i64(tokens.last_input),
                    token_to_i64(tokens.last_output),
                    token_to_i64(tokens.total_input),
                    token_to_i64(tokens.total_output),
                    token_to_i64(tokens.last_cache_read),
                    token_to_i64(tokens.last_cache_write),
                    token_to_i64(tokens.total_cache_read),
                    token_to_i64(tokens.total_cache_write),
                    serialize_model_usage(Some(&new_map)),
                    history_tokens.map(|v| v as i64),
                ],
            )?;
            apply_stats_delta(&tx, &old_map, &new_map)?;
            tx.commit()?;
            Ok(())
        })();
        result.map_err(to_acowork)
    }

    /// Ledger-wide cumulative totals — the Chat contribution to the
    /// ADR-066 `merge_token_totals` recovery scan (agent-scoped
    /// `(input, output, cache_read, cache_write)`).
    pub fn totals(&self) -> AcoworkResult<SessionTotals> {
        let conn = self.store.lock();
        let (i, o, cr, cw): (i64, i64, i64, i64) = conn
            .query_row(
                "SELECT COALESCE(SUM(token_total_input), 0), \n                 COALESCE(SUM(token_total_output), 0), \n                 COALESCE(SUM(token_total_cache_read), 0), \n                 COALESCE(SUM(token_total_cache_write), 0) \n                 FROM chat_token_usage",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map_err(to_acowork_sqlite)?;
        Ok((
            i.max(0) as u64,
            o.max(0) as u64,
            cr.max(0) as u64,
            cw.max(0) as u64,
        ))
    }

    /// Drop one chat's ledger row and subtract its breakdown from
    /// `token_stats` in the same transaction — the delete-side mirror of
    /// `SqliteSessionMetaStore::delete`. Without it a deleted Chat session
    /// (design 27 §4.4) would leave its spend folded into the aggregate
    /// forever: `rebuild_token_stats` and the ADR-066 recovery scan both
    /// read this table as a detail source, so a surviving row is
    /// indistinguishable from a live chat's spend.
    ///
    /// `Ok(false)` when no row exists (idempotent — a replayed delete or a
    /// chat that never recorded a router call); nothing is subtracted then.
    pub fn forget(&self, chat_session_id: &str) -> AcoworkResult<bool> {
        let mut conn = self.store.lock();
        let result: crate::Result<bool> = (|| {
            let tx = conn.transaction()?;
            let (_, old_map) = read_fold_state(&tx, chat_session_id)?;
            let removed =
                tx.execute("DELETE FROM chat_token_usage WHERE chat_session_id = ?1", params![chat_session_id])? > 0;
            if removed {
                apply_stats_delta(&tx, &old_map, &ModelUsageMap::new())?;
            }
            tx.commit()?;
            Ok(removed)
        })();
        result.map_err(to_acowork)
    }

    /// One chat's ledger row, `None` when this agent never recorded a
    /// call for it (the per-chat usage read behind `GET /chat-usage/{id}`).
    /// A miss is a legitimate empty answer, not an error: chats with only
    /// human messages never reach the router.
    pub fn get(&self, chat_session_id: &str) -> AcoworkResult<Option<ChatUsageRow>> {
        let conn = self.store.lock();
        let row = conn
            .query_row(
                "SELECT chat_session_id, chat_kind, created_at, last_active_at, llm_call_count, \n                 token_last_input, token_last_output, token_total_input, token_total_output, \n                 token_last_cache_read, token_last_cache_write, token_total_cache_read, token_total_cache_write, \n                 model_usage, history_tokens \n                 FROM chat_token_usage WHERE chat_session_id = ?1",
                params![chat_session_id],
                row_to_usage,
            )
            .ok();
        Ok(row)
    }

    /// Most-recent-first page of ledger rows (for a future ledger-wide
    /// view). PLAN: no production caller yet — the per-chat read goes
    /// through [`Self::get`].
    pub fn list(&self, limit: usize, offset: usize) -> AcoworkResult<Vec<ChatUsageRow>> {
        let conn = self.store.lock();
        let mut stmt = conn
            .prepare(
                "SELECT chat_session_id, chat_kind, created_at, last_active_at, llm_call_count, \n                 token_last_input, token_last_output, token_total_input, token_total_output, \n                 token_last_cache_read, token_last_cache_write, token_total_cache_read, token_total_cache_write, \n                 model_usage, history_tokens \n                 FROM chat_token_usage \n                 ORDER BY last_active_at DESC LIMIT ?1 OFFSET ?2",
            )
            .map_err(to_acowork_sqlite)?;
        let rows = stmt
            .query_map(params![limit as i64, offset as i64], row_to_usage)
            .map_err(to_acowork_sqlite)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(to_acowork_sqlite)
    }

    /// Total ledger rows (pagination companion for [`Self::list`]).
    /// PLAN: no production caller yet — same ledger-wide note as
    /// [`Self::list`].
    pub fn count(&self) -> AcoworkResult<u64> {
        let conn = self.store.lock();
        conn.query_row("SELECT COUNT(*) FROM chat_token_usage", [], |r| {
            r.get::<_, i64>(0)
        })
        .map(|c| c.max(0) as u64)
        .map_err(to_acowork_sqlite)
    }
}

/// Read the persisted fold state for one chat: the previous cumulative
/// tokens and the previous breakdown. `(default, empty)` when the row does
/// not exist yet.
fn read_fold_state(
    tx: &Connection,
    chat_session_id: &str,
) -> rusqlite::Result<(SessionTokens, ModelUsageMap)> {
    /// Persisted fold state columns: eight token counters plus the
    /// `model_usage` JSON blob.
    type FoldCols = (i64, i64, i64, i64, i64, i64, i64, i64, Option<String>);
    let row: Option<FoldCols> = match tx.query_row(
        "SELECT token_last_input, token_last_output, token_total_input, token_total_output, \n             token_last_cache_read, token_last_cache_write, token_total_cache_read, token_total_cache_write, \n             model_usage \n             FROM chat_token_usage WHERE chat_session_id = ?1",
        params![chat_session_id],
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
            ))
        },
    ) {
        Ok(v) => Some(v),
        // No row is the normal first-call state. Any other failure means the
        // persisted fold could not be read — returning a default would reset
        // the chat's running totals, so it aborts the write instead.
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => return Err(e),
    };
    let (li, lo, ti, to, lcr, lcw, tcr, tcw, usage_json) = match row {
        Some(v) => v,
        None => return Ok((SessionTokens::default(), ModelUsageMap::new())),
    };
    let as_u64 = |v: i64| v.max(0) as u64;
    Ok((
        SessionTokens {
            last_input: as_u64(li),
            last_output: as_u64(lo),
            total_input: as_u64(ti),
            total_output: as_u64(to),
            last_cache_read: as_u64(lcr),
            last_cache_write: as_u64(lcw),
            total_cache_read: as_u64(tcr),
            total_cache_write: as_u64(tcw),
        },
        parse_model_usage(usage_json.as_deref()),
    ))
}

fn row_to_usage(r: &rusqlite::Row<'_>) -> rusqlite::Result<ChatUsageRow> {
    let as_u64 = |v: i64| v.max(0) as u64;
    let usage_json: Option<String> = r.get("model_usage")?;
    let map = parse_model_usage(usage_json.as_deref());
    Ok(ChatUsageRow {
        chat_session_id: r.get("chat_session_id")?,
        chat_kind: r.get("chat_kind")?,
        created_at: ms_to_rfc3339(r.get::<_, i64>("created_at")?),
        last_active_at: ms_to_rfc3339(r.get::<_, i64>("last_active_at")?),
        llm_call_count: as_u64(r.get("llm_call_count")?),
        tokens: SessionTokens {
            last_input: as_u64(r.get("token_last_input")?),
            last_output: as_u64(r.get("token_last_output")?),
            total_input: as_u64(r.get("token_total_input")?),
            total_output: as_u64(r.get("token_total_output")?),
            last_cache_read: as_u64(r.get("token_last_cache_read")?),
            last_cache_write: as_u64(r.get("token_last_cache_write")?),
            total_cache_read: as_u64(r.get("token_total_cache_read")?),
            total_cache_write: as_u64(r.get("token_total_cache_write")?),
        },
        history_tokens: r
            .get::<_, Option<i64>>("history_tokens")?
            .map(|v| v.max(0) as u64),
        model_usage: (!map.is_empty()).then_some(map),
    })
}
