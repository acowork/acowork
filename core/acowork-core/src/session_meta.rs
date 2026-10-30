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
//! **ADR-101 note**: these types live in `acowork-core` rather than the memory
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

use crate::error::{AcoworkError, Result};

/// Default maximum pruned-after-pick when pruning is disabled.
pub const PRUNE_DISABLED: usize = 0;

/// `(input, output, cache_read, cache_write)` token totals across every
/// session a caller may read. Aliased so the tuple returned by the runtime's
/// session listing (`scan_sessions_async`) stays readable.
pub type SessionTotals = (u64, u64, u64, u64);

/// Sentinel segment for an unattributable provider/model in a
/// [`ModelUsageEntry`] map key (legacy rows backfilled by the v6 migration,
/// or a call site whose session never stamped a provider id).
pub const UNKNOWN_USAGE_SEGMENT: &str = "unknown";

/// Build a [`ModelUsageEntry`] map key: `"{provider}|{model}"`, with
/// [`UNKNOWN_USAGE_SEGMENT`] standing in for a missing segment.
///
/// A `|` inside a provider or model id would corrupt the split; provider
/// ids and model ids are config-declared identifiers that do not contain
/// one, and [`split_model_usage_key`] splits on the FIRST `|` so a stray
/// separator lands in the model half rather than misattributing the
/// provider.
pub fn model_usage_key(provider: Option<&str>, model: Option<&str>) -> String {
    format!(
        "{}|{}",
        provider.unwrap_or(UNKNOWN_USAGE_SEGMENT),
        model.unwrap_or(UNKNOWN_USAGE_SEGMENT)
    )
}

/// Inverse of [`model_usage_key`]: split on the first `|`. A key without a
/// separator reads as `(key, "unknown")` rather than failing.
pub fn split_model_usage_key(key: &str) -> (&str, &str) {
    match key.split_once('|') {
        Some((p, m)) => (p, m),
        None => (key, UNKNOWN_USAGE_SEGMENT),
    }
}



/// Per-provider/model token breakdown of one session (token statistics
/// feature). The map key is `"{provider_id}|{model_id}"`; either segment
/// falls back to the literal `"unknown"` when the call site could not
/// attribute it (pre-upgrade data, or a session whose provider id was
/// never stamped).
///
/// Persisted as the `sessions.model_usage` JSON column. This breakdown is
/// the **source of truth** for the agent-scoped `token_stats` aggregate
/// table: the aggregate is maintained as a same-transaction diff of these
/// maps (see `SqliteSessionMetaStore`) and can be rebuilt from them at any
/// time — the disposable-derived-index pattern of ADR-082.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelUsageEntry {
    /// Cumulative prompt tokens attributed to this provider/model.
    pub input: u64,
    /// Cumulative completion tokens attributed to this provider/model.
    pub output: u64,
    /// Cumulative cache-read tokens attributed to this provider/model.
    pub cache_read: u64,
    /// Cumulative cache-write tokens attributed to this provider/model.
    pub cache_write: u64,
    /// Cumulative estimated cost in USD. `None` = "no price known" — the
    /// "宁可 miss 也不估计" policy: a fabricated 0 would read as "free".
    pub cost: Option<f64>,
}

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

/// One row of the agent-scoped `token_stats` aggregate table.
///
/// Derived state: the aggregate of every session's [`SessionMeta::model_usage`]
/// breakdown for one (provider, model) pair. Reads of the model-share panel
/// go through this; a drifted or corrupted aggregate is repairable from the
/// breakdowns via [`SessionMetaStore::rebuild_token_stats`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenStatsRow {
    pub provider: String,
    pub model: String,
    pub total_input: u64,
    pub total_output: u64,
    pub total_cache_read: u64,
    pub total_cache_write: u64,
    /// Cumulative estimated cost in USD. `None` = at least one contributing
    /// call had no known price — never a fabricated 0.
    pub total_cost: Option<f64>,
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
/// (`is_readable_by` ignores the flag when `initiator` is absent).
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
/// Produced by the HTTP layer from the Gateway-injected `x-user-id` +
/// `x-acowork-scope-admin` headers (see `acowork-gateway`'s auth
/// middleware) and consumed by the session read/write paths.
///
/// **Read and write are not the same axis.** An administrator reads
/// everything (support inspection, `as_user`), but an execution session has
/// exactly one initiator and only that initiator may write it — an admin
/// overriding that would defeat the rule the field exists to enforce
/// (ADR-076 §决策 4). Local mode has no accounts at all, so there is nobody
/// to exclude: it stays fully unfiltered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionScope {
    /// No restriction at all: a local-mode Runtime (`AUTH_MODE=local`,
    /// ADR-076 §决策 12), where the account system is off and one implicit
    /// human owns everything.
    Unfiltered,
    /// An administrator: may **read** every session (`as_user` inspection
    /// included), may **write** only what it initiated itself.
    Admin {
        uid: String,
        chats: Vec<String>,
    },
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

    /// Construct an administrator scope with no Chat membership.
    pub fn admin(uid: impl Into<String>) -> Self {
        Self::Admin {
            uid: uid.into(),
            chats: Vec::new(),
        }
    }

    /// Parse the `x-user-id` + `x-acowork-scope-admin` +
    /// `x-acowork-chat-scope` headers.
    ///
    /// `user` absent (header missing) means local mode — the Gateway never
    /// strips or injects in that mode, so an absent header is the normal
    /// local case and must NOT be treated as "anonymous, deny everything".
    /// The all-axes sentinel `*` in `user` is the administrator's unfiltered
    /// view; `admin` then carries the administrator's own uid, because "may
    /// read everything" no longer implies "may write everything". An
    /// `as_user` request is a plain [`Self::User`] (the impersonated
    /// identity is the whole point).
    /// `chats` is the comma-separated Chat session id list; empty/absent
    /// yields an empty membership set.
    pub fn from_headers(user: Option<&str>, admin: Option<&str>, chats: Option<&str>) -> Self {
        let chats: Vec<String> = chats
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let admin_uid = admin.map(str::trim).filter(|s| !s.is_empty());
        match Self::from_header_value_with_admin(user, admin_uid) {
            Self::Unfiltered => Self::Unfiltered,
            Self::Admin { uid, .. } => Self::Admin { uid, chats },
            Self::User { uid, .. } => Self::User { uid, chats },
        }
    }

    /// Parse the `x-user-id` header value alone (no Chat membership).
    pub fn from_header_value(value: Option<&str>) -> Self {
        Self::from_header_value_with_admin(value, None)
    }

    fn from_header_value_with_admin(value: Option<&str>, admin: Option<&str>) -> Self {
        match (value.map(str::trim), admin) {
            (Some("*"), Some(uid)) => Self::admin(uid),
            (None | Some("") | Some("*"), _) => Self::Unfiltered,
            (Some(uid), _) => Self::user(uid),
        }
    }

    /// The caller's own account id, or `None` for a local-mode scope (no
    /// accounts exist to attribute anything to).
    pub fn caller_uid(&self) -> Option<&str> {
        match self {
            Self::Unfiltered => None,
            Self::Admin { uid, .. } | Self::User { uid, .. } => Some(uid.as_str()),
        }
    }

    /// The Chat-session membership set (empty for local mode).
    pub fn chats(&self) -> &[String] {
        match self {
            Self::Unfiltered => &[],
            Self::Admin { chats, .. } | Self::User { chats, .. } => chats,
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

    /// ADR-076 §决策 4: **who initiated this session** — an account UUID, or
    /// an agent instance UUID when the session came from an agent-initiated
    /// intent. One initiator, written once, never re-assigned.
    ///
    /// This is the single identity the session has, and it is also the
    /// permission: only the initiator may write (see [`SessionMeta::is_writable_by`]).
    /// Deliberately **not** called `user_id` — that name belongs to the
    /// account system (directory rows, Chat participants, PM `created_by`),
    /// and reusing it here is what made "the session owner", "the logged-in
    /// user" and "the message sender" look interchangeable.
    ///
    /// `None` = created before the account system existed, or under
    /// `AUTH_MODE=local` (no accounts at all) — only an unfiltered
    /// (local-mode) reader sees those.
    #[serde(default, alias = "user_id", skip_serializing_if = "Option::is_none")]
    pub initiator: Option<String>,
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
    /// Per-provider/model breakdown of the session's cumulative tokens —
    /// the source of truth behind the agent-scoped `token_stats` aggregate.
    /// `None` on rows written before the column existed; the v6 migration
    /// seeds those with a single `"unknown|unknown"` entry carrying the
    /// historical totals (old usage is deliberately NOT attributed to
    /// `SessionMeta.model` — a model switch mid-session would misbill it).
    /// `BTreeMap` so a serialised map is byte-stable across writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_usage: Option<std::collections::BTreeMap<String, ModelUsageEntry>>,
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
            // An administrator reads everything: support inspection and
            // `as_user` are the whole point of the role. Reading is not
            // writing — see `is_writable_by`.
            SessionScope::Unfiltered | SessionScope::Admin { .. } => true,
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
                    return member || self.initiator.as_deref() == Some(uid.as_str());
                }
                match self.initiator.as_deref() {
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
    /// Two ownerless cases are exceptions, and they pull in opposite
    /// directions:
    ///
    /// * `Private` — **nobody** writes it, admin included: an unclaimed
    ///   session nobody may read is also one nobody may write, or the
    ///   first user to touch it could flip the flag back and re-share it
    ///   with everyone.
    /// * `Members` — **only an administrator** writes it. On a *claimed*
    ///   session `Members` is a pure read grant (see below), but with no
    ///   initiator that rule protects nobody and merely strands the row:
    ///   readable by the chat's participants, writable by no one, which the
    ///   ChatPanel renders as a permanently locked composer. Admin adoption
    ///   is such a session's only exit — rows created before an admin scope
    ///   carried a uid look exactly like this (ADR-076 §决策 4).
    ///
    /// Otherwise an **administrator is held to the same rule as anyone
    /// else**: a claimed execution session has exactly one initiator, so
    /// letting an admin type into someone else's would put a second person
    /// into a conversation that is defined as single-initiator (ADR-076
    /// §决策 4). Reading is what the role buys, and `as_user` covers the
    /// "look at it as them" case without a write. Local mode keeps full
    /// access — there is no second account to protect a session from.
    pub fn is_writable_by(&self, scope: &SessionScope) -> bool {
        match scope {
            SessionScope::Unfiltered => true,
            SessionScope::Admin { uid, .. } | SessionScope::User { uid, .. } => {
                match self.initiator.as_deref() {
                // Nobody recorded a claim, so there is nobody to protect:
                // the pre-account rule above stands — whoever may read it
                // may write it, `Private` stays the one exception (an
                // unclaimed private session belongs to no one, and the
                // first writer could re-share it with everyone), and
                // `Members` stays a read grant (§8.2 — a member never owns
                // the group's execution session).
                //
                // An administrator is the exception to that last one, and
                // only here: with no initiator there is no single-initiator
                // rule to defeat, but an *unclaimed* chat-bound session is
                // unreachable from every other surface. Sessions created
                // before an admin scope carried a uid are exactly that: read
                // through the chat binding, locked as "shared by another
                // account" for everyone, admin included, with no way back.
                None => match self.visibility {
                    Some(SessionVisibility::Private) => false,
                    Some(SessionVisibility::Members) => {
                        matches!(scope, SessionScope::Admin { .. })
                    }
                    _ => true,
                },
                // A claimed session has exactly one writer — admin included.
                // `Members` never transfers ownership: chat participants read
                // the group execution history, their messages enter through
                // the agent's delivery path, not a direct session write
                // (design 27 §8.2).
                Some(initiator) => initiator == uid.as_str(),
                }
            }
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
    /// Agent-scoped per-(provider, model) token aggregate (`token_stats`).
    /// Backends without a statistics table return `Err`.
    fn token_stats(&self) -> Result<Vec<TokenStatsRow>> {
        Err(AcoworkError::Memory(
            "token_stats not supported by this backend".to_string(),
        ))
    }
    /// Drop the aggregate table and re-derive it from every row's
    /// `model_usage` breakdown (disposable-derived-index repair path). Not a
    /// request-path call — invoked manually when the aggregate is suspected
    /// to have drifted from the breakdowns.
    fn rebuild_token_stats(&self) -> Result<()> {
        Err(AcoworkError::Memory(
            "token_stats not supported by this backend".to_string(),
        ))
    }
    /// Cheap consistency verdict for the `token_stats` aggregate: does
    /// Σ(aggregate rows) still equal Σ(per-session `model_usage`
    /// breakdowns)? Backends without the aggregate table return `Err`.
    ///
    /// Token dimensions compare exactly. The cost dimension follows the
    /// aggregate's poison-to-NULL rule: a NULL aggregate cost counts as
    /// consistent (unpriced contributions permanently poison the row
    /// until [`SessionMetaStore::rebuild_token_stats`]), a priced
    /// aggregate must match Σ detail costs.
    fn token_stats_consistent(&self) -> Result<bool> {
        Err(AcoworkError::Memory(
            "token_stats not supported by this backend".to_string(),
        ))
    }
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

