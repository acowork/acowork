//! RuntimeSessionMetadataService — implements SessionMetadataService.
//!
//! ADR-024 / ADR-028 / ADR-040: wraps session scanning, message reading,
//! and agent-level token merging into a single-use case implementation.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::agent::session_state::{SharedLatestSession, SharedSessionSnapshots};
use crate::conversation::{self, SessionScope};
use crate::error::{Result, RuntimeError};
use crate::usecases::agent_token::AgentTokenService;
use crate::usecases::session_metadata::{
    ChatListingScope, ConversationEntryView, MessagesResponse, SessionDetail,
    SessionMetadataService, SessionSummary, SessionsListResponse, TokenStatsResponse,
};

/// The `Active` set and each resident session's stored status, from ONE read.
///
/// `Active` is "present in the in-memory map" (ADR-038 lifecycle;
/// `close_session` removes the key); the status is that same snapshot's
/// `status` field, still in its stored JSON form. Both come from the same
/// acquisition on purpose: a row that says `is_active: true` but carries a
/// status from a different instant would let the client believe a session is
/// still working after it parked, which is exactly the fact this listing
/// exists to report.
///
/// A poisoned per-session guard yields an empty status string — "no readable
/// status", not "not active": the key is the lifecycle fact and the parse step
/// turns the empty string into `None`.
fn resident_snapshots(
    snapshots: &SharedSessionSnapshots,
) -> (
    std::collections::HashSet<String>,
    std::collections::HashMap<String, String>,
) {
    let mut active = std::collections::HashSet::new();
    let mut statuses = std::collections::HashMap::new();
    if let Ok(map) = snapshots.read() {
        for (sid, snap) in map.iter() {
            active.insert(sid.clone());
            let json = snap
                .read()
                .map(|guard| guard.status.clone())
                .unwrap_or_default();
            statuses.insert(sid.clone(), json);
        }
    }
    (active, statuses)
}

/// Parse one snapshot's stored status JSON into the wire DTO.
///
/// The stored string is the serde JSON of the runtime `SessionStatus`
/// (`{"status":"waiting_approval","detail":{...}}`), which the retained
/// `sessions/{sid}/state` topic ships verbatim and both frontends already
/// decode. It is parsed into the DTO rather than passed through so this API
/// stays typed, and a variant the DTO does not model (`errored`) degrades to
/// `None`: an error needs no click, and inventing a status the client cannot
/// name would flip a badge from a guess.
fn parse_live_status(json: &str) -> Option<acowork_core::SessionStatusDto> {
    if json.is_empty() {
        return None;
    }
    let mut value = serde_json::from_str::<serde_json::Value>(json).ok()?;
    // One compatibility shim, and it is the reader's job because the reader
    // outlives the writer: a snapshot or a retained `sessions/{sid}/state`
    // message written before `tool_executing` grew its `tools` list carries
    // the bare tag `{"status":"tool_executing"}`, and adjacent tagging needs
    // the `detail` key to exist for a struct variant. Repairing it is what
    // keeps a session mid-tool-call across a binary swap reading as an
    // unnamed dispatch instead of as no status at all.
    if value.get("status").and_then(|s| s.as_str()) == Some("tool_executing")
        && value.get("detail").is_none()
    {
        value.as_object_mut()?.insert(
            "detail".to_string(),
            serde_json::Value::Object(serde_json::Map::new()),
        );
    }
    serde_json::from_value(value).ok()
}

pub struct RuntimeSessionMetadataService {
    work_dir: PathBuf,
    agent_token: Arc<dyn AgentTokenService>,
    /// Chat Router token ledger — its totals join the disk scan in the
    /// ADR-066 merge so router spend survives a process restart in the
    /// agent counters exactly like session spend.
    chat_usage: Arc<dyn crate::usecases::ChatUsageService>,
    session_snapshots: SharedSessionSnapshots,
    latest_session: SharedLatestSession,
}

impl RuntimeSessionMetadataService {
    pub fn new(
        work_dir: PathBuf,
        agent_token: Arc<dyn AgentTokenService>,
        chat_usage: Arc<dyn crate::usecases::ChatUsageService>,
        session_snapshots: SharedSessionSnapshots,
        latest_session: SharedLatestSession,
    ) -> Self {
        Self {
            work_dir,
            agent_token,
            chat_usage,
            session_snapshots,
            latest_session,
        }
    }
}

#[async_trait]
impl SessionMetadataService for RuntimeSessionMetadataService {
    async fn list_sessions(
        &self,
        page: u32,
        size: u32,
        scope: &SessionScope,
        active_only: bool,
        chat: Option<&ChatListingScope>,
    ) -> Result<SessionsListResponse> {
        // Read the snapshot keys once, before the scan: Active = present in
        // the in-memory map (ADR-038 lifecycle; close_session removes the
        // key). Two uses, one read — the `active=true` pre-pagination filter
        // and the per-row `is_active` flag must see the same snapshot.
        //
        // The same read also carries each resident session's live status, so
        // the rows below can report `waiting_approval` / `paused` without a
        // second lock acquisition per row (the status is a field of the very
        // snapshot whose presence defines `is_active`).
        let (active_ids, live_status) = resident_snapshots(&self.session_snapshots);

        let conversations = self.work_dir.join("conversations");
        // ADR-106 D7: the Chat-scoped read rule, handed to the scan as a row
        // predicate so the page slice and `total_*` describe only what this chat
        // may address. Its terminal exclusions come off the intent-binding
        // index — the same small second metadata read the router's candidate
        // pass makes, read here rather than passed in so no caller has to know
        // that finished PM chains are part of the rule. The rule itself lives in
        // `chat_candidates`, which is the only place it may be written.
        let meta_filter = chat.map(|chat| {
            use crate::agent::session::chat_candidates::{ChatScopeRule, scan_bindings};
            let terminal_session_ids = scan_bindings(&self.work_dir.to_string_lossy())
                .into_iter()
                .filter(|binding| binding.closed)
                .map(|binding| binding.session_id)
                .collect();
            let rule = ChatScopeRule {
                chat_session_id: chat.chat_session_id.clone(),
                owner_user_id: chat.owner_user_id.clone(),
                surface: chat.surface,
                terminal_session_ids,
            };
            let keep: conversation::MetaFilter =
                Box::new(move |meta: &conversation::SessionMeta| rule.allows(meta));
            keep
        });
        let join = conversation::scan_sessions_async_filtered(
            conversations.clone(),
            Some(page),
            Some(size),
            scope.clone(),
            // An empty set is meaningful ("nothing is resident"), so it is
            // passed through as `Some(empty)` and yields an empty page —
            // only `active_only == false` turns the filter off.
            if active_only {
                Some(active_ids.clone())
            } else {
                None
            },
            meta_filter,
        );
        // ADR-066: scan_sessions_async returns a 4-tuple
        // (input, output, cache_read, cache_write) — feed all four into
        // the AgentTokenService merge so the in-process counters
        // recover the cache totals on first scan (e.g. after a
        // process restart, matching the in/out recovery path).
        let (sessions, total_count, (disk_in, disk_out, disk_cache_read, disk_cache_write)) = join
            .await
            .map_err(|e| RuntimeError::Io(std::io::Error::other(e)))?;

        // ADR-028 / ADR-066: merge disk totals into live counters so the
        // in-memory AtomicU64 counters pick up historical totals
        // (cold-start recovery, before the first live push). The values
        // are intentionally NOT echoed into the HTTP response —
        // agent totals belong on the live `context_usage` push path
        // only (regression t-83afab47).
        //
        // The Chat ledger rides the same scan: router spend is persisted
        // in `chat_token_usage`, not in the session metas the join reads,
        // so without this term a restart would drop every chat token the
        // pre-restart counters held (the live counter starts at zero;
        // `merge_token_totals` can only recover what the scan sees).
        let (chat_in, chat_out, chat_cr, chat_cw) = self.chat_usage.ledger_totals();
        self.agent_token.merge_token_totals((
            Some(disk_in.saturating_add(chat_in)),
            Some(disk_out.saturating_add(chat_out)),
            Some(disk_cache_read.saturating_add(chat_cr)),
            Some(disk_cache_write.saturating_add(chat_cw)),
        ));

        let page_sessions: Vec<SessionSummary> = sessions
            .into_iter()
            .map(|s| SessionSummary {
                is_active: active_ids.contains(&s.session_id),
                status: live_status
                    .get(&s.session_id)
                    .and_then(|json| parse_live_status(json)),
                session_id: s.session_id.clone(),
                title: s.title,
                created_at: s.created_at,
                last_active_at: s.last_active_at,
                message_count: s.message_count,
                workspace_id: s.workspace_id,
                model: s.model,
                provider: s.provider,
                visibility: s.visibility,
                initiator: s.initiator,
                can_write: s.can_write,
                authority: s.authority,
                config_fields: s.config_fields,
                // Design 27 §8.2 / ADR-100 D12: the Chat binding is not part
                // of the scan projection, so it is a point read per row on
                // this page only (the scan itself already touched every
                // session). A row whose meta cannot be read simply reports no
                // binding — the client then keeps the direct composer path.
                chat_session_id: conversation::read_session_meta(&conversations, &s.session_id)
                    .ok()
                    .and_then(|m| m.chat_session_id),
            })
            .collect();

        let total_pages = if total_count == 0 {
            0
        } else {
            (total_count as u32).div_ceil(size)
        };

        Ok(SessionsListResponse {
            sessions: page_sessions,
            total_count,
            total_pages,
            page,
            size,
        })
    }

    async fn get_latest_session(&self) -> Result<Option<(String, Option<String>)>> {
        self.latest_session
            .read()
            .map(|g| g.clone())
            .map_err(|_| RuntimeError::Io(std::io::Error::other("latest_session lock poisoned")))
    }

    async fn get_session(&self, session_id: &str) -> Result<SessionDetail> {
        let meta =
            conversation::read_session_meta(&self.work_dir.join("conversations"), session_id)?;

        // Live state snapshot from SessionManager's shared snapshots.
        // Construct the full JSON object the desktop panel expects
        // (status, model, provider, ratio, todos, context_usage).
        let live_state = {
            let snaps = self
                .session_snapshots
                .read()
                .map_err(|_| RuntimeError::Io(std::io::Error::other("lock poisoned")))?;
            match snaps.get(session_id) {
                Some(snap) => match snap.read() {
                    Ok(guard) => {
                        let status: serde_json::Value =
                            serde_json::from_str(&guard.status).unwrap_or(serde_json::Value::Null);
                        let todos: Option<serde_json::Value> = guard
                            .todos_json
                            .as_deref()
                            .and_then(|s| serde_json::from_str(s).ok());
                        let context_usage: Option<serde_json::Value> = guard
                            .context_usage
                            .as_deref()
                            .and_then(|s| serde_json::from_str(s).ok());
                        Some(serde_json::json!({
                            "status": status,
                            "model": guard.model,
                            "provider": guard.provider,
                            "ratio": guard.ratio,
                            "todos": todos,
                            "context_usage": context_usage,
                        }))
                    }
                    Err(_) => None,
                },
                None => None,
            }
        };

        // ADR-047: SessionDetail no longer carries config fields
        // (model, provider, workspace_id, title). Config is served
        // separately by `GET /sessions/{sid}/config` via
        // `SessionConfigService`, which reads from the in-memory
        // `Arc<ConversationSession>` for authoritative values.
        Ok(SessionDetail {
            session_id: meta.session_id,
            created_at: meta.created_at,
            last_active_at: meta.last_active_at,
            message_count: meta.message_count as u32,
            live_state,
        })
    }

    async fn get_messages(
        &self,
        session_id: &str,
        offset: Option<u64>,
        limit: Option<u32>,
        from_tail: bool,
    ) -> Result<MessagesResponse> {
        let file_path = self
            .work_dir
            .join("conversations")
            .join(format!("{}.jsonl", session_id));

        let off = offset.unwrap_or(0);
        let lim = limit.unwrap_or(50).clamp(1, 500);

        let paginated = conversation::read_messages_paginated(&file_path, off, lim, from_tail)?;

        // ADR-035 D9.2: truncate tool_result content to first 5 lines
        // for display in ALL HTTP paths. Full content stays in JSONL
        // for LLM context.
        let messages: Vec<serde_json::Value> = paginated
            .messages
            .into_iter()
            .map(|mut entry| {
                if entry.role == "tool_result" {
                    entry.content = truncate_tool_result(&entry.content);
                }
                // The wire shape is the stored row plus its human projection; the
                // panel never resolves `metadata.intent` itself.
                serde_json::to_value(ConversationEntryView::new(&entry))
                    .unwrap_or(serde_json::Value::Null)
            })
            .collect();
        let count = messages.len();

        Ok(MessagesResponse {
            session_id: session_id.to_string(),
            messages,
            offset: paginated.offset,
            limit: paginated.limit,
            total: paginated.total,
            count,
        })
    }

    async fn token_stats(&self) -> Result<TokenStatsResponse> {
        let store = conversation::session_meta_store(&self.work_dir.join("conversations"))
            .map_err(RuntimeError::Io)?;
        let items = store.token_stats().map_err(RuntimeError::Core)?;
        // Drift is surfaced as a flag, never as a failed read — the panel
        // shows the numbers with a caveat and a rebuild stays the repair
        // path (disposable derived index, ADR-082 pattern).
        let consistent = store.token_stats_consistent().unwrap_or(false);
        Ok(TokenStatsResponse { items, consistent })
    }
}

/// Truncate tool_result content to the first 5 lines for HTTP display.
///
/// ADR-035 D9.2: full content stays in JSONL for LLM context; HTTP
/// responses only carry the truncated preview.
///
/// Single canonical home for this rule (ADR-040): the HTTP layer calls
/// this through [`crate::usecases::SessionMetadataService::get_messages`],
/// and `cli.rs::truncate_tool_result_for_display` is a `pub(crate)`
/// re-export so any CLI preview path stays consistent without
/// duplicating the lines-count constant.
pub(crate) fn truncate_tool_result(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= 5 {
        return content.to_string();
    }
    let mut truncated = lines.into_iter().take(5).collect::<Vec<_>>().join("\n");
    truncated.push_str("\n...(truncated)");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::session_state::SessionRuntimeSnapshot;
    use acowork_core::protocol::{PauseReasonDto, SessionStatusDto};

    /// A resident session snapshot carrying `status_json` verbatim, exactly as
    /// `emit_session_state` stores it.
    fn resident(
        session_id: &str,
        status_json: &str,
    ) -> Arc<std::sync::RwLock<SessionRuntimeSnapshot>> {
        Arc::new(std::sync::RwLock::new(SessionRuntimeSnapshot {
            session_id: session_id.to_string(),
            status: status_json.to_string(),
            model: None,
            provider: None,
            account_id: None,
            ratio: None,
            todos_json: None,
            context_usage: None,
        }))
    }

    fn snapshots(
        entries: Vec<Arc<std::sync::RwLock<SessionRuntimeSnapshot>>>,
    ) -> SharedSessionSnapshots {
        let mut map = std::collections::HashMap::new();
        for snap in entries {
            let sid = snap.read().unwrap().session_id.clone();
            map.insert(sid, snap);
        }
        Arc::new(std::sync::RwLock::new(map))
    }

    const WAITING: &str = r#"{"status":"waiting_approval","detail":{"request_id":"req-1"}}"#;
    const ITERATION_LIMIT: &str =
        r#"{"status":"paused","detail":{"iteration":12,"max_iterations":12,"reason":"iteration_limit"}}"#;

    #[test]
    fn one_read_yields_both_the_active_set_and_the_stored_status() {
        // The listing's two per-row facts (`is_active`, `status`) come from the
        // same acquisition, so a row can never pair a live key with a status
        // from another instant.
        let snaps = snapshots(vec![resident("s-1", WAITING)]);
        let (active, live_status) = resident_snapshots(&snaps);
        assert!(active.contains("s-1"));
        assert_eq!(live_status.get("s-1").map(String::as_str), Some(WAITING));
    }

    #[test]
    fn a_parked_session_reports_the_request_id_its_card_answers() {
        // This is the fact the sidebar badge and the Desktop's ADR-014 pull
        // repair read: a resident session blocked on a human.
        assert_eq!(
            parse_live_status(WAITING),
            Some(SessionStatusDto::WaitingApproval {
                request_id: "req-1".to_string()
            })
        );
    }

    /// A snapshot written before `tool_executing` grew its `tools` list is the
    /// bare tag with no `detail` object. The reader must still report a
    /// dispatch — unnamed, which every surface falls back on — rather than
    /// "no status", because a session parked mid-tool-call across a binary
    /// swap is exactly the state a listing is read to show.
    #[test]
    fn a_pre_tools_tool_executing_snapshot_still_reads_as_a_dispatch() {
        match parse_live_status(r#"{"status":"tool_executing"}"#) {
            Some(SessionStatusDto::ToolExecuting { tools }) => assert!(tools.is_empty()),
            other => panic!("expected an unnamed dispatch, got {other:?}"),
        }
    }

    /// The shape the Runtime writes now: names present, and read back as
    /// names, not dropped on the way through the stored string.
    #[test]
    fn a_tool_executing_snapshot_keeps_the_names_it_wrote() {
        let json = r#"{"status":"tool_executing","detail":{"tools":["memory_recall"]}}"#;
        assert_eq!(
            parse_live_status(json),
            Some(SessionStatusDto::ToolExecuting {
                tools: vec!["memory_recall".to_string()]
            })
        );
    }

    #[test]
    fn an_iteration_limit_pause_keeps_its_reason_through_the_parse() {
        // "paused" alone is ambiguous: with `iteration_limit` (or
        // `loop_detected` / `debug`) a human must decide, with `retry_info`
        // (429) the Runtime resumes on its own. The reason is what tells the
        // surfaces which of the two they are looking at.
        match parse_live_status(ITERATION_LIMIT) {
            Some(SessionStatusDto::Paused { reason, .. }) => {
                assert_eq!(reason, Some(PauseReasonDto::IterationLimit));
            }
            other => panic!("expected an iteration-limit pause, got {other:?}"),
        }
    }

    #[test]
    fn a_status_the_wire_cannot_model_degrades_to_none() {
        // `errored` has no DTO arm. Reporting no status is honest (the pushed
        // retained value stays in place); inventing one would flip a badge from
        // a guess.
        assert_eq!(
            parse_live_status(r#"{"status":"errored","detail":{"reason":"x","message":"y","recoverable":false}}"#),
            None
        );
    }

    #[test]
    fn a_session_without_a_readable_status_reports_none() {
        // Empty = never resident / poisoned guard. Not an error, not a status.
        assert_eq!(parse_live_status(""), None);
    }
}
