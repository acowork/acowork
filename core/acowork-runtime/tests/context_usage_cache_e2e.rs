//! End-to-end integration tests for the ADR-066 push path that
//! delivers `ContextUsageInfo.cache_*` to the desktop client.
//!
//! These tests exercise the full chain that was broken by the P0 bug
//! fixed in `loop_context.rs:1497` (the main push path was not copying
//! `total_cache_read_tokens` / `total_cache_write_tokens` from the
//! persisted `SessionTokens` into the push payload, so the frontend
//! always showed "—" and 0% cached even after real cache hits).
//!
//! Coverage:
//!   1. `patch_session_totals` is the centralised helper that every push
//!      site MUST use — direct test of the helper.
//!   2. End-to-end: `accumulate_llm_usage` with cache → close → resume
//!      → `build_context_usage_from_persisted` → ContextUsageInfo
//!      carries `Some(_)` for all four cache fields.
//!   3. Cumulative cache accumulates correctly across multiple LLM
//!      round-trips.
//!   4. OpenAI-style path (cache_write stays 0 — OpenAI has no concept)
//!      does not panic and yields `Some(0)` rather than `None`.
//!   5. Anthropic-style path (both cache_read and cache_write > 0)
//!      round-trips both cumulative fields.
//!   6. `build_context_usage_from_persisted` with `None` cumulative
//!      tokens leaves cache fields at `None` (legacy scalar path
//!      unaffected).

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use acowork_core::protocol::{ContextUsageInfo, ModelCapabilitiesInfo};
use acowork_core::providers::traits::UsageInfo;
use acowork_runtime::agent::context::{build_context_usage_from_persisted, patch_session_totals};
use acowork_runtime::conversation::{ ConversationSession, SessionConfig, SessionTokens, ModelUsageAttribution };
use tempfile::TempDir;

const AGENT_ID: &str = "com.acowork.test.cache_e2e";

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
    }
}

fn usage_with_cache(prompt: u64, completion: u64, cache_read: u64, cache_write: u64) -> UsageInfo {
    UsageInfo {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt.saturating_add(completion),
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        ..Default::default()
    }
}

// ── 1. patch_session_totals direct test ────────────────────────────────

/// The single source of truth for cumulative session fields. Three push
/// sites in `loop_context.rs` go through this helper — verifying it
/// here means any future refactor that breaks one site must break all
/// three, which is what we want.
#[test]
fn patch_session_totals_populates_all_cumulative_fields() {
    let mut info = ContextUsageInfo {
        context_window: 200_000,
        input_tokens: 1_000,
        output_tokens: 200,
        total_tokens: 1_200,
        max_input_tokens: None,
        usable_context: 183_616,
        usage_percent: 1,
        total_input_tokens: None,
        total_output_tokens: None,
        agent_total_input_tokens: None,
        agent_total_output_tokens: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        total_cache_read_tokens: None,
        total_cache_write_tokens: None,
        agent_total_cache_read_tokens: None,
        agent_total_cache_write_tokens: None,
        sections: None,
        iteration: None,
        avg_tps: None,
        avg_ttft_ms: None,
    };
    // Before patch: cache fields default to None.
    assert_eq!(info.total_input_tokens, None);
    assert_eq!(info.total_cache_read_tokens, None);
    assert_eq!(info.total_cache_write_tokens, None);

    let tokens = SessionTokens {
        last_input: 1_000,
        last_output: 200,
        total_input: 5_000,
        total_output: 800,
        last_cache_read: 1_600,
        last_cache_write: 100,
        total_cache_read: 4_000,
        total_cache_write: 250,
    };

    patch_session_totals(&mut info, &tokens);

    // Cumulative fields patched from SessionTokens.
    assert_eq!(info.total_input_tokens, Some(5_000));
    assert_eq!(info.total_output_tokens, Some(800));
    assert_eq!(info.total_cache_read_tokens, Some(4_000));
    assert_eq!(info.total_cache_write_tokens, Some(250));

    // Non-cumulative fields untouched.
    assert_eq!(info.input_tokens, 1_000);
    assert_eq!(info.output_tokens, 200);
    assert_eq!(info.total_tokens, 1_200);
}

// ── 2. End-to-end push path (regression for the loop_context.rs P0 bug)

/// Real `ConversationSession` records cache hits through
/// `accumulate_llm_usage`. After close + resume, the persisted
/// `SessionTokens` is passed to `build_context_usage_from_persisted` —
/// the EXACT call that the runtime makes to produce the push payload.
/// Before the fix, `total_cache_read_tokens` was `None` here and the
/// "Total Cache Read" indicator always showed "—".
#[tokio::test]
async fn push_path_carries_total_cache_fields_after_resume() {
    let dir = TempDir::new().unwrap();
    let session_id = "push_path_cache_regression";

    // Anthropic-style round: cache_read + cache_write both > 0.
    let session = make_session(&dir, session_id);
    session.accumulate_llm_usage(&usage_with_cache(10_000, 800, 2_000, 500), &ModelUsageAttribution::default());
    session.accumulate_llm_usage(&usage_with_cache(15_000, 1_500, 4_500, 1_000), &ModelUsageAttribution::default());
    session.close().await.expect("close");

    // Resume reads the persisted SessionTokens.
    let resumed =
        ConversationSession::resume(dir.path(), session_id, Arc::new(AtomicUsize::new(0)))
            .expect("resume");
    let persisted = resumed.0.tokens().expect("persisted tokens");

    // Sanity-check the persisted snapshot covers the cumulative cache fields.
    assert_eq!(persisted.last_cache_read, 4_500);
    assert_eq!(persisted.last_cache_write, 1_000);
    assert_eq!(persisted.total_cache_read, 6_500);
    assert_eq!(persisted.total_cache_write, 1_500);

    // The runtime's actual call to produce the push payload.
    let info = build_context_usage_from_persisted(
        &caps(200_000, 16_384),
        persisted.last_input,
        persisted.last_output,
        32_768,
        None,
        Some(&persisted),
        None,
        Some(&acowork_runtime::conversation::SessionPerf {
            tps_sum: 184.5,
            tps_samples: 3,
            ttft_ms_sum: 720.0,
            ttft_ms_samples: 3,
        }),
    );

    // Per-turn cache (last turn only — comes from SessionTokens.last_*).
    assert_eq!(
        info.cache_read_tokens,
        Some(4_500),
        "per-turn cache_read must surface on the wire (was the original P0 bug)"
    );
    assert_eq!(info.cache_write_tokens, Some(1_000));

    // Cumulative cache (TOTAL — also was the original P0 bug at
    // loop_context.rs:1497 before the patch_session_totals helper).
    assert_eq!(
        info.total_cache_read_tokens,
        Some(6_500),
        "cumulative cache_read must surface on the wire"
    );
    assert_eq!(
        info.total_cache_write_tokens,
        Some(1_500),
        "cumulative cache_write must surface on the wire"
    );

    // Cumulative in/out also patched.
    assert_eq!(info.total_input_tokens, Some(25_000));
    assert_eq!(info.total_output_tokens, Some(2_300));
}

// ── 3. Cumulative cache accumulates across multiple LLM calls ────────

/// Three round-trips with different cache hits: cumulative cache must
/// be the saturating sum of every round (mirrors the input/output
/// rule in `accumulate_llm_usage`).
#[tokio::test]
async fn cumulative_cache_accumulates_across_calls() {
    let dir = TempDir::new().unwrap();
    let session_id = "cache_accumulate";

    let session = make_session(&dir, session_id);
    session.accumulate_llm_usage(&usage_with_cache(8_000, 600, 1_000, 200), &ModelUsageAttribution::default());
    session.accumulate_llm_usage(&usage_with_cache(12_000, 900, 3_000, 0), &ModelUsageAttribution::default());
    session.accumulate_llm_usage(&usage_with_cache(20_000, 1_500, 7_500, 1_500), &ModelUsageAttribution::default());
    session.close().await.expect("close");

    let resumed =
        ConversationSession::resume(dir.path(), session_id, Arc::new(AtomicUsize::new(0))).unwrap();
    let persisted = resumed.0.tokens().unwrap();

    let info = build_context_usage_from_persisted(
        &caps(200_000, 16_384),
        persisted.last_input,
        persisted.last_output,
        32_768,
        None,
        Some(&persisted),
        None,
        Some(&acowork_runtime::conversation::SessionPerf {
            tps_sum: 184.5,
            tps_samples: 3,
            ttft_ms_sum: 720.0,
            ttft_ms_samples: 3,
        }),
    );

    // Per-turn values are the most-recent round only.
    assert_eq!(info.cache_read_tokens, Some(7_500));
    assert_eq!(info.cache_write_tokens, Some(1_500));

    // Cumulative cache is the sum across all three rounds.
    assert_eq!(info.total_cache_read_tokens, Some(11_500));
    assert_eq!(info.total_cache_write_tokens, Some(1_700));
}

// ── 4. OpenAI path: cache_write stays 0 → Some(0) not None ────────────

/// OpenAI providers don't report cache_write (only Anthropic does).
/// `cache_write_tokens: 0` in the source MUST surface as `Some(0)` on
/// the wire, not `None` — the frontend distinguishes "Provider doesn't
/// report this" (`None` → "—") from "Provider reports 0 cache writes"
/// (`Some(0)` → "0").
#[tokio::test]
async fn openai_zero_cache_write_surfaces_as_some_zero() {
    let dir = TempDir::new().unwrap();
    let session_id = "openai_zero_cache_write";

    let session = make_session(&dir, session_id);
    session.accumulate_llm_usage(&usage_with_cache(5_000, 400, 2_500, 0), &ModelUsageAttribution::default());
    session.close().await.expect("close");

    let resumed =
        ConversationSession::resume(dir.path(), session_id, Arc::new(AtomicUsize::new(0))).unwrap();
    let persisted = resumed.0.tokens().unwrap();

    let info = build_context_usage_from_persisted(
        &caps(200_000, 16_384),
        persisted.last_input,
        persisted.last_output,
        32_768,
        None,
        Some(&persisted),
        None,
        Some(&acowork_runtime::conversation::SessionPerf {
            tps_sum: 184.5,
            tps_samples: 3,
            ttft_ms_sum: 720.0,
            ttft_ms_samples: 3,
        }),
    );

    // cache_read populated (OpenAI does report cached_tokens).
    assert_eq!(info.cache_read_tokens, Some(2_500));
    assert_eq!(info.total_cache_read_tokens, Some(2_500));

    // cache_write explicitly Some(0) — frontend uses this to render
    // "0" instead of "—" for OpenAI providers.
    assert_eq!(
        info.cache_write_tokens,
        Some(0),
        "OpenAI cache_write=0 must surface as Some(0), not None"
    );
    assert_eq!(info.total_cache_write_tokens, Some(0));
}

// ── 5. Anthropic path: cache_write > 0 round-trips ─────────────────────

/// Sanity-check that the Anthropic-style path (cache_read + cache_write
/// both > 0) round-trips both cumulative fields — independent of the
/// OpenAI path so a regression in one cannot mask the other.
#[tokio::test]
async fn anthropic_cache_write_round_trips() {
    let dir = TempDir::new().unwrap();
    let session_id = "anthropic_cache_write";

    let session = make_session(&dir, session_id);
    session.accumulate_llm_usage(&usage_with_cache(12_000, 1_000, 6_000, 3_000), &ModelUsageAttribution::default());
    session.accumulate_llm_usage(&usage_with_cache(14_000, 1_200, 8_000, 0), &ModelUsageAttribution::default());
    session.close().await.expect("close");

    let resumed =
        ConversationSession::resume(dir.path(), session_id, Arc::new(AtomicUsize::new(0))).unwrap();
    let persisted = resumed.0.tokens().unwrap();

    let info = build_context_usage_from_persisted(
        &caps(200_000, 16_384),
        persisted.last_input,
        persisted.last_output,
        32_768,
        None,
        Some(&persisted),
        None,
        Some(&acowork_runtime::conversation::SessionPerf {
            tps_sum: 184.5,
            tps_samples: 3,
            ttft_ms_sum: 720.0,
            ttft_ms_samples: 3,
        }),
    );

    // Last-turn cache_write is 0 (round 2 wrote 0) but cumulative is 3000.
    assert_eq!(info.cache_write_tokens, Some(0));
    assert_eq!(
        info.total_cache_write_tokens,
        Some(3_000),
        "Anthropic cache_write must accumulate across calls"
    );
    assert_eq!(info.total_cache_read_tokens, Some(14_000));
}

// ── 6. Legacy scalar path: cumulative=None leaves cache at None ───────

/// When `cumulative_tokens` is `None` (legacy callers, unit tests that
/// only have last-turn scalars), cache fields MUST stay at `None` so
/// the wire format is "field absent" — the frontend uses the absence
/// to render "—". This protects the path from accidentally inventing
/// a fake `Some(0)`.
#[test]
fn legacy_scalar_path_leaves_cache_fields_absent() {
    let info = build_context_usage_from_persisted(
        &caps(128_000, 16_384),
        45_000,
        1_200,
        32_768,
        None,
        None, // no cumulative snapshot
        None, // no persisted iteration count
        None, // no persisted speed / latency averages
    );

    assert_eq!(info.input_tokens, 45_000);
    assert_eq!(info.output_tokens, 1_200);

    // Per-turn cache fields come from the synthetic UsageInfo (which
    // has cache_read_tokens=0 by default). They're Some(0), not None,
    // because `compute_context_usage` always copies them through.
    assert_eq!(info.cache_read_tokens, Some(0));
    assert_eq!(info.cache_write_tokens, Some(0));

    // Cumulative fields stay at None — the wire format omits them so
    // the frontend renders "—" for total_cache_read/write.
    assert_eq!(info.total_input_tokens, None);
    assert_eq!(info.total_output_tokens, None);
    assert_eq!(info.total_cache_read_tokens, None);
    assert_eq!(info.total_cache_write_tokens, None);
    assert_eq!(info.agent_total_cache_read_tokens, None);
    assert_eq!(info.agent_total_cache_write_tokens, None);
}
