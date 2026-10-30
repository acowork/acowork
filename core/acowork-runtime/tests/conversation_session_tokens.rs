//! Integration tests for the session token accounting path on
//! `ConversationSession`.
//!
//! These tests exercise the full lifecycle across the modify points:
//!   - `ConversationSession::accumulate_llm_usage` → in-memory state
//!   - `ConversationSession::write_meta`           → session-meta store
//!   - `ConversationSession::resume`               → reload from the store
//!   - `ConversationSession::tokens()`             → observed by callers
//!   - `SessionMeta` serde round-trip              → JSON shape correctness
//!   - `build_context_usage_from_persisted`        → resume → ContextUsage
//!   - `Clone` after `close`                       → spawn safety
//!
//! No mocks. All state is a real SQLite store on a `tempfile::TempDir`.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use acowork_core::protocol::ModelCapabilitiesInfo;
use acowork_core::providers::traits::UsageInfo;
use acowork_runtime::agent::context::build_context_usage_from_persisted;
use acowork_runtime::conversation::{
    ConversationSession, ModelUsageAttribution, SessionConfig, SessionMeta, SessionTokens, read_session_meta,
    write_session_meta,
};
use tempfile::TempDir;

const AGENT_ID: &str = "com.acowork.test.session_tokens";

/// Build a `ConversationSession` with a real `TempDir` work directory.
fn make_session(dir: &TempDir, session_id: &str) -> ConversationSession {
    let cfg = SessionConfig {
        agent_id: AGENT_ID.to_string(),
        workspace_id: None,
        model: Some("gpt-4".to_string()),
        provider: Some("openai".to_string()),
    };
    let committed = Arc::new(AtomicUsize::new(0));
    let (session, _config_rx, _state_rx) =
        ConversationSession::new(dir.path(), session_id, cfg, 0, committed).expect("new");
    session
}

/// Build a `UsageInfo` from raw prompt/completion counts.
///
/// `total_tokens` uses saturating arithmetic so overflow-protection tests
/// (e.g. `prompt_tokens == u64::MAX`) can reuse this helper without the
/// helper itself panicking on the `prompt + completion` line.
fn usage(prompt: u64, completion: u64) -> UsageInfo {
    UsageInfo {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt.saturating_add(completion),
        ..Default::default()
    }
}

/// Build a `ModelCapabilitiesInfo` for `build_context_usage_from_persisted`
/// smoke checks.
fn caps(context_window: u64, max_output_tokens: u64) -> ModelCapabilitiesInfo {
    ModelCapabilitiesInfo {
        context_window,
        max_output_tokens,
        max_input_tokens: None,
        supports_tool_calling: true,
        supports_reasoning: None,
        supports_attachment: None,
        supports_temperature: None,
        cost: None,
        modalities: None,
        name: None,
        family: None,
        knowledge_cutoff: None,
        default_reasoning_effort: None,
        thinking_mode: None,
        reasoning_efforts: None,
    }
}

// ─── accumulate → close → resume round-trip ─────────────────────────────

#[tokio::test]
async fn accumulate_then_resume_round_trip_preserves_session_tokens() {
    let dir = TempDir::new().unwrap();
    let session_id = "session_tokens_roundtrip";

    // Create + accumulate (simulating 3 LLM round-trips, including one
    // Provider fallback where prompt_tokens == 0).
    let session = make_session(&dir, session_id);
    session.accumulate_llm_usage(&usage(10_000, 800), &ModelUsageAttribution::default());
    session.accumulate_llm_usage(&usage(0, 200), &ModelUsageAttribution::default()); // Provider fallback — must skip total_input
    session.accumulate_llm_usage(&usage(15_000, 1_500), &ModelUsageAttribution::default());

    let in_memory = session
        .tokens()
        .expect("tokens should be set after 3 calls");
    assert_eq!(
        in_memory,
        SessionTokens {
            last_input: 15_000, // most recent raw value
            last_output: 1_500,
            total_input: 25_000, // skipped the prompt=0 call (10k + 15k)
            total_output: 2_500, // all 3 calls counted (800 + 200 + 1500)
            // ADR-066: cache fields are 0 in this test because the
            // `usage()` helper does not populate them (zero-cache path).
            ..Default::default()
        }
    );

    session.close().await.expect("close");

    // Resume and confirm disk round-trip is lossless.
    let resumed =
        ConversationSession::resume(dir.path(), session_id, Arc::new(AtomicUsize::new(0)))
            .expect("resume must succeed");

    let from_disk = resumed
        .0
        .tokens()
        .expect("resumed session should have tokens on disk");
    assert_eq!(
        from_disk, in_memory,
        "meta file round-trip must preserve all 4 fields exactly"
    );

    // Verify raw JSON on disk (catches accidentally-omitted fields).
    let meta_on_disk: SessionMeta =
        read_session_meta(&dir.path().join("conversations"), session_id).expect("read meta");
    assert_eq!(meta_on_disk.tokens, Some(in_memory));
}

// ─── resume → ContextUsage emission path ────────────────────────────────

/// Validates the chain that `session_task` depends on at resume time.
///
/// The resume path reads `tokens()` and feeds `last_input/last_output`
/// into `build_context_usage_from_persisted` to reproduce a
/// `ContextUsageInfo` for the frontend. This test replicates the exact
/// arguments that call site would pass.
#[tokio::test]
async fn resume_tokens_feed_context_usage_emission() {
    let dir = TempDir::new().unwrap();
    let session_id = "session_tokens_resume_ctx";

    let session = make_session(&dir, session_id);
    session.accumulate_llm_usage(&usage(45_000, 1_200), &ModelUsageAttribution::default());
    session.close().await.expect("close");

    let resumed =
        ConversationSession::resume(dir.path(), session_id, Arc::new(AtomicUsize::new(0)))
            .expect("resume");

    let persisted = resumed.0.tokens().expect("tokens after resume");
    let caps128k = caps(128_000, 16_384);

    let ctx = build_context_usage_from_persisted(
        &caps128k,
        persisted.last_input,
        persisted.last_output,
        32_768,           // max_output_limit
        None,             // no context_window_override
        Some(&persisted), // ADR-027: cumulative totals must flow through
        None,             // no persisted iteration count
        None,             // no persisted speed / latency averages
    );

    assert_eq!(ctx.input_tokens, 45_000);
    assert_eq!(ctx.output_tokens, 1_200);
    assert_eq!(ctx.total_tokens, 46_200);
    // Window-derived fields come from *current* model caps, not persisted
    // values — guarantees the resume path reflects the active model.
    assert_eq!(ctx.context_window, 128_000);
    assert!(ctx.usage_percent > 0 && ctx.usage_percent <= 100);

    // ADR-027: cumulative session totals must be populated on the emitted
    // ContextUsageInfo so the frontend status panel can render session-level
    // Total Input / Total Output figures (distinct from per-turn last values).
    // We only had one accumulate call so cumulative == last here, but the
    // wiring matters: future sessions with multiple rounds will surface the
    // accumulated sum.
    assert_eq!(ctx.total_input_tokens, Some(45_000));
    assert_eq!(ctx.total_output_tokens, Some(1_200));
}

/// After multiple LLM rounds, `last_*` and `total_*` must diverge — the
/// frontend status panel relies on this to render distinct rows for
/// "Prompt / Completion" (per-turn) and "Total Input / Total Output"
/// (cumulative session). This test asserts the wiring from
/// `build_context_usage_from_persisted` carries the cumulative sum through
/// the ContextUsageInfo payload that the runtime pushes to the WebSocket.
#[tokio::test]
async fn resume_tokens_cumulative_totals_diverge_from_per_turn() {
    let dir = TempDir::new().unwrap();
    let session_id = "session_tokens_cumulative_divergence";

    let session = make_session(&dir, session_id);
    // Two LLM rounds with non-zero usage on both.
    session.accumulate_llm_usage(&usage(12_000, 800), &ModelUsageAttribution::default());
    session.accumulate_llm_usage(&usage(18_000, 1_400), &ModelUsageAttribution::default());
    session.close().await.expect("close");

    let resumed =
        ConversationSession::resume(dir.path(), session_id, Arc::new(AtomicUsize::new(0)))
            .expect("resume");

    let persisted = resumed.0.tokens().expect("tokens after resume");

    // Per-turn = most recent raw Provider value (18_000 / 1_400).
    assert_eq!(persisted.last_input, 18_000);
    assert_eq!(persisted.last_output, 1_400);
    // Cumulative = sum across rounds (30_000 / 2_200).
    assert_eq!(persisted.total_input, 30_000);
    assert_eq!(persisted.total_output, 2_200);

    // The frontend-facing payload must carry BOTH the per-turn fields
    // (input_tokens / output_tokens) and the cumulative fields
    // (total_input_tokens / total_output_tokens) — they are NOT the same.
    let caps128k = caps(128_000, 16_384);
    let ctx = build_context_usage_from_persisted(
        &caps128k,
        persisted.last_input,
        persisted.last_output,
        32_768,
        None,
        Some(&persisted),
        None,
        None,
    );

    assert_eq!(ctx.input_tokens, 18_000, "per-turn input tokens");
    assert_eq!(ctx.output_tokens, 1_400, "per-turn output tokens");
    assert_eq!(
        ctx.total_input_tokens,
        Some(30_000),
        "cumulative total input tokens across both rounds",
    );
    assert_eq!(
        ctx.total_output_tokens,
        Some(2_200),
        "cumulative total output tokens across both rounds",
    );

    // Sanity: cumulative != per-turn (the whole point of having both).
    assert_ne!(
        ctx.total_input_tokens.unwrap(),
        ctx.input_tokens,
        "cumulative total_input_tokens must NOT equal per-turn input_tokens",
    );
    assert!(ctx.total_input_tokens.unwrap() > ctx.input_tokens);
}

// ─── Provider fallback honesty ──────────────────────────────────────────

/// When `prompt_tokens == 0` the snapshot MUST reflect the raw 0 (not a
/// backfilled local estimate) so the frontend display matches what the
/// Provider actually returned.
#[tokio::test]
async fn provider_fallback_records_raw_zero_in_last_input() {
    let dir = TempDir::new().unwrap();
    let session_id = "session_tokens_fallback";

    let session = make_session(&dir, session_id);
    session.accumulate_llm_usage(&usage(8_000, 500), &ModelUsageAttribution::default());
    session.accumulate_llm_usage(&usage(0, 0), &ModelUsageAttribution::default()); // Provider fallback: full zero

    let t = session.tokens().unwrap();
    assert_eq!(t.last_input, 0, "raw zero, not a local estimate");
    assert_eq!(t.last_output, 0);
    assert_eq!(
        t.total_input, 8_000,
        "prior reliable call must remain in total_input"
    );
    assert_eq!(t.total_output, 500);

    session.close().await.unwrap();

    let resumed =
        ConversationSession::resume(dir.path(), session_id, Arc::new(AtomicUsize::new(0))).unwrap();
    let from_disk = resumed.0.tokens().unwrap();
    assert_eq!(from_disk.last_input, 0);
    assert_eq!(from_disk.last_output, 0);
    assert_eq!(from_disk.total_input, 8_000);
    assert_eq!(from_disk.total_output, 500);
}

// ─── Saturating arithmetic ──────────────────────────────────────────────

#[tokio::test]
async fn accumulate_saturates_at_u64_max_without_panic() {
    let dir = TempDir::new().unwrap();
    let session_id = "session_tokens_overflow";

    let session = make_session(&dir, session_id);
    session.accumulate_llm_usage(&usage(u64::MAX, u64::MAX), &ModelUsageAttribution::default());
    session.accumulate_llm_usage(&usage(1, 1), &ModelUsageAttribution::default()); // would overflow naive add

    let t = session.tokens().unwrap();
    assert_eq!(t.last_input, 1);
    assert_eq!(t.last_output, 1);
    assert_eq!(t.total_input, u64::MAX, "must saturate, not panic");
    assert_eq!(t.total_output, u64::MAX);

    session.close().await.unwrap();

    let resumed =
        ConversationSession::resume(dir.path(), session_id, Arc::new(AtomicUsize::new(0))).unwrap();
    let t2 = resumed.0.tokens().unwrap();
    // ponytail: the SQLite token columns are i64, so the persisted counter
    // saturates at i64::MAX rather than u64::MAX (see `token_to_i64`). The
    // point here is "no wrap to 0", not the exact ceiling.
    assert_eq!(t2.total_input, i64::MAX as u64);
    assert_eq!(t2.total_output, i64::MAX as u64);
}

// ─── Clone-after-close semantics ────────────────────────────────────────

/// Replicates the spawn pattern used by the tail-distiller on session
/// close: a clone is captured into `tokio::spawn` while the parent
/// proceeds to `.close()`. The clone must still be able to record to the
/// on-disk meta (its `accumulate_llm_usage` calls `write_meta` directly
/// via `std::fs`, independent of the closed writer thread).
#[tokio::test]
async fn clone_after_parent_close_still_persists_meta() {
    let dir = TempDir::new().unwrap();
    let session_id = "session_tokens_clone_after_close";

    let session = make_session(&dir, session_id);
    let distiller_view = session.clone(); // simulate tokio::spawn capture

    session.close().await.expect("parent close");

    distiller_view.accumulate_llm_usage(&usage(42_000, 7_000), &ModelUsageAttribution::default());

    let from_disk = read_session_meta(&dir.path().join("conversations"), session_id)
        .expect("read meta from clone-write");
    assert_eq!(
        from_disk.tokens,
        Some(SessionTokens {
            last_input: 42_000,
            last_output: 7_000,
            total_input: 42_000,
            total_output: 7_000,
            // ADR-066: cache fields default to 0 (zero-cache path).
            ..Default::default()
        })
    );
}

// ─── write_meta cooldown bypass for token accumulation ──────────────────

/// `accumulate_llm_usage` calls `write_meta` on every invocation. The
/// cooldown that guards `append_message` does NOT apply — consecutive
/// calls must each land on disk so the context-usage indicator never
/// lags behind an LLM round.
#[tokio::test]
async fn consecutive_accumulations_each_reach_disk() {
    let dir = TempDir::new().unwrap();
    let session_id = "session_tokens_no_cooldown";

    let session = make_session(&dir, session_id);
    for i in 1..=5 {
        session.accumulate_llm_usage(&usage(i * 1000, i * 100), &ModelUsageAttribution::default());
    }
    session.close().await.unwrap();

    let meta = read_session_meta(&dir.path().join("conversations"), session_id).unwrap();
    assert_eq!(
        meta.tokens.unwrap().total_input,
        15_000,
        "every call must accumulate into total_input (1+2+3+4+5)*1000"
    );

    // Resume and cross-check the snapshot.
    let resumed =
        ConversationSession::resume(dir.path(), session_id, Arc::new(AtomicUsize::new(0))).unwrap();
    let from_resume = resumed.0.tokens().unwrap();
    assert_eq!(from_resume.total_input, 15_000);
    assert_eq!(from_resume.last_input, 5_000);
    assert_eq!(from_resume.last_output, 500);
}

// ─── Backward compatibility with old meta files ─────────────────────────

/// A meta file written by the previous shape (no `tokens` field) must
/// load successfully with `tokens: None` so the frontend can degrade
/// gracefully instead of crashing on missing fields.
#[tokio::test]
async fn legacy_meta_file_loads_with_tokens_none() {
    let dir = TempDir::new().unwrap();
    let conv_dir = dir.path().join("conversations");
    std::fs::create_dir_all(&conv_dir).unwrap();

    let session_id = "legacy_session";
    let legacy_meta = SessionMeta {
        version: 2,
        session_id: session_id.to_string(),
        agent_id: AGENT_ID.to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        initiator: None,
        visibility: None,
        title: Some("legacy".to_string()),
        workspace_id: None,
        model: Some("gpt-4".to_string()),
        provider: Some("openai".to_string()),
        account_id: None,
        reasoning_effort: None,
        temperature: None,
        context_window: None,
        todos: None,
        message_count: 7,
        last_active_at: "2026-01-01T00:00:00Z".to_string(),
        tokens: None, // missing in legacy format
        model_ratio: None,
        llm_call_counter: None,
        perf: None,
        last_compaction_offset: None,
        corrupted: false,
    chat_session_id: None,
        model_usage: None,
    };
    write_session_meta(&conv_dir, &legacy_meta).expect("write legacy");

    // Hand-craft an empty JSONL file so `resume` doesn't bail out.
    let jsonl_path = conv_dir.join(format!("{}.jsonl", session_id));
    std::fs::write(&jsonl_path, "").unwrap();

    let resumed =
        ConversationSession::resume(dir.path(), session_id, Arc::new(AtomicUsize::new(0)))
            .expect("legacy resume must succeed");
    assert_eq!(
        resumed.0.tokens(),
        None,
        "legacy meta must yield tokens=None (frontend fallback)"
    );
}

// ─── Wire format stability ──────────────────────────────────────────────

/// Pins the on-disk JSON keys for `SessionTokens` so a future refactor
/// cannot silently rename a field and break the desktop app's reader.
#[test]
fn session_tokens_wire_format_is_stable() {
    let t = SessionTokens {
        last_input: 100,
        last_output: 50,
        total_input: 250,
        total_output: 75,
        // ADR-066: cache fields are persisted alongside the legacy
        // 4 (no schema break — they ride on the same v3 envelope).
        last_cache_read: 25,
        last_cache_write: 10,
        total_cache_read: 60,
        total_cache_write: 20,
    };
    let json = serde_json::to_string(&t).unwrap();
    assert!(json.contains("\"last_input\":100"));
    assert!(json.contains("\"last_output\":50"));
    assert!(json.contains("\"total_input\":250"));
    assert!(json.contains("\"total_output\":75"));
    // ADR-066: cache keys ship alongside the legacy 4 (same v3
    // envelope, no schema break — old readers ignore them).
    assert!(json.contains("\"last_cache_read\":25"));
    assert!(json.contains("\"last_cache_write\":10"));
    assert!(json.contains("\"total_cache_read\":60"));
    assert!(json.contains("\"total_cache_write\":20"));

    // Reverse round-trip.
    let back: SessionTokens = serde_json::from_str(&json).unwrap();
    assert_eq!(back, t);

    // Default value serializes as a stable 0-filled object.
    let default_json = serde_json::to_string(&SessionTokens::default()).unwrap();
    assert_eq!(
        default_json,
        "{\"last_input\":0,\"last_output\":0,\"total_input\":0,\"total_output\":0,\"last_cache_read\":0,\"last_cache_write\":0,\"total_cache_read\":0,\"total_cache_write\":0}"
    );
}

// ─── last_compaction_offset persistence (regression for the
//     "restore loads full session because meta.json never recorded the
//     compaction offset" bug) ────────────────────────────────────────

use acowork_runtime::conversation::CompactionEventMeta;

/// After `append_compaction_event`, the offset must be visible in
/// `build_meta()` immediately (the writer uses a sync handshake) and
/// must survive a `write_meta` → `read_session_meta` round-trip.
#[tokio::test]
async fn compaction_offset_persists_to_meta_after_append() {
    let dir = TempDir::new().unwrap();
    let session_id = "compaction_persist";

    // Seed the JSONL with a user + assistant round so compaction has a
    // non-zero starting offset (compaction captures `seek(End)` from
    // whatever the writer's current EOF is).
    let session = make_session(&dir, session_id);
    session.append_message("user", "hello", None);
    session.append_message("assistant", "world", None);
    // Push a non-compaction entry through first so the JSONL EOF is
    // non-zero. Then the compaction entry will land at a byte offset
    // strictly greater than 0.
    let compact_meta = CompactionEventMeta {
        compacted_from_id: "msg_a".to_string(),
        compacted_to_id: "msg_b".to_string(),
        level: 1,
        model: "gpt-4".to_string(),
        before_tokens: 1000,
        after_tokens: 500,
    };
    session.append_compaction_event("summary", compact_meta);

    // Flush the meta so the next read sees the new offset.
    // (Most mutators call write_meta internally; append_compaction_event
    // intentionally does NOT — the offset update is observable through
    // build_meta() and survives the next write_meta() that any caller
    // triggers. We trigger one explicitly here.)
    // Need a way to call the private write_meta — instead, read the
    // JSONL directly and verify the meta gets persisted through
    // accumulate_llm_usage, which always calls write_meta().
    session.accumulate_llm_usage(&usage(100, 20), &ModelUsageAttribution::default());

    let conv_dir = dir.path().join("conversations");
    let on_disk = read_session_meta(&conv_dir, session_id).expect("read meta after compaction");
    let offset = on_disk
        .last_compaction_offset
        .expect("last_compaction_offset must be Some after a compaction event");
    assert!(
        offset > 0,
        "compaction offset must be > 0 (compaction entry appended after prior entries); got {offset}"
    );

    // The JSONL itself must also contain the compaction entry — this
    // confirms both halves of the persistence path:
    //   - raw `ConversationEntry { kind: "compaction" ... }` in JSONL
    //   - `last_compaction_offset: Some(<abs>)` in meta.json
    let jsonl =
        std::fs::read_to_string(conv_dir.join(format!("{session_id}.jsonl"))).expect("read jsonl");
    assert!(
        jsonl.contains("\"kind\":\"compaction\""),
        "compaction entry must be present in JSONL"
    );
}

/// On resume, `last_compaction_offset` must be hydrated into the shared
/// Arc so the very first `build_meta()` (called by `emit_session_state`
/// before the first new LLM call) reads the persisted offset instead of
/// `None`. This is what lets the restorer do O(1) offset skip on the
/// next restore.
#[tokio::test]
async fn resume_hydrates_last_compaction_offset_from_meta() {
    let dir = TempDir::new().unwrap();
    let session_id = "compaction_resume";

    // Phase 1: write a meta row with `last_compaction_offset: Some(1234)`
    // and a JSONL file with at least one entry so resume() doesn't bail.
    let conv_dir = dir.path().join("conversations");
    std::fs::create_dir_all(&conv_dir).unwrap();
    let meta = SessionMeta {
        version: 2,
        session_id: session_id.to_string(),
        agent_id: AGENT_ID.to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        initiator: None,
        visibility: None,
        title: None,
        workspace_id: None,
        model: Some("gpt-4".to_string()),
        provider: Some("openai".to_string()),
        account_id: None,
        reasoning_effort: None,
        temperature: None,
        context_window: None,
        todos: None,
        message_count: 1,
        last_active_at: "2026-01-01T00:00:00Z".to_string(),
        tokens: None,
        llm_call_counter: None,
        perf: None,
        model_ratio: None,
        last_compaction_offset: Some(1234),
        corrupted: false,
    chat_session_id: None,
        model_usage: None,
    };
    write_session_meta(&conv_dir, &meta).expect("write meta");
    std::fs::write(conv_dir.join(format!("{session_id}.jsonl")), "").unwrap();

    // Phase 2: resume and force a write_meta so we can observe the
    // hydrated offset.
    let (session, _config_rx, _state_rx) =
        ConversationSession::resume(dir.path(), session_id, Arc::new(AtomicUsize::new(0)))
            .expect("resume");

    session.accumulate_llm_usage(&usage(50, 10), &ModelUsageAttribution::default());

    let on_disk = read_session_meta(&conv_dir, session_id).expect("read meta after resume");
    assert_eq!(
        on_disk.last_compaction_offset,
        Some(1234),
        "resumed session must preserve the persisted last_compaction_offset"
    );
}

/// `Clone` of `ConversationSession` must share the same
/// `last_compaction_offset` Arc — otherwise a clone that outlives the
/// parent (e.g. session-close distillation that calls
/// `accumulate_llm_usage` after the parent has dropped its
/// `ConversationWriter`) would lose the offset on the next meta write.
#[tokio::test]
async fn clone_shares_last_compaction_offset_arc() {
    let dir = TempDir::new().unwrap();
    let session_id = "compaction_clone";

    let session = make_session(&dir, session_id);
    // Append a non-compaction entry so the JSONL EOF is non-zero.
    session.append_message("user", "hi", None);

    // Drop a compaction marker through the parent; the shared Arc is
    // updated synchronously via the writer's reply.
    let compact_meta = CompactionEventMeta {
        compacted_from_id: String::new(),
        compacted_to_id: String::new(),
        level: 1,
        model: "gpt-4".to_string(),
        before_tokens: 0,
        after_tokens: 0,
    };
    session.append_compaction_event("summary", compact_meta);

    // Clone the session, then close the parent. The clone must still
    // observe the offset.
    let clone = session.clone();
    drop(session);

    // Trigger a meta write via the clone (accumulate_llm_usage always
    // writes meta) so we can read the persisted offset back.
    clone.accumulate_llm_usage(&usage(10, 1), &ModelUsageAttribution::default());

    let conv_dir = dir.path().join("conversations");
    let on_disk = read_session_meta(&conv_dir, session_id).expect("read meta");
    assert!(
        on_disk.last_compaction_offset.is_some(),
        "clone that outlives parent must persist the compaction offset"
    );
}

// ─── model_ratio → meta → resume round-trip ─────────────────────────────

/// Validates the resume scene for the session token ratio: after a reliable
/// LLM usage report the loop writes both the API-counted `last_input` (via
/// `accumulate_llm_usage`) and the calibrated chars/token ratio (via
/// `set_model_ratio`) into the meta file. On resume,
/// `SessionManager::restore_anchor` reads exactly these two fields, so this
/// test pins the disk round-trip for both.
#[tokio::test]
async fn model_ratio_round_trips_through_meta_and_resume() {
    let dir = TempDir::new().unwrap();
    let session_id = "ratio_roundtrip";

    let session = make_session(&dir, session_id);
    session.accumulate_llm_usage(&usage(1000, 200), &ModelUsageAttribution::default());
    session.set_model_ratio(2.5);

    session.close().await.expect("close");

    let resumed =
        ConversationSession::resume(dir.path(), session_id, Arc::new(AtomicUsize::new(0)))
            .expect("resume must succeed");

    // Both fields SessionManager::restore_anchor consumes at resume time.
    assert_eq!(resumed.0.model_ratio(), Some(2.5));
    assert_eq!(resumed.0.tokens().map(|t| t.last_input), Some(1000));

    // Raw JSON on disk carries the field (serde round-trip honesty).
    let meta_on_disk: SessionMeta =
        read_session_meta(&dir.path().join("conversations"), session_id).expect("read meta");
    assert_eq!(meta_on_disk.model_ratio, Some(2.5));
}

// ─── session speed / TTFT samples → meta → resume → push ────────────────

/// The perf pair rides the same path as the token counters: one
/// `record_llm_round` per LLM response → one meta write → resume → the
/// `ContextUsageInfo` the frontend renders. A resumed session must show the
/// same means its live panel did, so this pins all four hops.
///
/// The four rounds deliberately mix fully-measurable and partially
/// measurable calls:
///
///   1. speed + TTFT
///   2. TTFT only (Provider reported no completion count)
///   3. speed + TTFT
///   4. neither (stream cancelled before its first token)
///
/// The speed mean therefore divides by 2 while the latency mean divides by
/// 3. Regression: the first implementation shared one `samples` counter
/// between them, which reported 233.3 t/s for this exact sequence instead
/// of the true 200 — a user-visible figure inflated by calls that never
/// measured a speed at all.
#[tokio::test]
async fn perf_samples_round_trip_through_meta_resume_and_push() {
    let dir = TempDir::new().unwrap();
    let session_id = "perf_roundtrip";

    let session = make_session(&dir, session_id);
    session.record_llm_round(&usage(1_000, 300), Some(300.0), Some(500.0), &ModelUsageAttribution::default());
    session.record_llm_round(&usage(1_000, 0), None, Some(100.0), &ModelUsageAttribution::default());
    session.record_llm_round(&usage(1_000, 100), Some(100.0), Some(300.0), &ModelUsageAttribution::default());
    // An unmeasurable round must not drag either mean toward a fake 0.
    session.record_llm_round(&usage(1_000, 50), None, None, &ModelUsageAttribution::default());

    let perf = session.perf().expect("perf recorded");
    assert_eq!(perf.tps_samples, 2, "two speed samples");
    assert_eq!(perf.ttft_ms_samples, 3, "three latency samples");
    assert!((perf.avg_tps().unwrap() - 200.0).abs() < 1e-6, "got {:?}", perf.avg_tps());
    assert!(
        (perf.avg_ttft_ms().unwrap() - 300.0).abs() < 1e-6,
        "got {:?}",
        perf.avg_ttft_ms()
    );

    session.close().await.expect("close");

    // Persisted meta carries both accumulators.
    let meta_on_disk: SessionMeta =
        read_session_meta(&dir.path().join("conversations"), session_id).expect("read meta");
    let disk_perf = meta_on_disk.perf.expect("perf persisted");
    assert_eq!(disk_perf.tps_samples, 2);
    assert_eq!(disk_perf.ttft_ms_samples, 3);

    let resumed =
        ConversationSession::resume(dir.path(), session_id, Arc::new(AtomicUsize::new(0)))
            .expect("resume must succeed");
    let tokens = resumed.0.tokens().unwrap_or_default();
    let perf = resumed.0.perf().expect("perf survives resume");
    assert!((perf.avg_tps().unwrap() - 200.0).abs() < 1e-6);
    assert!((perf.avg_ttft_ms().unwrap() - 300.0).abs() < 1e-6);

    // The resume push the frontend actually consumes carries them.
    let ctx = build_context_usage_from_persisted(
        &caps(128_000, 16_384),
        tokens.last_input,
        tokens.last_output,
        32_768,
        None,
        Some(&tokens),
        None,
        Some(&perf),
    );
    assert_eq!(ctx.avg_tps, Some(200.0));
    assert_eq!(ctx.avg_ttft_ms, Some(300.0));
}

/// One LLM round-trip is one persisted write.
///
/// `record_llm_round` is the single owner of the round's three accumulators
/// (tokens, perf, call count). Splitting them back into their individually
/// persisting wrappers would issue three full-row meta upserts per LLM call
/// on the runtime's hottest path, so the observable contract is that ONE
/// call advances all three and leaves meta consistent with them.
#[tokio::test]
async fn record_llm_round_advances_every_accumulator_in_one_call() {
    let dir = TempDir::new().unwrap();
    let session_id = "perf_single_write";

    let session = make_session(&dir, session_id);
    let iteration = session.record_llm_round(&usage(1_000, 200), Some(64.0), Some(420.0), &ModelUsageAttribution::default());
    assert_eq!(iteration, 1, "returns the post-increment 1-based count");

    assert_eq!(session.llm_call_counter(), Some(1));
    let tokens = session.tokens().expect("tokens recorded");
    assert_eq!(tokens.total_output, 200);
    let perf = session.perf().expect("perf recorded");
    assert_eq!(perf.avg_tps(), Some(64.0));
    assert_eq!(perf.avg_ttft_ms(), Some(420.0));

    // A second round accumulates rather than replaces.
    assert_eq!(session.record_llm_round(&usage(1_000, 100), Some(32.0), Some(20.0), &ModelUsageAttribution::default()), 2);
    let perf = session.perf().unwrap();
    assert_eq!(perf.avg_tps(), Some(48.0));
    assert_eq!(perf.avg_ttft_ms(), Some(220.0));

    session.close().await.expect("close");
    let meta_on_disk = read_session_meta(&dir.path().join("conversations"), session_id).unwrap();
    assert_eq!(meta_on_disk.llm_call_counter, Some(2));
    assert_eq!(meta_on_disk.perf.unwrap().tps_samples, 2);
}

/// A fresh session has no measurement at all — the push must carry
/// `None` (frontend "—"), never a fabricated zero.
#[tokio::test]
async fn perf_absent_on_a_session_that_never_called_an_llm() {
    let dir = TempDir::new().unwrap();
    let session = make_session(&dir, "perf_none");
    assert_eq!(session.perf(), None);
    session.close().await.expect("close");

    let meta_on_disk: SessionMeta =
        read_session_meta(&dir.path().join("conversations"), "perf_none").expect("read meta");
    assert_eq!(meta_on_disk.perf, None, "absent, not Some(0.0)");
}
