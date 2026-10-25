//! Session metadata query use case.
//!
//! ADR-040: this trait is the single source of truth for session-listing
//! and message-reading operations. Both the HTTP server and any future
//! transport adapter call through this trait.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::conversation::{SessionScope, SessionVisibility};
use crate::error::{Result, RuntimeError};

/// Summary of a single session for list responses (ADR-024 / ADR-028).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub title: Option<String>,
    pub created_at: String,
    pub last_active_at: String,
    pub message_count: u32,
    pub workspace_id: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    /// ADR-076 §决策 4: `None` = public (the default).
    pub visibility: Option<SessionVisibility>,
    /// ADR-076 §决策 4: may the caller modify this session?
    ///
    /// Not an ownership flag: admins and `local`-mode callers may write
    /// sessions they do not own (see `SessionInfo::can_write`).
    pub can_write: bool,
    /// Design 27 §8.2 / ADR-100 D12: the Chat session this execution
    /// session was started from. An execution view's composer sends through
    /// that Chat (`for_execution_session`) so the turn becomes a real Chat
    /// message instead of a result with no trigger; `None` = a session that
    /// never came from a Chat, which keeps the direct composer path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_session_id: Option<String>,
}

/// Response for `list_sessions` — paginated session list.
///
/// ADR-028 §P3 / ADR-066 §4: agent-level cumulative token totals are
/// NO LONGER included in this HTTP response. They live exclusively on
/// the live `context_usage` push path (every MQTT `session_state` and
/// `ChunkEvent::ContextUsage` carries them via `patch_agent_totals`).
/// Following the "HTTP 拉完整数据, MQTT 推变化数据" rule, agent
/// totals are session-listening-time independent of session-list
/// fetches — the desktop must not maintain a stale `agentTokenTotals`
/// fallback that drift-flickers against the live push (regression
/// t-83afab47).
///
/// Cold-start recovery still happens here: `list_sessions_impl` calls
/// `AgentTokenService::merge_token_totals` with the disk-scan result
/// so the in-memory AtomicU64 counters pick up historical totals
/// before the first live push. That side effect is invisible on the
/// wire; only the merge happens, the value is not echoed back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionsListResponse {
    pub sessions: Vec<SessionSummary>,
    pub total_count: usize,
    pub total_pages: u32,
    pub page: u32,
    pub size: u32,
}

/// Detail view of a single session (panel-4 endpoint).
///
/// ADR-047: config fields (model, provider, workspace_id,
/// reasoning_effort, temperature, title) have been moved to
/// `GET /sessions/{sid}/config` (served by `SessionConfigService`).
/// This struct now carries only state + metadata fields.
///
/// `live_state` is an optional JSON object carrying the runtime
/// snapshot fields (status, model, provider, ratio, todos,
/// context_usage). The impl constructs this from
/// `SharedSessionSnapshots`; `None` means no live snapshot exists.
///
/// Note: `model` and `provider` in `live_state` are runtime telemetry
/// (which model the LLM is currently using), not config. Config-level
/// model/provider are in `SessionConfigSnapshot`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDetail {
    pub session_id: String,
    pub created_at: String,
    pub last_active_at: String,
    pub message_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_state: Option<serde_json::Value>,
}

/// Response for `get_messages` — paginated messages from a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagesResponse {
    pub session_id: String,
    pub messages: Vec<serde_json::Value>,
    pub offset: u64,
    pub limit: u32,
    pub total: u64,
    pub count: usize,
}

/// Response for `token_stats` — agent-scoped per-(provider, model) token
/// aggregate read straight off the `token_stats` table (O(#models)).
///
/// `consistent` is a cheap Σ(aggregate) vs Σ(per-session breakdown)
/// validation bit: `false` means the derived index has drifted and a
/// `rebuild_token_stats` is warranted — the numbers are still served so
/// the UI can show them with a caveat rather than failing the read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenStatsResponse {
    pub items: Vec<crate::conversation::TokenStatsRow>,
    pub consistent: bool,
}

/// Session metadata query methods.
#[async_trait]
pub trait SessionMetadataService: Send + Sync {
    /// List sessions with pagination and agent-level token totals (ADR-028).
    ///
    /// ADR-076 §决策 4: `scope` filters the list *before* pagination, so
    /// `total_count` / `total_pages` count only what the caller may read.
    /// Agent-level token totals are summed over the same filtered set.
    async fn list_sessions(
        &self,
        page: u32,
        size: u32,
        scope: &SessionScope,
    ) -> Result<SessionsListResponse>;

    /// Return the most recently active session, if any.
    async fn get_latest_session(&self) -> Result<Option<(String, Option<String>)>>;

    /// Return a single session's detail (meta + live state).
    async fn get_session(&self, session_id: &str) -> Result<SessionDetail>;

    /// Read messages from a session with offset-based pagination
    /// (ADR-050 forward semantics: `offset = 0` = oldest entry).
    ///
    /// When `from_tail` is `true`, the window is anchored to the last
    /// `limit` entries regardless of `offset` (initial-load path).
    async fn get_messages(
        &self,
        session_id: &str,
        offset: Option<u64>,
        limit: Option<u32>,
        from_tail: bool,
    ) -> Result<MessagesResponse>;

    /// Agent-scoped per-model token statistics (SQLite `token_stats`
    /// aggregate). Backends without the statistics table return `Err`.
    async fn token_stats(&self) -> Result<TokenStatsResponse> {
        Err(RuntimeError::Core(acowork_core::AcoworkError::Memory(
            "token_stats not supported by this session-metadata backend".to_string(),
        )))
    }
}
