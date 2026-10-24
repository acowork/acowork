//! Session metadata storage: trait + types (ADR-082 §4 step 3).
//!
//! The session list is the metadata side of `conversations/`: titles, model
//! choice, token totals, todo snapshots. Each implementation is responsible for
//! the storage and retrieval of the row keyed by `session_id`; the JSONL
//! conversation log itself is not in scope.
//!
//! `SqliteSessionMetaStore` (in `acowork-sqlite`) is the only implementation.
//! The runtime holds an `Arc<dyn SessionMetaStore>` the same way it holds any
//! other late-bound store, so the storage backend and the call sites stay apart.
//!
//! **ADR-100 note**: these types live in `acowork-core` rather than the memory
//! crate. Session metadata never was memory business — it only shared the
//! SQLite file — and the Runtime must be able to name the trait without
//! depending on `acowork-memory`, which is now a standalone service.
//!
//! # Atomicity
//!
//! `upsert` replaces the whole row. Callers in this codebase already mutate
//! the in-memory `SessionMeta` and `write_meta` it whole — the trait matches
//! that unit of work rather than threading individual setters through every
//! column.
//!
//! # Pruning
//!
//! `prune_to` deletes sessions whose `last_active_at` is older than the
//! (n+1)-th most-recent **of that session's owner**, and returns the deleted
//! ids. Bucketing by owner is deliberate (ADR-076 §决策 4): a single global cap
//! lets one account's busy week evict another account's history. The matching
//! JSONL files are not this backend's concern — the caller cleans them up.
//! Keeping the side-effect split lets the trait stay pure on the storage axis.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Default maximum pruned-after-pick when pruning is disabled.
pub const PRUNE_DISABLED: usize = 0;

/// `(input, output, cache_read, cache_write)` token totals across every
/// session a caller may read. Aliased so the tuple returned by the runtime's
/// session listing (`scan_sessions_async`) stays readable.
pub type SessionTotals = (u64, u64, u64, u64);


/// Per-session token counters (ADR-027 snapshot + cumulative).
///
/// Persisted as seven flattened integer columns in SQLite (see
/// `SqliteSessionMetaStore`).
///
/// `#[serde(default)]` keeps a row written before ADR-066 — which omits the
/// four cache fields — deserialisable; the defaults are all zero, matching the
/// "宁可 miss 也不估计" policy.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionTokens {
    pub last_input: u64,
    pub last_output: u64,
    pub total_input: u64,
    pub total_output: u64,
    pub last_cache_read: u64,
    pub last_cache_write: u64,
    pub total_cache_read: u64,
    pub total_cache_write: u64,
}

/// Per-session LLM performance accumulator (speed / responsiveness).
///
/// Two independent sample means, each stored as a running **sum plus
/// count**:
///
///   - [`Self::avg_tps`] — mean output tokens per second. Per call it is
///     `completion_tokens / stream_seconds`, timed from the **first**
///     output token, so the pre-token wait is not counted as generation.
///   - [`Self::avg_ttft_ms`] — mean time to first token: the wait from
///     "LLM call started" to the first output event.
///
/// Sum + count is the storage form, not the display form — the means are
/// derived on read. A folded running mean cannot carry two *independent*
/// denominators: a call that measured TTFT but not speed (a Provider that
/// reported no completion count) would either pollute the speed mean's
/// denominator or drop a real responsiveness sample. Keeping the counts
/// apart makes each mean exact, and it stays exact rather than drifting
/// with rounding as a session accumulates hundreds of calls.
///
/// `None` from either accessor means "no measurement yet" — the frontend
/// renders "—" rather than a fake 0, matching the `total_input_tokens`
/// convention.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionPerf {
    /// Sum of per-call output speeds, in tokens/second.
    pub tps_sum: f64,
    /// How many calls contributed to [`Self::tps_sum`].
    pub tps_samples: u32,
    /// Sum of per-call time-to-first-token, in milliseconds.
    pub ttft_ms_sum: f64,
    /// How many calls contributed to [`Self::ttft_ms_sum`].
    pub ttft_ms_samples: u32,
}

impl SessionPerf {
    /// Mean output tokens/second, or `None` when nothing has been measured.
    pub fn avg_tps(&self) -> Option<f64> {
        mean(self.tps_sum, self.tps_samples)
    }

    /// Mean time-to-first-token in milliseconds, or `None`.
    pub fn avg_ttft_ms(&self) -> Option<f64> {
        mean(self.ttft_ms_sum, self.ttft_ms_samples)
    }

    /// `true` when no call has produced a measurement for either field.
    /// Lets a caller avoid persisting an accumulator that carries no
    /// information (the "absent, not zero" rule the meta fields follow).
    pub fn is_empty(&self) -> bool {
        self.tps_samples == 0 && self.ttft_ms_samples == 0
    }

    /// Fold one call's measurements in, returning the updated snapshot.
    ///
    /// Each field advances against its OWN count, so a call that measured
    /// only TTFT moves the latency mean and leaves the speed mean exactly
    /// where it was — and vice versa.
    ///
    /// A call with neither measurement (cancelled before its first token,
    /// or a Provider that reported no usage) is ignored entirely: it cannot
    /// drag either mean and bumps neither count.
    pub fn merged(&self, tps: Option<f64>, ttft_ms: Option<f64>) -> Self {
        let mut out = self.clone();
        if let Some(v) = tps.filter(|v| v.is_finite() && *v > 0.0) {
            out.tps_sum += v;
            out.tps_samples = out.tps_samples.saturating_add(1);
        }
        if let Some(v) = ttft_ms.filter(|v| v.is_finite() && *v >= 0.0) {
            out.ttft_ms_sum += v;
            out.ttft_ms_samples = out.ttft_ms_samples.saturating_add(1);
        }
        out
    }
}

/// Derive a mean from a running sum, `None` when nothing was sampled or the
/// accumulation is not a usable number.
fn mean(sum: f64, samples: u32) -> Option<f64> {
    if samples == 0 || !sum.is_finite() {
        return None;
    }
    let m = sum / samples as f64;
    m.is_finite().then_some(m)
}

/// One todo entry (ADR-060). Lives inside `SessionMeta.todos`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// Who may read a session (ADR-076 §决策 4).
///
/// The field exists so a session can be *shared* rather than owned
/// exclusively: a [`Public`](Self::Public) session is readable by every
/// authenticated user, a [`Private`](Self::Private) one only by its owner
/// (or an administrator).
///
/// **Absent means public** — but only absent *and* ownerless.
///
/// `SessionMeta.visibility` is `Option<SessionVisibility>` with
/// `skip_serializing_if = "Option::is_none"`, so an unset visibility is
/// byte-identical on disk to a pre-ADR-076 row. That is the whole point:
/// upgrading must not retroactively hide every existing session from its
/// user, and those sessions have no owner to restrict them to anyway
/// (`is_readable_by` ignores the flag when `user_id` is absent).
///
/// A session created by an identified account is **not** left absent: the
/// creation path stamps `Private` (ADR-076 §决策 4). `None` therefore
/// means "predates accounts, or local mode" — never "a new session whose
/// creator nobody bothered to ask" — which is why this value stopped
/// being the multi-user default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionVisibility {
    /// Readable by any authenticated user.
    ///
    /// The default only where no owner exists to restrict it to; an owned
    /// session is created `Private` and must be shared deliberately.
    Public,
    /// Readable only by the owning account (and administrators).
    Private,
    /// Readable by the **participants of the Chat session this execution
    /// session belongs to** (design 27 §8.2), plus its owner. The binding
    /// is [`SessionMeta::chat_session_id`]; the caller's Chat membership
    /// arrives as a Gateway-injected scope token, so no ACL table exists
    /// (the participant relation *is* the permission).
    ///
    /// Deliberately **not** `Public`: `Public` is platform-wide and would
    /// leak group execution history to any signed-in account (§8.2).
    Members,
}

impl SessionVisibility {
    /// Whether a session with this setting is restricted to its owner.
    pub fn is_private(self) -> bool {
        matches!(self, Self::Private)
    }

    /// Stable `TEXT` encoding, used by the SQLite `sessions.visibility` column.
    ///
    /// Deliberately hand-written instead of going through `serde_json`: the
    /// column is plain `TEXT`, and `serde_json` would store the quotes too.
    /// A test pins this in lockstep with the `#[serde(rename_all = "lowercase")]`
    /// representation, so a JSON-sidecar row and a SQLite row agree.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Private => "private",
            Self::Members => "members",
        }
    }

    /// Inverse of [`Self::as_str`].
    ///
    /// Unrecognised text returns `None`, i.e. "absent" → public. A column
    /// value this code does not know (hand-edited database, a future variant
    /// read by an older binary) must not fail the whole listing, and it must
    /// not silently mean "private" either.
    pub fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "public" => Some(Self::Public),
            "private" => Some(Self::Private),
            "members" => Some(Self::Members),
            _ => None,
        }
    }
}

/// Who is asking for a session (ADR-076 §决策 4).
///
/// Produced by the HTTP layer from the Gateway-injected `x-user-id`
/// header (see `acowork-gateway`'s auth middleware) and consumed by the
/// session read/write paths. Two variants, because "administrator" and
/// "`AUTH_MODE=local`, no accounts at all" (ADR-076 §决策 12) have
/// genuinely identical access — both are unfiltered — and splitting them
/// would mean two code paths that can never diverge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionScope {
    /// No restriction: an administrator, or a local-mode Runtime where
    /// the account system is off.
    Unfiltered,
    /// A signed-in account, plus the ids of the Chat sessions it currently
    /// participates in (design 27 §8.2). `chats` is empty unless the
    /// Gateway injected the `x-acowork-chat-scope` header; an empty set
    /// simply means no `Members`-visible session is readable through the
    /// chat path — fail closed, never fail open.
    User {
        uid: String,
        chats: Vec<String>,
    },
}

impl SessionScope {
    /// Construct a plain user scope with no Chat membership.
    pub fn user(uid: impl Into<String>) -> Self {
        Self::User {
            uid: uid.into(),
            chats: Vec::new(),
        }
    }

    /// Parse the `x-user-id` + `x-acowork-chat-scope` headers.
    ///
    /// `user` absent (header missing) means local mode — the Gateway never
    /// strips or injects in that mode, so an absent header is the normal
    /// local case and must NOT be treated as "anonymous, deny everything".
    /// The all-axes sentinel `*` is the administrator's unfiltered view.
    /// `chats` is the comma-separated Chat session id list; empty/absent
    /// yields an empty membership set.
    pub fn from_headers(user: Option<&str>, chats: Option<&str>) -> Self {
        let scope = Self::from_header_value(user);
        match scope {
            Self::User { uid, .. } => Self::User {
                uid,
                chats: chats
                    .map(|v| {
                        v.split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
            },
            other => other,
        }
    }

    /// Parse the `x-user-id` header value alone (no Chat membership).
    pub fn from_header_value(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            None | Some("") | Some("*") => Self::Unfiltered,
            Some(uid) => Self::user(uid),
        }
    }

    /// The account id, or `None` for an unfiltered scope.
    pub fn user_id(&self) -> Option<&str> {
        match self {
            Self::Unfiltered => None,
            Self::User { uid, .. } => Some(uid.as_str()),
        }
    }
}

/// One persisted session, mapped 1:1 onto the `sessions` row.
///
/// `version` is the `conversations/{id}.jsonl` format version the row was
/// written by (the runtime's `CONVERSATION_FORMAT_VERSION`), `corrupted` is set
/// when the JSONL had to be salvaged. Both round-trip through
/// `SqliteSessionMetaStore` and are served verbatim by the `/sessions` API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub version: u32,
    pub session_id: String,
    pub agent_id: String,
    pub created_at: String,

    /// ADR-076 §决策 4: the account that owns this session.
    ///
    /// `None` = created before the account system existed, or under
    /// `AUTH_MODE=local` (no accounts at all) — only an unfiltered
    /// (administrator) reader sees those. Immutable after creation: the
    /// metadata builder never invents one, and the only writer is the
    /// creation path (see `ConversationSession::set_user_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// ADR-076 §决策 4: who may read this session. **Absent = public**
    /// (see [`SessionVisibility`]). Mutable — the owner can flip it at
    /// any time via `ConversationSession::set_visibility`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<SessionVisibility>,
    /// Design 27 §8.2: the Chat session this execution session belongs to,
    /// stamped when the session is created from a `chat:{chat_session_id}`
    /// routing key. Meaningful only with `visibility = Members`; the read
    /// check matches it against the caller's Chat membership set injected
    /// by the Gateway (`x-acowork-chat-scope`), so no ACL table is needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_session_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todos: Option<Vec<TodoItem>>,

    pub message_count: u64,
    pub last_active_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<SessionTokens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_call_counter: Option<u32>,
    /// Session-level LLM speed / responsiveness averages. `None` until the
    /// first LLM call has produced a measurement; absent on sessions created
    /// before this field existed, which deserialise via the `Option` default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perf: Option<SessionPerf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_compaction_offset: Option<u64>,
    /// Text of the most recent compaction summary, persisted at write time
    /// (design 27 §4 iron rule: the T2-handoff read must never rescan the
    /// JSONL). `None` on sessions created before this field existed — the
    /// reader then falls back to a one-time legacy scan and heals the row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_compaction_summary: Option<String>,

    #[serde(default)]
    pub corrupted: bool,
}

impl SessionMeta {
    /// May `scope` **read** this session (ADR-076 §决策 4)?
    ///
    /// An unset (`None`) visibility is public: every signed-in account
    /// may read it. Only an explicit `private` restricts reading to the
    /// owning account.
    ///
    /// In practice `None` now shows up only on sessions with no owner
    /// (pre-ADR-076 data, or local mode): the creation path stamps
    /// `Private` on anything it can attribute to an account.
    pub fn is_readable_by(&self, scope: &SessionScope) -> bool {
        match scope {
            SessionScope::Unfiltered => true,
            SessionScope::User { uid, chats } => {
                if self.visibility == Some(SessionVisibility::Members) {
                    // Design 27 §8.2: a group execution session is readable
                    // by its owner and by any current participant of the
                    // bound Chat session. Membership is the Gateway-injected
                    // set — removed participants lose access immediately
                    // (§8.3), no stale grant exists.
                    let member = self
                        .chat_session_id
                        .as_deref()
                        .is_some_and(|c| chats.iter().any(|x| x == c));
                    return member || self.user_id.as_deref() == Some(uid.as_str());
                }
                match self.user_id.as_deref() {
                // Ownerless sessions are a two-way split, and the split is
                // the whole reason `None` and `Some(Private)` must not be
                // collapsed:
                //
                //   `None` / `Public` → readable by every account.
                //     This is pre-ADR-076 data (no owner to restrict it to)
                //     and local-mode sessions. Hiding them on upgrade would
                //     strand users outside their own history.
                //
                //   `Private` → readable by nobody but an administrator.
                //     An *unclaimed* session. The agent creates one at cold
                //     start, before any account has touched the process;
                //     handing that to every account as a shared session is
                //     how two users end up typing into the same
                //     conversation. "Private with no owner" honestly means
                //     "belongs to no one", so no account gets it.
                //
                // Note this is the *explicit* flag, set by the creation
                // path or an administrator — not something a user can
                // reach (see `is_writable_by` and `put_session_visibility`).
                None => !self.visibility.is_some_and(SessionVisibility::is_private),
                Some(owner) => {
                    !self.visibility.is_some_and(SessionVisibility::is_private)
                        || owner == uid.as_str()
                }
                }
            }
        }
    }

    /// May `scope` **modify** this session (open / close / delete /
    /// retitle / re-share)?
    ///
    /// Deliberately stricter than [`Self::is_readable_by`]: a public
    /// session is *visible* to every account but *owned* by exactly one,
    /// so sharing is not the same as handing over the delete button.
    ///
    /// Ownerless sessions keep the pre-ADR-076 rule — modifiable by any
    /// signed-in account — for the same reason [`Self::is_readable_by`]
    /// keeps them readable: that is the behaviour data predating accounts
    /// (and local mode) depends on, and locking them to admins would
    /// strand users outside their own history.
    ///
    /// The exception is an ownerless session marked `Private`: an
    /// unclaimed session nobody may read is also one nobody may write, or
    /// the first user to touch it could flip the flag back and re-share
    /// it with everyone.
    pub fn is_writable_by(&self, scope: &SessionScope) -> bool {
        match scope {
            SessionScope::Unfiltered => true,
            SessionScope::User { uid, .. } => match self.user_id.as_deref() {
                // `Members` behaves like `Private` for writes: chat
                // participants read the group execution history, they do
                // not own or mutate the session itself — their messages
                // enter through the agent's delivery path, not a direct
                // session write (design 27 §8.2).
                None => !self
                    .visibility
                    .is_some_and(|v| v.is_private() || v == SessionVisibility::Members),
                Some(owner) => owner == uid.as_str(),
            },
        }
    }
}

/// Trait every session-meta backend implements.
///
/// Method semantics:
/// - `get`: `Ok(None)` when the row is missing, `Err` only for backend failure.
/// - `upsert`: replaces the entire row. No `insert_or_update` distinction.
/// - `delete`: removes one session row (and any derived index entry). No-op
///   when the session is absent, so callers do not need a pre-check.
/// - `list_recent`: cap `limit`. Order is `last_active_at` descending, read off
///   the `last_active_at` index.
/// - `find_latest`: zero-cost shortcut for `list_recent(1).into_iter().next()`.
/// - `search`: substring query against `title` first, falling back to
///   `agent_id` / `workspace_id` (FTS5 trigram). Empty `query` returns the same
///   as `list_recent`.
/// - `prune_to`: leaves the first `max_sessions` newest rows **per owner** and
///   removes the rest, returning the deleted ids. Bucketing by owner is what
///   keeps one account's busy week from evicting another account's history
///   (ADR-076 §决策 4).
pub trait SessionMetaStore: Send + Sync {
    fn get(&self, session_id: &str) -> Result<Option<SessionMeta>>;
    fn upsert(&self, meta: &SessionMeta) -> Result<()>;
    /// Remove a single session row. No-op when the session is absent.
    fn delete(&self, session_id: &str) -> Result<()>;
    fn list_recent(&self, limit: usize) -> Result<Vec<SessionMeta>>;
    fn find_latest(&self) -> Result<Option<SessionMeta>>;
    fn search(&self, query: &str, limit: usize) -> Result<Vec<SessionMeta>>;
    /// Returns the deleted session_ids so the caller can clean up JSONL.
    fn prune_to(&self, max_sessions: usize) -> Result<Vec<String>>;
}

/// Converts `last_active_at` (RFC3339 string) to the epoch-ms `i64` the
/// `last_active_at` column indexes. Returns 0 for unparseable input, which
/// sorts the row to the bottom (SQLite compares `INTEGER` numerically).
pub fn last_active_at_ms(meta: &SessionMeta) -> i64 {
    DateTime::parse_from_rfc3339(&meta.last_active_at)
        .map(|dt| dt.with_timezone(&Utc).timestamp_millis())
        .unwrap_or(0)
}

// `Send + Sync` sanity: the trait is used behind `Arc<dyn _>` across threads.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Box<dyn SessionMetaStore>>();
};

#[cfg(test)]
mod perf_tests {
    use super::SessionPerf;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-6, "expected {b}, got {a}");
    }

    #[test]
    fn merged_averages_each_call_in() {
        let mut perf = SessionPerf::default();

        // Call 1: 900 tokens over 3.0s = 300 t/s, TTFT 500ms.
        perf = perf.merged(Some(300.0), Some(500.0));
        approx(perf.avg_tps().unwrap(), 300.0);
        approx(perf.avg_ttft_ms().unwrap(), 500.0);
        assert_eq!(perf.tps_samples, 1);
        assert_eq!(perf.ttft_ms_samples, 1);

        // Call 2: 100 t/s, TTFT 100ms → sample mean of both.
        perf = perf.merged(Some(100.0), Some(100.0));
        approx(perf.avg_tps().unwrap(), 200.0);
        approx(perf.avg_ttft_ms().unwrap(), 300.0);
        assert_eq!(perf.tps_samples, 2);

        // Call 3: 200 t/s, TTFT 300ms → mean of three.
        perf = perf.merged(Some(200.0), Some(300.0));
        approx(perf.avg_tps().unwrap(), 200.0);
        approx(perf.avg_ttft_ms().unwrap(), 300.0);
        assert_eq!(perf.tps_samples, 3);
        assert_eq!(perf.ttft_ms_samples, 3);
    }

    /// The two means count DIFFERENT sets of calls, so each must divide by
    /// its own sample count.
    ///
    /// Regression: the first implementation folded a running mean under a
    /// single shared `samples` counter, so a call that measured TTFT but not
    /// speed (a Provider reporting no completion count — the common
    /// `usage == None` / `completion_tokens == 0` case) still advanced the
    /// denominator of the speed mean. `{300 t/s}, {no speed}, {100 t/s}`
    /// then reported 233.3 t/s instead of the true 200: a silently inflated
    /// figure shown to the user.
    #[test]
    fn partial_samples_do_not_inflate_the_other_mean() {
        let mut perf = SessionPerf::default();
        perf = perf.merged(Some(300.0), Some(500.0));
        // Speed unmeasurable, TTFT measurable.
        perf = perf.merged(None, Some(100.0));
        // Speed measurable again.
        perf = perf.merged(Some(100.0), Some(300.0));

        assert_eq!(perf.tps_samples, 2, "two speed samples");
        assert_eq!(perf.ttft_ms_samples, 3, "three latency samples");
        approx(perf.avg_tps().unwrap(), 200.0);
        approx(perf.avg_ttft_ms().unwrap(), 300.0);
    }

    #[test]
    fn unmeasurable_call_does_not_move_the_average() {
        let mut perf = SessionPerf::default();
        perf = perf.merged(Some(300.0), Some(500.0));

        // A cancelled stream: no TTFT, no completion count, no duration.
        // Folding in a 0 here would report "the model stopped generating".
        let after = perf.merged(None, None);
        assert_eq!(after, perf);
        assert_eq!(after.tps_samples, 1);
        assert_eq!(after.ttft_ms_samples, 1);
    }

    #[test]
    fn first_call_seeds_each_field_independently() {
        // A provider that reports no completion count still yields a TTFT —
        // the user cares about responsiveness even when the speed is unknown.
        let perf = SessionPerf::default().merged(None, Some(250.0));
        assert_eq!(perf.avg_tps(), None);
        approx(perf.avg_ttft_ms().unwrap(), 250.0);
        assert_eq!(perf.ttft_ms_samples, 1);
        assert_eq!(perf.tps_samples, 0);
    }

    #[test]
    fn non_positive_and_non_finite_samples_are_rejected() {
        let perf = SessionPerf::default()
            // A 0 t/s would mean "generated nothing"; a NaN/inf from a
            // degenerate duration must not poison a running sum.
            .merged(Some(0.0), None)
            .merged(Some(f64::NAN), None)
            .merged(Some(-12.0), None)
            .merged(None, Some(f64::INFINITY))
            .merged(None, Some(-1.0));
        assert!(perf.is_empty());
        assert_eq!(perf.avg_tps(), None);
        assert_eq!(perf.avg_ttft_ms(), None);

        // TTFT of exactly 0ms is a legitimate measurement (sub-ms first
        // token), unlike a 0 speed.
        let perf = SessionPerf::default().merged(None, Some(0.0));
        assert_eq!(perf.avg_ttft_ms(), Some(0.0));
        assert_eq!(perf.ttft_ms_samples, 1);
        assert!(!perf.is_empty());
    }

    #[test]
    fn empty_accumulator_reads_as_none_not_zero() {
        let perf = SessionPerf::default();
        assert!(perf.is_empty());
        assert_eq!(perf.avg_tps(), None);
        assert_eq!(perf.avg_ttft_ms(), None);
    }
}
