//! Tests for `SqliteChatUsageStore` — the Chat Router token ledger
//! (schema v9, design 29 / ADR-099 follow-up).
//!
//! Contract under test:
//!
//! 1. `record_call` folds one model call into the chat's row with the same
//!    ADR-027/066 gates as session totals (zero-skip on input, `last_*`
//!    overwritten with raw Provider values).
//! 2. Σ per-model detail == cumulative totals at every point (the shared
//!    fold primitives make this structural; the test pins it).
//! 3. The agent-scoped `token_stats` aggregate is maintained through the
//!    ledger's same-transaction diff — and `token_stats_consistent` /
//!    `rebuild_token_stats` see the ledger as a detail source (a chat row
//!    must not break the check).
//! 4. `totals` is the ADR-066 recovery scan over the ledger.
//! 5. `list` / `count` page most-recent-first.
//! 6. `forget` is the delete-side mirror of `SqliteSessionMetaStore::delete`:
//!    the row drops AND its breakdown nets out of `token_stats`, so a
//!    deleted Chat session never leaves its spend in the aggregate.
//! 7. `get` is the per-chat read behind `GET /chat-usage/{id}` (plan §4.6
//!    phase two): the full row or `None` — never an error on a miss.

use std::sync::Arc;

use acowork_core::providers::traits::UsageInfo;
use acowork_core::session_meta::{SessionMeta, SessionMetaStore, SessionTokens};
use acowork_sqlite::{ChatUsageCall, SqliteChatUsageStore, SqliteSessionMetaStore, SqliteStore};

const DIM: usize = 4;

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

fn stores(dir: &std::path::Path) -> (SqliteChatUsageStore, SqliteSessionMetaStore) {
    let store = Arc::new(SqliteStore::open(dir.join("private.sqlite"), DIM).unwrap());
    (
        SqliteChatUsageStore::new(store.clone()),
        SqliteSessionMetaStore::new(store),
    )
}

fn stats_sum(sm: &SqliteSessionMetaStore) -> (i64, i64) {
    let rows = sm.token_stats().unwrap();
    let i: i64 = rows.iter().map(|r| r.total_input as i64).sum();
    let o: i64 = rows.iter().map(|r| r.total_output as i64).sum();
    (i, o)
}

fn make_session(sid: &str, tokens: SessionTokens) -> SessionMeta {
    SessionMeta {
        version: 4,
        session_id: sid.to_string(),
        agent_id: "a-1".into(),
        created_at: "2026-01-01T00:00:00.000Z".into(),
        initiator: None,
        visibility: None,
        chat_session_id: None,
        title: None,
        workspace_id: None,
        model: None,
        provider: None,
        account_id: None,
        reasoning_effort: None,
        temperature: None,
        context_window: None,
        todos: None,
        message_count: 0,
        last_active_at: "2026-01-02T00:00:00.000Z".into(),
        tokens: Some(tokens),
        model_usage: None,
        llm_call_counter: None,
        perf: None,
        model_ratio: None,
        last_compaction_offset: None,
        corrupted: false,
    }
}

#[test]
fn record_call_folds_totals_detail_and_aggregate() {
    let dir = tempfile::tempdir().unwrap();
    let (chat, sm) = stores(dir.path());

    chat.record_call(&ChatUsageCall {
        chat_session_id: "c-1",
        chat_kind: Some("group"),
        usage: &usage(100, 20, 60, 5),
        provider_id: "openai",
        model_id: "gpt-4o-mini",
        cost_delta: Some(0.01),
        history_tokens: None,
    })
        .unwrap();
    // Second call, same chat, same model: totals accumulate, `last_*`
    // overwrites with the raw values of THIS call.
    chat.record_call(&ChatUsageCall {
        chat_session_id: "c-1",
        chat_kind: Some("group"),
        usage: &usage(150, 30, 100, 0),
        provider_id: "openai",
        model_id: "gpt-4o-mini",
        cost_delta: Some(0.02),
        history_tokens: None,
    })
        .unwrap();
    // Third call, same chat, different model: new detail bucket.
    chat.record_call(&ChatUsageCall {
        chat_session_id: "c-1",
        chat_kind: Some("group"),
        usage: &usage(50, 10, 0, 0),
        provider_id: "anthropic",
        model_id: "claude-x",
        cost_delta: None,
        history_tokens: None,
    })
        .unwrap();

    let rows = chat.list(10, 0).unwrap();
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r.llm_call_count, 3);
    assert_eq!(r.tokens.total_input, 300);
    assert_eq!(r.tokens.total_output, 60);
    assert_eq!(r.tokens.total_cache_read, 160);
    assert_eq!(r.tokens.total_cache_write, 5);
    assert_eq!(r.tokens.last_input, 50);
    assert_eq!(r.tokens.last_output, 10);

    // Σ detail == totals (the invariant `token_stats` is maintained against).
    let detail = r.model_usage.as_ref().unwrap();
    assert_eq!(
        detail.values().map(|e| e.input).sum::<u64>(),
        r.tokens.total_input
    );
    assert_eq!(
        detail.values().map(|e| e.output).sum::<u64>(),
        r.tokens.total_output
    );
    // Cost poison-on-unknown: the unpriced claude bucket flips its own
    // cost to None but must not touch the priced openai bucket.
    assert_eq!(detail["openai|gpt-4o-mini"].cost, Some(0.03));
    assert_eq!(detail["anthropic|claude-x"].cost, None);

    // Aggregate carries the chat spend.
    let stats = sm.token_stats().unwrap();
    let openai = stats.iter().find(|s| s.model == "gpt-4o-mini").unwrap();
    assert_eq!(openai.total_input, 250);
    assert_eq!(openai.total_output, 50);
    let anth = stats.iter().find(|s| s.model == "claude-x").unwrap();
    assert_eq!(anth.total_input, 50);
    assert_eq!(anth.total_cost, None); // unpriced contribution → poisoned

    // Consistency check counts the ledger as a detail source.
    assert!(sm.token_stats_consistent().unwrap());
}

#[test]
fn zero_skip_gate_matches_session_rules() {
    let dir = tempfile::tempdir().unwrap();
    let (chat, sm) = stores(dir.path());

    // A Provider fallback reports prompt == 0: input/cache_read must not
    // accumulate, completion must, and `last_*` honestly shows the zero.
    chat.record_call(&ChatUsageCall {
        chat_session_id: "c-1",
        chat_kind: None,
        usage: &usage(0, 40, 999, 0),
        provider_id: "openai",
        model_id: "gpt-4o-mini",
        cost_delta: None,
        history_tokens: None,
    })
        .unwrap();
    let r = &chat.list(10, 0).unwrap()[0];
    assert_eq!(r.tokens.total_input, 0);
    assert_eq!(r.tokens.total_cache_read, 0);
    assert_eq!(r.tokens.total_output, 40);
    assert_eq!(r.tokens.last_input, 0);
    let detail = r.model_usage.as_ref().unwrap();
    assert_eq!(detail["openai|gpt-4o-mini"].input, 0);
    assert_eq!(detail["openai|gpt-4o-mini"].output, 40);
    assert!(sm.token_stats_consistent().unwrap());
}

#[test]
fn consistency_and_rebuild_cover_sessions_and_ledger_together() {
    let dir = tempfile::tempdir().unwrap();
    let (chat, sm) = stores(dir.path());

    sm.upsert(&make_session(
        "s-1",
        SessionTokens {
            last_input: 10,
            last_output: 5,
            total_input: 10,
            total_output: 5,
            ..Default::default()
        },
    ))
    .unwrap();
    chat.record_call(&ChatUsageCall {
        chat_session_id: "c-1",
        chat_kind: Some("direct"),
        usage: &usage(100, 20, 0, 0),
        provider_id: "openai",
        model_id: "gpt-4o-mini",
        cost_delta: Some(0.001),
        history_tokens: None,
    })
        .unwrap();

    let before = stats_sum(&sm);
    assert!(sm.token_stats_consistent().unwrap());
    sm.rebuild_token_stats().unwrap();
    assert!(sm.token_stats_consistent().unwrap());
    assert_eq!(stats_sum(&sm), before);
}

#[test]
fn totals_is_the_recovery_scan_over_the_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let (chat, _sm) = stores(dir.path());

    assert_eq!(chat.totals().unwrap(), (0, 0, 0, 0));
    chat.record_call(&ChatUsageCall {
        chat_session_id: "c-1",
        chat_kind: None,
        usage: &usage(100, 20, 10, 1),
        provider_id: "openai",
        model_id: "m",
        cost_delta: Some(0.001),
        history_tokens: None,
    })
        .unwrap();
    chat.record_call(&ChatUsageCall {
        chat_session_id: "c-2",
        chat_kind: None,
        usage: &usage(7, 3, 0, 0),
        provider_id: "openai",
        model_id: "m",
        cost_delta: Some(0.001),
        history_tokens: None,
    })
        .unwrap();
    chat.record_call(&ChatUsageCall {
        chat_session_id: "c-1",
        chat_kind: None,
        usage: &usage(0, 4, 0, 0),
        provider_id: "openai",
        model_id: "m",
        cost_delta: None,
        history_tokens: None,
    })
        .unwrap();
    assert_eq!(chat.totals().unwrap(), (107, 27, 10, 1));
    assert_eq!(chat.count().unwrap(), 2);
}

#[test]
fn list_pages_most_recent_first() {
    let dir = tempfile::tempdir().unwrap();
    let (chat, _sm) = stores(dir.path());

    chat.record_call(&ChatUsageCall {
        chat_session_id: "c-old",
        chat_kind: None,
        usage: &usage(1, 1, 0, 0),
        provider_id: "openai",
        model_id: "m",
        cost_delta: None,
        history_tokens: None,
    })
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2));
    chat.record_call(&ChatUsageCall {
        chat_session_id: "c-new",
        chat_kind: None,
        usage: &usage(1, 1, 0, 0),
        provider_id: "openai",
        model_id: "m",
        cost_delta: None,
        history_tokens: None,
    })
        .unwrap();
    let page = chat.list(1, 0).unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].chat_session_id, "c-new");
    assert_eq!(chat.list(10, 1).unwrap()[0].chat_session_id, "c-old");
}

#[test]
fn schema_stamps_v9_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("private.sqlite"), DIM).unwrap();
    // The ledger table must exist on a fresh open (the v9 migration ran) —
    // a write is the cheapest existence proof, and every other test in this
    // file would fail loudly if it did not.
    let chat = SqliteChatUsageStore::new(Arc::new(store));
    chat.record_call(&ChatUsageCall {
        chat_session_id: "c-v9",
        chat_kind: None,
        usage: &usage(1, 1, 0, 0),
        provider_id: "openai",
        model_id: "m",
        cost_delta: None,
        history_tokens: None,
    })
        .unwrap();
    assert_eq!(chat.count().unwrap(), 1);
}

/// The router's projected-history occupancy is a SNAPSHOT, not a cumulative
/// counter: the window is refilled on every turn, so only the latest
/// occupancy is meaningful against the `chat_router.history_max_tokens`
/// budget. A turn that does not measure it (`None`) must not erase the
/// previous measurement — "no measurement" is not "zero".
#[test]
fn history_tokens_is_a_snapshot_and_none_keeps_the_last_measurement() {
    let dir = tempfile::tempdir().unwrap();
    let (chat, _sm) = stores(dir.path());

    chat.record_call(&ChatUsageCall {
        chat_session_id: "c-1",
        chat_kind: Some("group"),
        usage: &usage(100, 20, 0, 0),
        provider_id: "openai",
        model_id: "m",
        cost_delta: None,
        history_tokens: Some(4_000),
    })
        .unwrap();
    assert_eq!(
        chat.get("c-1").unwrap().unwrap().history_tokens,
        Some(4_000),
        "the measured occupancy must be readable back"
    );

    // A later turn that measured a smaller window overwrites it.
    chat.record_call(&ChatUsageCall {
        chat_session_id: "c-1",
        chat_kind: Some("group"),
        usage: &usage(100, 20, 0, 0),
        provider_id: "openai",
        model_id: "m",
        cost_delta: None,
        history_tokens: Some(1_500),
    })
        .unwrap();
    assert_eq!(
        chat.get("c-1").unwrap().unwrap().history_tokens,
        Some(1_500),
        "the occupancy is a per-turn snapshot, not an accumulator"
    );

    // A turn that measured nothing keeps the last known value — a missing
    // measurement is not a zero.
    chat.record_call(&ChatUsageCall {
        chat_session_id: "c-1",
        chat_kind: Some("group"),
        usage: &usage(100, 20, 0, 0),
        provider_id: "openai",
        model_id: "m",
        cost_delta: None,
        history_tokens: None,
    })
        .unwrap();
    assert_eq!(
        chat.get("c-1").unwrap().unwrap().history_tokens,
        Some(1_500),
        "an unmeasured turn must not erase the last measurement"
    );
}

/// A row that never measured its occupancy reads as "no measurement"
/// (`None`), never as a fabricated zero (ADR-027: 宁可 miss 也不估计).
#[test]
fn history_tokens_is_none_when_never_measured() {
    let dir = tempfile::tempdir().unwrap();
    let (chat, _sm) = stores(dir.path());
    chat.record_call(&ChatUsageCall {
        chat_session_id: "c-1",
        chat_kind: None,
        usage: &usage(10, 2, 0, 0),
        provider_id: "openai",
        model_id: "m",
        cost_delta: None,
        history_tokens: None,
    })
        .unwrap();
    assert_eq!(chat.get("c-1").unwrap().unwrap().history_tokens, None);
}

#[test]
fn forget_drops_the_row_and_nets_the_aggregate() {
    let dir = tempfile::tempdir().unwrap();
    let (chat, sm) = stores(dir.path());

    // Two chats plus a session row: forget must subtract ONLY the targeted
    // chat's breakdown, leaving the other detail sources untouched.
    sm.upsert(&make_session(
        "s-1",
        SessionTokens {
            last_input: 10,
            last_output: 5,
            total_input: 10,
            total_output: 5,
            ..Default::default()
        },
    ))
    .unwrap();
    chat.record_call(&ChatUsageCall {
        chat_session_id: "c-1",
        chat_kind: Some("group"),
        usage: &usage(100, 20, 0, 0),
        provider_id: "openai",
        model_id: "gpt-4o-mini",
        cost_delta: Some(0.01),
        history_tokens: None,
    })
        .unwrap();
    chat.record_call(&ChatUsageCall {
        chat_session_id: "c-2",
        chat_kind: Some("direct"),
        usage: &usage(7, 3, 0, 0),
        provider_id: "openai",
        model_id: "gpt-4o-mini",
        cost_delta: Some(0.001),
        history_tokens: None,
    })
        .unwrap();
    assert!(sm.token_stats_consistent().unwrap());

    assert!(chat.forget("c-1").unwrap());

    assert_eq!(chat.count().unwrap(), 1);
    assert_eq!(chat.totals().unwrap(), (7, 3, 0, 0));
    let stats = sm.token_stats().unwrap();
    let row = stats.iter().find(|s| s.model == "gpt-4o-mini").unwrap();
    // Only the surviving chat's detail feeds the aggregate: the session
    // row carries totals but no `model_usage` breakdown, so it contributes
    // nothing to `token_stats` (the aggregate is detail-driven).
    assert_eq!(row.total_input, 7);
    assert_eq!(row.total_output, 3);
    // The deleted chat's spend is gone from the aggregate too — exactly
    // the session-delete contract, extended to the ledger.
    assert!(sm.token_stats_consistent().unwrap());
    sm.rebuild_token_stats().unwrap();
    assert!(sm.token_stats_consistent().unwrap());
    assert_eq!(stats_sum(&sm), (7, 3));
}

#[test]
fn forget_is_idempotent_on_a_missing_row() {
    let dir = tempfile::tempdir().unwrap();
    let (chat, sm) = stores(dir.path());

    chat.record_call(&ChatUsageCall {
        chat_session_id: "c-1",
        chat_kind: None,
        usage: &usage(100, 20, 0, 0),
        provider_id: "openai",
        model_id: "m",
        cost_delta: None,
        history_tokens: None,
    })
        .unwrap();
    // Unknown chat: reports false and must not touch the aggregate.
    assert!(!chat.forget("c-never-recorded").unwrap());
    // Second hit on an already-forgotten chat: same.
    assert!(chat.forget("c-1").unwrap());
    assert!(!chat.forget("c-1").unwrap());
    assert_eq!(chat.count().unwrap(), 0);
    let stats = sm.token_stats().unwrap();
    assert!(stats.iter().all(|s| s.total_input == 0 && s.total_output == 0));
    assert!(sm.token_stats_consistent().unwrap());
}

/// The per-chat usage view (plan §4.6 phase two) reads one row by chat
/// id: counters, call count, and the per-model breakdown — and a chat
/// this ledger never saw is `None`, not an error. The serde shape is
/// pinned too: the runtime endpoint serializes the row verbatim onto the
/// wire the desktop `chatUsage.ts` decodes.
#[test]
fn get_returns_the_row_or_none() {
    let dir = tempfile::tempdir().unwrap();
    let (chat, _sm) = stores(dir.path());
    chat.record_call(&ChatUsageCall {
        chat_session_id: "c-1",
        chat_kind: Some("user_group"),
        usage: &usage(100, 10, 60, 5),
        provider_id: "mock",
        model_id: "m1",
        cost_delta: Some(0.5),
        history_tokens: None,
    })
    .unwrap();

    let row = chat.get("c-1").unwrap().expect("row exists");
    assert_eq!(row.chat_session_id, "c-1");
    assert_eq!(row.chat_kind.as_deref(), Some("user_group"));
    assert_eq!(row.llm_call_count, 1);
    assert_eq!(
        (row.tokens.total_input, row.tokens.total_output),
        (100, 10)
    );
    let detail = row.model_usage.as_ref().expect("one bucket");
    let entry = detail.get("mock|m1").unwrap();
    assert_eq!(
        (entry.input, entry.output, entry.cache_read, entry.cache_write),
        (100, 10, 60, 5)
    );
    assert_eq!(entry.cost, Some(0.5));

    // Miss = legitimate empty answer (human-only chat, other agent's chat).
    assert!(chat.get("c-missing").unwrap().is_none());

    // Wire shape consumed by the desktop (snake_case, flat counters).
    let json = serde_json::to_value(&row).unwrap();
    assert_eq!(json["chat_session_id"], "c-1");
    assert_eq!(json["llm_call_count"], 1);
    assert_eq!(json["tokens"]["total_input"], 100);
    assert_eq!(json["model_usage"]["mock|m1"]["cost"], 0.5);
}
