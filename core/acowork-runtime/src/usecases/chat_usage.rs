//! Chat Router token accounting use case (design 29 / ADR-099 follow-up).
//!
//! The router's own LLM spend is real, billable, and belongs to a Chat
//! session — not to any execution session. This trait is the single funnel
//! that turns one successful router model call into (a) the chat's ledger
//! row (`chat_token_usage`, schema v9) and (b) the agent-level cumulative
//! counters (ADR-028), so the agent-scoped `token_stats` panel and the
//! live `context_usage` push both stop under-counting chat spend.
//!
//! A Chat session is NOT an execution session: the ledger row carries pure
//! aggregates, no conversation state — matching the router's stateless
//! design (ADR-099 D1).

use acowork_core::providers::traits::UsageInfo;
use acowork_core::session_meta::SessionTotals;

/// Token accounting for Chat-surface LLM calls.
///
/// Synchronous: the ledger write is one short SQLite transaction on the
/// shared runtime store, mirroring how session-meta writes are issued from
/// the dialog path. Recording is best-effort by contract — a ledger write
/// failure logs and never aborts the router turn.
pub trait ChatUsageService: Send + Sync {
    /// Record one successful model call of a router turn.
    ///
    /// `provider_label` / `model_id` attribute the call to a
    /// `"{provider}|{model}"` detail bucket (same key shape as
    /// `SessionMeta.model_usage`); the cost estimate is resolved inside the
    /// implementation from the agent's model capabilities, so callers do
    /// not duplicate the pricing path. `chat_kind` is the router's
    /// Session-kind string; blank / empty records "kind not known" (the
    /// ledger stores NULL), never a value that pretends to be a kind.
    ///
    /// `history_tokens` is the router's projected-history occupancy for
    /// this turn (design 29 §4, Block B) — `None` when the turn did not
    /// measure it. It is the ONLY number comparable to the
    /// `chat_router.history_max_tokens` budget: the provider-reported
    /// prompt count also carries the system prompt and the tool schemas,
    /// so it routinely exceeds that budget and must not be used as the
    /// budget's occupancy.
    fn record_chat_llm_usage(
        &self,
        chat_session_id: &str,
        chat_kind: &str,
        usage: &UsageInfo,
        provider_label: &str,
        model_id: &str,
        history_tokens: Option<u64>,
    );

    /// Drop one Chat session's ledger row and net its breakdown out of
    /// `token_stats` — the delete-side mirror of `record_chat_llm_usage`,
    /// issued when the chat domain confirms the session is gone (design 27
    /// §4.4, fanned out by the Gateway to the agent participants).
    /// Idempotent: `false` when this agent never recorded a call for the
    /// chat (no row, nothing subtracted). Best-effort by contract, like
    /// recording: a store failure logs and returns `false`.
    fn forget_chat(&self, chat_session_id: &str) -> bool;

    /// Read one Chat session's ledger row — the per-chat usage view
    /// (plan §4.6 phase two) behind `GET /chat-usage/{chat_id}`.
    /// `None` when this agent never recorded a router call for the chat
    /// (human-only traffic, no ledger store, or a degraded read): the
    /// caller renders an empty view, never an error.
    fn get_chat(&self, chat_session_id: &str) -> Option<acowork_sqlite::ChatUsageRow>;

    /// Ledger-wide cumulative totals — the Chat contribution to the
    /// ADR-066 `merge_token_totals` recovery scan. `(0,0,0,0)` when no
    /// ledger is available (no SQLite store) or no chat call has been
    /// recorded yet.
    fn ledger_totals(&self) -> SessionTotals;
}
