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
    MessagesResponse, SessionDetail, SessionMetadataService, SessionSummary, SessionsListResponse,
    TokenStatsResponse,
};

pub struct RuntimeSessionMetadataService {
    work_dir: PathBuf,
    agent_token: Arc<dyn AgentTokenService>,
    session_snapshots: SharedSessionSnapshots,
    latest_session: SharedLatestSession,
}

impl RuntimeSessionMetadataService {
    pub fn new(
        work_dir: PathBuf,
        agent_token: Arc<dyn AgentTokenService>,
        session_snapshots: SharedSessionSnapshots,
        latest_session: SharedLatestSession,
    ) -> Self {
        Self {
            work_dir,
            agent_token,
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
    ) -> Result<SessionsListResponse> {
        let conversations = self.work_dir.join("conversations");
        let join = conversation::scan_sessions_async(
            conversations.clone(),
            Some(page),
            Some(size),
            scope.clone(),
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
        self.agent_token.merge_token_totals((
            Some(disk_in),
            Some(disk_out),
            Some(disk_cache_read),
            Some(disk_cache_write),
        ));

        let page_sessions: Vec<SessionSummary> = sessions
            .into_iter()
            .map(|s| SessionSummary {
                session_id: s.session_id.clone(),
                title: s.title,
                created_at: s.created_at,
                last_active_at: s.last_active_at,
                message_count: s.message_count,
                workspace_id: s.workspace_id,
                model: s.model,
                provider: s.provider,
                visibility: s.visibility,
                can_write: s.can_write,
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
                serde_json::to_value(&entry).unwrap_or(serde_json::Value::Null)
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
