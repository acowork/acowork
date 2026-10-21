//! Session resume: rebuild in-memory `HistoryManager` state from a JSONL file.
//!
//! Triggered on cold-start when an existing conversation is resumed
//! (see [`crate::startup::session_init::phase_b_init_session`]).
//!
//! ## Replay rules
//!
//! The JSONL is an append-only event log. Restoration walks it front-to-back
//! and translates each entry into protocol-level [`ChatMessage`]s that match
//! the in-memory state the previous session would have had at exit time.
//!
//! ### Filtering
//!
//! - `role="thought"` → dropped (frontend-only; never enters LLM context).
//! - `role="user" | "assistant" | "system"` → preserved as-is.
//! - `role="tool_call"` → merged onto the immediately preceding `Assistant`
//!   message as a `tool_calls` entry. If no preceding assistant exists, a new
//!   empty-content assistant is synthesized to host it.
//! - `role="tool_result"` → emitted as `MessageRole::Tool` with `tool_call_id`
//!   from metadata; orphaned results (no matching tool_call in the same
//!   contiguous block) are dropped.
//! - `kind="compaction"` → produces a `User{name="compaction_summary"}`
//!   marker. Only the **last** compaction event is honored: every entry
//!   strictly before the last compaction marker (except leading `system`
//!   messages) is discarded.
//!
//!   NOTE: The marker uses `User` role (not `Assistant`) in memory to
//!   avoid an `Assistant → Assistant{tool_calls}` adjacency in the
//!   rebuilt request, which glm-5.2 on Volcano Ark rejects with
//!   `400 InvalidParameter`.  Consumers identify the marker by
//!   `name == "compaction_summary"` regardless of role.
//!
//! ### Tool-call pairing
//!
//! After replay, any `Tool` message whose `tool_call_id` does not match a
//! preceding `Assistant.tool_calls[*].id` is dropped (defensive cleanup;
//! prevents provider-side sanitize errors).
//!
//! ## Failure handling
//!
//! - Corrupt JSONL line → skipped, counted as `skipped_entry_count`.
//! - I/O error opening the file → returns `Err(RestoreError::Io)`; caller
//!   should fall back to an empty history.

use std::path::Path;

use acowork_core::providers::traits::{ChatMessage, FunctionCall, MessageRole, ToolCall};

use crate::conversation::{ConversationEntry, ENTRY_KIND_COMPACTION};

/// Outcome of a successful restore call.
#[derive(Debug, Clone)]
pub struct RestoreOutcome {
    /// Messages ready to install into `HistoryManager` via `load_restored`.
    pub messages: Vec<ChatMessage>,
    /// Whether the JSONL contained at least one `kind="compaction"` event,
    /// i.e. replay was anchored at the most recent compaction summary.
    pub had_compaction: bool,
    /// Number of JSONL entries that contributed to the final message list
    /// (after merging tool_calls into assistants).
    pub replayed_entry_count: usize,
    /// Number of JSONL entries that were skipped (corrupt, orphaned tool
    /// results, pre-compaction noise, or `thought` filter).
    pub skipped_entry_count: usize,
    /// ADR-060 v2 §5.4: the `tool_call_id`s of every *synthetic* round found
    /// in the JSONL (entry ids prefixed `inject-call-` / `inject-result-`).
    /// `HistoryManager` re-derives [`HistoryManager::injected_round_call_ids`]
    /// from this on restart so a subsequent compression skips re-injecting a
    /// round it already persisted (idempotency across process restarts, not
    /// just within one process).
    ///
    /// A set rather than the former single `Option<String>`: skill-body
    /// recall persists one round per distinct skill, so a session can legitimately
    /// hold several injected rounds at once and each needs its own gate.
    ///
    /// Empty when the JSONL holds no injected round (fresh session, or a
    /// session whose only todo_write / load_skill rounds are genuine
    /// invocations).
    pub injected_round_call_ids: std::collections::HashSet<String>,
}

/// Errors that abort restoration (caller should fall back to empty history).
#[derive(Debug)]
pub enum RestoreError {
    /// File could not be opened or read.
    Io(std::io::Error),
}

impl std::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error: {}", e),
        }
    }
}

impl std::error::Error for RestoreError {}

impl From<std::io::Error> for RestoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Parse a JSONL conversation file into a replay-ready message sequence.
///
/// ADR-024: the file has no metadata header — parsing starts at line 0.
/// The caller provides `compaction_abs` (absolute byte offset of the last
/// compaction entry, read from `meta/{session_id}.json`). When present we
/// seek straight to that byte position, retain only the leading `system`
/// entries that live before it, and replay the compaction marker plus
/// everything after it. The restored history is therefore exactly the
/// post-compaction tail — the raw rounds an earlier compaction summarised
/// away never re-enter the LLM context.
///
/// When `compaction_abs` is `None` (legacy session, or a session that has
/// never been compacted) we fall back to the original full-scan behaviour
/// and anchor on the *last* compaction marker via `rposition`.
///
/// See module docs for the full set of replay rules.
pub fn restore_history_from_jsonl(
    path: &Path,
    compaction_abs: Option<u64>,
) -> Result<RestoreOutcome, RestoreError> {
    use std::io::{BufRead, BufReader, Seek, SeekFrom};

    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);

    // ── Pass 1: collect the entries that may enter the LLM context ──────
    //
    // Fast path (`compaction_abs` = Some): the offset points at the byte
    // where the *last* compaction marker starts. Phase A scans the head of
    // the file only far enough to capture leading `system` entries (agent
    // identity / workspace context); Phase B seeks to the marker and reads
    // it plus all following entries.
    //
    // Legacy path (`compaction_abs` = None): read the whole file; Pass 2
    // anchors on the last compaction marker.
    let mut entries: Vec<ConversationEntry> = Vec::new();
    let mut skipped = 0usize;

    match compaction_abs {
        Some(abs) => {
            // Phase A: capture leading system entries located strictly
            // before the marker. Pre-compaction non-system entries are
            // pre-compaction noise and never enter the LLM context, so they
            // are consumed without being stored. A cheap substring guard
            // avoids full JSON parsing of the (possibly huge) pre-marker
            // body; compaction rows carry their own `"kind":"compaction"`
            // and are excluded by the same guard.
            let mut reached_marker = false;
            loop {
                let pos = reader.stream_position()?;
                if pos >= abs {
                    reached_marker = true;
                    break;
                }
                let mut line = String::new();
                let n = reader.read_line(&mut line)?;
                if n == 0 {
                    // EOF before the hint: the persisted offset is stale
                    // (file rewritten/truncated). Fall back to a full scan.
                    break;
                }
                if line.trim().is_empty() {
                    continue;
                }
                // Only system-kind-none rows are retained. Everything else
                // (user / assistant / tool / older compaction rows) is
                // intentionally dropped without allocation.
                let looks_like_system = line.contains("\"role\":\"system\"")
                    && !line.contains("\"kind\":\"compaction\"");
                if looks_like_system {
                    match serde_json::from_str::<ConversationEntry>(&line) {
                        Ok(entry) if entry.role == "system" && entry.kind.is_none() => {
                            entries.push(entry);
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "Restore: malformed entry before compaction offset, skipping"
                            );
                            skipped += 1;
                        }
                    }
                }
            }

            if reached_marker {
                // Phase B: seek to the marker and replay it + the rest of
                // the file verbatim.
                reader.seek(SeekFrom::Start(abs))?;
                let mut line = String::new();
                let n = reader.read_line(&mut line)?;
                if n > 0 && !line.trim().is_empty() {
                    match serde_json::from_str::<ConversationEntry>(line.trim()) {
                        Ok(entry) if entry.kind.as_deref() == Some(ENTRY_KIND_COMPACTION) => {
                            entries.push(entry);
                        }
                        _ => {
                            tracing::warn!(
                                abs,
                                "Restore: line at compaction offset is not a compaction marker"
                            );
                        }
                    }
                }
                read_all_entries(&mut reader, &mut entries, &mut skipped)?;
            }

            // The offset hint must end with the anchored marker actually
            // present. If it could not be verified (stale/corrupt hint, EOF
            // before the offset), never silently drop the conversation —
            // fall back to a top-down full scan.
            let anchored = entries
                .iter()
                .any(|e| e.kind.as_deref() == Some(ENTRY_KIND_COMPACTION));
            if !anchored {
                tracing::warn!(
                    abs,
                    "Restore: compaction offset not verifiable, falling back to full scan"
                );
                entries.clear();
                skipped = 0;
                reader.seek(SeekFrom::Start(0))?;
                read_all_entries(&mut reader, &mut entries, &mut skipped)?;
            }
        }
        None => {
            read_all_entries(&mut reader, &mut entries, &mut skipped)?;
        }
    }

    // Pass 2: locate the most recent compaction marker. On the fast path
    // there is exactly one (the anchored one); on the legacy path there may
    // be several and only the last is honored.
    let last_compaction_idx = entries
        .iter()
        .rposition(|e| e.kind.as_deref() == Some(ENTRY_KIND_COMPACTION));

    // Pass 3: build the working entry slice based on whether a compaction
    // exists. With compaction: keep leading System entries + the compaction
    // entry itself (transformed into the marker) + all entries after it.
    // Without compaction: use the full entry list.
    let working: Vec<&ConversationEntry> = if let Some(comp_idx) = last_compaction_idx {
        let leading_system: Vec<&ConversationEntry> = entries[..comp_idx]
            .iter()
            .filter(|e| e.role == "system" && e.kind.is_none())
            .collect();
        let mut v = leading_system;
        v.push(&entries[comp_idx]);
        v.extend(entries[comp_idx + 1..].iter());
        v
    } else {
        entries.iter().collect()
    };

    // Pass 4: translate entries into ChatMessages, merging adjacent tool_call
    // rows onto their preceding assistant.
    //
    // DeepSeek thinking mode requires each assistant turn that carries
    // `tool_calls` to echo back its `reasoning_content` on subsequent
    // requests (HTTP 400 otherwise).  In JSONL the reasoning lives on the
    // `thought` rows that immediately precede the tool-call assistant turn.
    // Restoring those rows straight into history would pollute the LLM
    // context (thought text is internal monologue), so instead we carry the
    // most recent `thought` as `pending_reasoning` and fold it into the
    // *next assistant message that actually makes tool calls*.  Text-only
    // assistant turns deliberately keep `reasoning_content: None` to stay
    // consistent with `handle_text_response`'s runtime behaviour.
    let mut messages: Vec<ChatMessage> = Vec::new();
    let mut replayed = 0usize;
    let mut pending_reasoning: Option<String> = None;
    // ADR-060 v2 §5.4: tool_call_ids of *synthetic* (injected) rounds seen in
    // the JSONL. Entry ids for injected rows are prefixed `inject-call-` /
    // `inject-result-` by the compact-and-inject path (loop_context.rs), so we
    // can distinguish them from genuine user todo_write / load_skill rounds —
    // HistoryManager must only remember injected ids for cross-restart
    // idempotency; treating a genuine round as already-injected would wrongly
    // suppress its injection after the next compression.
    let mut injected_round_call_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for (entry_idx, entry) in working.iter().enumerate() {
        // Compaction event → synthetic assistant marker (only honored once;
        // older compactions inside `working` shouldn't exist by construction,
        // but defensively skip them).
        if entry.kind.as_deref() == Some(ENTRY_KIND_COMPACTION) {
            pending_reasoning = None;
            // Compaction markers live at `User` role in memory (see
            // `HistoryManager::replace_middle_with_summary`).  Using
            // `Assistant` here would recreate the
            // `Assistant → Assistant{tool_calls}` adjacency that some
            // providers reject with 400 InvalidParameter.
            messages.push(ChatMessage {
                role: MessageRole::User,
                content: entry.content.clone(),
                name: Some("compaction_summary".to_string()),
                ..Default::default()
            });
            replayed += 1;
            continue;
        }

        // Internal runtime entries (e.g. the output-budget nudge) are part of
        // the live LLM context but must NOT be replayed as genuine turns: an
        // interrupted turn is never resumed after a restart, so replaying the
        // nudge would leak a stale internal instruction into subsequent turns.
        // Mirrors the Desktop transcript, which filters the same marker.
        if entry.is_internal() {
            pending_reasoning = None;
            skipped += 1;
            continue;
        }

        match entry.role.as_str() {
            "thought" => {
                // Reasoning content for the upcoming tool-call assistant
                // turn. Kept out of `messages` (never a standalone LLM
                // message) but preserved as `reasoning_content` on the
                // assistant that actually carries tool_calls.
                pending_reasoning = Some(entry.content.clone());
                skipped += 1;
            }
            "system" => {
                pending_reasoning = None;
                // Intent-rotation handoff notes (design 27 §4.1) are stored as
                // `system` rows but must survive as model context after a
                // cold restart — `ContextBuilder::build` strips every
                // `MessageRole::System` history message, so replay the tagged
                // handoff as a named User message instead. This mirrors the
                // compaction marker and the live `SystemNotification` path
                // (both delivered at User role to dodge providers that reject
                // non-first system messages). Genuine system rows (agent
                // identity / workspace context) keep their System role.
                let is_handoff = entry
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("rotation_handoff"))
                    .and_then(|v| v.as_bool())
                    == Some(true);
                messages.push(ChatMessage {
                    role: if is_handoff {
                        MessageRole::User
                    } else {
                        MessageRole::System
                    },
                    content: entry.content.clone(),
                    name: is_handoff.then(|| "intent_handoff".to_string()),
                    ..Default::default()
                });
                replayed += 1;
            }
            "user" => {
                pending_reasoning = None;
                messages.push(ChatMessage::user(entry.content.clone()));
                replayed += 1;
            }
            "assistant" => {
                // Only a tool-call turn carries its own `thought` back onto
                // the wire. Text-only turns leave it None (matches the
                // runtime path and avoids sending `reasoning_content` to
                // providers that do not accept it).
                let next_is_tool_call = working
                    .get(entry_idx + 1)
                    .map(|e| e.role == "tool_call")
                    .unwrap_or(false);
                let reasoning = if next_is_tool_call {
                    pending_reasoning.take()
                } else {
                    pending_reasoning = None;
                    None
                };
                messages.push(ChatMessage {
                    role: MessageRole::Assistant,
                    content: entry.content.clone(),
                    reasoning_content: reasoning,
                    ..Default::default()
                });
                replayed += 1;
            }
            "tool_call" => {
                let tool_call_id = entry
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("tool_call_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let tool_name = entry
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("tool_name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                if tool_call_id.is_empty() || tool_name.is_empty() {
                    tracing::warn!(
                        entry_id = %entry.id,
                        "Restore: tool_call missing tool_call_id or tool_name, dropping"
                    );
                    skipped += 1;
                    continue;
                }

                // ADR-060 v2 §5.4: remember every injected round's
                // tool_call_id so HistoryManager can restore its
                // `injected_round_call_ids` across process restarts.
                if entry.id.starts_with("inject-call-") {
                    injected_round_call_ids.insert(tool_call_id.clone());
                }

                let new_call = ToolCall {
                    id: tool_call_id,
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: tool_name,
                        arguments: entry.content.clone(),
                    },
                };

                // A `thought` directly before this tool_call means the model
                // started a NEW assistant turn (the JSONL writer does not
                // emit an `assistant` row when that turn's content is empty),
                // so we must NOT merge into an earlier assistant. Otherwise
                // merge into the immediately preceding assistant, or
                // synthesize an empty-content assistant when none precedes.
                let is_new_round = pending_reasoning.is_some();
                let merged = !is_new_round
                    && matches!(
                        messages.last(),
                        Some(m) if m.role == MessageRole::Assistant
                    );
                if merged {
                    let last = messages.last_mut().unwrap();
                    last.tool_calls.get_or_insert_with(Vec::new).push(new_call);
                } else {
                    messages.push(ChatMessage {
                        role: MessageRole::Assistant,
                        content: String::new(),
                        reasoning_content: pending_reasoning.take(),
                        tool_calls: Some(vec![new_call]),
                        ..Default::default()
                    });
                }
                replayed += 1;
            }
            "tool_result" => {
                let tool_call_id = entry
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("tool_call_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let tool_name = entry
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("tool_name"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                if tool_call_id.is_empty() {
                    tracing::warn!(
                        entry_id = %entry.id,
                        "Restore: tool_result missing tool_call_id, dropping"
                    );
                    skipped += 1;
                    continue;
                }

                messages.push(ChatMessage {
                    role: MessageRole::Tool,
                    content: entry.content.clone(),
                    tool_call_id: Some(tool_call_id),
                    name: tool_name,
                    ..Default::default()
                });
                replayed += 1;
            }
            other => {
                tracing::warn!(role = other, "Restore: unknown role, dropping");
                skipped += 1;
            }
        }
    }

    // Pass 5: sanitize tool pairing — drop any Tool message whose tool_call_id
    // doesn't match a known preceding Assistant.tool_calls[*].id.
    let dropped = drop_orphan_tool_results(&mut messages);
    if dropped > 0 {
        tracing::warn!(
            dropped,
            "Restore: dropped orphan tool_result messages with no matching tool_call"
        );
    }
    let skipped = skipped + dropped;

    Ok(RestoreOutcome {
        messages,
        had_compaction: last_compaction_idx.is_some(),
        replayed_entry_count: replayed,
        skipped_entry_count: skipped,
        injected_round_call_ids,
    })
}

/// Read every non-empty entry from `reader` at its current position into
/// `entries`. Corrupt lines are counted in `skipped` and skipped.
fn read_all_entries<R: std::io::BufRead>(
    reader: &mut R,
    entries: &mut Vec<ConversationEntry>,
    skipped: &mut usize,
) -> Result<(), RestoreError> {
    let mut line_idx = 0usize;
    loop {
        let mut line = String::new();
        let n = match reader.read_line(&mut line) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e, "Restore: I/O error reading line, skipping");
                *skipped += 1;
                continue;
            }
        };
        if n == 0 {
            break; // EOF
        }
        line_idx += 1;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<ConversationEntry>(&line) {
            Ok(entry) => entries.push(entry),
            Err(e) => {
                tracing::warn!(line_idx, error = %e, "Restore: malformed entry, skipping");
                *skipped += 1;
            }
        }
    }
    Ok(())
}

/// Drop any `Tool` message whose `tool_call_id` cannot be matched to a
/// preceding `Assistant.tool_calls[*].id` in the same sequence.
///
/// Returns the number of messages removed.
fn drop_orphan_tool_results(messages: &mut Vec<ChatMessage>) -> usize {
    let mut known_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut keep_flags: Vec<bool> = Vec::with_capacity(messages.len());

    for msg in messages.iter() {
        match msg.role {
            MessageRole::Assistant => {
                if let Some(ref calls) = msg.tool_calls {
                    for c in calls {
                        known_ids.insert(c.id.clone());
                    }
                }
                keep_flags.push(true);
            }
            MessageRole::Tool => {
                let keep = msg
                    .tool_call_id
                    .as_ref()
                    .map(|id| known_ids.contains(id))
                    .unwrap_or(false);
                keep_flags.push(keep);
            }
            _ => keep_flags.push(true),
        }
    }

    let mut removed = 0usize;
    let mut idx = 0usize;
    messages.retain(|_| {
        let keep = keep_flags[idx];
        idx += 1;
        if !keep {
            removed += 1;
        }
        keep
    });
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::{CompactionEventMeta, ConversationSession, SessionConfig};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    fn temp_workdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "acowork-restorer-{}-{}",
            tag,
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Deterministic durability barrier: ask the writer thread to flush
    /// and wait for its acknowledgement. The previous fixed 80 ms sleep
    /// flaked under full-workspace parallel load (the writer thread was
    /// starved and the JSONL lagged behind the assertions).
    fn flush(session: &ConversationSession) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        rt.block_on(session.flush_pending())
            .expect("flush pending");
    }

    #[test]
    fn restore_simple_user_assistant_roundtrip() {
        let work = temp_workdir("simple");
        let session_id = "sess-simple";
        let (session, _config_rx, _state_rx) = ConversationSession::new(
            &work,
            session_id,
            SessionConfig {
                agent_id: "test".into(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            Arc::new(AtomicUsize::new(0)), // unlimited in tests
        )
        .unwrap();
        session.append_message("user", "hi", None);
        session.append_message("assistant", "hello", None);
        flush(&session);

        let path = work
            .join("conversations")
            .join(format!("{}.jsonl", session_id));
        let outcome = restore_history_from_jsonl(&path, None).unwrap();
        assert!(!outcome.had_compaction);
        assert_eq!(outcome.messages.len(), 2);
        assert!(matches!(outcome.messages[0].role, MessageRole::User));
        assert_eq!(outcome.messages[0].content, "hi");
        assert!(matches!(outcome.messages[1].role, MessageRole::Assistant));
        assert_eq!(outcome.messages[1].content, "hello");
    }

    #[test]
    fn restore_drops_thought_lines() {
        let work = temp_workdir("thought");
        let session_id = "sess-thought";
        let (session, _config_rx, _state_rx) = ConversationSession::new(
            &work,
            session_id,
            SessionConfig {
                agent_id: "test".into(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            Arc::new(AtomicUsize::new(0)), // unlimited in tests
        )
        .unwrap();
        session.append_message("user", "q", None);
        session.append_message("thought", "internal monologue", None);
        session.append_message("assistant", "a", None);
        flush(&session);

        let path = work
            .join("conversations")
            .join(format!("{}.jsonl", session_id));
        let outcome = restore_history_from_jsonl(&path, None).unwrap();
        assert_eq!(
            outcome.messages.len(),
            2,
            "thought should not enter context"
        );
        assert!(
            outcome
                .messages
                .iter()
                .all(|m| !matches!(m.role, MessageRole::System))
        );
        assert!(outcome.skipped_entry_count >= 1);
    }

    #[test]
    fn restore_skips_internal_entries() {
        // The output-budget nudge is persisted to JSONL marked
        // `metadata.internal`. It must NOT be replayed as a genuine user turn:
        // an interrupted turn is never resumed, so a replayed nudge would leak
        // a stale internal instruction into subsequent turns.
        let work = temp_workdir("internal");
        let session_id = "sess-internal";
        let (session, _config_rx, _state_rx) = ConversationSession::new(
            &work,
            session_id,
            SessionConfig {
                agent_id: "test".into(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            Arc::new(AtomicUsize::new(0)), // unlimited in tests
        )
        .unwrap();
        session.append_message("user", "real question", None);
        session.append_internal_message("user", "output budget exhausted — wrap up");
        session.append_message("assistant", "real answer", None);
        flush(&session);

        let path = work
            .join("conversations")
            .join(format!("{}.jsonl", session_id));
        let outcome = restore_history_from_jsonl(&path, None).unwrap();
        assert_eq!(
            outcome.messages.len(),
            2,
            "internal entry must not enter the replay context"
        );
        assert_eq!(outcome.messages[0].content, "real question");
        assert_eq!(outcome.messages[1].content, "real answer");
        assert!(
            outcome
                .messages
                .iter()
                .all(|m| !m.content.contains("output budget exhausted")),
            "no message should carry the nudge text"
        );
        assert!(outcome.skipped_entry_count >= 1);
    }

    #[test]
    fn restore_folds_reasoning_onto_tool_call_rounds() {
        // DeepSeek thinking mode: a tool-call assistant turn must echo its
        // reasoning_content back on the wire (400 otherwise). JSONL stores
        // that reasoning as `thought` rows. This test locks in that
        // recovery restores them onto the assistant that carries tool_calls
        // — for both the "thought + assistant + tool_call" shape and the
        // "thought + tool_call only (empty assistant content)" shape —
        // while keeping text-only turns free of reasoning_content.
        let work = temp_workdir("rc-tool-round");
        let session_id = "sess-rc-tool";
        let (session, _config_rx, _state_rx) = ConversationSession::new(
            &work,
            session_id,
            SessionConfig {
                agent_id: "test".into(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            Arc::new(AtomicUsize::new(0)), // unlimited in tests
        )
        .unwrap();
        session.append_message("user", "q1", None);
        // Round 1: thought + assistant content + tool_call
        session.append_message("thought", "rc-one", None);
        session.append_message("assistant", "listing", None);
        session.append_message(
            "tool_call",
            r#"{"path":"."}"#,
            Some(serde_json::json!({"tool_name":"glob_search","tool_call_id":"tc_1"})),
        );
        session.append_message(
            "tool_result",
            "a.rs",
            Some(serde_json::json!({"tool_name":"glob_search","tool_call_id":"tc_1"})),
        );
        // Round 2: thought + tool_call only (empty assistant content) —
        // must start a NEW assistant turn, not merge into round 1.
        session.append_message("thought", "rc-two", None);
        session.append_message(
            "tool_call",
            r#"{"path":"./src"}"#,
            Some(serde_json::json!({"tool_name":"glob_search","tool_call_id":"tc_2"})),
        );
        session.append_message(
            "tool_result",
            "main.rs",
            Some(serde_json::json!({"tool_name":"glob_search","tool_call_id":"tc_2"})),
        );
        flush(&session);

        let path = work
            .join("conversations")
            .join(format!("{}.jsonl", session_id));
        let outcome = restore_history_from_jsonl(&path, None).unwrap();

        // User, Assistant{tc_1, rc-one}, Tool(tc_1),
        // Assistant{tc_2, rc-two}, Tool(tc_2)
        assert_eq!(outcome.messages.len(), 5);
        assert!(matches!(outcome.messages[0].role, MessageRole::User));
        assert!(matches!(outcome.messages[1].role, MessageRole::Assistant));
        assert_eq!(
            outcome.messages[1].reasoning_content.as_deref(),
            Some("rc-one")
        );
        let calls1 = outcome.messages[1].tool_calls.as_ref().unwrap();
        assert_eq!(calls1.len(), 1);
        assert_eq!(calls1[0].id, "tc_1");
        assert!(matches!(outcome.messages[2].role, MessageRole::Tool));
        assert!(matches!(outcome.messages[3].role, MessageRole::Assistant));
        assert_eq!(
            outcome.messages[3].reasoning_content.as_deref(),
            Some("rc-two"),
            "thought before an empty-content tool round must become that \
             round's reasoning_content (not merge into the previous round)"
        );
        let calls2 = outcome.messages[3].tool_calls.as_ref().unwrap();
        assert_eq!(calls2.len(), 1);
        assert_eq!(calls2[0].id, "tc_2");
        assert!(matches!(outcome.messages[4].role, MessageRole::Tool));
        assert_eq!(outcome.messages[4].tool_call_id.as_deref(), Some("tc_2"));
    }

    #[test]
    fn restore_merges_tool_calls_onto_assistant() {
        let work = temp_workdir("tools");
        let session_id = "sess-tools";
        let (session, _config_rx, _state_rx) = ConversationSession::new(
            &work,
            session_id,
            SessionConfig {
                agent_id: "test".into(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            Arc::new(AtomicUsize::new(0)), // unlimited in tests
        )
        .unwrap();
        session.append_message("user", "list files", None);
        // Assistant text content + 2 tool_calls follow
        session.append_message("assistant", "", None);
        session.append_message(
            "tool_call",
            r#"{"path":"."}"#,
            Some(serde_json::json!({"tool_name":"glob_search","tool_call_id":"tc_1"})),
        );
        session.append_message(
            "tool_call",
            r#"{"path":"./src"}"#,
            Some(serde_json::json!({"tool_name":"glob_search","tool_call_id":"tc_2"})),
        );
        session.append_message(
            "tool_result",
            "a.rs\nb.rs",
            Some(serde_json::json!({"tool_name":"glob_search","tool_call_id":"tc_1"})),
        );
        session.append_message(
            "tool_result",
            "main.rs",
            Some(serde_json::json!({"tool_name":"glob_search","tool_call_id":"tc_2"})),
        );
        flush(&session);

        let path = work
            .join("conversations")
            .join(format!("{}.jsonl", session_id));
        let outcome = restore_history_from_jsonl(&path, None).unwrap();

        // Expected: User, Assistant{tool_calls:[tc_1,tc_2]}, Tool(tc_1), Tool(tc_2)
        assert_eq!(outcome.messages.len(), 4);
        assert!(matches!(outcome.messages[0].role, MessageRole::User));
        assert!(matches!(outcome.messages[1].role, MessageRole::Assistant));
        let calls = outcome.messages[1].tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "tc_1");
        assert_eq!(calls[1].id, "tc_2");
        assert!(matches!(outcome.messages[2].role, MessageRole::Tool));
        assert_eq!(outcome.messages[2].tool_call_id.as_deref(), Some("tc_1"));
        assert!(matches!(outcome.messages[3].role, MessageRole::Tool));
        assert_eq!(outcome.messages[3].tool_call_id.as_deref(), Some("tc_2"));
    }

    #[test]
    fn restore_drops_orphan_tool_result() {
        let work = temp_workdir("orphan");
        let session_id = "sess-orphan";
        let (session, _config_rx, _state_rx) = ConversationSession::new(
            &work,
            session_id,
            SessionConfig {
                agent_id: "test".into(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            Arc::new(AtomicUsize::new(0)), // unlimited in tests
        )
        .unwrap();
        // tool_result with no preceding tool_call → orphan
        session.append_message("user", "q", None);
        session.append_message(
            "tool_result",
            "stale",
            Some(serde_json::json!({"tool_name":"x","tool_call_id":"missing"})),
        );
        session.append_message("assistant", "ok", None);
        flush(&session);

        let path = work
            .join("conversations")
            .join(format!("{}.jsonl", session_id));
        let outcome = restore_history_from_jsonl(&path, None).unwrap();
        // user + assistant, orphan tool_result dropped
        assert_eq!(outcome.messages.len(), 2);
        assert!(outcome.skipped_entry_count >= 1);
    }

    #[test]
    fn restore_anchors_at_last_compaction() {
        let work = temp_workdir("compact");
        let session_id = "sess-compact";
        let (session, _config_rx, _state_rx) = ConversationSession::new(
            &work,
            session_id,
            SessionConfig {
                agent_id: "test".into(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            Arc::new(AtomicUsize::new(0)), // unlimited in tests
        )
        .unwrap();
        // Pre-compaction noise
        session.append_message("user", "u1", None);
        session.append_message("assistant", "a1", None);
        session.append_message("user", "u2", None);
        session.append_message("assistant", "a2", None);
        // Compaction event covering the above
        session.append_compaction_event(
            "<summary>compacted u1..a2</summary>",
            CompactionEventMeta {
                compacted_from_id: String::new(),
                compacted_to_id: String::new(),
                level: 1,
                model: "test-model".into(),
                before_tokens: 1000,
                after_tokens: 100,
            },
        );
        // Post-compaction tail
        session.append_message("user", "u3", None);
        session.append_message("assistant", "a3", None);
        flush(&session);

        let path = work
            .join("conversations")
            .join(format!("{}.jsonl", session_id));
        let outcome = restore_history_from_jsonl(&path, None).unwrap();
        assert!(outcome.had_compaction);
        // Expected: [compaction_summary marker, u3, a3]
        assert_eq!(outcome.messages.len(), 3);
        // Compaction marker lives at `User` role to avoid producing
        // Assistant→Assistant adjacency in the request payload (see
        // module-level docs).  Consumers identify the marker by name.
        assert!(matches!(outcome.messages[0].role, MessageRole::User));
        assert_eq!(
            outcome.messages[0].name.as_deref(),
            Some("compaction_summary")
        );
        assert!(outcome.messages[0].content.contains("compacted u1..a2"));
        assert!(matches!(outcome.messages[1].role, MessageRole::User));
        assert_eq!(outcome.messages[1].content, "u3");
        assert!(matches!(outcome.messages[2].role, MessageRole::Assistant));
        assert_eq!(outcome.messages[2].content, "a3");
    }

    #[test]
    fn restore_skips_corrupt_lines() {
        let work = temp_workdir("corrupt");
        let session_id = "sess-corrupt";
        let (session, _config_rx, _state_rx) = ConversationSession::new(
            &work,
            session_id,
            SessionConfig {
                agent_id: "test".into(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            Arc::new(AtomicUsize::new(0)), // unlimited in tests
        )
        .unwrap();
        session.append_message("user", "ok1", None);
        flush(&session);
        // Inject a bogus line directly into the file
        let path = work
            .join("conversations")
            .join(format!("{}.jsonl", session_id));
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(f, "{{not valid json").unwrap();
        }
        session.append_message("user", "ok2", None);
        flush(&session);

        let outcome = restore_history_from_jsonl(&path, None).unwrap();
        assert_eq!(outcome.messages.len(), 2);
        assert!(outcome.skipped_entry_count >= 1);
    }

    /// Byte offset of the *start* of the last `kind="compaction"` line in a
    /// JSONL file. Each entry is one physical line (serde escapes embedded
    /// newlines), so we can accumulate byte offsets line by line.
    fn last_compaction_offset(path: &std::path::Path) -> u64 {
        let bytes = std::fs::read(path).unwrap();
        let mut cursor = 0u64;
        let mut last_start = None;
        for line in bytes.split(|&b| b == b'\n') {
            let content = String::from_utf8_lossy(line);
            if content.contains("\"kind\":\"compaction\"") {
                last_start = Some(cursor);
            }
            cursor += (line.len() + 1) as u64;
        }
        last_start.expect("expected at least one compaction entry")
    }

    #[test]
    fn restore_fast_path_honors_last_compaction_offset() {
        // Regression: the fast path (Some(offset)) used the persisted
        // offset only as a boolean and anchored on the *first* compaction
        // entry in the file. For a session compacted more than once this
        // resurrected every raw round between two compactions (see incident
        // 2026-09-06 — a cold-started session instantly reached 70% context
        // and re-compacted). The restored history must be exactly
        // [leading system…, last compaction marker, post-marker tail].
        let work = temp_workdir("compact-fastpath");
        let session_id = "sess-compact-fastpath";
        let (session, _config_rx, _state_rx) = ConversationSession::new(
            &work,
            session_id,
            SessionConfig {
                agent_id: "test".into(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            Arc::new(AtomicUsize::new(0)), // unlimited in tests
        )
        .unwrap();
        // Leading system context (must survive restore).
        session.append_message("system", "identity", None);
        // Raw rounds before the FIRST compaction (must NOT survive).
        session.append_message("user", "u1", None);
        session.append_message("assistant", "a1", None);
        session.append_compaction_event(
            "<summary>first compaction</summary>",
            CompactionEventMeta {
                compacted_from_id: String::new(),
                compacted_to_id: String::new(),
                level: 1,
                model: "test-model".into(),
                before_tokens: 1000,
                after_tokens: 100,
            },
        );
        // Raw rounds BETWEEN compactions (must NOT survive: they were
        // already folded into the second summary).
        session.append_message("user", "stale-between", None);
        session.append_message("assistant", "stale-between-a", None);
        session.append_compaction_event(
            "<summary>second compaction</summary>",
            CompactionEventMeta {
                compacted_from_id: String::new(),
                compacted_to_id: String::new(),
                level: 1,
                model: "test-model".into(),
                before_tokens: 500,
                after_tokens: 80,
            },
        );
        // Post-compaction tail (must survive).
        session.append_message("user", "u3", None);
        session.append_message("assistant", "a3", None);
        flush(&session);

        let path = work
            .join("conversations")
            .join(format!("{}.jsonl", session_id));
        let abs = last_compaction_offset(&path);
        let outcome = restore_history_from_jsonl(&path, Some(abs)).unwrap();

        assert!(outcome.had_compaction);
        // [system, second-compaction marker, u3, a3]
        assert_eq!(outcome.messages.len(), 4);
        assert!(matches!(outcome.messages[0].role, MessageRole::System));
        assert_eq!(outcome.messages[0].content, "identity");
        assert!(matches!(outcome.messages[1].role, MessageRole::User));
        assert_eq!(
            outcome.messages[1].name.as_deref(),
            Some("compaction_summary")
        );
        assert!(outcome.messages[1].content.contains("second compaction"));
        assert_eq!(outcome.messages[2].content, "u3");
        assert_eq!(outcome.messages[3].content, "a3");
        // The stale rounds between the two compactions must be gone.
        assert!(
            !outcome
                .messages
                .iter()
                .any(|m| m.content.contains("stale-between"))
        );
        assert!(
            !outcome
                .messages
                .iter()
                .any(|m| m.content.contains("first compaction"))
        );
    }

    #[test]
    fn restore_fast_path_falls_back_when_offset_stale() {
        // A stale/corrupt offset must never cause silent data loss: fall
        // back to the legacy full scan (anchored at the last marker).
        let work = temp_workdir("compact-stale");
        let session_id = "sess-compact-stale";
        let (session, _config_rx, _state_rx) = ConversationSession::new(
            &work,
            session_id,
            SessionConfig {
                agent_id: "test".into(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            Arc::new(AtomicUsize::new(0)), // unlimited in tests
        )
        .unwrap();
        session.append_message("user", "pre", None);
        session.append_compaction_event(
            "<summary>only</summary>",
            CompactionEventMeta {
                compacted_from_id: String::new(),
                compacted_to_id: String::new(),
                level: 1,
                model: "test-model".into(),
                before_tokens: 100,
                after_tokens: 50,
            },
        );
        session.append_message("user", "post", None);
        flush(&session);

        let path = work
            .join("conversations")
            .join(format!("{}.jsonl", session_id));
        // Offset far beyond EOF — stale hint.
        let huge = std::fs::metadata(&path).unwrap().len() + 4096;
        let outcome = restore_history_from_jsonl(&path, Some(huge)).unwrap();
        assert!(outcome.had_compaction);
        // Legacy semantics: [marker, post]; pre-compaction "pre" is dropped.
        assert_eq!(outcome.messages.len(), 2);
        assert_eq!(
            outcome.messages[0].name.as_deref(),
            Some("compaction_summary")
        );
        assert_eq!(outcome.messages[1].content, "post");
    }

    #[test]
    fn restore_recovers_all_injected_round_call_ids() {
        // ADR-060 v2 §5.4 cross-restart idempotency: a synthetic round
        // persisted by the compact-and-inject path (entry ids prefixed
        // `inject-call-` / `inject-result-`) must surface its tool_call_id so
        // HistoryManager can restore `injected_round_call_ids`. Without
        // it, the next compression after restart would re-inject the same
        // round and accumulate duplicate rows.
        //
        // Two injected rounds now (todo_write + load_skill): skill recall
        // persists one round per distinct skill, so the restore path must
        // collect *all* of them, not just the most recent — the former
        // single-slot field could only remember one.
        let work = temp_workdir("inject-id");
        let session_id = "sess-inject-id";
        let (session, _config_rx, _state_rx) = ConversationSession::new(
            &work,
            session_id,
            SessionConfig {
                agent_id: "test".into(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            Arc::new(AtomicUsize::new(0)), // unlimited in tests
        )
        .unwrap();

        session.append_message("user", "hi", None);
        session.append_compaction_event(
            "<summary>compacted</summary>",
            CompactionEventMeta {
                compacted_from_id: String::new(),
                compacted_to_id: String::new(),
                level: 5,
                model: "test-model".into(),
                before_tokens: 1000,
                after_tokens: 120,
            },
        );

        // Emulate the compact-and-inject persistence: a synthetic tool_call
        // row + its tool_result, with ids carrying the `inject-` prefix
        // (mirrors loop_context.rs append_message_with_id("tool_call"|"tool_result")).
        let call_meta = serde_json::json!({
            "tool_name": "todo_write",
            "tool_call_id": "toolu_inj_42",
        });
        session.append_message_with_id(
            "tool_call",
            r#"{"todos":[{"id":"t1","content":"Survive restart"}]}"#,
            Some(call_meta),
            Some("inject-call-11111111-1111-1111-1111-111111111111".to_string()),
        );
        let result_meta = serde_json::json!({
            "tool_call_id": "toolu_inj_42",
            "tool_name": "todo_write",
        });
        session.append_message_with_id(
            "tool_result",
            "- [ ] Survive restart",
            Some(result_meta),
            Some("inject-result-22222222-2222-2222-2222-222222222222".to_string()),
        );

        // A second injected round, this one a preserved load_skill body —
        // the case the old single-slot field silently forgot.
        let sk_call_meta = serde_json::json!({
            "tool_name": "load_skill",
            "tool_call_id": "toolu_inj_sk9",
        });
        session.append_message_with_id(
            "tool_call",
            r#"{"name":"debugging"}"#,
            Some(sk_call_meta),
            Some("inject-call-55555555-5555-5555-5555-555555555555".to_string()),
        );
        let sk_result_meta = serde_json::json!({
            "tool_call_id": "toolu_inj_sk9",
            "tool_name": "load_skill",
        });
        session.append_message_with_id(
            "tool_result",
            r#"{"name":"debugging","instructions":"1. reproduce 2. isolate"}"#,
            Some(sk_result_meta),
            Some("inject-result-66666666-6666-6666-6666-666666666666".to_string()),
        );

        // A later, genuine todo_write round must NOT be mistaken for an
        // injected one.
        let real_call_meta = serde_json::json!({
            "tool_name": "todo_write",
            "tool_call_id": "toolu_real_1",
        });
        session.append_message_with_id(
            "tool_call",
            r#"{"todos":[{"id":"t2","content":"Real user write"}]}"#,
            Some(real_call_meta),
            Some("33e23ab4-0000-0000-0000-000000000000".to_string()),
        );
        let real_result_meta = serde_json::json!({
            "tool_call_id": "toolu_real_1",
            "tool_name": "todo_write",
        });
        session.append_message_with_id(
            "tool_result",
            "- [ ] Real user write",
            Some(real_result_meta),
            Some("44e23ab4-0000-0000-0000-000000000000".to_string()),
        );
        flush(&session);

        let path = work
            .join("conversations")
            .join(format!("{}.jsonl", session_id));
        let outcome = restore_history_from_jsonl(&path, None).unwrap();

        // BOTH synthetic rounds are collected; the later genuine
        // `toolu_real_1` round must not be mistaken for an injected one.
        let expected: std::collections::HashSet<String> =
            ["toolu_inj_42".to_string(), "toolu_inj_sk9".to_string()]
                .into_iter()
                .collect();
        assert_eq!(
            outcome.injected_round_call_ids, expected,
            "restorer must surface every injected round's tool_call_id, ignoring genuine rounds"
        );
        assert!(
            !outcome.injected_round_call_ids.contains("toolu_real_1"),
            "a genuine round must never enter the idempotency set — it would \
             suppress its own injection after the next compression"
        );
        // Sanity: both todo rounds are present in the restored messages.
        let tool_ids: Vec<_> = outcome
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .filter_map(|m| m.tool_call_id.clone())
            .collect();
        assert!(
            tool_ids.contains(&"toolu_inj_42".to_string())
                && tool_ids.contains(&"toolu_real_1".to_string()),
            "both injected and genuine todo rounds must be restored: {tool_ids:?}"
        );
    }

    #[test]
    fn restore_replays_rotation_handoff_as_user_message() {
        // Design 27 §4.1: an intent-rotation handoff is stored as a `system`
        // JSONL row, but `ContextBuilder::build` strips System history — so the
        // restorer must replay the tagged row as a named User message (so it
        // still reaches the model after a cold restart), while a genuine system
        // row keeps its System role.
        let work = temp_workdir("handoff-replay");
        let session_id = "sess-handoff-replay";
        let (session, _config_rx, _state_rx) = ConversationSession::new(
            &work,
            session_id,
            SessionConfig {
                agent_id: "test".into(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            Arc::new(AtomicUsize::new(0)),
        )
        .unwrap();
        // Genuine leading system context (identity) — must stay System.
        session.append_message("system", "identity context", None);
        // Rotation handoff row (tagged via metadata, deterministic id).
        session.append_message_with_id(
            "system",
            "[intent rotation] previous session s-old rotated",
            Some(serde_json::json!({ "rotation_handoff": true })),
            Some("handoff:msg-1".to_string()),
        );
        session.append_message("user", "real question", None);
        flush(&session);

        let path = work
            .join("conversations")
            .join(format!("{}.jsonl", session_id));
        let outcome = restore_history_from_jsonl(&path, None).unwrap();

        assert_eq!(outcome.messages.len(), 3);
        assert!(matches!(outcome.messages[0].role, MessageRole::System));
        assert_eq!(outcome.messages[0].content, "identity context");
        // The handoff replays at User role with the discriminator name.
        assert!(matches!(outcome.messages[1].role, MessageRole::User));
        assert_eq!(outcome.messages[1].name.as_deref(), Some("intent_handoff"));
        assert!(outcome.messages[1].content.contains("[intent rotation]"));
        // The genuine user message is untouched (no injected name).
        assert!(matches!(outcome.messages[2].role, MessageRole::User));
        assert_eq!(outcome.messages[2].content, "real question");
        assert_eq!(outcome.messages[2].name, None);
    }
}
