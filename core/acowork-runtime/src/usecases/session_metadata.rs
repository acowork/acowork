//! Session metadata query use case.
//!
//! ADR-040: this trait is the single source of truth for session-listing
//! and message-reading operations. Both the HTTP server and any future
//! transport adapter call through this trait.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::agent::intent_turn::IntentDisplay;
use crate::conversation::{
    ConfigField, ConversationEntry, SessionAuthority, SessionScope, SessionVisibility,
};
use crate::error::{Result, RuntimeError};

/// Deserialize default for [`SessionSummary::authority`]: a client that has a
/// cached payload from before the tier existed reads it as the one thing it
/// already believed — read-only — which disables controls rather than offering
/// a write the server would refuse.
fn default_authority() -> SessionAuthority {
    SessionAuthority::ReadOnly
}

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
    /// ADR-076 §决策 4: who initiated this session (an account UUID, or an
    /// agent instance UUID for agent-initiated work). The execution surface
    /// shows **this** identity, not the logged-in account's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initiator: Option<String>,
    /// ADR-076 §决策 4: may the caller modify this session?
    ///
    /// Not an ownership flag: a `local`-mode caller may write sessions it
    /// did not initiate (see `SessionInfo::can_write`). A `false` value means
    /// the execution view is read-only for this caller.
    pub can_write: bool,
    /// Plan D1: the caller's write tier over this session — `read_only`,
    /// `intervener` or `owner`. An intervener is a real tier, not a rounding
    /// error: an agent-to-agent execution session has no human owner, so
    /// without it the only party that can fix an unusable launch parameter is
    /// the agent that dispatched the work — which has already been told the
    /// run failed and never retries.
    #[serde(default = "default_authority")]
    pub authority: SessionAuthority,
    /// Plan D1: which config fields this caller may write, in
    /// [`ConfigField::ALL`] order. The surface enables exactly these, so a
    /// field the server would refuse never gets an input that looks live.
    #[serde(default)]
    pub config_fields: Vec<ConfigField>,
    /// Design 27 §8.2: the Chat session this execution session was started
    /// from. It is where a turn's *output* goes — the interaction card the run
    /// may stop on, and the terminal echo — never where its input came from:
    /// a message typed in the execution view is a session-scoped write and is
    /// not mirrored into that Chat (`None` = a session that never came from a
    /// Chat, which gets neither card nor echo).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_session_id: Option<String>,
    /// Whether this session is currently loaded in the Runtime's in-memory
    /// `sessions` map (ADR-038 `SessionLifecycleState::Active`). The Desktop
    /// tab strip mirrors this: every `is_active` session gets a tab. The
    /// restore path is `list_sessions(active_only = true)` — the Desktop asks
    /// once per (re)launch and projects the result onto the strip, so the set
    /// is never cached client-side (`chatStore.adoptRuntimeTabs`).
    #[serde(default)]
    pub is_active: bool,
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

/// ADR-106 D7: which Chat surface a session listing is being read for.
///
/// A *description*, not an authority: the caller states the chat and the
/// surface, and [`crate::agent::session::chat_candidates::ChatScopeRule`]
/// answers which rows that admits. The identity is never read from the query
/// string — `owner_user_id` is the caller's own uid as the Gateway's scope
/// headers established it, and the chat-membership wall is checked by the HTTP
/// layer before this reaches the scan.
#[derive(Debug, Clone)]
pub struct ChatListingScope {
    pub chat_session_id: String,
    pub surface: crate::agent::session::chat_candidates::CandidateSurface,
    /// `None` = local mode, where no account exists to be the initiator.
    pub owner_user_id: Option<String>,
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

/// One entry of [`MessagesResponse`]: the persisted row verbatim, plus the human
/// rendering the row carries.
///
/// The persisted row *is* the stored shape ([`ConversationEntry`]) and stays the
/// only source of it: `#[serde(flatten)]` means a field added to the row later
/// reaches this wire shape without anyone remembering to copy it here. What this
/// view adds is exactly one thing the stored shape cannot declare — `display`, the
/// projection for the human reader, resolved by the producer's own rule.
///
/// That resolution belongs here, at the transport boundary, and not in the
/// clients: a surface that had to read `metadata.intent` itself would own a copy
/// of the key name and of the "is it usable" predicate, per surface, forever.
#[derive(Debug, Serialize)]
pub struct ConversationEntryView<'a> {
    #[serde(flatten)]
    pub entry: &'a ConversationEntry,
    /// `None` on every human turn and on every row written before the projection
    /// existed; a surface then renders `content`, which is the honest fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<IntentDisplay>,
    /// Plan D3: who spoke this row, when that is not the session's initiator.
    ///
    /// Resolved server-side like `display`, for the same reason: the client must
    /// not have to know the metadata key or the "is it usable" rule. `None` on
    /// an ordinary turn — absence means "nothing to announce", never "unknown
    /// speaker", so a surface that gets it shows no badge and a surface that
    /// predates the field renders exactly as before.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speaker: Option<crate::conversation::TurnSpeaker>,
}

impl<'a> ConversationEntryView<'a> {
    /// The wire shape of one stored row.
    pub fn new(entry: &'a ConversationEntry) -> Self {
        Self {
            display: entry
                .metadata
                .as_ref()
                .and_then(IntentDisplay::from_metadata),
            speaker: entry
                .metadata
                .as_ref()
                .and_then(crate::conversation::TurnSpeaker::from_metadata),
            entry,
        }
    }
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
    ///
    /// ADR-038: `active_only` narrows the set to sessions that are `Active`
    /// in this Runtime right now (resident in the in-memory session map).
    /// This is the desktop tab strip's restore source after a client
    /// restart — the *set* of open sessions is Runtime truth, so the client
    /// derives it instead of remembering it. Also pre-pagination: `total_*`
    /// describe the resident set.
    ///
    /// ADR-106 D7: `chat` narrows the set to the execution sessions one Chat
    /// surface is allowed to address — the second `@` level's list, the
    /// session-history picker's suggestions, and nothing else. Same placement
    /// argument: a page sliced off the unfiltered set would leak how much work
    /// this chat cannot see. `None` is the ordinary listing.
    async fn list_sessions(
        &self,
        page: u32,
        size: u32,
        scope: &SessionScope,
        active_only: bool,
        chat: Option<&ChatListingScope>,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row(metadata: Option<serde_json::Value>) -> ConversationEntry {
        ConversationEntry {
            id: "m1".to_string(),
            ts: "2026-11-09T08:00:00.000Z".to_string(),
            role: "user".to_string(),
            content: "[intent:abc:create_dev_tasks] {\"project_id\":\"p-1\"}".to_string(),
            metadata,
            kind: None,
        }
    }

    #[test]
    fn the_wire_shape_is_the_stored_row_plus_display() {
        // The panel reads `content` for the machine text and `display` for the
        // human one. Every stored field must survive the view (`flatten`), and the
        // projection must arrive parsed — a client that had to read
        // `metadata.intent` itself would own a copy of the key and of the
        // usability rule.
        let display = serde_json::json!({
            "origin": "request",
            "action": "create_dev_tasks",
            "summary": "布署需求",
            "body": "### 布署需求\n\n- **project_id**: p-1",
            "truncated": false
        });
        let entry = row(Some(serde_json::json!({ "round_id": "r-1", "intent": display })));
        let wire = serde_json::to_value(ConversationEntryView::new(&entry)).unwrap();

        assert_eq!(wire["id"], "m1");
        assert_eq!(wire["role"], "user");
        assert_eq!(wire["ts"], "2026-11-09T08:00:00.000Z");
        assert_eq!(wire["content"], entry.content);
        assert_eq!(
            wire["metadata"][crate::agent::intent_turn::META_KEY_INTENT_DISPLAY],
            display
        );
        assert_eq!(wire["display"]["summary"], "布署需求");
        assert_eq!(wire["display"]["body"], "### 布署需求\n\n- **project_id**: p-1");
        assert_eq!(wire["display"]["truncated"], false);
        assert!(wire.get("kind").is_none());
    }

    #[test]
    fn a_row_without_a_usable_display_keeps_todays_wire_shape() {
        // Human turns and pre-projection rows: the key is absent, not `null`, so an
        // older client sees exactly the payload it saw before.
        for metadata in [None, Some(serde_json::json!({ "round_id": "r-1" }))] {
            let entry = row(metadata);
            let wire = serde_json::to_value(ConversationEntryView::new(&entry)).unwrap();
            assert!(wire.get("display").is_none(), "wire: {wire}");
            assert_eq!(wire["content"], entry.content);
        }
    }
}
