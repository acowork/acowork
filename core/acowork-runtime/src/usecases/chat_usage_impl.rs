//! RuntimeChatUsageService — the Chat Router token ledger funnel.
//!
//! One funnel, two sinks, per successful router model call:
//!
//! 1. the agent-level atomic counters (ADR-028) — always fed, process-local,
//!    so the live `context_usage` push and the agent totals stop
//!    under-counting chat spend even where the ledger store is unavailable;
//! 2. the per-chat ledger row (`chat_token_usage`, schema v9) with its
//!    same-transaction `token_stats` diff — the persisted truth that the
//!    ADR-066 recovery scan merges back after a restart.
//!
//! The cost estimate is resolved here (from the agent's model capabilities)
//! rather than at the call site, so the router and the dialog path price a
//! call through the same `estimate_call_cost` path.

use std::sync::Arc;

use acowork_core::providers::traits::UsageInfo;
use acowork_core::session_meta::SessionTotals;
use acowork_sqlite::{ChatUsageCall, SqliteChatUsageStore};

use crate::agent::agent_core::AgentCore;
use crate::conversation::estimate_call_cost;
use crate::usecases::chat_usage::ChatUsageService;

pub struct RuntimeChatUsageService {
    /// `None` when the Runtime-local SQLite store never opened (the same
    /// degraded state in which session meta is unwriteable). Recording
    /// then keeps feeding the in-process counters and logs once per call.
    ledger: Option<Arc<SqliteChatUsageStore>>,
    core: Arc<AgentCore>,
}

impl RuntimeChatUsageService {
    /// Build the service for `core`, attaching the ledger to the already
    /// open Runtime-local store when there is one.
    pub fn new(core: Arc<AgentCore>) -> Arc<Self> {
        let ledger = core
            .sqlite_store()
            .map(SqliteChatUsageStore::new)
            .map(Arc::new);
        Arc::new(Self { ledger, core })
    }
}

impl ChatUsageService for RuntimeChatUsageService {
    fn record_chat_llm_usage(
        &self,
        chat_session_id: &str,
        chat_kind: &str,
        usage: &UsageInfo,
        provider_label: &str,
        model_id: &str,
        history_tokens: Option<u64>,
    ) {
        // Sink 1 — the atomic counters. Unconditional: a ledger-store
        // failure must not silently drop the call from the agent total
        // (same contract as the dialog path's `core.accumulate_llm_usage`).
        self.core.accumulate_llm_usage(usage);

        let Some(store) = &self.ledger else {
            tracing::debug!(
                chat_id = %chat_session_id,
                "chat usage: ledger store unavailable — counted in agent totals only"
            );
            return;
        };
        // Price through the same path the dialog loop uses; `None` folds
        // as "no cost known", never as free (ADR-027 poison rule).
        let cost_delta = self
            .core
            .get_model_capabilities(model_id)
            .and_then(|caps| estimate_call_cost(caps.cost.as_ref(), usage));
        // A blank kind is "not known", not a kind: fold it to `None` so the
        // ledger column never stores `""` (an unrecorded and a blank-kind
        // row must read identically — see `SqliteChatUsageStore::record_call`).
        let kind = (!chat_kind.trim().is_empty()).then_some(chat_kind);
        if let Err(e) = store.record_call(&ChatUsageCall {
            chat_session_id,
            chat_kind: kind,
            usage,
            provider_id: provider_label,
            model_id,
            cost_delta,
            history_tokens,
        }) {
            // Best-effort by contract: the turn already happened; losing
            // one ledger row is a statistics miss, not a failure to abort.
            tracing::warn!(
                chat_id = %chat_session_id,
                error = %e,
                "chat usage: ledger write failed — agent totals still carry this call"
            );
        }
    }

    fn forget_chat(&self, chat_session_id: &str) -> bool {
        let Some(store) = &self.ledger else {
            // No store: there is no persisted row this process could drop.
            // The counters never decrement (ADR-028) — same one-way rule
            // as session delete on the live counters.
            return false;
        };
        match store.forget(chat_session_id) {
            Ok(removed) => removed,
            Err(e) => {
                tracing::warn!(
                    chat_id = %chat_session_id,
                    error = %e,
                    "chat usage: ledger forget failed — the aggregate keeps carrying this chat until the next attempt"
                );
                false
            }
        }
    }

    fn get_chat(&self, chat_session_id: &str) -> Option<acowork_sqlite::ChatUsageRow> {
        let store = self.ledger.as_ref()?;
        match store.get(chat_session_id) {
            Ok(row) => row,
            Err(e) => {
                // A read failure is a statistics miss, not a 500-worthy
                // event: the view falls back to "no data" like a real miss.
                tracing::warn!(
                    chat_id = %chat_session_id,
                    error = %e,
                    "chat usage: ledger read failed — the per-chat view shows no data"
                );
                None
            }
        }
    }

    fn ledger_totals(&self) -> SessionTotals {
        match &self.ledger {
            Some(store) => store.totals().unwrap_or((0, 0, 0, 0)),
            None => (0, 0, 0, 0),
        }
    }
}

/// No-op ledger for in-process tests that build a `RouterDeps` without a
/// Runtime store. Records nothing, totals read as zero.
#[cfg(test)]
pub(crate) struct NoopChatUsageService;

#[cfg(test)]
impl ChatUsageService for NoopChatUsageService {
    fn record_chat_llm_usage(
        &self,
        _chat_session_id: &str,
        _chat_kind: &str,
        _usage: &UsageInfo,
        _provider_label: &str,
        _model_id: &str,
        _history_tokens: Option<u64>,
    ) {
    }

    fn forget_chat(&self, _chat_session_id: &str) -> bool {
        false
    }

    fn get_chat(&self, _chat_session_id: &str) -> Option<acowork_sqlite::ChatUsageRow> {
        None
    }

    fn ledger_totals(&self) -> SessionTotals {
        (0, 0, 0, 0)
    }
}

// ── RuntimeChatUsageService integration tests ───────────────────────
//
// The contract under test is the two-sink funnel: every recorded call
// lands in the agent atomic counters (live push) AND in the persisted
// ledger (restart recovery + token_stats). The degraded path (no SQLite
// store) must keep the counter sink honest — a statistics miss may never
// become a lost call from the agent total.

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use acowork_core::AgentManifest;
    use acowork_core::providers::mock::MockProvider;
    use acowork_core::providers::traits::UsageInfo;
    use acowork_core::session_meta::SessionMetaStore;
    use acowork_sqlite::SqliteSessionMetaStore;

    use crate::agent::agent_core::AgentCore;
    use crate::config::RuntimeConfig;

    fn usage(prompt: u64, completion: u64, cache_read: u64, cache_write: u64) -> UsageInfo {
        UsageInfo {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            reasoning_tokens: 0,
        }
    }

    fn make_core(temp: &Path, with_sqlite: bool) -> Arc<AgentCore> {
        let config = RuntimeConfig {
            history_max_tokens: 8000,
            work_dir: temp.to_str().unwrap().to_string(),
            ..RuntimeConfig::default()
        };
        let manifest = AgentManifest::from_toml(
            r#"
            agent_id = "com.test.chat-usage"
            version = "1.0.0"
            name = "Chat usage test"
            description = "Chat Router token ledger integration"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "test-model"
            "#,
        )
        .unwrap();
        let provider = Arc::new(MockProvider::single_text("test"));
        let mut core = AgentCore::new(config, manifest, provider, vec![]);
        if with_sqlite {
            core.init_runtime_sqlite(temp);
        }
        Arc::new(core)
    }

    #[test]
    fn record_feeds_counters_ledger_and_aggregate() {
        let temp = tempfile::tempdir().unwrap();
        let core = make_core(temp.path(), true);
        let svc = RuntimeChatUsageService::new(core.clone());

        // One chat, three model calls (the shape of a turn with an
        // information round + a dispatch retry): every call is booked.
        svc.record_chat_llm_usage("c-1", "group", &usage(100, 10, 60, 5), "mock", "test-model", None);
        svc.record_chat_llm_usage("c-1", "group", &usage(120, 20, 100, 0), "mock", "test-model", None);
        // A second chat, and one call whose Provider reported no input
        // (zero-skip: totals must not move on the input side).
        svc.record_chat_llm_usage("c-2", "direct", &usage(0, 7, 0, 0), "mock", "test-model", None);

        // Sink 1 — agent atomic counters carry every reported token.
        assert_eq!(core.agent_token_totals(), (220, 37, 160, 5));

        // Sink 2 — the persisted ledger, with the ADR-027 gates applied.
        assert_eq!(svc.ledger_totals(), (220, 37, 160, 5));

        // The aggregate carries the chat spend and stays consistent with
        // the detail side (which now includes the ledger).
        let sm = SqliteSessionMetaStore::new(core.sqlite_store().unwrap());
        let stats = sm.token_stats().unwrap();
        let row = stats.iter().find(|s| s.model == "test-model").unwrap();
        assert_eq!(row.total_input, 220);
        assert_eq!(row.total_output, 37);
        assert!(sm.token_stats_consistent().unwrap());
    }

    /// The router's history occupancy must survive the whole chain:
    /// recorded → persisted → read back through the public `get_chat`
    /// (the shape `GET /chat-usage/{id}` serves) → and the column must
    /// be present on the persisted row, not silently dropped.
    ///
    /// This is the test that would have caught a broken write: without it
    /// the UI would show a dash for the occupancy while every other
    /// number in the same row read fine.
    #[test]
    fn the_history_occupancy_survives_the_ledger_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let core = make_core(temp.path(), true);
        let svc = RuntimeChatUsageService::new(core.clone());

        svc.record_chat_llm_usage(
            "c-1",
            "group",
            &usage(4_000, 200, 0, 0),
            "mock",
            "test-model",
            Some(3_979),
        );

        let row = svc.get_chat("c-1").expect("the row must be readable back");
        assert_eq!(
            row.history_tokens,
            Some(3_979),
            "the occupancy must be readable back from the ledger row"
        );
        // The other columns must be unaffected by the new one.
        assert_eq!(row.llm_call_count, 1);
        assert_eq!(row.tokens.total_input, 4_000);
    }

    /// A turn that measured nothing must not overwrite the last known
    /// occupancy with a zero — "not measured" is not "measured zero"
    /// (ADR-027: rather miss than estimate).
    #[test]
    fn an_unmeasured_turn_keeps_the_last_occupancy() {
        let temp = tempfile::tempdir().unwrap();
        let core = make_core(temp.path(), true);
        let svc = RuntimeChatUsageService::new(core.clone());

        svc.record_chat_llm_usage(
            "c-1",
            "group",
            &usage(1_000, 10, 0, 0),
            "mock",
            "test-model",
            Some(2_500),
        );
        svc.record_chat_llm_usage(
            "c-1",
            "group",
            &usage(1_100, 10, 0, 0),
            "mock",
            "test-model",
            None,
        );

        let row = svc.get_chat("c-1").expect("row");
        assert_eq!(
            row.history_tokens,
            Some(2_500),
            "an unmeasured turn must not erase the last measurement"
        );
        assert_eq!(row.llm_call_count, 2);
    }

    /// Restart recovery, faithful to the production merge input: a fresh
    /// `AgentCore` (counters at zero) re-opens the same workspace store,
    /// the ledger scan yields the persisted chat totals, and the
    /// `merge_token_totals` the `list_sessions` scan performs — sessions
    /// sum + ledger sum — puts them back into the agent counters. Without
    /// the ledger term these tokens would be permanently under-counted.
    #[test]
    fn restart_recovers_chat_spend_through_the_scan_merge() {
        let temp = tempfile::tempdir().unwrap();

        // Process 1: record chat spend.
        let core1 = make_core(temp.path(), true);
        let svc1 = RuntimeChatUsageService::new(core1.clone());
        svc1.record_chat_llm_usage("c-1", "group", &usage(100, 10, 60, 5), "mock", "test-model", None);
        svc1.record_chat_llm_usage("c-2", "direct", &usage(30, 4, 0, 0), "mock", "test-model", None);
        drop(svc1);
        drop(core1);

        // Process 2: fresh core, zero counters, same workspace store.
        let core2 = make_core(temp.path(), true);
        assert_eq!(core2.agent_token_totals(), (0, 0, 0, 0));
        let svc2 = RuntimeChatUsageService::new(core2.clone());
        let (ci, co, ccr, ccw) = svc2.ledger_totals();
        assert_eq!((ci, co, ccr, ccw), (130, 14, 60, 5));

        // The merge the session scan performs (disk session sums here are
        // zero — no sessions were created; the chat term alone must land).
        core2.merge_token_totals((Some(ci), Some(co), Some(ccr), Some(ccw)));
        assert_eq!(core2.agent_token_totals(), (130, 14, 60, 5));
    }

    #[test]
    fn degraded_without_sqlite_store_still_counts() {
        let temp = tempfile::tempdir().unwrap();
        let core = make_core(temp.path(), false);
        let svc = RuntimeChatUsageService::new(core.clone());

        svc.record_chat_llm_usage("c-1", "group", &usage(50, 5, 0, 0), "mock", "test-model", None);

        // No ledger store: the call is still in the agent totals (never
        // silently dropped), and the recovery scan reads zero rather than
        // failing.
        assert_eq!(core.agent_token_totals(), (50, 5, 0, 0));
        assert_eq!(svc.ledger_totals(), (0, 0, 0, 0));
    }

    /// Delete-side symmetry (design 27 §4.4): forgetting a chat drops its
    /// ledger row, nets its breakdown out of `token_stats`, and leaves the
    /// live counters untouched (ADR-028 never decrements). A blank kind is
    /// "not known", never a stored value.
    #[test]
    fn forget_nets_the_ledger_and_aggregate_but_not_the_counters() {
        let temp = tempfile::tempdir().unwrap();
        let core = make_core(temp.path(), true);
        let svc = RuntimeChatUsageService::new(core.clone());

        svc.record_chat_llm_usage("c-1", "", &usage(100, 10, 0, 0), "mock", "test-model", None);
        svc.record_chat_llm_usage("c-2", "user_group", &usage(30, 4, 0, 0), "mock", "test-model", None);
        assert!(svc.forget_chat("c-1"));
        assert!(!svc.forget_chat("c-1"), "idempotent: the second hit removes nothing");
        assert!(!svc.forget_chat("c-unknown"));

        // Ledger carries only the surviving chat…
        assert_eq!(svc.ledger_totals(), (30, 4, 0, 0));
        // …and the per-chat read (plan §4.6 phase two) mirrors that:
        // the forgotten chat is a miss, the survivor returns its row.
        assert!(svc.get_chat("c-1").is_none());
        let row = svc.get_chat("c-2").expect("c-2 recorded two counters");
        assert_eq!(row.llm_call_count, 1);
        assert_eq!((row.tokens.total_input, row.tokens.total_output), (30, 4));
        // …and so does the aggregate, net of the forgotten row.
        let sm = SqliteSessionMetaStore::new(core.sqlite_store().unwrap());
        let stats = sm.token_stats().unwrap();
        let row = stats.iter().find(|s| s.model == "test-model").unwrap();
        assert_eq!((row.total_input, row.total_output), (30, 4));
        assert!(sm.token_stats_consistent().unwrap());
        // The blank kind never became a stored value, and the live atomic
        // counters keep the deleted chat's spend (one-way, ADR-028).
        let ledged = SqliteChatUsageStore::new(core.sqlite_store().unwrap());
        let remaining = ledged.list(10, 0).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].chat_kind.as_deref(), Some("user_group"));
        assert_eq!(core.agent_token_totals(), (130, 14, 0, 0));
    }
}
