//! Chat session persistence (design doc 27 §4.2 / §3 / §8, Phase 1).
//!
//! The Chat session is the authoritative message history between users and
//! agents (and between agents). It is owned by this service — Runtime, PM
//! and Desktop all reach it through the single backend write entry, never by
//! touching another agent's home. Agent execution sessions are a different
//! thing (§2.3): chat messages never carry agent thinking or tool calls.
//!
//! Message types are a fixed communication vocabulary (§2.2):
//!
//! - `user_message` — a human utterance; archive only, it triggers nothing.
//! - `intent_request` / `intent_response` — an Intent sent or returned,
//!   with direction, sender, receiver and reply-link metadata.
//!
//! ```text
//! {data_dir}/chat-sessions/{session_id}/
//!   session.json     kind, participants, title, project binding,
//!                    last_active_at, message_count (physical-line counter),
//!                    last_message_preview (derived at write time)
//!   messages.jsonl   append-only, each line carries a session-local
//!                    monotonic `seq` and a globally unique `message_id`
//!   lines.index      derived byte spans + `message_id` lookup for the log
//! ```
//!
//! The request path NEVER reads the log whole: paging, the `seq` counter,
//! idempotency and fetch-by-id all resolve through the per-session line
//! index ([`ensure_index`]). The index is rebuilt from the log once per
//! session per process (first touch) and maintained incrementally
//! afterwards; the `lines.index` sidecar keeps a restart from paying the
//! rebuild. A full pass over the log exists only as that first-touch (or
//! self-heal) reconciliation — never per request, never per write.
//!
//! Concurrency follows the `chat.rs` model (ADR-092): one mutex per session
//! directory; the whole load → dedup → assign seq → append → save sequence
//! is one atomic read-modify-write under that lock. Different sessions stay
//! parallel (§8: per-session single writer, not a global serial point).
//!
//! Idempotency: a write carrying a `message_id` already present in the log
//! returns the existing message instead of appending a duplicate — QoS 1
//! redelivery and client retries must never double-write (§8-4). The
//! lookup hits the line index by id, not by scanning the log.
//!
//! Ordering: the canonical order is the order the backend write entry
//! accepted and committed, expressed as `seq`; timestamps are for display
//! only (§8).

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

/// Fixed message vocabulary (design 27 §4.2, Wave 2B). Anything else is
/// an agent execution detail and has no business in a chat log.
///
/// The retired `intent_request` / `intent_response` kinds are **not**
/// writable kinds — their semantics live on these rows as fields
/// (`conversation_id` / `in_reply_to` / `status`), and the old durable
/// rows are re-labelled at the single read path ([`normalize_kind`],
/// plan D-A/A1: write new, read old, never migrate).
pub const KIND_USER_MESSAGE: &str = "user_message";
pub const KIND_AGENT_MESSAGE: &str = "agent_message";
pub const KIND_INTERACTION_REQUEST: &str = "interaction_request";
pub const KIND_SYSTEM_NOTICE: &str = "system_notice";

/// `project_group` sessions are keyed by project id: the same project can
/// never get two group chats (§3.3, §14 Phase 2 "幂等创建").
const KINDS: &[&str] = &[
    KIND_USER_MESSAGE,
    KIND_AGENT_MESSAGE,
    KIND_INTERACTION_REQUEST,
    KIND_SYSTEM_NOTICE,
];

/// Design 27 §4.2 / plan D-A (A1 "write new, read old"): render a retired
/// intent kind in the new vocabulary at the read path. Old rows are never
/// rewritten — the intent facts already live in `meta` fields, so this is
/// a pure re-label:
///
/// - `intent_request` written by an agent (agent-to-agent ask) → `agent_message`
/// - `intent_request` written by a human (the old group-mention row) → `user_message`
/// - `intent_response` (always the answering agent) → `agent_message`
///
/// A writer whose identity type is unknown (legacy untyped participant)
/// reads as the agent side, since the mention path postdates the typed
/// participants only marginally and pair sessions are agent-agent by
/// construction.
pub fn normalize_kind(kind: &str, writer_is_human: bool) -> String {
    match kind {
        "intent_request" if writer_is_human => KIND_USER_MESSAGE.to_string(),
        "intent_request" | "intent_response" => KIND_AGENT_MESSAGE.to_string(),
        other => other.to_string(),
    }
}

/// Chat session kinds (design 27 §4.2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// One-to-one between two users (what `chat.rs` stores today).
    UserUser,
    /// A user-created general group.
    UserGroup,
    /// The special group created with a project; carries `project_id`.
    ProjectGroup,
    /// A user talking to an agent through chat (not the agent's own
    /// execution session).
    UserAgent,
    /// Two agents' one-to-one Intent channel (§6.3).
    AgentAgent,
}

impl SessionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionKind::UserUser => "user_user",
            SessionKind::UserGroup => "user_group",
            SessionKind::ProjectGroup => "project_group",
            SessionKind::UserAgent => "user_agent",
            SessionKind::AgentAgent => "agent_agent",
        }
    }
}

/// Identity type of a session participant (design 27 §8.1). The single
/// write entry validates message kinds against it, so the type must be
/// recorded at membership time: a user deleted later must not have the
/// writer of their `user_message` rows retroactively flip to `Agent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantIdentity {
    Human,
    Agent,
}

/// One entry of `ChatSession::participants`.
///
/// Wire format: `{"id": "...", "identity": "human"|"agent"}`, or a bare
/// string for rows written before §8.1 — those carry `identity: None` and
/// skip the type check (membership is still enforced).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Participant {
    pub id: String,
    pub identity: Option<ParticipantIdentity>,
}

impl Participant {
    pub fn new(id: impl Into<String>, identity: ParticipantIdentity) -> Self {
        Self { id: id.into(), identity: Some(identity) }
    }

    /// A participant with no recorded identity type (legacy session rows).
    pub fn untyped(id: impl Into<String>) -> Self {
        Self { id: id.into(), identity: None }
    }

    pub fn is_human(&self) -> bool {
        self.identity == Some(ParticipantIdentity::Human)
    }
}

impl From<&str> for Participant {
    fn from(id: &str) -> Self {
        Participant::untyped(id)
    }
}

impl From<String> for Participant {
    fn from(id: String) -> Self {
        Participant::untyped(id)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum ParticipantWire {
    Legacy(String),
    Typed {
        id: String,
        #[serde(default)]
        identity: Option<ParticipantIdentity>,
    },
}

impl From<ParticipantWire> for Participant {
    fn from(w: ParticipantWire) -> Self {
        match w {
            ParticipantWire::Legacy(id) => Participant::untyped(id),
            ParticipantWire::Typed { id, identity } => Participant { id, identity },
        }
    }
}

impl Serialize for Participant {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.identity {
            None => self.id.serialize(s),
            Some(identity) => ParticipantWire::Typed {
                id: self.id.clone(),
                identity: Some(identity),
            }
            .serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for Participant {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        ParticipantWire::deserialize(d).map(Participant::from)
    }
}

/// Intent-direction metadata every chat message carries (§4). All fields
/// are empty on a `user_message`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentMeta {
    /// Stable message identity (§4 `message_id`). Required on every
    /// durable row: the write entry assigns one when the client does not
    /// submit it (2a.1). A submitted id is the idempotency key of the
    /// write entry and the target of `in_reply_to`.
    #[serde(default)]
    pub message_id: String,
    /// The conversation this message belongs to (§6.5). Requests create
    /// it; responses reuse the request's value.
    #[serde(default)]
    pub conversation_id: String,
    /// The sender's execution session (design 27 `source_agent_session_id`).
    #[serde(default)]
    pub sender_session_id: String,
    /// For a response: the message this answers (§4, system-generated).
    /// Optional since 2a.2 — a 1:1 agent reply points at the human
    /// `user_message` row when the write carries the coordinate, and the
    /// write entry no longer refuses a response without one.
    #[serde(default)]
    pub in_reply_to: String,
    /// The execution session that actually handled the request, once
    /// routing resolved it (`target_agent_session_id`).
    #[serde(default)]
    pub agent_session_id: String,
    /// ADR-100 (superseded): the routing decision stamped on a turn pointer —
    /// `"new"` when the turn starts a fresh task, `"continue"` when it reuses
    /// or rotates an existing one. The ADR-100 revision stopped persisting the
    /// dispatch/start pointers, so nothing writes this any more; the field
    /// stays deserializable so transcripts recorded before the revision still
    /// load with their meta intact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_mode: Option<String>,
    /// Repo the request concerns (§4.1). A chat log records the
    /// normalised remote URL, never a filesystem path.
    #[serde(default)]
    pub repo: String,
    /// Response outcome (§7.1): completed / failed / aborted / timeout.
    #[serde(default)]
    pub status: String,
    /// The agent instance ids this row explicitly @-targets (§5.3.1:
    /// one human utterance is one `user_message` row; the mention
    /// fan-out is a field, not extra rows). Empty = no mentions.
    #[serde(default)]
    pub mentions: Vec<String>,
    /// For a `system_notice`: which notice this is. `system_notice` is
    /// legacy-only now (ADR-100 revision stopped writing routing milestones);
    /// `interaction_resolved` and the older dispatch/start/fallback values
    /// remain readable so old transcripts still project their cards correctly.
    #[serde(default)]
    pub notice_type: String,
    /// For an `interaction_request`: the decision payload (§5.5.4).
    /// Required on every card row — a card without a decision to make
    /// is not a card.
    #[serde(default)]
    pub interaction: Option<InteractionPayload>,
    /// ADR-100 D12: the display names of whatever an execution view's
    /// composer handed to the task alongside this message. Labels **only** —
    /// no path, no documentId, nothing the chat side could try to open, since
    /// the execution session is the attachment's only home (design 27 §4.2.1 /
    /// ADR-009 §5). Empty on every other row, including a chat-surface
    /// `user_message` with genuine chat attachments (those are
    /// [`SessionMessage::attachments`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exec_attachments: Vec<ExecAttachmentLabel>,
}

/// One entry of [`IntentMeta::exec_attachments`] (ADR-100 D12): what the item
/// was called on the surface that sent it, plus which arm of the ADR-046
/// `AttachedItem` union it came from. Pure display data — the chat renders it
/// and never resolves it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecAttachmentLabel {
    /// Upload arms send `filename`, workspace arms send `name` (a repo-relative
    /// path is display text there, not a location); an item that sent only a
    /// path is reduced to its last segment by the Gateway.
    pub filename: String,
    /// The ADR-046 item type, quoted from the wire. Empty = the sender did not
    /// say which arm it was.
    #[serde(default)]
    pub kind: String,
}

/// The decision an `interaction_request` card asks a human to make
/// (design 27 §5.5.4). The card carries the summary needed to decide —
/// never tool payload, command line, or internal error detail (§2.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionPayload {
    /// Card family: "ask_question" | "approval" | "pause" | "retry".
    pub kind: String,
    /// The execution-side request id the answer resolves (approval
    /// request id, question message id). The Gateway routes the human
    /// answer back with this coordinate (§5.5.4 answer channel).
    pub request_id: String,
    /// Short summary of what is being decided, for rendering.
    #[serde(default)]
    pub question: String,
    /// Choices the human can pick; empty = free-form / yes-no.
    #[serde(default)]
    pub options: Vec<String>,
}

/// One line of `messages.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMessage {
    /// Session-local monotonic sequence number, assigned by
    /// [`append_message`] (1-based). Never rewritten once durable.
    #[serde(default)]
    pub seq: u64,
    /// Unix seconds, for display. `seq` is the ordering truth (§8).
    pub ts: i64,
    /// Participant id of the sender — a user UUID or an agent instance UUID.
    pub from: String,
    /// One of [`KIND_USER_MESSAGE`] / [`KIND_AGENT_MESSAGE`] /
    /// [`KIND_INTERACTION_REQUEST`] / [`KIND_SYSTEM_NOTICE`] — or a
    /// retired `intent_*` kind on rows written before Wave 2B, which the
    /// read path renders through [`normalize_kind`].
    pub kind: String,
    /// Message text. Intent bodies are the plain message content; routing
    /// facts live in `meta`, not in the text.
    #[serde(default)]
    pub body: String,
    /// Participant id of the receiver. Empty for broadcast kinds
    /// (`user_message` in a group — every participant can see it).
    #[serde(default)]
    pub to: String,
    #[serde(default)]
    pub meta: IntentMeta,
    /// Write-only: attachment ids the sender resolved into this session's
    /// `files/` dir beforehand (§4.2.1). [`append_message`] replaces them
    /// with the on-disk metadata in [`SessionMessage::attachments`] — the
    /// client never supplies name, mime or size.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachment_ids: Vec<String>,
    /// Resolved attachment metadata, durable part of the row. Only
    /// `user_message` / `agent_message` rows may carry attachments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<crate::chat::Attachment>,
}

/// The Chat surface's last `session_config` pick, durable on the session
/// (design 27 §4.5). The initiator chooses model / provider / workspace /
/// reasoning-effort on the Chat surface before sending; the pick must
/// survive reloads and devices, so it lives on the server-side session
/// record — the frontend copy is a cache of this row, never an independent
/// state. Plain strings: the Runtime clamps each value against the target
/// agent at execution-session creation, so an out-of-range pick here
/// degrades there, not at write time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SeedConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

impl SeedConfig {
    /// Trim every field, map empty strings to `None`, and collapse an
    /// all-`None` config to `None` — a draft with no opinion is stored as
    /// no draft, so `GET` responses do not distinguish "empty" from
    /// "unset" (design 27 §4.5: absent = the agent's own defaults).
    pub fn normalize(self) -> Option<SeedConfig> {
        let clean = |v: Option<String>| {
            v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty() && s.len() <= 256)
        };
        let out = SeedConfig {
            model: clean(self.model),
            provider: clean(self.provider),
            workspace: clean(self.workspace),
            reasoning_effort: clean(self.reasoning_effort),
        };
        if out.model.is_none()
            && out.provider.is_none()
            && out.workspace.is_none()
            && out.reasoning_effort.is_none()
        {
            None
        } else {
            Some(out)
        }
    }
}

/// `session.json` — session metadata, rewritten atomically on change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatSession {
    /// Session identity (UUID v4, server-generated).
    pub session_id: String,
    pub kind: SessionKind,
    /// Participants (users and/or agent instances), each carrying its
    /// identity type (§8.1). Deduplicated, order preserved as given.
    /// Rows written before §8.1 deserialize to untyped participants and
    /// skip the writer-type check at the write entry.
    pub participants: Vec<Participant>,
    #[serde(default)]
    pub title: String,
    /// Group owner for a `user_group` (ADR-098 governance). The founder is
    /// the first owner; ownership moves only via [`transfer_owner`]. `None`
    /// for every other kind and for sessions written before the field
    /// existed, so a legacy row deserialises without an owner rather than
    /// failing to load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// The Chat surface's last seed pick, persisted server-side (design 27
    /// §4.5). `None` = the user never chose one on this pair's surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_config: Option<SeedConfig>,
    /// Project binding for `project_group` sessions — the idempotency key
    /// for "one project, exactly one group chat".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Normalised git remote URL bound to this session (design 27 §4.5.1:
    /// a project carries 0 or 1 repos). The Gateway injects it into every
    /// Intent delivered out of a project group chat; nothing else reads
    /// or enforces it here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub created_at: i64,
    pub last_active_at: i64,
    /// Derived physical-line counter of `messages.jsonl` (same role as
    /// `Conversation::message_count` in `chat.rs`: read-path total and
    /// next `seq` come from it; recomputed from the file when it drifts).
    #[serde(default)]
    pub message_count: u64,
    /// `participant_id -> unread count` (design 27 §4.6, plan 3.6 — the
    /// same `BTreeMap` scheme as `chat.rs` `Conversation::unread`). A new
    /// message increments every participant except the sender; a read
    /// clears the reader's own entry. Agents are participants too, though
    /// the Runtime reads through tools that clear as it goes.
    #[serde(default)]
    pub unread: std::collections::BTreeMap<String, u32>,
    /// Last-message preview shown in the sidebar rows. Write-time derived
    /// state: [`commit_row`] recomputes it on every append and persists it
    /// in `session.json`, so the list surfaces never touch
    /// `messages.jsonl` to derive it. Sessions written before the field
    /// existed self-heal once through [`ensure_preview`].
    #[serde(default)]
    pub last_message_preview: Option<String>,
}

// ── Paths ─────────────────────────────────────────────────────────────

pub fn session_dir(data_dir: &Path, session_id: &str) -> PathBuf {
    data_dir.join("chat-sessions").join(session_id)
}

fn session_path(dir: &Path) -> PathBuf {
    dir.join("session.json")
}

fn messages_path(dir: &Path) -> PathBuf {
    dir.join("messages.jsonl")
}

fn projects_index_path(data_dir: &Path) -> PathBuf {
    data_dir.join("chat-sessions.projects.json")
}

/// One mutex per session directory, created on first use (mirrors
/// `chat::thread_lock`).
static SESSION_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

use std::collections::HashMap;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn session_lock(dir: &Path) -> Arc<Mutex<()>> {
    let mut reg = lock(&SESSION_LOCKS);
    let e = reg
        .entry(dir.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())));
    Arc::clone(e)
}

// ── Session lifecycle ─────────────────────────────────────────────────

/// Create a chat session.
///
/// `project_group` sessions are idempotent on `project_id`: a second create
/// for the same project returns the existing session (design 27 §4.4 /
/// Phase 2). Other kinds are created as given; duplicate participant ids are
/// rejected, not silently deduped — a caller that passes the same id twice
/// is confused about the membership it asked for.
pub fn create_session(
    data_dir: &Path,
    kind: SessionKind,
    participants: &[Participant],
    title: &str,
    owner: Option<&str>,
    project_id: Option<&str>,
    now: i64,
) -> Result<ChatSession, String> {
    if participants.is_empty() {
        return Err("a chat session needs at least one participant".to_string());
    }
    let unique: HashSet<&str> = participants.iter().map(|p| p.id.as_str()).collect();
    if unique.len() != participants.len() {
        return Err("duplicate participant id in session membership".to_string());
    }
    if participants.iter().any(|p| p.id.is_empty() || p.id.contains('/') || p.id.contains('\\')) {
        return Err("participant id is not a valid identifier".to_string());
    }
    if kind == SessionKind::ProjectGroup && project_id.map(str::trim).unwrap_or("").is_empty() {
        return Err("a project_group session needs a project_id".to_string());
    }

    if kind == SessionKind::ProjectGroup {
        let pid = project_id.unwrap().to_string();
        let mut index = read_projects_index(data_dir);
        // Index row pointing at a deleted session falls through and
        // recreates, re-pointing the row below.
        if let Some(existing_id) = index.get(&pid)
            && let Ok(s) = load(data_dir, existing_id)
        {
            return Ok(s);
        }
        let s = write_new(data_dir, kind, participants, title, owner, Some(&pid), now)?;
        index.insert(pid, s.session_id.clone());
        write_projects_index(data_dir, &index)?;
        return Ok(s);
    }
    write_new(data_dir, kind, participants, title, owner, None, now)
}

fn write_new(
    data_dir: &Path,
    kind: SessionKind,
    participants: &[Participant],
    title: &str,
    owner: Option<&str>,
    project_id: Option<&str>,
    now: i64,
) -> Result<ChatSession, String> {
    // §8.1: a user_agent channel is exactly one human + one agent. The
    // check lives here (not in `create_session`) because `ensure_pair`
    // reaches session creation by a second path and must not bypass it.
    if kind == SessionKind::UserAgent
        && (participants.len() != 2
            || participants.iter().filter(|p| p.is_human()).count() != 1)
    {
        return Err("a user_agent session is exactly one human and one agent".to_string());
    }
    let session_id = uuid::Uuid::new_v4().to_string();
    let dir = session_dir(data_dir, &session_id);
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let s = ChatSession {
        session_id,
        kind,
        participants: participants.to_vec(),
        title: title.to_string(),
        owner: owner.map(str::to_string),
        session_config: None,
        project_id: project_id.map(str::to_string),
        repo: None,
        created_at: now,
        last_active_at: now,
        message_count: 0,
        unread: Default::default(),
        last_message_preview: None,
    };
    save(&dir, &s)?;
    // Index after persist: a crash between the two leaves the session
    // invisible to `list_for` (same window class as the pairs index).
    reindex(data_dir, &s.session_id, None, &s.participants)?;
    Ok(s)
}

/// Load `session.json`.
pub fn load(data_dir: &Path, session_id: &str) -> Result<ChatSession, String> {
    validate_session_id(session_id)?;
    let path = session_path(&session_dir(data_dir, session_id));
    let raw = fs::read_to_string(&path).map_err(|e| {
        // A missing session is the hidden-404 case on every surface; the
        // OS error string must not leak the data-dir path, and its
        // platform-specific wording must not fall through to 422 either.
        if e.kind() == std::io::ErrorKind::NotFound {
            "chat session not found".to_string()
        } else {
            format!("{}: {e}", path.display())
        }
    })?;
    serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))
}

fn validate_session_id(id: &str) -> Result<(), String> {
    // Session ids are server-generated UUIDs; anything else is either a
    // client mistake or a path-traversal attempt.
    uuid::Uuid::parse_str(id).map_err(|_| format!("invalid session_id: {id}"))?;
    Ok(())
}

// ── The single write entry (§8) ───────────────────────────────────────

/// Append one message — the ONLY writer path for chat history.
///
/// Guarantees (design 27 §4.6):
/// 1. the sender must be a session participant (participant relation is the
///    permission, not the session id itself — §5.3), and the message kind
///    must match the participant's identity type (§8.1);
/// 2. `message_id` idempotency: a repeat returns the stored message with
///    `already_written = true` and appends nothing;
/// 3. `seq` is assigned in acceptance order, monotonic per session;
/// 4. the append is durable (JSONL) before the metadata is updated.
///
/// Returns the stored message (with its assigned `seq`) and whether this
/// call was the one that wrote it.
pub fn append_message(
    data_dir: &Path,
    session_id: &str,
    msg: &SessionMessage,
    now: i64,
) -> Result<(SessionMessage, bool), String> {
    validate_session_id(session_id)?;
    if !KINDS.contains(&msg.kind.as_str()) {
        return Err(format!("unknown chat message kind: {}", msg.kind));
    }
    if msg.kind == KIND_USER_MESSAGE
        && msg.body.trim().is_empty()
        && msg.attachment_ids.is_empty()
        // ADR-100 D12: an execution view's composer may hand the task a file
        // and say nothing. The bytes stay on the execution side — the chat row
        // carries only their display names — so those labels *are* this row's
        // content, and they count as content here. Refusing it would drop the
        // head of a turn the execution surface accepts, which is the one
        // outcome a transcript must never produce.
        && msg.meta.exec_attachments.is_empty()
    {
        return Err("a user_message needs a body or an attachment".to_string());
    }
    // §4.2.1 (plan 3.5): attachments belong to speech rows only — a card
    // or notice carrying them is a producer bug.
    if !msg.attachment_ids.is_empty()
        && msg.kind != KIND_USER_MESSAGE
        && msg.kind != KIND_AGENT_MESSAGE
    {
        return Err(format!("{} rows cannot carry attachments", msg.kind));
    }

    let dir = session_dir(data_dir, session_id);
    let tlock = session_lock(&dir);
    let _g = lock(&tlock);

    let mut session = load(data_dir, session_id)?;
    let writer = session
        .participants
        .iter()
        .find(|p| p.id == msg.from)
        .ok_or_else(|| "sender is not a participant of this session".to_string())?;
    // §8.1: the message kind must match the writer's identity type —
    // validation lives at the single write entry, not at the callers.
    // Wave 2B vocabulary (§4.2): `agent_message` / `interaction_request`
    // are agent speech (a human writing them is a producer bug, not a
    // variant); `system_notice` is written by the system on an agent's
    // behalf and carries no identity constraint beyond membership.
    // Untyped (legacy) participants skip the type check; membership is
    // still enforced.
    match (msg.kind.as_str(), writer.identity) {
        (KIND_USER_MESSAGE, Some(ParticipantIdentity::Agent)) => {
            return Err("only human participants may write user_message".to_string());
        }
        (KIND_AGENT_MESSAGE | KIND_INTERACTION_REQUEST, Some(ParticipantIdentity::Human)) => {
            return Err(format!("only agent participants may write {}", msg.kind));
        }
        _ => {}
    }
    // §5.5.4 / §5.5.5 payload shape: a card without a decision or a
    // target is not a card; a notice without its type is not a notice.
    // Checked at the single write entry, like every other invariant.
    if msg.kind == KIND_INTERACTION_REQUEST {
        if msg.meta.interaction.is_none() {
            return Err("an interaction_request must carry meta.interaction".to_string());
        }
        if msg.to.is_empty() {
            return Err("an interaction_request must name its answerable target (to)".to_string());
        }
    }
    if msg.kind == KIND_SYSTEM_NOTICE && msg.meta.notice_type.is_empty() {
        return Err("a system_notice must carry meta.notice_type".to_string());
    }

    // Idempotency: resolve the submitted `message_id` through the line
    // index — O(1), never a scan of the log. `ensure_index` is also what
    // reconciles the derived state with the physical file once per process
    // (crash window: a line durable but the metadata save failed).
    let index = ensure_index(&dir)?;
    if !msg.meta.message_id.is_empty()
        && let Some(existing) = lookup_message(&dir, &mut lock(&index), &msg.meta.message_id)?
    {
        return Ok((existing, false));
    }

    // §4.2 (Wave 2a.1): every durable row carries a `message_id`. The
    // client may submit one (the Gateway's delivery id — the idempotency
    // and correlation key); when it does not, the write entry assigns a
    // fresh one so a `user_message` can be pointed at by `in_reply_to`.
    let message_id = if msg.meta.message_id.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        msg.meta.message_id.clone()
    };

    // Reconcile the counter before allocating a seq (crash window: a line
    // durable but metadata save failed must not hand out a used seq).
    // `ensure_index` just brought the index in sync with the log, so
    // `spans.len()` is the physical line count.
    let physical = lock(&index).spans.len() as u64;
    if session.message_count < physical {
        session.message_count = physical;
    }
    let seq = session.message_count + 1;

    // §4.2.1: ids are the only client-supplied attachment fact — name, mime
    // and size are re-read from the sidecars, and an id that does not name
    // a file of *this* session's `files/` dir refuses the row.
    let attachments = if msg.attachment_ids.is_empty() {
        Vec::new()
    } else {
        crate::chat::resolve_attachments(&dir, &msg.attachment_ids)?
    };

    let stored = SessionMessage {
        seq,
        ts: if msg.ts == 0 { now } else { msg.ts },
        from: msg.from.clone(),
        kind: msg.kind.clone(),
        body: msg.body.clone(),
        to: msg.to.clone(),
        meta: IntentMeta {
            message_id,
            ..msg.meta.clone()
        },
        attachment_ids: Vec::new(),
        attachments,
    };
    // §4.6 (plan 3.6): every participant except the sender gains one
    // unread. Keyed by id, never by position — same rule as `chat.rs`.
    // Bookkeeping rows are exempt: a bodyless `system_notice` (the
    // `turn_dispatched` / `turn_started` event lines, receipts) is a
    // milestone in the transcript, not new content to catch up on, so it
    // must not inflate the unread badge. Without this guard a round writes
    // two pointer rows and would count double against the human's unread.
    let bookkeeping = stored.kind == KIND_SYSTEM_NOTICE && stored.body.is_empty();
    if !bookkeeping {
        for p in &session.participants {
            if p.id != stored.from {
                *session.unread.entry(p.id.clone()).or_insert(0) += 1;
            }
        }
    }
    session.last_active_at = stored.ts;
    // Durable append + derived counters + `session.json` in one step —
    // the same entry the migration replay uses, so the log, the line
    // index, the preview and the metadata can never drift apart.
    commit_row(&dir, &mut session, &mut lock(&index), &stored)?;
    // Best-effort incremental index of this row into the session's
    // disposable `search.sqlite` (per-session search index). Runs inside
    // the same `session_lock`, so no new global serial point. A failure here
    // must never fail the durable message — the log is the source of truth
    // and the lazy rebuild on next search heals the index.
    {
        let line_pos = lock(&index).spans.len().saturating_sub(1) as i64;
        if let Err(e) = crate::search_index::index_append(&dir, line_pos, &stored) {
            tracing::warn!(session_id, error = %e, "chat search index write failed (will self-heal on next search)");
        }
    }
    // Wave 5 (§6.1): wake every human surface viewing this session. Same
    // content-free doorbell the old chat domain uses (design 25 §8) — the
    // Desktop bridge already forwards it and the chat view re-reads over
    // HTTP. Typed agents are skipped (no listener); legacy untyped ids are
    // notified too — a doorbell to an unknown id is a no-op, a missed one
    // is a stale inbox. Deduped writes returned earlier: the first writer
    // already sent the signal.
    for p in session
        .participants
        .iter()
        .filter(|p| p.identity != Some(ParticipantIdentity::Agent))
    {
        crate::mqtt_publisher::notify_chat_changed(&p.id);
    }
    Ok((stored, true))
}

/// Clear `who`'s unread counter for a session (plan 3.6: "读取后清零").
/// Called by the read path with the authenticated participant; the entry
/// is removed rather than zeroed so `session.json` stays small.
pub fn mark_read(data_dir: &Path, session_id: &str, who: &str) -> Result<(), String> {
    validate_session_id(session_id)?;
    let dir = session_dir(data_dir, session_id);
    let tlock = session_lock(&dir);
    let _g = lock(&tlock);
    let mut session = load(data_dir, session_id)?;    if session.unread.remove(who).is_some() {
        save(&dir, &session)?;
        // Badge clears on every device of `who`, once. Only when the
        // counter actually changed — an unconditional doorbell would make
        // the reader's own refetch→mark_read cycle re-signal forever.
        crate::mqtt_publisher::notify_chat_changed(who);
    }
    Ok(())
}

/// Delete a Chat session (plan 5.2: non-default sessions are deletable).
/// Guards: participants only; the default pair session (the entry the pair
/// index resolves) and a project-bound group (§4.4) refuse — deleting them
/// would only re-create an empty one on next entry and silently lose the
/// history, which is exactly the "no session to type into" problem §6.1
/// rules out.
pub fn delete_session(data_dir: &Path, session_id: &str, caller: &str) -> Result<(), String> {
    validate_session_id(session_id)?;
    let dir = session_dir(data_dir, session_id);
    let tlock = session_lock(&dir);
    let _g = lock(&tlock);
    let session = load(data_dir, session_id)?;
    if !session.participants.iter().any(|p| p.id == caller) {
        // Hidden 404, same rule as the read paths (§5.3): a non-member
        // must not learn the session exists.
        return Err("chat session not found".to_string());
    }
    if session.project_id.is_some() {
        return Err("the project group session cannot be deleted".to_string());
    }
    {
        let _g = lock(&PAIRS_LOCK);
        let index = read_index(data_dir, pairs_index_path(data_dir));
        if index.values().any(|id| id == session_id) {
            return Err("the default pair session cannot be deleted".to_string());
        }
    }
    // Release the disposable search handle BEFORE deleting the directory:
    // SQLite opens its file without `FILE_SHARE_DELETE` on Windows, so a
    // cached `Connection` (created by any append or search in this process)
    // makes `remove_dir_all` fail with a sharing violation. We hold
    // `session_lock`, so the registry's `Arc` is the only strong reference
    // and dropping the entry closes the connection.
    crate::search_index::forget(&dir);
    fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // Drop the derived line index with the directory — the id can never
    // come back (session ids are server UUIDs), so a cached entry would
    // be leaked memory.
    let _ = lock(&LINE_INDEXES).remove(&dir);
    reindex(data_dir, session_id, Some(&session.participants), &[])?;
    Ok(())
}

// ── Attachments (design 27 §4.2.1, plan 3.5) ──────────────────────────
//
// Same on-disk layout as `chat.rs` conversations — `files/` under the
// session directory, blob first, JSON sidecar second, optional thumbnail —
// and the same primitives (this module delegates to `chat::` dir-level
// helpers). The only difference is the directory: attachments belong to
// the Chat session, shared by its participants.

/// Store an attachment in a session's `files/` dir. `from` must be a
/// participant — an attachment that cannot be referenced by a member of
/// this session is just orphaned bytes.
pub fn store_attachment(
    data_dir: &Path,
    session_id: &str,
    from: &str,
    filename: &str,
    mime: &str,
    bytes: &[u8],
) -> Result<crate::chat::Attachment, String> {
    validate_session_id(session_id)?;
    let session = load(data_dir, session_id)?;
    if !session.participants.iter().any(|p| p.id == from) {
        return Err("sender is not a participant of this session".to_string());
    }
    crate::chat::store_attachment_at(&session_dir(data_dir, session_id), filename, mime, bytes)
}

/// Read one attachment back: metadata + blob path. `Ok(None)` covers both
/// "no such attachment" and "not a well-formed id" — the API answers 404
/// either way, same rule as `chat::load_attachment_meta`.
pub fn load_attachment(
    data_dir: &Path,
    session_id: &str,
    id: &str,
) -> Result<Option<(crate::chat::Attachment, PathBuf)>, String> {
    validate_session_id(session_id)?;
    crate::chat::load_attachment_meta_at(&session_dir(data_dir, session_id), id)
}

/// Whether `id` addresses an attachment of this session (so the API can
/// answer a made-up id with 4xx instead of a server fault).
pub fn attachment_exists(data_dir: &Path, session_id: &str, id: &str) -> bool {
    validate_session_id(session_id).is_ok()
        && crate::chat::attachment_exists_at(&session_dir(data_dir, session_id), id)
}

/// Thumbnail sidecar path for `id` (`thumb` = "jpg" | "txt"), if requested
/// and well-formed. Closed extension set — nothing from the query reaches
/// the filesystem unchecked.
pub fn thumb_sidecar(
    data_dir: &Path,
    session_id: &str,
    id: &str,
    thumb: Option<&str>,
) -> Option<PathBuf> {
    validate_session_id(session_id).ok()?;
    let ext = match thumb? {
        "jpg" => "jpg",
        "txt" => "txt",
        _ => return None,
    };
    if uuid::Uuid::parse_str(id).is_err() {
        return None;
    }
    Some(crate::chat::attachment_thumb_path(
        &session_dir(data_dir, session_id),
        id,
        ext,
    ))
}

// ── System paths (Gateway-mediated, design 27 §4.6) ────────────────────

/// Bind (or clear) the repo of a session (design 27 §4.5.1). Called by the
/// Gateway when a project's repo is set; the repo is then injected into
/// every Intent delivered out of this session.
pub fn set_repo(data_dir: &Path, session_id: &str, repo: Option<&str>) -> Result<ChatSession, String> {
    validate_session_id(session_id)?;
    if let Some(r) = repo
        && (r.trim().is_empty() || r.len() > 512)
    {
        return Err("repo must be a non-empty git URL (<= 512 chars)".to_string());
    }
    let dir = session_dir(data_dir, session_id);
    let tlock = session_lock(&dir);
    let _g = lock(&tlock);
    let mut session = load(data_dir, session_id)?;
    session.repo = repo.map(str::to_string);
    save(&dir, &session)?;
    Ok(session)
}

/// Store the Chat surface's seed draft for this session (design 27 §4.5).
/// Whole-record replace: the request carries the complete draft, an
/// absent/empty field clears it ("no opinion" — the agent's defaults
/// apply to the next turn). Participation is the permission, same rule as
/// the message write entry; a `None` result after normalization clears
/// the draft entirely.
pub fn set_session_config(
    data_dir: &Path,
    session_id: &str,
    who: &str,
    cfg: SeedConfig,
) -> Result<ChatSession, String> {
    validate_session_id(session_id)?;
    let dir = session_dir(data_dir, session_id);
    let tlock = session_lock(&dir);
    let _g = lock(&tlock);
    let mut session = load(data_dir, session_id)?;
    if !session.participants.iter().any(|p| p.id == who) {
        // Established refusal vocabulary — the HTTP layer maps this to the
        // same hidden 404 as a missing session (§5.3).
        return Err("not_a_participant".to_string());
    }
    session.session_config = cfg.normalize();
    save(&dir, &session)?;
    Ok(session)
}

/// Rename a `user_group` (ADR-098 governance). The permission is ownership:
/// only the current owner or an admin may rename. Checked on the locked
/// snapshot so a concurrent transfer cannot flip the gate mid-write. The
/// caller (route) has already hidden non-participants with the `not_a_participant`
/// 404 vocabulary; a member who is neither owner nor admin gets the
/// `not_authorized` string the route maps to 403.
pub fn rename_session(
    data_dir: &Path,
    session_id: &str,
    new_title: &str,
    who: &str,
    is_admin: bool,
) -> Result<ChatSession, String> {
    validate_session_id(session_id)?;
    let title = new_title.trim().to_string();
    if title.is_empty() {
        return Err("a group name cannot be empty".to_string());
    }
    let dir = session_dir(data_dir, session_id);
    let tlock = session_lock(&dir);
    let _g = lock(&tlock);
    let mut session = load(data_dir, session_id)?;
    if !session.participants.iter().any(|p| p.id == who) {
        return Err("not_a_participant".to_string());
    }
    if session.kind != SessionKind::UserGroup {
        return Err("only user_group sessions are user-managed".to_string());
    }
    if !(is_admin || session.owner.as_deref() == Some(who)) {
        return Err("not_authorized".to_string());
    }
    session.title = title;
    save(&dir, &session)?;
    Ok(session)
}

/// Transfer ownership of a `user_group` to another member (ADR-098). Only the
/// current owner or an admin may transfer, and the target must be a *human*
/// participant — an agent cannot own a group, and a non-member cannot be made
/// owner (invite first). The new owner stays a permanent member until they
/// transfer again (see [`remove_participant`] with `protect_owner`).
pub fn transfer_owner(
    data_dir: &Path,
    session_id: &str,
    new_owner: &str,
    who: &str,
    is_admin: bool,
) -> Result<ChatSession, String> {
    validate_session_id(session_id)?;
    let dir = session_dir(data_dir, session_id);
    let tlock = session_lock(&dir);
    let _g = lock(&tlock);
    let mut session = load(data_dir, session_id)?;
    if !session.participants.iter().any(|p| p.id == who) {
        return Err("not_a_participant".to_string());
    }
    if session.kind != SessionKind::UserGroup {
        return Err("only user_group sessions are user-managed".to_string());
    }
    if !(is_admin || session.owner.as_deref() == Some(who)) {
        return Err("not_authorized".to_string());
    }
    let target = session
        .participants
        .iter()
        .find(|p| p.id == new_owner)
        .ok_or_else(|| "the new owner is not a member of this group".to_string())?;
    if !target.is_human() {
        return Err("an agent cannot own a group".to_string());
    }
    session.owner = Some(new_owner.to_string());
    save(&dir, &session)?;
    Ok(session)
}

/// Pair index file: sorted participant-pair key → agent-agent session id.
fn pairs_index_path(data_dir: &Path) -> PathBuf {
    data_dir.join("chat-sessions.pairs.json")
}

/// The pairs index has its own lock (the per-session locks cover session
/// files only; this map is written outside any single session dir).
static PAIRS_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Idempotently ensure the one-to-one chat session between two
/// participants (design 27 §5.4.3: A–B Intents always land on one shared
/// agent-agent Chat session, created on first use). The pair key is
/// order-insensitive; the kind is `agent_agent` only when both ids look
/// like agent instances is *not* judged here — the caller (Gateway)
/// decides the kind by who the participants are.
///
/// §10.19 (plan 6.3) stale-pair healing: the pair key is derived from the
/// ids handed in by the *live* delivery path, so a reinstalled agent (new
/// instance id, ADR-073) resolves under a key the index never had. The
/// index row for the OLD pair then points at a session whose membership
/// names an instance that no longer exists. When a fresh pair is created
/// and one of its ids is a brand-new session-less agent, any other
/// `user_agent` pair session holding that agent's *predecessor* cannot
/// actually be identified here (instance ids carry no lineage), so the
/// policy is: history never dies (session_id is a server UUID, §4.2),
/// the stale session simply stops being reachable through the pair index
/// and remains in the owner's `list_for` until an operator removes it —
/// the row it would strand is a READ surface only, never a write target.
/// This is the "标记，不静默丢历史" branch of §10.19; a re-bind path
/// requires an agent-reinstall event carrying the old instance id, which
/// the package manager does not emit yet.
pub fn ensure_pair(
    data_dir: &Path,
    a: Participant,
    b: Participant,
    kind: SessionKind,
    now: i64,
) -> Result<ChatSession, String> {
    if a.id == b.id {
        return Err("a pair session needs two distinct participants".to_string());
    }
    if a.id.is_empty() || b.id.is_empty() || a.id.contains('/') || b.id.contains('/') {
        return Err("participant id is not a valid identifier".to_string());
    }
    let key = pair_key(&a.id, &b.id);
    let _g = lock(&PAIRS_LOCK);
    let mut index = read_index(data_dir, pairs_index_path(data_dir));
    if let Some(existing_id) = index.get(&key)
        && let Ok(s) = load(data_dir, existing_id)
    {
        return Ok(s);
    }
    let participants: [Participant; 2] = if a.id < b.id {
        [a, b]
    } else {
        [b, a]
    };
    let s = write_new(data_dir, kind, &participants, "", None, None, now)?;
    index.insert(key, s.session_id.clone());
    write_index(data_dir, &index, pairs_index_path(data_dir))?;
    Ok(s)
}

/// The order-insensitive pair index key for two participant ids.
fn pair_key(a: &str, b: &str) -> String {
    if a < b {
        format!("{a}\u{1f}{b}")
    } else {
        format!("{b}\u{1f}{a}")
    }
}

/// Resolve the pair session for two participants **without creating it**.
/// The legacy-surface adapter (`chat_api`) uses this on read paths, where
/// a miss must be a 404 rather than an empty session minted by a GET.
pub fn pair_lookup(data_dir: &Path, a: &str, b: &str) -> Option<ChatSession> {
    let _g = lock(&PAIRS_LOCK);
    let index = read_index(data_dir, pairs_index_path(data_dir));
    let id = index.get(&pair_key(a, b))?;
    load(data_dir, id).ok()
}

// ── Legacy layout migration (plan Wave 6.2) ───────────────────────────
//
// The old `chat.rs` store kept human↔human conversations in
// `users/{lo}/chats/{hi}/` (ADR-084 layout). Wave 6.2 converges the chat
// domain onto this module as the single authoritative store: the adapter
// routes (`/api/users/{id}/chats*`) read and write `UserUser` pair
// sessions, and the directories below are folded in once at startup,
// before the router serves.
//
// Idempotency is per row, not per run: every migrated line carries a
// deterministic `message_id` (`migrated:{pair}:{physical line}`), so a
// crash between the row copy and the staging-directory removal replays
// without duplicating anything. `ensure_pair` is the pair-index write, so
// the wire `chat_id` resolves to the migrated session from the first
// commit point onward.

/// Counts of one migration pass (for the startup log line).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MigrationReport {
    /// Legacy pair directories folded into `UserUser` sessions.
    pub pairs: usize,
    /// Message rows copied (deterministic-id dups skipped are not counted).
    pub messages: usize,
}

/// The subset of the old `conversation.json` the migration needs. Fields
/// not read here are deliberately absent: the new store recomputes its
/// own counters, and `version` / `last_message_preview` have no successor
/// (preview is derived from the last row on read).
#[derive(Deserialize)]
struct LegacyConversation {
    #[serde(default)]
    created_at: i64,
    #[serde(default)]
    last_active_at: i64,
    #[serde(default)]
    unread: std::collections::BTreeMap<String, u32>,
}

/// Fold every legacy `users/{lo}/chats/{hi}/` directory into a `UserUser`
/// pair session. Runs single-threaded at startup; no doorbell is fired
/// (history rows are not news).
pub fn migrate_legacy(data_dir: &Path) -> Result<MigrationReport, String> {
    let users_root = data_dir.join("users");
    if !users_root.is_dir() {
        return Ok(MigrationReport::default());
    }
    let mut report = MigrationReport::default();
    for lo_entry in fs::read_dir(&users_root).map_err(|e| format!("{users_root:?}: {e}"))? {
        let lo_dir = lo_entry.map_err(|e| e.to_string())?.path();
        let Some(lo) = lo_dir.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        let chats_root = lo_dir.join("chats");
        if !chats_root.is_dir() {
            continue; // not a user-id directory (foreign layout) — leave it
        }
        for hi_entry in fs::read_dir(&chats_root).map_err(|e| format!("{chats_root:?}: {e}"))? {
            let dir = hi_entry.map_err(|e| e.to_string())?.path();
            let Some(raw_name) = dir.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            if raw_name.ends_with(".migrated") || raw_name.starts_with('.') {
                continue;
            }
            // Stage first: the rename is the commit point that makes a
            // crash replayable (the original is never mutated in place).
            let staged = if raw_name.ends_with(".migrating") {
                dir.clone()
            } else {
                let to = chats_root.join(format!("{raw_name}.migrating"));
                if fs::rename(&dir, &to).is_err() {
                    continue; // vanished or already staged by a racing run
                }
                to
            };
            let hi = raw_name
                .trim_end_matches(".migrating")
                .to_string();
            match migrate_one_pair(data_dir, &staged, &lo, &hi) {
                Ok(n) => {
                    report.pairs += 1;
                    report.messages += n;
                    if fs::remove_dir_all(&staged).is_err() {
                        let _ = fs::rename(&staged, chats_root.join(format!("{hi}.migrated")));
                    }
                }
                Err(e) => {
                    tracing::error!(pair = %format!("{lo}__{hi}"), error = %e, "legacy chat migration failed; directory kept for retry");
                }
            }
        }
    }
    Ok(report)
}

/// Convert one staged directory into the pair session. Returns the number
/// of rows appended.
fn migrate_one_pair(data_dir: &Path, staged: &Path, lo: &str, hi: &str) -> Result<usize, String> {
    if lo == hi || lo.is_empty() || hi.is_empty() {
        return Err("degenerate pair directory".to_string());
    }
    let convo: LegacyConversation = fs::read_to_string(staged.join("conversation.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| LegacyConversation {
            created_at: 0,
            last_active_at: 0,
            unread: Default::default(),
        });
    let session = ensure_pair(
        data_dir,
        Participant::new(lo, ParticipantIdentity::Human),
        Participant::new(hi, ParticipantIdentity::Human),
        SessionKind::UserUser,
        convo.created_at,
    )?;
    let sess_dir = session_dir(data_dir, &session.session_id);

    // Copy the attachment directory first: rows reference sidecars by id,
    // and an id with no blob would render as a dead chip.
    let src_files = staged.join("files");
    if src_files.is_dir() {
        let dst_files = sess_dir.join("files");
        fs::create_dir_all(&dst_files).map_err(|e| format!("{dst_files:?}: {e}"))?;
        for entry in fs::read_dir(&src_files).map_err(|e| e.to_string())? {
            let p = entry.map_err(|e| e.to_string())?.path();
            if p.is_file() {
                let name = p.file_name().ok_or_else(|| "no file name".to_string())?;
                fs::copy(&p, dst_files.join(name)).map_err(|e| format!("{p:?}: {e}"))?;
            }
        }
    }

    // Replay the message log. Old rows carry the `ChatMessage` shape (inline
    // resolved attachments, `kind` in the text/image/document vocabulary);
    // human↔human rows are all speech, so they land as `user_message` with
    // the attachment metadata kept on the row exactly as `append_message`
    // would have resolved it. Reading the legacy source whole is fine —
    // this is the one-time startup migration, not a request path.
    let old_lines = fs::read_to_string(staged.join("messages.jsonl")).unwrap_or_default();
    let tlock = session_lock(&sess_dir);
    let _g = lock(&tlock);
    let mut session = load(data_dir, &session.session_id)?;
    let index = ensure_index(&sess_dir)?;
    let mut appended = 0usize;
    for (i, line) in old_lines.lines().filter(|l| !l.trim().is_empty()).enumerate() {
        let m: crate::chat::ChatMessage = match serde_json::from_str(line) {
            Ok(m) => m,
            Err(_) => continue, // corrupt line: occupies its number, migrates nothing
        };
        if m.from != lo && m.from != hi {
            continue; // sender not in the pair: refuse to forge attribution
        }
        let message_id = format!("migrated:{lo}{}{hi}:{i}", crate::chat::SEP);
        {
            let idx = lock(&index);
            if idx.ids.contains_key(&message_id) {
                continue; // crash-replay: the deterministic id is already durable
            }
            // Counter reconciliation before allocating a seq, same rule as
            // `append_message`.
            let physical = idx.spans.len() as u64;
            if session.message_count < physical {
                session.message_count = physical;
            }
        }
        let seq = session.message_count + 1;
        let row = SessionMessage {
            seq,
            ts: m.ts,
            from: m.from.clone(),
            kind: KIND_USER_MESSAGE.to_string(),
            body: m.body.clone(),
            to: String::new(),
            meta: IntentMeta {
                message_id,
                ..Default::default()
            },
            attachment_ids: Vec::new(),
            attachments: m.attachments.clone(),
        };
        if m.ts > session.last_active_at {
            session.last_active_at = m.ts;
        }
        // Same durable entry the live write path uses: log line, line
        // index, preview, counter — one step, one `session.json` write.
        commit_row(&sess_dir, &mut session, &mut lock(&index), &row)?;
        appended += 1;
    }
    // The sidecar unread map is the last durable badge state; adopt it only
    // when the session has none (a replay must not resurrect cleared badges).
    if session.unread.is_empty() && !convo.unread.is_empty() {
        session.unread = convo.unread.clone();
    }
    if session.created_at == 0 && convo.created_at > 0 {
        session.created_at = convo.created_at;
    }
    if convo.last_active_at > session.last_active_at {
        session.last_active_at = convo.last_active_at;
    }
    save(&sess_dir, &session)?;
    Ok(appended)
}

fn read_index(data_dir: &Path, path: PathBuf) -> HashMap<String, String> {
    let _ = data_dir;
    fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_index(
    data_dir: &Path,
    index: &HashMap<String, String>,
    path: PathBuf,
) -> Result<(), String> {
    let _ = data_dir;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let json = serde_json::to_string(index).map_err(|e| format!("serialize index: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &json).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Fetch one durable row by its stable `message_id` (design 27 §5.5.4 answer
/// channel: resolving the `interaction_request` card a human's
/// `in_reply_to` points at). Resolved through the line index — one seek,
/// never a scan. `Ok(None)` when no row carries the id.
pub fn message_by_id(
    data_dir: &Path,
    session_id: &str,
    message_id: &str,
) -> Result<Option<SessionMessage>, String> {
    validate_session_id(session_id)?;
    let dir = session_dir(data_dir, session_id);
    let tlock = session_lock(&dir);
    let _g = lock(&tlock);
    let index = ensure_index(&dir)?;
    lookup_message(&dir, &mut lock(&index), message_id)
}

// ── The line index (no-whole-log reads) ───────────────────────────────
//
// Paging, `seq` allocation, idempotency and fetch-by-id all resolve
// against this derived state instead of the log body. One entry per
// non-blank physical line: its byte span, plus a `message_id` lookup.
// Rebuilt from the log on first touch per process, maintained
// incrementally after; the `lines.index` sidecar (one `offset\tlen\tid`
// row per newline-terminated line) keeps a restart from paying the
// rebuild. A half-written trailing line (crash mid-append) is indexed in
// memory for counter parity but never persisted.

/// Byte span of one physical line in `messages.jsonl` (`len` excludes
/// the terminating newline).
#[derive(Debug, Clone, Copy)]
struct LineSpan {
    offset: u64,
    len: u64,
}

#[derive(Debug, Default)]
struct LineIndex {
    /// Every non-blank physical line, in file order. When `last_partial`
    /// is set, the final entry is the unterminated tail of a crashed
    /// append (in memory only, never in the sidecar).
    spans: Vec<LineSpan>,
    /// `message_id` of each indexed line, parallel to `spans` (empty for
    /// pre-Wave-2a.1 rows and for lines whose JSON does not parse).
    line_ids: Vec<Option<String>>,
    /// `message_id` → position in `spans` (stale positions from a popped
    /// partial line are guarded at lookup, never cleaned).
    ids: HashMap<String, u32>,
    last_partial: bool,
    /// The sidecar has been read in this process.
    loaded: bool,
}

static LINE_INDEXES: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<LineIndex>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn line_index_path(dir: &Path) -> PathBuf {
    dir.join("lines.index")
}

/// First touch for a session: load the sidecar and reconcile it against
/// the log's current length. The reconciliation reads only the bytes
/// appended since the last scan; a full pass over the log happens only
/// when the sidecar is missing or lying (the file shrank under it) — the
/// one sanctioned whole-log read, once per session per process. Callers
/// hold the per-session lock so the index and the log never interleave.
fn ensure_index(dir: &Path) -> Result<Arc<Mutex<LineIndex>>, String> {
    let arc = {
        let mut reg = lock(&LINE_INDEXES);
        reg.entry(dir.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(LineIndex::default())))
            .clone()
    };
    let mut idx = lock(&arc);
    if !idx.loaded {
        load_sidecar(dir, &mut idx);
        idx.loaded = true;
    }
    let path = messages_path(dir);
    let file_len = match fs::metadata(&path) {
        Ok(m) => m.len(),
        // A brand-new session legitimately has no log yet. Any *other*
        // metadata error must not be read as "empty file": a transient error
        // treated as length 0 would take the shrink branch below and rewrite an
        // empty sidecar, permanently wiping the idempotency id index.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let covered = covered_end(&idx);
    if covered > file_len {
        // The log shrank under the sidecar (crash mid-line, truncation,
        // copied directory): self-heal with one full pass.
        idx.spans.clear();
        idx.line_ids.clear();
        idx.ids.clear();
        idx.last_partial = false;
        scan_range(dir, &mut idx, 0, file_len, false)?;
        rewrite_sidecar(dir, &idx)?;
    } else if covered < file_len {
        if idx.last_partial {
            // The half-written line has since been completed (or extended)
            // on disk: drop the provisional entry and re-index from its
            // start.
            idx.spans.pop();
            idx.line_ids.pop();
            idx.last_partial = false;
        }
        let resume = covered_end(&idx);
        scan_range(dir, &mut idx, resume, file_len, true)?;
    }
    drop(idx);
    Ok(arc)
}

/// First byte not covered by the index. With `last_partial` the covered
/// end is the partial line's start — it must be re-scanned, its newline
/// may have arrived.
fn covered_end(idx: &LineIndex) -> u64 {
    match idx.spans.last() {
        Some(last) if !idx.last_partial => last.offset + last.len + 1,
        Some(last) => last.offset,
        None => 0,
    }
}

fn load_sidecar(dir: &Path, idx: &mut LineIndex) {
    let Ok(raw) = fs::read_to_string(line_index_path(dir)) else {
        return; // pre-index data dir: the reconcile below rebuilds once
    };
    for line in raw.lines() {
        let mut cols = line.splitn(3, '\t');
        let offset = cols.next().and_then(|v| v.parse::<u64>().ok());
        let len = cols.next().and_then(|v| v.parse::<u64>().ok());
        let (Some(offset), Some(len)) = (offset, len) else {
            // A malformed sidecar row means the whole file is untrusted:
            // drop it and let the reconcile rebuild from the log.
            idx.spans.clear();
            idx.line_ids.clear();
            idx.ids.clear();
            return;
        };
        push_entry(idx, LineSpan { offset, len }, cols.next().unwrap_or(""));
    }
}

/// Record one line at the end of the index (sidecar load, scans and
/// commits all enter here, so `spans` / `line_ids` / `ids` stay aligned).
fn push_entry(idx: &mut LineIndex, span: LineSpan, id: &str) {
    let pos = idx.spans.len() as u32;
    idx.spans.push(span);
    if id.is_empty() {
        idx.line_ids.push(None);
    } else {
        idx.line_ids.push(Some(id.to_string()));
        idx.ids.insert(id.to_string(), pos);
    }
}

/// Index every non-blank line whose bytes lie in `[from, to)` (`to` must
/// be the current file length). A trailing segment without `\n` becomes
/// the provisional `last_partial` entry and is never persisted. With
/// `persist`, terminated entries are appended to the sidecar (the delta
/// reconcile); a full rebuild rewrites the sidecar instead.
fn scan_range(dir: &Path, idx: &mut LineIndex, from: u64, to: u64, persist: bool) -> Result<(), String> {
    idx.last_partial = false;
    if from >= to {
        return Ok(());
    }
    let path = messages_path(dir);
    let mut bytes = vec![0u8; (to - from) as usize];
    {
        use std::io::{Read, Seek, SeekFrom};
        let mut file =
            fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        file.seek(SeekFrom::Start(from))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        file.read_exact(&mut bytes)
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let mut pos = from;
    for seg in bytes.split_inclusive(|b| *b == b'\n') {
        let terminated = seg.last() == Some(&b'\n');
        let body = if terminated { &seg[..seg.len() - 1] } else { seg };
        let text = String::from_utf8_lossy(body).into_owned();
        pos += seg.len() as u64;
        if text.trim().is_empty() {
            continue; // blank line: occupies bytes, not a message number
        }
        let id = serde_json::from_str::<SessionMessage>(&text)
            .ok()
            .map(|m| m.meta.message_id)
            .unwrap_or_default();
        let span = LineSpan {
            offset: pos - seg.len() as u64,
            len: body.len() as u64,
        };
        push_entry(idx, span, &id);
        if terminated {
            if persist {
                sidecar_append(dir, span, &id)?;
            }
        } else {
            idx.last_partial = true;
        }
    }
    Ok(())
}

fn sidecar_append(dir: &Path, span: LineSpan, id: &str) -> Result<(), String> {
    use std::io::Write as _;
    let path = line_index_path(dir);
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    writeln!(f, "{}\t{}\t{}", span.offset, span.len, id)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Rewrite the sidecar from the index, skipping any provisional partial
/// tail (only terminated lines are ever persisted).
fn rewrite_sidecar(dir: &Path, idx: &LineIndex) -> Result<(), String> {
    let path = line_index_path(dir);
    let n = idx.spans.len().saturating_sub(if idx.last_partial { 1 } else { 0 });
    let mut buf = String::new();
    for i in 0..n {
        let id = idx.line_ids[i].as_deref().unwrap_or("");
        buf.push_str(&format!("{}\t{}\t{}\n", idx.spans[i].offset, idx.spans[i].len, id));
    }
    fs::write(&path, buf).map_err(|e| format!("{}: {e}", path.display()))
}

/// Read one indexed line. `Ok(None)` covers a vanished span (the log
/// changed under the index since the reconcile — the next
/// `ensure_index` heals it) and a missing log.
fn read_span(path: &Path, span: LineSpan) -> Result<Option<String>, String> {
    use std::io::{Seek, SeekFrom};
    let mut file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    file.seek(SeekFrom::Start(span.offset))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    read_current_line(&mut file, span)
}

/// Read the line at the file handle's current position. Shared by
/// `read_span` and the windowed `read_messages` (one open per page).
fn read_current_line(file: &mut fs::File, span: LineSpan) -> Result<Option<String>, String> {
    use std::io::Read as _;
    let mut buf = vec![0u8; span.len as usize];
    // `File::read` may return fewer bytes than requested (a tail still being
    // appended, a network share, a filter driver); keep filling until the whole
    // span is present, matching the reader on the runtime side. A read that
    // reaches EOF before the span is complete means the log truncated under the
    // index — skip the row.
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) => return Err(e.to_string()),
        }
    }
    if filled < buf.len() {
        return Ok(None); // truncated under the index — skip the row
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

/// Resolve a `message_id` through the index: one seek to the recorded
/// span, never a scan of the log.
fn lookup_message(
    dir: &Path,
    idx: &mut LineIndex,
    message_id: &str,
) -> Result<Option<SessionMessage>, String> {
    let Some(span) = idx
        .ids
        .get(message_id)
        .and_then(|&p| idx.spans.get(p as usize).copied())
    else {
        return Ok(None);
    };
    let Some(raw) = read_span(&messages_path(dir), span)? else {
        return Ok(None);
    };
    // Trust the id→position index only as far as it agrees with the log: if the
    // recorded span parses to a *different* message_id (a stale or copied
    // sidecar), treat it as a miss rather than handing the wrong body to the
    // idempotency gate — the next `ensure_index` heals the drift with one
    // reconcile pass.
    Ok(serde_json::from_str::<SessionMessage>(&raw)
        .ok()
        .filter(|m| m.meta.message_id == message_id))
}

/// Durably commit one message row. This is the ONLY place a chat log
/// grows: the `messages.jsonl` append, the line index and its sidecar,
/// the derived counters in the session record (`message_count`,
/// `last_message_preview`) and the `session.json` rewrite happen in one
/// step under the caller's per-session lock, so the log and its derived
/// state can never drift apart. `last_active_at` and `unread` stay
/// caller-owned — the write entry and the migration replay set them by
/// different rules.
fn commit_row(
    dir: &Path,
    session: &mut ChatSession,
    idx: &mut LineIndex,
    stored: &SessionMessage,
) -> Result<(), String> {
    let path = messages_path(dir);
    let mut line = serde_json::to_string(stored).map_err(|e| format!("serialize message: {e}"))?;
    line.push('\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    // The append offset is the file's size *before* this write. A metadata
    // failure must abort, not default to 0 — a span recorded at offset 0 would
    // collide with line 0 and be persisted into the sidecar as a permanent
    // corruption the next open cannot distinguish from real data.
    let offset = file
        .metadata()
        .map_err(|e| format!("{}: {e}", path.display()))?
        .len();
    file.write_all(line.as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))?;

    if idx.last_partial {
        // The append landed on the half-written line the last scan saw:
        // re-index from that line's start (the merged row is corrupt and
        // unparseable exactly as the whole-log reader left it).
        let from = idx.spans.last().map(|s| s.offset).unwrap_or(0);
        idx.spans.pop();
        idx.line_ids.pop();
        idx.last_partial = false;
        let to = fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?.len();
        scan_range(dir, idx, from, to, true)?;
    } else {
        let span = LineSpan {
            offset,
            len: line.len() as u64 - 1,
        };
        push_entry(idx, span, &stored.meta.message_id);
        sidecar_append(dir, span, &stored.meta.message_id)?;
    }

    session.message_count = stored.seq;
    session.last_message_preview = Some(preview_of_message(stored));
    save(dir, session)
}

// ── Reads ─────────────────────────────────────────────────────────────

/// A page of messages plus the total count. `offset` counts from the
/// newest message backwards (the chat UI reads its tail), `limit` is
/// capped by the caller. The window resolves through the line index and
/// the log is read by byte spans only — a page never costs more than
/// the page.
pub fn read_messages(
    data_dir: &Path,
    session_id: &str,
    offset: usize,
    limit: usize,
) -> Result<(Vec<SessionMessage>, u64), String> {
    validate_session_id(session_id)?;
    let dir = session_dir(data_dir, session_id);
    // Serialize with the write path: the window plan and the log bytes
    // must come from the same state, and `ensure_index` may extend the
    // index from appended bytes.
    let tlock = session_lock(&dir);
    let _g = lock(&tlock);
    let path = messages_path(&dir);
    let index = ensure_index(&dir)?;
    let window: Vec<LineSpan> = {
        let idx = lock(&index);
        let total = idx.spans.len();
        let end = total.saturating_sub(offset);
        let start = end.saturating_sub(limit.max(1));
        idx.spans[start..end].to_vec()
    };
    let total = window_total(&index);
    // A missing log is an empty conversation (same rule as the old
    // whole-file read).
    let mut file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    // Plan D-A/A1: old `intent_*` rows are re-labelled at this single
    // read path (write new, read old). The writer's identity type comes
    // from the session; a session that cannot be read (or a writer not
    // listed in it) reads as the agent side.
    let participants = load(data_dir, session_id).map(|s| s.participants).unwrap_or_default();
    let mut out = Vec::with_capacity(window.len());
    use std::io::{Seek, SeekFrom};
    for span in &window {
        // One corrupt line must not brick the history (same rule as chat.rs).
        file.seek(SeekFrom::Start(span.offset))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let raw = match read_current_line(&mut file, *span) {
            Ok(Some(r)) => r,
            Ok(None) => continue,
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        match serde_json::from_str::<SessionMessage>(&raw) {
            Ok(mut m) => {
                if m.kind == "intent_request" || m.kind == "intent_response" {
                    let human = participants
                        .iter()
                        .any(|p| p.id == m.from && p.identity == Some(ParticipantIdentity::Human));
                    m.kind = normalize_kind(&m.kind, human);
                }
                out.push(m);
            }
            Err(e) => tracing::warn!(
                path = %path.display(), error = %e, "skipping corrupt message line"
            ),
        }
    }
    Ok((out, total))
}

/// The total physical line count behind a paging answer. Kept as a
/// helper so the count is taken under the index lock in one place.
fn window_total(index: &Arc<Mutex<LineIndex>>) -> u64 {
    lock(index).spans.len() as u64
}

/// In-conversation categorized search (WeChat-style 文本 / 图片 / 文件).
/// Reads ONLY the session's disposable `search.sqlite` sidecar — never
/// `messages.jsonl` — so it honors the "no whole-log read on the request
/// path" rule. The sidecar is reconciled with the authoritative line count
/// here (a mismatch triggers the one sanctioned full rebuild).
///
/// `before_line` is a `line_pos` cursor (rows strictly older than it);
/// `None` starts from the newest. Returns the hits plus the total line
/// count so the caller can convert a `line_pos` into a paging offset.
pub fn search_messages(
    data_dir: &Path,
    session_id: &str,
    category: crate::search_index::Category,
    q: &str,
    before_line: Option<i64>,
    limit: usize,
) -> Result<(Vec<crate::search_index::SearchHit>, u64), String> {
    validate_session_id(session_id)?;
    let dir = session_dir(data_dir, session_id);
    // Serialize with the write path: the search index is maintained under
    // the same lock, and `ensure_index` may reconcile the log first.
    let tlock = session_lock(&dir);
    let _g = lock(&tlock);
    let index = ensure_index(&dir)?;
    let total = window_total(&index);
    let conn = crate::search_index::ensure(&dir, total)?;
    let hits = crate::search_index::query(&lock(&conn), category, q, before_line, limit)?;
    Ok((hits, total))
}

/// `last_message_preview` length, in characters — kept identical to the
/// old `chat.rs` one-to-one list preview (ADR-076 §决策 8) so the group
/// list and the one-to-one list truncate their meta line the same way.
const GROUP_PREVIEW_CHARS: usize = 80;

/// The sidebar's last-message preview for a session's tail row: the body, or
/// a kind-appropriate stand-in when the row carries no text (an
/// attachment-only message, an interaction card, a bare system notice).
/// Truncated on a char boundary so multi-byte content cannot panic — the
/// rules the old `chat::preview` applied to the one-to-one list, kept
/// byte-identical on the wire.
fn preview_of_message(msg: &SessionMessage) -> String {
    let source = if !msg.body.trim().is_empty() {
        msg.body.clone()
    } else if let Some(a) = msg.attachments.first() {
        if a.kind() == "image" {
            format!("[image] {}", a.filename)
        } else {
            format!("[file] {}", a.filename)
        }
    } else if msg.kind == KIND_INTERACTION_REQUEST {
        msg.meta
            .interaction
            .as_ref()
            .map(|i| i.question.clone())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let trimmed = source.trim();
    if trimmed.chars().count() <= GROUP_PREVIEW_CHARS {
        return trimmed.to_string();
    }
    let mut s: String = trimmed.chars().take(GROUP_PREVIEW_CHARS).collect();
    s.push('…');
    s
}

/// Ensure a session's stored `last_message_preview` is usable. The
/// preview is write-time derived state ([`commit_row`]), so the normal
/// path is a pure metadata read with no log access at all. A session
/// written before the field existed self-heals here exactly once: one
/// indexed seek to the tail line, preview derived, persisted back into
/// `session.json`. Best-effort — a session whose log cannot be read, or
/// that has no messages, keeps `None` so the sidebar row just shows an
/// empty meta line rather than failing the list.
pub fn ensure_preview(data_dir: &Path, s: &mut ChatSession) {
    if s.last_message_preview.is_some() || s.message_count == 0 {
        return;
    }
    let dir = session_dir(data_dir, &s.session_id);
    // Serialize with the write path: `ensure_index` may extend the index
    // from appended bytes, and the healed preview is persisted below.
    let tlock = session_lock(&dir);
    let _g = lock(&tlock);
    let Ok(index) = ensure_index(&dir) else { return };
    let tail = lock(&index).spans.last().copied();
    let Some(span) = tail else { return };
    let Ok(Some(raw)) = read_span(&messages_path(&dir), span) else {
        return;
    };
    let Ok(m) = serde_json::from_str::<SessionMessage>(&raw) else {
        return; // corrupt tail: same answer as the old paging read — no preview
    };
    let preview = preview_of_message(&m);
    s.last_message_preview = Some(preview.clone());
    if let Ok(mut session) = load(data_dir, &s.session_id) {
        session.last_message_preview = Some(preview);
        let _ = save(&dir, &session);
    }
}

/// Fill `last_message_preview` for every `project_group` session in place.
/// Other kinds are left untouched — their lists derive the preview
/// elsewhere (`chat_api` for one-to-one, both through [`ensure_preview`],
/// which is a no-op once the write path has stored the field).
///
/// Read-path only, so it must be driven from inside an `offload` closure (it
/// touches the filesystem synchronously).
pub fn attach_group_previews(data_dir: &Path, sessions: &mut [ChatSession]) {
    for s in sessions
        .iter_mut()
        .filter(|s| s.kind == SessionKind::ProjectGroup)
    {
        ensure_preview(data_dir, s);
    }
}

// ── Per-participant index (plan §10.12 / Wave 6.1) ───────────────────
//
// `list_for` used to scan every `chat-sessions/*/session.json` on every
// history open — O(all sessions) per request, which the frontend does on
// each agent-page mount. The index file maps participant id → session
// ids, so a list read costs one index read + one file per *own* session.
// Crash semantics match the pairs index: the session file is persisted
// first, the index updated after; a crash between the two leaves the
// session invisible to `list_for` (it still resolves by exact id), and a
// stale index entry is filtered on read by re-checking membership.

static PARTICIPANTS_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn participants_index_path(data_dir: &Path) -> PathBuf {
    data_dir.join("chat-sessions.participants.json")
}

fn read_participants_index(data_dir: &Path) -> HashMap<String, Vec<String>> {
    let path = participants_index_path(data_dir);
    match fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

fn write_participants_index(data_dir: &Path, map: &HashMap<String, Vec<String>>) -> Result<(), String> {
    let path = participants_index_path(data_dir);
    let json = serde_json::to_string_pretty(map).map_err(|e| format!("serialize: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &json).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

/// Apply a membership transition to the index. `before = None` marks a
/// newly created session (everything in `after` is added); a session
/// deletion passes `before = Some(members)` and `after = &[]`.
fn reindex(
    data_dir: &Path,
    session_id: &str,
    before: Option<&[Participant]>,
    after: &[Participant],
) -> Result<(), String> {
    let _g = PARTICIPANTS_LOCK.lock().map_err(|_| "participants index lock poisoned".to_string())?;
    let mut map = read_participants_index(data_dir);
    let before_ids: HashSet<&str> = before
        .map(|b| b.iter().map(|p| p.id.as_str()).collect())
        .unwrap_or_default();
    for p in after {
        if !before_ids.contains(p.id.as_str()) {
            let e = map.entry(p.id.clone()).or_default();
            if !e.iter().any(|s| s == session_id) {
                e.push(session_id.to_string());
            }
        }
    }
    if let Some(before) = before {
        let after_ids: HashSet<&str> = after.iter().map(|p| p.id.as_str()).collect();
        for p in before {
            if !after_ids.contains(p.id.as_str())
                && let Some(v) = map.get_mut(&p.id)
            {
                v.retain(|s| s != session_id);
                if v.is_empty() {
                    map.remove(&p.id);
                }
            }
        }
    }
    write_participants_index(data_dir, &map)
}

/// Full-directory scan. Only used to backfill the index the first time
/// `list_for` runs against a data dir created before the index existed.
fn scan_all_sessions(data_dir: &Path) -> Vec<ChatSession> {
    let root = data_dir.join("chat-sessions");
    let mut out = Vec::new();
    let entries = match fs::read_dir(&root) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path().join("session.json");
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(s) = serde_json::from_str::<ChatSession>(&raw) {
            out.push(s);
        }
    }
    out
}

/// Plan §10.12: the participant's sessions, most recently active first.
/// Served from the per-participant index; a missing index file (pre-6.1
/// data dir) is backfilled by one scan, then never scanned again.
pub fn list_for(data_dir: &Path, participant: &str) -> Result<Vec<ChatSession>, String> {
    let path = participants_index_path(data_dir);
    if !path.exists() {
        let sessions = scan_all_sessions(data_dir);
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for s in &sessions {
            for p in &s.participants {
                map.entry(p.id.clone()).or_default().push(s.session_id.clone());
            }
        }
        write_participants_index(data_dir, &map)?;
        let mut out: Vec<ChatSession> = sessions
            .into_iter()
            .filter(|s| s.participants.iter().any(|p| p.id == participant))
            .collect();
        out.sort_by_key(|s| std::cmp::Reverse(s.last_active_at));
        return Ok(out);
    }
    let ids = read_participants_index(data_dir)
        .get(participant)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        // Load failures (deleted session) and membership drift (crashed
        // reindex) are filtered here — the entry dies on the next write.
        if let Ok(s) = load(data_dir, &id)
            && s.participants.iter().any(|p| p.id == participant)
        {
            out.push(s);
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.last_active_at));
    Ok(out)
}

/// Admin inspection surface (ADR-076 §决策 4): every project group chat,
/// regardless of who created it. The sidebar lists these to an admin so a
/// deployment owner can see all project groups, not only the ones their own
/// account joined. Read is granted project-group-wide in the message path;
/// posting stays gated on participation. Newest activity first.
pub fn list_project_groups(data_dir: &Path) -> Vec<ChatSession> {
    // Served from the project → session index: one index read plus one
    // `session.json` per group, never a directory scan on the request
    // path. A missing index file (pre-projects-index data dir) gets one
    // backfill scan that also writes the index, then never again.
    if !projects_index_path(data_dir).exists() {
        let mut groups: Vec<ChatSession> = scan_all_sessions(data_dir)
            .into_iter()
            .filter(|s| s.kind == SessionKind::ProjectGroup)
            .collect();
        let mut index = read_projects_index(data_dir);
        for g in &groups {
            if let Some(pid) = &g.project_id {
                index.insert(pid.clone(), g.session_id.clone());
            }
        }
        let _ = write_projects_index(data_dir, &index);
        groups.sort_by_key(|s| std::cmp::Reverse(s.last_active_at));
        groups
    } else {
        let index = read_projects_index(data_dir);
        let mut out = Vec::with_capacity(index.len());
        for sid in index.values() {
            // Load failures (deleted session) and kind drift are filtered
            // here — the same stale-row rule as `list_for`.
            if let Ok(s) = load(data_dir, sid)
                && s.kind == SessionKind::ProjectGroup
            {
                out.push(s);
            }
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.last_active_at));
        out
    }
}

/// Design 27 §8.3: sync a project group's participant set to exactly
/// `participants` — ids missing from the session are added, ids absent
/// from the set are removed. Resolves the session through the project
/// index; a project without a session yet creates one (the `ensure_group`
/// case). Removal is the revocation half of §8.3: chat history the former
/// member wrote stays (the log is append-only), but bound execution
/// sessions become unreachable for them on their next scope injection.
pub fn sync_group(
    data_dir: &Path,
    project_id: &str,
    participants: &[Participant],
    now: i64,
) -> Result<ChatSession, String> {
    let existing = read_projects_index(data_dir)
        .get(project_id)
        .cloned()
        .and_then(|id| load(data_dir, &id).ok());
    let s = match existing {
        Some(s) => s,
        None => {
            return create_session(
                data_dir,
                SessionKind::ProjectGroup,
                participants,
                "",
                None,
                Some(project_id),
                now,
            );
        }
    };
    // Same per-session lock the append path holds (§8 atomic RMW): without
    // it a concurrent `append_message` save could be overwritten by this
    // load → mutate → save, losing unread / last_active updates.
    let dir = session_dir(data_dir, &s.session_id);
    let tlock = session_lock(&dir);
    let _g = lock(&tlock);
    let mut s = load(data_dir, &s.session_id)?;
    let want: HashSet<&str> = participants.iter().map(|p| p.id.as_str()).collect();
    let before = s.participants.clone();
    s.participants.retain(|p| want.contains(p.id.as_str()));
    for p in participants {
        if !s.participants.iter().any(|x| x.id == p.id) {
            s.participants.push(p.clone());
        }
    }
    if s.participants != before {
        save(&dir, &s)?;
        reindex(data_dir, &s.session_id, Some(&before), &s.participants)?;
        // §6.1 doorbell (same rule as `append_message`): a participant-set
        // change is a backend-driven refresh — every human surface viewing
        // this group must re-read. The newcomer discovers the chat (composer
        // unlocks, it leaves the read-only state), the still-listed members
        // refresh the header's member count, and a removed member drops it.
        // Notify the new set plus whoever was in `before` but is gone (they
        // are no longer in the new set, so a removal still reaches them);
        // each id fires once. Typed agents are skipped (no listener).
        for p in s
            .participants
            .iter()
            .chain(before.iter().filter(|p| !want.contains(p.id.as_str())))
        {
            if p.identity != Some(ParticipantIdentity::Agent) {
                crate::mqtt_publisher::notify_chat_changed(&p.id);
            }
        }
    }
    Ok(s)
}

/// Design 27 §8.3: add a participant to an existing Chat session.
/// Idempotent — an already-listed id (by identity too) is left as-is and
/// the session is returned unchanged. This is the participant-relation
/// mutation the Gateway mediates when a member joins a project group.
pub fn add_participant(
    data_dir: &Path,
    session_id: &str,
    participant: &Participant,
    now: i64,
) -> Result<ChatSession, String> {
    validate_session_id(session_id)?;
    if participant.id.is_empty() || participant.id.contains('/') {
        return Err("participant id is not a valid identifier".to_string());
    }
    let dir = session_dir(data_dir, session_id);
    let tlock = session_lock(&dir);
    let _g = lock(&tlock);
    let mut s = load(data_dir, session_id)?;
    if s.participants.iter().any(|p| p.id == participant.id) {
        return Ok(s);
    }
    let before = s.participants.clone();
    s.participants.push(participant.clone());
    s.last_active_at = s.last_active_at.max(now);
    save(&dir, &s)?;
    reindex(data_dir, session_id, Some(&before), &s.participants)?;
    Ok(s)
}

/// Design 27 §8.3: remove a participant. Idempotent — an unknown id is a
/// no-op. The asymmetry is deliberate: chat history the removed party
/// wrote stays (the log is append-only), but every bound execution
/// session becomes unreachable for them *immediately*, because the read
/// check consults this very participant list (via the Gateway-injected
/// `x-acowork-chat-scope`), not a materialised grant.
///
/// `min_participants` is the floor the removal must not breach: when the id
/// is present and dropping it would leave fewer than this many members, the
/// whole check-and-remove fails atomically *inside the session lock* (an
/// error the caller maps to 4xx) rather than on a lock-free snapshot — so two
/// concurrent removals can never interleave past the floor. `0` disables the
/// floor (the PM-mediated `/internal` path governs membership by other rules
/// and may shrink a roster below two).
///
/// `protect_owner` makes the current `user_group` owner a permanent member:
/// dropping the id that holds `owner` fails atomically with a distinct error
/// (the caller must transfer ownership first, ADR-098). The `/internal` PM
/// path passes `false` — its roster is synced from the project, whose own
/// owner semantics are enforced there, not here.
///
/// `actor` is the `(who, is_admin)` identity for the governance gate (ADR-098,
/// revised): removing ANOTHER member is owner-or-admin only, while anyone may
/// remove themselves (leave). Checked on the same locked snapshot as the
/// removal, so a concurrent transfer cannot flip the gate mid-write. `None`
/// disables it — the PM-mediated `/internal` path governs membership by the
/// project's own rules and carries no user actor.
pub fn remove_participant(
    data_dir: &Path,
    session_id: &str,
    participant_id: &str,
    min_participants: usize,
    protect_owner: bool,
    actor: Option<(&str, bool)>,
) -> Result<ChatSession, String> {
    validate_session_id(session_id)?;
    let dir = session_dir(data_dir, session_id);
    let tlock = session_lock(&dir);
    let _g = lock(&tlock);
    let mut s = load(data_dir, session_id)?;
    let before = s.participants.clone();
    if !s.participants.iter().any(|p| p.id == participant_id) {
        return Ok(s);
    }
    // Governance before permanence: a member who cannot remove anyone should
    // hear "not allowed" (403), not learn that the target happens to be the
    // owner (422). Self-removal (leave) skips the gate.
    if let Some((who, is_admin)) = actor
        && participant_id != who
        && !(is_admin || s.owner.as_deref() == Some(who))
    {
        return Err("not_authorized".to_string());
    }
    // Owner permanence, checked on the locked snapshot so a concurrent
    // transfer cannot interleave a removal that drops the group's owner.
    if protect_owner
        && s.kind == SessionKind::UserGroup
        && s.owner.as_deref() == Some(participant_id)
    {
        return Err("a group must keep an owner - transfer ownership first".to_string());
    }
    s.participants.retain(|p| p.id != participant_id);
    if s.participants.len() < min_participants {
        return Err("a group must keep at least two members".to_string());
    }
    save(&dir, &s)?;
    reindex(data_dir, session_id, Some(&before), &s.participants)?;
    Ok(s)
}

/// Save `session.json` atomically (tmp + rename).
fn save(dir: &Path, session: &ChatSession) -> Result<(), String> {
    let path = session_path(dir);
    let json =
        serde_json::to_string_pretty(session).map_err(|e| format!("serialize session: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &json).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

// ── Project → session index ───────────────────────────────────────────

fn read_projects_index(data_dir: &Path) -> HashMap<String, String> {
    let path = projects_index_path(data_dir);
    fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_projects_index(data_dir: &Path, index: &HashMap<String, String>) -> Result<(), String> {
    let path = projects_index_path(data_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let json = serde_json::to_string(index).map_err(|e| format!("serialize index: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &json).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-chatsess-{}-{}",
            std::process::id(),
            unique
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn msg(from: &str, kind: &str, body: &str, message_id: &str) -> SessionMessage {
        SessionMessage {
            seq: 0,
            ts: 0,
            from: from.to_string(),
            kind: kind.to_string(),
            body: body.to_string(),
            to: String::new(),
            meta: IntentMeta {
                message_id: message_id.to_string(),
                ..Default::default()
            },
            attachment_ids: Vec::new(),
            attachments: Vec::new(),
        }
    }

    use crate::search_index::Category;

    fn pair_session(dir: &Path) -> ChatSession {
        let human = Participant::new("human-1", ParticipantIdentity::Human);
        let agent = Participant::new("agent-1", ParticipantIdentity::Agent);
        ensure_pair(dir, human, agent, SessionKind::UserAgent, 100).expect("pair")
    }

    #[test]
    fn search_categorizes_text_image_and_file() {
        let dir = temp_dir();
        let s = pair_session(&dir);
        let sid = &s.session_id;
        // Text rows (newest appended last → higher line_pos).
        append_message(&dir, sid, &msg("human-1", KIND_USER_MESSAGE, "hello wide world", ""), 1)
            .unwrap();
        append_message(&dir, sid, &msg("agent-1", KIND_AGENT_MESSAGE, "another note here", ""), 2)
            .unwrap();
        // An image row and a file row (attachments resolved from the dir).
        let img = store_attachment(&dir, sid, "human-1", "shot.png", "image/png", b"fakepng").unwrap();
        let mut im = msg("human-1", KIND_USER_MESSAGE, "see the picture", "");
        im.attachment_ids = vec![img.id.clone()];
        append_message(&dir, sid, &im, 3).unwrap();
        let doc =
            store_attachment(&dir, sid, "human-1", "report.pdf", "application/pdf", b"fakepdf")
                .unwrap();
        let mut dm = msg("human-1", KIND_USER_MESSAGE, "", "");
        dm.attachment_ids = vec![doc.id.clone()];
        append_message(&dir, sid, &dm, 4).unwrap();

        // Text tab: trigram MATCH (>=3 chars) finds the body substring.
        let (hits, total) =
            search_messages(&dir, sid, Category::Text, "wide world", None, 50).unwrap();
        assert_eq!(total, 4, "four physical lines");
        assert_eq!(hits.len(), 1, "one text match");
        assert_eq!(hits[0].line_pos, 0, "first line is the oldest");
        assert!(hits[0].body.contains("wide world"));

        // Short query (< trigram min) falls back to LIKE on the body.
        let (hits, _) = search_messages(&dir, sid, Category::Text, "lo", None, 50).unwrap();
        assert!(hits.iter().any(|h| h.body.contains("hello")), "LIKE fallback matched");

        // Image tab returns only the image-carrying row; file tab only the
        // document row — the two never cross over on mime.
        let (imgs, _) = search_messages(&dir, sid, Category::Image, "", None, 50).unwrap();
        assert_eq!(imgs.len(), 1);
        assert!(imgs[0].attachments.iter().any(|a| a.mime == "image/png"));
        let (files, _) = search_messages(&dir, sid, Category::File, "", None, 50).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].attachments[0].mime, "application/pdf");

        // File tab filename substring query.
        let (files, _) = search_messages(&dir, sid, Category::File, "report", None, 50).unwrap();
        assert_eq!(files.len(), 1);
        let (none, _) = search_messages(&dir, sid, Category::File, "zzz", None, 50).unwrap();
        assert!(none.is_empty());

        // Cursor: only rows strictly older than `before_line`.
        let (older, _) = search_messages(&dir, sid, Category::Text, "", Some(2), 50).unwrap();
        assert!(older.iter().all(|h| h.line_pos < 2), "cursor bounds line_pos");
    }

    #[test]
    fn search_rebuilds_from_log_when_sidecar_missing_or_corrupt() {
        let dir = temp_dir();
        let s = pair_session(&dir);
        let sid = &s.session_id;
        append_message(&dir, sid, &msg("human-1", KIND_USER_MESSAGE, "alpha message", ""), 1)
            .unwrap();
        append_message(&dir, sid, &msg("human-1", KIND_USER_MESSAGE, "beta message", ""), 2)
            .unwrap();

        let (before, _) = search_messages(&dir, sid, Category::Text, "beta", None, 50).unwrap();
        assert_eq!(before.len(), 1);

        // Simulate a corrupt sidecar + a process restart (fresh handle
        // registry): overwrite `search.sqlite` with junk, drop the cached
        // connection, append a new row, then search. The write entry must
        // still land the message, and search must self-heal to cover every
        // line — proving the log, not the index, is the source of truth.
        let sess = session_dir(&dir, sid);
        fs::write(sess.join("search.sqlite"), b"this is not a database").unwrap();
        crate::search_index::forget(&sess);
        append_message(&dir, sid, &msg("human-1", KIND_USER_MESSAGE, "gamma message", ""), 3)
            .unwrap();

        let (rows, total) = search_messages(&dir, sid, Category::Text, "", None, 50).unwrap();
        assert_eq!(total, 3, "all three lines present after self-heal");
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().any(|h| h.body == "gamma message"));
        assert!(rows.iter().any(|h| h.body == "alpha message"));
    }

    #[test]
    fn agent_reinstall_leaves_old_pair_readable_not_silent() {
        // Plan 6.3 / design §10.19: instance ids change on reinstall (ADR-073)
        // but session ids are server UUIDs. The strategy is "mark, never
        // drop": the old pair session stays listed and readable for the
        // human (its history is the point), while the live pair key simply
        // resolves to a fresh session. Nothing is silently lost.
        let dir = temp_dir();
        let human = Participant::new("human-1", ParticipantIdentity::Human);
        let old_agent = Participant::new("agent-old", ParticipantIdentity::Agent);
        let old_pair =
            ensure_pair(&dir, human.clone(), old_agent, SessionKind::UserAgent, 100).expect("old");
        append_message(
            &dir,
            &old_pair.session_id,
            &msg("human-1", "user_message", "history stays", "m-1"),
            101,
        )
        .expect("append");

        let new_pair = ensure_pair(
            &dir,
            human.clone(),
            Participant::new("agent-new", ParticipantIdentity::Agent),
            SessionKind::UserAgent,
            200,
        )
        .expect("new");
        assert_ne!(old_pair.session_id, new_pair.session_id);

        // The human still sees the old session with its history.
        let listed = list_for(&dir, "human-1").expect("list");
        assert!(listed.iter().any(|s| s.session_id == old_pair.session_id));
        let (rows, total) = read_messages(&dir, &old_pair.session_id, 0, 10).expect("read");
        assert_eq!(total, 1);
        assert_eq!(rows[0].body, "history stays");
    }

    #[test]
    fn delete_session_guards_default_project_and_outsider() {
        let dir = temp_dir();
        let human = Participant::new("human-1", ParticipantIdentity::Human);
        let agent = Participant::new("agent-1", ParticipantIdentity::Agent);
        let pair = ensure_pair(&dir, human.clone(), agent.clone(), SessionKind::UserAgent, 100)
            .expect("pair");
        // The default pair session refuses (§6.1: it is the landing view).
        assert!(delete_session(&dir, &pair.session_id, "human-1").is_err());
        // A second same-pair session is deletable by a member.
        let extra = create_session(
            &dir,
            SessionKind::UserAgent,
            &[human.clone(), agent.clone()],
            "second",
            None,
            None,
            101,
        )
        .expect("create");
        delete_session(&dir, &extra.session_id, "human-1").expect("delete");
        assert!(load(&dir, &extra.session_id).is_err());
        // A project-bound group refuses (§4.4 fixed group).
        let grp = create_session(
            &dir,
            SessionKind::ProjectGroup,
            &[human.clone(), Participant::untyped("u2")],
            "proj",
            None,
            Some("p-1"),
            102,
        )
        .expect("group");
        assert!(delete_session(&dir, &grp.session_id, "human-1").is_err());
        // An ordinary group: outsider refused (hidden), member deletes.
        let free = create_session(
            &dir,
            SessionKind::UserGroup,
            &[human.clone(), Participant::untyped("u2")],
            "team",
            None,
            None,
            103,
        )
        .expect("free");
        assert!(delete_session(&dir, &free.session_id, "stranger").is_err());
        assert!(load(&dir, &free.session_id).is_ok(), "outsider must not delete");
        delete_session(&dir, &free.session_id, "human-1").expect("member delete");
    }

    #[test]
    fn delete_session_releases_search_sidecar_before_removing_dir() {
        // Regression (ADR-097 D1): the cached `search.sqlite` handle created
        // by the write-time index must be dropped before the directory is
        // removed — Windows refuses to delete an open SQLite file.
        let dir = temp_dir();
        let human = Participant::new("human-1", ParticipantIdentity::Human);
        let agent = Participant::new("agent-1", ParticipantIdentity::Agent);
        ensure_pair(&dir, human.clone(), agent.clone(), SessionKind::UserAgent, 100)
            .expect("pair");
        let extra = create_session(
            &dir,
            SessionKind::UserAgent,
            &[human, agent],
            "second",
            None,
            None,
            101,
        )
        .expect("create");
        append_message(
            &dir,
            &extra.session_id,
            &msg("human-1", KIND_USER_MESSAGE, "searchable words", ""),
            102,
        )
        .expect("append");
        // Both paths cache the connection: the write-time index above and
        // the search below. Deletion must still succeed.
        let (hits, _) = search_messages(&dir, &extra.session_id, Category::Text, "searchable", None, 10)
            .expect("search");
        assert_eq!(hits.len(), 1);

        delete_session(&dir, &extra.session_id, "human-1").expect("delete with a live search handle");
        assert!(!session_dir(&dir, &extra.session_id).exists());
        assert!(load(&dir, &extra.session_id).is_err());
    }

    #[test]
    fn search_index_write_failure_never_fails_the_message() {
        // ADR-097 D7⑥: the sidecar is best-effort. A directory squatting on
        // the sidecar path makes every open fail (and cannot be removed by
        // the open-failure retry), yet the durable row must still land.
        let dir = temp_dir();
        let s = pair_session(&dir);
        let sid = &s.session_id;
        let sess = session_dir(&dir, sid);
        fs::create_dir_all(sess.join("search.sqlite")).unwrap();

        let (stored, written) = append_message(
            &dir,
            sid,
            &msg("human-1", KIND_USER_MESSAGE, "durable despite index", "m-1"),
            1,
        )
        .expect("append must not fail on a search-index error");
        assert!(written);
        assert_eq!(stored.body, "durable despite index");

        let (rows, total) = read_messages(&dir, sid, 0, 10).expect("read");
        assert_eq!(total, 1);
        assert_eq!(rows[0].body, "durable despite index");

        // Leave the temp dir cleanable (Windows cannot remove a non-empty
        // dir through the sidecar path retry).
        fs::remove_dir_all(sess.join("search.sqlite")).unwrap();
    }

    #[test]
    fn append_wakes_human_participants_only() {        // Wave 5 doorbell: a new row wakes the humans' surfaces; typed
        // agents are skipped, and a deduped write does not re-signal.
        use acowork_core::mqtt_proto::user_chat_changed_topic;

        let dir = temp_dir();
        let s = ensure_pair(
            &dir,
            Participant::new("doorbell-human", ParticipantIdentity::Human),
            Participant::new("doorbell-agent", ParticipantIdentity::Agent),
            SessionKind::UserAgent,
            100,
        )
        .expect("pair");
        let before = crate::mqtt_publisher::test_signals().len();
        append_message(
            &dir,
            &s.session_id,
            &msg("doorbell-human", "user_message", "hi", "m-1"),
            101,
        )
        .expect("append");
        let sent = crate::mqtt_publisher::test_signals();
        let window = &sent[before..];
        assert!(
            window.contains(&user_chat_changed_topic("doorbell-human")),
            "human participant must be signalled: {window:?}"
        );
        assert!(
            !window.contains(&user_chat_changed_topic("doorbell-agent")),
            "typed agent must not be signalled: {window:?}"
        );

        let mid = crate::mqtt_publisher::test_signals().len();
        let (dup, appended) = append_message(
            &dir,
            &s.session_id,
            &msg("doorbell-human", "user_message", "hi", "m-1"),
            102,
        )
        .expect("dedup append");
        assert_eq!(dup.seq, 1);
        assert!(!appended);
        // Window-contains, not exact length: the sink is process-wide and
        // concurrent tests signal their own users (same rule as chat_api's
        // doorbell test). `doorbell-human` is this test's id alone.
        let sent2 = crate::mqtt_publisher::test_signals();
        assert!(
            !sent2[mid..].contains(&user_chat_changed_topic("doorbell-human")),
            "a deduped write must not re-signal"
        );
    }

    #[test]
    fn create_and_load_roundtrip() {        let dir = temp_dir();
        let s = create_session(
            &dir,
            SessionKind::UserGroup,
            &["u1".into(), "u2".into()],
            "team",
            None,
            None,
            100,
        )
        .expect("create");
        let loaded = load(&dir, &s.session_id).expect("load");
        assert_eq!(s, loaded);
        assert_eq!(loaded.kind, SessionKind::UserGroup);
    }

    #[test]
    fn project_group_create_is_idempotent_on_project_id() {
        let dir = temp_dir();
        let a = create_session(
            &dir,
            SessionKind::ProjectGroup,
            &["owner".into()],
            "p",
            None,
            Some("proj-1"),
            1,
        )
        .unwrap();
        let b = create_session(
            &dir,
            SessionKind::ProjectGroup,
            &["owner".into()],
            "p",
            None,
            Some("proj-1"),
            2,
        )
        .unwrap();
        assert_eq!(a.session_id, b.session_id, "one project, one group chat");
    }

    #[test]
    fn rejects_duplicate_participants_and_bad_project_group() {
        let dir = temp_dir();
        assert!(
            create_session(&dir, SessionKind::UserGroup, &["u".into(), "u".into()], "", None, None, 1)
                .is_err()
        );
        assert!(
            create_session(&dir, SessionKind::ProjectGroup, &["u".into()], "", None, None, 1).is_err()
        );
    }

    #[test]
    fn append_assigns_monotonic_seq_and_validates_participation() {
        let dir = temp_dir();
        let s =
            create_session(&dir, SessionKind::UserUser, &["a".into(), "b".into()], "", None, None, 1)
                .unwrap();
        let (m1, wrote) = append_message(&dir, &s.session_id, &msg("a", KIND_USER_MESSAGE, "hi", ""), 10)
            .unwrap();
        assert!(wrote);
        assert_eq!(m1.seq, 1);
        let (m2, _) =
            append_message(&dir, &s.session_id, &msg("b", KIND_USER_MESSAGE, "yo", ""), 11).unwrap();
        assert_eq!(m2.seq, 2);
        // Non-participant sender is refused (§8-1).
        assert!(append_message(&dir, &s.session_id, &msg("c", KIND_USER_MESSAGE, "x", ""), 12).is_err());
        let (page, total) = read_messages(&dir, &s.session_id, 0, 10).unwrap();
        assert_eq!(total, 2);
        // chronological order, same contract as chat::read_messages — the
        // client suffix-diffs on `seq`.
        assert_eq!(page[0].seq, 1);
        assert_eq!(page[1].seq, 2);
    }

    #[test]
    fn intent_message_id_is_idempotent() {
        let dir = temp_dir();
        let s = create_session(
            &dir,
            SessionKind::AgentAgent,
            &["inst-a".into(), "inst-b".into()],
            "",
            None,
            None,
            1,
        )
        .unwrap();
        let m = msg("inst-a", KIND_AGENT_MESSAGE, "do it", "mid-1");
        let (first, wrote1) = append_message(&dir, &s.session_id, &m, 5).unwrap();
        let (second, wrote2) = append_message(&dir, &s.session_id, &m, 9).unwrap();
        assert!(wrote1 && !wrote2, "redelivery must not double-write");
        assert_eq!(first.seq, second.seq);
        assert_eq!(second.ts, first.ts, "the stored row is returned verbatim");
        let (_, total) = read_messages(&dir, &s.session_id, 0, 10).unwrap();
        assert_eq!(total, 1);
    }

    #[test]
    fn dispatch_and_start_rows_coexist_and_each_dedups() {
        // §5.5.5 event-log model: one routed turn leaves BOTH a
        // `turn_dispatched` row (id `td:`) and a `turn_started` row (id
        // `ts:`). They carry distinct message_ids, so the store keeps them
        // as two separate event rows (never deduped against each other),
        // while a redelivery of the same milestone still collapses.
        let dir = temp_dir();
        let s = create_session(
            &dir,
            SessionKind::UserAgent,
            &[
                Participant::new("u", ParticipantIdentity::Human),
                Participant::new("ag", ParticipantIdentity::Agent),
            ],
            "",
            None,
            None,
            1,
        )
        .unwrap();
        let mut dispatched = msg("ag", KIND_SYSTEM_NOTICE, "", "td:m1");
        dispatched.meta.notice_type = "turn_dispatched".into();
        dispatched.meta.in_reply_to = "m1".into();
        let mut started = msg("ag", KIND_SYSTEM_NOTICE, "", "ts:m1");
        started.meta.notice_type = "turn_started".into();
        started.meta.in_reply_to = "m1".into();

        assert!(append_message(&dir, &s.session_id, &dispatched, 1)
            .unwrap()
            .1);
        let (start_row, wrote_start) = append_message(&dir, &s.session_id, &started, 2).unwrap();
        assert!(
            wrote_start,
            "a `ts:` start row must not collapse into the `td:` dispatch row"
        );
        assert_eq!(start_row.seq, 2, "the two milestones are ordered rows");
        // A redelivery of the dispatch milestone is still idempotent.
        assert!(
            !append_message(&dir, &s.session_id, &dispatched, 3)
                .unwrap()
                .1,
            "redelivery of the same `td:` id must not double-write"
        );
        let (_, total) = read_messages(&dir, &s.session_id, 0, 10).unwrap();
        assert_eq!(total, 2, "exactly one dispatch row + one start row");
    }

    #[test]
    fn turn_mode_survives_the_persist_read_round_trip() {
        // ADR-100 two-state reaction: the routing `turn_mode` the Runtime
        // stamps on a turn pointer is a durable fact the surfaces read back.
        // The store's `IntentMeta` is a typed struct, so an unknown field
        // would be silently dropped at deserialization — this test is the
        // regression net that keeps `turn_mode` a known, persisted field.
        let dir = temp_dir();
        let s = create_session(
            &dir,
            SessionKind::UserAgent,
            &[
                Participant::new("u", ParticipantIdentity::Human),
                Participant::new("ag", ParticipantIdentity::Agent),
            ],
            "",
            None,
            None,
            1,
        )
        .unwrap();
        let mut started = msg("ag", KIND_SYSTEM_NOTICE, "", "ts:m1");
        started.meta.notice_type = "turn_started".into();
        started.meta.in_reply_to = "m1".into();
        started.meta.agent_session_id = "exec-9".into();
        started.meta.turn_mode = Some("new".into());
        append_message(&dir, &s.session_id, &started, 1).unwrap();

        let (rows, _) = read_messages(&dir, &s.session_id, 0, 10).unwrap();
        let row = rows
            .iter()
            .find(|r| r.meta.message_id == "ts:m1")
            .expect("the start row reads back");
        assert_eq!(
            row.meta.turn_mode.as_deref(),
            Some("new"),
            "turn_mode must persist and read back verbatim"
        );
    }

    #[test]
    fn bodyless_notice_rows_do_not_inflate_unread() {
        // §4.6 unread rule with the §5.5.5 event-log exemption: a routed turn
        // writes two bodyless `system_notice` pointer rows (`td:` + `ts:`).
        // They are milestones in the transcript, not new content to catch up
        // on, so they must not climb the human's unread badge. A content row
        // still counts.
        let dir = temp_dir();
        let s = create_session(
            &dir,
            SessionKind::UserAgent,
            &[
                Participant::new("u", ParticipantIdentity::Human),
                Participant::new("ag", ParticipantIdentity::Agent),
            ],
            "",
            None,
            None,
            1,
        )
        .unwrap();
        // The agent's reply: the human (not the sender) gains one unread.
        append_message(
            &dir,
            &s.session_id,
            &msg("ag", KIND_AGENT_MESSAGE, "a reply", "a-1"),
            1,
        )
        .unwrap();
        assert_eq!(
            load(&dir, &s.session_id).unwrap().unread.get("u").copied(),
            Some(1)
        );

        // Both pointer rows land (distinct ids, not deduped) but move no unread.
        let mut dispatched = msg("ag", KIND_SYSTEM_NOTICE, "", "td:m1");
        dispatched.meta.notice_type = "turn_dispatched".into();
        dispatched.meta.in_reply_to = "m1".into();
        let mut started = msg("ag", KIND_SYSTEM_NOTICE, "", "ts:m1");
        started.meta.notice_type = "turn_started".into();
        started.meta.in_reply_to = "m1".into();
        assert!(append_message(&dir, &s.session_id, &dispatched, 2)
            .unwrap()
            .1);
        assert!(append_message(&dir, &s.session_id, &started, 3)
            .unwrap()
            .1);
        assert_eq!(
            load(&dir, &s.session_id).unwrap().unread.get("u").copied(),
            Some(1),
            "bodyless system_notice rows must not inflate the unread badge"
        );
    }

    #[test]
    fn vocabulary_is_the_four_new_kinds_only() {
        // 2b.1 acceptance: new writes only produce the four kinds; the
        // retired `intent_*` kinds are rejected at the write entry (they
        // exist only in the read-side normalization).
        let dir = temp_dir();
        let s = create_session(
            &dir,
            SessionKind::UserAgent,
            &[
                Participant::new("u", ParticipantIdentity::Human),
                Participant::new("ag", ParticipantIdentity::Agent),
            ],
            "",
            None,
            None,
            1,
        )
        .unwrap();
        // The four writable kinds.
        assert!(append_message(&dir, &s.session_id, &msg("u", KIND_USER_MESSAGE, "hi", ""), 1)
            .is_ok());
        assert!(
            append_message(&dir, &s.session_id, &msg("ag", KIND_AGENT_MESSAGE, "yo", ""), 2)
                .is_ok()
        );
        // A well-formed card: `interaction` payload + answerable target
        // (the §5.5.4 shape checks reject a bare row).
        let mut ir = msg("ag", KIND_INTERACTION_REQUEST, "pick one", "ir-1");
        ir.to = "u".into();
        ir.meta.interaction = Some(InteractionPayload {
            kind: "ask_question".into(),
            request_id: "ir-1".into(),
            question: "pick one".into(),
            options: vec![],
        });
        assert!(append_message(&dir, &s.session_id, &ir, 3).is_ok());
        assert!(append_message(
            &dir,
            &s.session_id,
            &msg("ag", KIND_SYSTEM_NOTICE, "turn started", "sn-1"),
            4
        )
        .is_err()); // a notice without its `notice_type` is not a notice
        let mut sn = msg("ag", KIND_SYSTEM_NOTICE, "turn started", "sn-2");
        sn.meta.notice_type = "turn_started".into();
        assert!(append_message(&dir, &s.session_id, &sn, 4).is_ok());
        // Retired kinds are no longer writable.
        assert!(append_message(&dir, &s.session_id, &msg("ag", "intent_request", "x", ""), 5)
            .is_err());
        assert!(append_message(&dir, &s.session_id, &msg("ag", "intent_response", "x", ""), 6)
            .is_err());
        // Unknown kinds stay rejected.
        assert!(append_message(&dir, &s.session_id, &msg("u", "agent_thought", "x", ""), 7).is_err());
    }

    #[test]
    fn golden_sample_old_rows_render_as_new_rows() {
        // 2b.2 / D-A A1 acceptance: a session containing old `intent_*`
        // rows reads back field-for-field identical to what the new write
        // side produces — the rows on disk are never rewritten.
        let dir = temp_dir();
        let s = create_session(
            &dir,
            SessionKind::UserGroup,
            &[
                Participant::new("u1", ParticipantIdentity::Human),
                Participant::new("ag", ParticipantIdentity::Agent),
            ],
            "",
            None,
            None,
            1,
        )
        .unwrap();
        // Hand-write the old rows (the write entry no longer accepts them).
        let legacy = [
            SessionMessage {
                seq: 1,
                ts: 10,
                from: "u1".into(),
                kind: "intent_request".into(),
                body: "@ag help".into(),
                to: "ag".into(),
                meta: IntentMeta {
                    message_id: "old-1".into(),
                    conversation_id: "conv-9".into(),
                    ..Default::default()
                },
            attachment_ids: Vec::new(),
            attachments: Vec::new(),
            },
            SessionMessage {
                seq: 2,
                ts: 11,
                from: "ag".into(),
                kind: "intent_response".into(),
                body: "on it".into(),
                to: "u1".into(),
                meta: IntentMeta {
                    message_id: "old-2".into(),
                    conversation_id: "conv-9".into(),
                    in_reply_to: "old-1".into(),
                    status: "completed".into(),
                    ..Default::default()
                },
            attachment_ids: Vec::new(),
            attachments: Vec::new(),
            },
        ];
        {
            use std::io::Write as _;
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(session_dir(&dir, &s.session_id).join("messages.jsonl"))
                .unwrap();
            for m in &legacy {
                writeln!(f, "{}", serde_json::to_string(m).unwrap()).unwrap();
            }
        }
        let (rows, total) = read_messages(&dir, &s.session_id, 0, 10).unwrap();
        assert_eq!(total, 2);
        // Human-written mention row → user_message; agent answer →
        // agent_message; intent facts preserved as fields.
        assert_eq!(rows[0].kind, KIND_USER_MESSAGE);
        assert_eq!(rows[0].meta.conversation_id, "conv-9");
        assert_eq!(rows[0].meta.message_id, "old-1");
        assert_eq!(rows[1].kind, KIND_AGENT_MESSAGE);
        assert_eq!(rows[1].meta.in_reply_to, "old-1");
        assert_eq!(rows[1].meta.status, "completed");
        // Disk truth: the JSONL still holds the original rows.
        let raw =
            std::fs::read_to_string(session_dir(&dir, &s.session_id).join("messages.jsonl"))
                .unwrap();
        assert!(raw.contains("\"intent_request\"") && raw.contains("\"intent_response\""),
            "old rows are never rewritten (append-only, A2 rejected)");
    }

    #[test]
    fn user_message_gets_a_server_assigned_message_id() {
        // 2a.1 acceptance: a `user_message` without an id is accepted and
        // the server fills it in; a repeated submitted id writes no second
        // row.
        let dir = temp_dir();
        let s = create_session(&dir, SessionKind::UserUser, &["a".into(), "b".into()], "", None, None, 1)
            .unwrap();
        let (m, wrote) = append_message(&dir, &s.session_id, &msg("a", KIND_USER_MESSAGE, "hi", ""), 2)
            .unwrap();
        assert!(wrote);
        assert!(!m.meta.message_id.is_empty(), "the stored row carries an id");
        // The id is durable — a reload returns the same row content.
        let (rows, total) = read_messages(&dir, &s.session_id, 0, 10).unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].meta.message_id, m.meta.message_id);

        // A client-submitted id dedups like every other kind.
        let dup = msg("a", KIND_USER_MESSAGE, "hi", "um-1");
        let (first, wrote1) = append_message(&dir, &s.session_id, &dup, 3).unwrap();
        let (second, wrote2) = append_message(&dir, &s.session_id, &dup, 4).unwrap();
        assert!(wrote1 && !wrote2);
        assert_eq!(first.meta.message_id, second.meta.message_id);
        let (_, total) = read_messages(&dir, &s.session_id, 0, 10).unwrap();
        assert_eq!(total, 2, "one assigned + one submitted, no duplicate");
    }

    #[test]
    fn seq_survives_metadata_counter_crash_window() {
        // Simulate: JSONL line durable, session.json save failed → counter
        // behind. The next append must reconcile, not reuse the seq.
        let dir = temp_dir();
        let s = create_session(&dir, SessionKind::UserUser, &["a".into(), "b".into()], "", None, None, 1)
            .unwrap();
        append_message(&dir, &s.session_id, &msg("a", KIND_USER_MESSAGE, "one", ""), 2).unwrap();
        let mut stale = load(&dir, &s.session_id).unwrap();
        stale.message_count = 0;
        save(&session_dir(&dir, &s.session_id), &stale).unwrap();
        let (m, _) = append_message(&dir, &s.session_id, &msg("b", KIND_USER_MESSAGE, "two", ""), 3)
            .unwrap();
        assert_eq!(m.seq, 2, "must not reissue seq 1");
    }

    #[test]
    fn list_for_filters_by_participant() {
        let dir = temp_dir();
        let s1 = create_session(&dir, SessionKind::UserGroup, &["x".into(), "y".into()], "", None, None, 1)
            .unwrap();
        let _s2 = create_session(&dir, SessionKind::UserGroup, &["y".into(), "z".into()], "", None, None, 2)
            .unwrap();
        let xs = list_for(&dir, "x").unwrap();
        assert_eq!(xs.len(), 1);
        assert_eq!(xs[0].session_id, s1.session_id);
        assert_eq!(list_for(&dir, "nobody").unwrap().len(), 0);
    }

    #[test]
    fn ensure_pair_is_idempotent_and_order_insensitive() {
        let dir = temp_dir();
        let a = ensure_pair(
            &dir,
            Participant::new("agent-b", ParticipantIdentity::Agent),
            Participant::new("agent-a", ParticipantIdentity::Agent),
            SessionKind::AgentAgent,
            1,
        )
        .unwrap();
        let b = ensure_pair(
            &dir,
            Participant::new("agent-a", ParticipantIdentity::Agent),
            Participant::new("agent-b", ParticipantIdentity::Agent),
            SessionKind::AgentAgent,
            2,
        )
        .unwrap();
        assert_eq!(a.session_id, b.session_id, "one pair, one chat session");
        assert_eq!(
            a.participants,
            vec![
                Participant::new("agent-a", ParticipantIdentity::Agent),
                Participant::new("agent-b", ParticipantIdentity::Agent),
            ]
        );
        assert!(ensure_pair(
            &dir,
            Participant::new("x", ParticipantIdentity::Agent),
            Participant::new("x", ParticipantIdentity::Agent),
            SessionKind::AgentAgent,
            3,
        )
        .is_err());
    }

    #[test]
    fn user_agent_pair_requires_one_human_and_one_agent() {
        let dir = temp_dir();
        // Two humans are a `user_user` pair, not a user-agent channel.
        assert!(ensure_pair(
            &dir,
            Participant::new("u1", ParticipantIdentity::Human),
            Participant::new("u2", ParticipantIdentity::Human),
            SessionKind::UserAgent,
            1,
        )
        .is_err());
        let s = ensure_pair(
            &dir,
            Participant::new("ag", ParticipantIdentity::Agent),
            Participant::new("u", ParticipantIdentity::Human),
            SessionKind::UserAgent,
            1,
        )
        .unwrap();
        // The pair key is order-insensitive: the flipped call is idempotent.
        let again = ensure_pair(
            &dir,
            Participant::new("u", ParticipantIdentity::Human),
            Participant::new("ag", ParticipantIdentity::Agent),
            SessionKind::UserAgent,
            2,
        )
        .unwrap();
        assert_eq!(s.session_id, again.session_id);
    }

    #[test]
    fn writer_identity_type_is_enforced_at_the_write_entry() {
        // §8.1: the kind must match the writer's recorded identity type.
        let dir = temp_dir();
        let s = create_session(
            &dir,
            SessionKind::UserAgent,
            &[
                Participant::new("u", ParticipantIdentity::Human),
                Participant::new("ag", ParticipantIdentity::Agent),
            ],
            "",
            None,
            None,
            1,
        )
        .unwrap();
        // An agent cannot write a human utterance.
        assert!(
            append_message(&dir, &s.session_id, &msg("ag", KIND_USER_MESSAGE, "hi", ""), 2).is_err()
        );
        // A human cannot forge an agent response.
        let mut r = msg("u", KIND_AGENT_MESSAGE, "x", "mid-9");
        r.meta.in_reply_to = "mid-8".into();
        assert!(append_message(&dir, &s.session_id, &r, 2).is_err());
        // The honest directions work.
        assert!(
            append_message(&dir, &s.session_id, &msg("u", KIND_USER_MESSAGE, "hi", ""), 2).is_ok()
        );
        let mut ok_r = msg("ag", KIND_AGENT_MESSAGE, "done", "mid-10");
        ok_r.meta.in_reply_to = "mid-8".into();
        assert!(append_message(&dir, &s.session_id, &ok_r, 2).is_ok());
    }

    #[test]
    fn legacy_untyped_participants_skip_the_type_check() {
        // §3.3: history is not migrated. A session row written before
        // §8.1 carries bare-string participants; membership is still
        // enforced, the type check is skipped.
        let dir = temp_dir();
        let s = create_session(&dir, SessionKind::UserGroup, &["a".into(), "b".into()], "", None, None, 1)
            .unwrap();
        assert!(
            append_message(&dir, &s.session_id, &msg("a", KIND_USER_MESSAGE, "hi", ""), 2).is_ok()
        );
        assert!(append_message(&dir, &s.session_id, &msg("c", KIND_USER_MESSAGE, "hi", ""), 2)
            .is_err());
    }

    #[test]
    fn set_repo_roundtrips_and_validates() {
        let dir = temp_dir();
        let s = create_session(
            &dir,
            SessionKind::ProjectGroup,
            &["u".into()],
            "",
            None,
            Some("proj-9"),
            1,
        )
        .unwrap();
        let s = set_repo(&dir, &s.session_id, Some("https://example.com/r.git")).unwrap();
        assert_eq!(s.repo.as_deref(), Some("https://example.com/r.git"));
        assert_eq!(load(&dir, &s.session_id).unwrap().repo.as_deref(), Some("https://example.com/r.git"));
        let s = set_repo(&dir, &s.session_id, None).unwrap();
        assert_eq!(s.repo, None);
        assert!(set_repo(&dir, &s.session_id, Some("  ")).is_err());
    }

    #[test]
    fn session_config_draft_roundtrips_normalizes_and_gates_membership() {
        // Design 27 §4.5: the Chat surface's seed draft is durable on the
        // session record. Whole-replace write, trim/empty normalization,
        // an all-empty draft stores as None, and a non-participant gets
        // the established hidden refusal vocabulary.
        let dir = temp_dir();
        let s = create_session(
            &dir,
            SessionKind::UserAgent,
            &[
                Participant::new("u", ParticipantIdentity::Human),
                Participant::new("ag", ParticipantIdentity::Agent),
            ],
            "",
            None,
            None,
            1,
        )
        .unwrap();
        // Fresh sessions carry no draft.
        assert_eq!(load(&dir, &s.session_id).unwrap().session_config, None);
        // Pick + trim: whitespace is stripped on the way in.
        let s = set_session_config(
            &dir,
            &s.session_id,
            "u",
            SeedConfig {
                model: Some(" gpt-x ".into()),
                provider: Some("openai".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let got = s.session_config.clone().unwrap();
        assert_eq!(got.model.as_deref(), Some("gpt-x"));
        assert_eq!(got.provider.as_deref(), Some("openai"));
        assert_eq!(got.workspace, None);
        assert_eq!(load(&dir, &s.session_id).unwrap().session_config, Some(got));
        // Whole-replace: omitting a field CLEARS it (absent = no opinion).
        let s = set_session_config(
            &dir,
            &s.session_id,
            "u",
            SeedConfig {
                workspace: Some("w-1".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let got = s.session_config.unwrap();
        assert_eq!(got.model, None);
        assert_eq!(got.workspace.as_deref(), Some("w-1"));
        // All-empty normalizes to no draft at all.
        let s = set_session_config(
            &dir,
            &s.session_id,
            "u",
            SeedConfig {
                workspace: Some("  ".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(s.session_config, None);
        // Participation gates the write; the refusal uses the hidden-404
        // vocabulary (§5.3) — the outsider must not learn the session id
        // is real.
        assert_eq!(
            set_session_config(
                &dir,
                &s.session_id,
                "intruder",
                SeedConfig {
                    model: Some("m".into()),
                    ..Default::default()
                },
            )
            .unwrap_err(),
            "not_a_participant"
        );
    }

    #[test]
    fn message_by_id_finds_the_card_row_and_misses_cleanly() {
        let dir = temp_dir();
        let s = ensure_pair(
            &dir,
            Participant::new("u1", ParticipantIdentity::Human),
            Participant::new("a1", ParticipantIdentity::Agent),
            SessionKind::UserAgent,
            1,
        )
        .unwrap();
        let mut card = msg("a1", KIND_INTERACTION_REQUEST, "Approve `bash`?", "card-1");
        card.to = "u1".into();
        card.meta.notice_type = String::new();
        card.meta.interaction = Some(InteractionPayload {
            kind: "approval".into(),
            request_id: "req-9".into(),
            question: "Approve `bash`?".into(),
            options: vec![],
        });
        append_message(&dir, &s.session_id, &card, 2).unwrap();

        let found = message_by_id(&dir, &s.session_id, "card-1").unwrap();
        let found = found.expect("card row is fetchable by message_id");
        assert_eq!(found.kind, KIND_INTERACTION_REQUEST);
        let interaction = found.meta.interaction.expect("card carries its payload");
        assert_eq!(interaction.kind, "approval");
        assert_eq!(interaction.request_id, "req-9");

        // A miss is `Ok(None)` — the answer channel falls through to a
        // plain delivery, it is not an error.
        assert!(message_by_id(&dir, &s.session_id, "nope").unwrap().is_none());
    }

    // ── Attachments (§4.2.1, plan 3.5) ────────────────────────────────

    #[test]
    fn group_previews_derive_from_last_message() {
        // The list surfaces hand rows straight from `session.json` (the
        // `load` below mirrors `list_for`): the preview is write-time
        // derived state, and a pre-field session self-heals once through
        // `ensure_preview`.
        let dir = temp_dir();
        let g = create_session(
            &dir,
            SessionKind::ProjectGroup,
            &[
                Participant::new("u1", ParticipantIdentity::Human),
                Participant::new("ag", ParticipantIdentity::Agent),
            ],
            "P",
            None,
            Some("proj-1"),
            1,
        )
        .unwrap();
        let fresh = |dir: &Path| vec![load(dir, &g.session_id).unwrap()];

        // No messages yet → no preview (the sidebar row shows an empty meta
        // line rather than a placeholder).
        let mut empty = fresh(&dir);
        attach_group_previews(&dir, &mut empty);
        assert_eq!(empty[0].last_message_preview, None);

        append_message(
            &dir,
            &g.session_id,
            &msg("u1", KIND_USER_MESSAGE, "first", "m1"),
            2,
        )
        .unwrap();
        append_message(
            &dir,
            &g.session_id,
            &msg("ag", KIND_AGENT_MESSAGE, "  latest tail  ", "m2"),
            3,
        )
        .unwrap();
        // The newest row wins, trimmed — and the log is not read for it.
        let mut rows = fresh(&dir);
        attach_group_previews(&dir, &mut rows);
        assert_eq!(
            rows[0].last_message_preview.as_deref(),
            Some("latest tail")
        );

        // A long body truncates on a char boundary with a trailing ellipsis.
        let long = "话".repeat(GROUP_PREVIEW_CHARS + 10);
        append_message(&dir, &g.session_id, &msg("u1", KIND_USER_MESSAGE, &long, "m3"), 4)
            .unwrap();
        let mut rows = fresh(&dir);
        attach_group_previews(&dir, &mut rows);
        let p = rows[0].last_message_preview.clone().unwrap();
        assert_eq!(p.chars().count(), GROUP_PREVIEW_CHARS + 1); // body + ellipsis
        assert!(p.ends_with('…'));

        // Only project groups are touched — a sibling one-to-one / agent
        // session keeps `None` (its preview is derived in `chat_api`).
        let ua = ua_session(&dir);
        let mut mixed = vec![ua.clone()];
        attach_group_previews(&dir, &mut mixed);
        assert_eq!(mixed[0].last_message_preview, None);
    }

    fn ua_session(dir: &Path) -> ChatSession {
        ensure_pair(
            dir,
            Participant::new("u1", ParticipantIdentity::Human),
            Participant::new("ag", ParticipantIdentity::Agent),
            SessionKind::UserAgent,
            1,
        )
        .unwrap()
    }

    #[test]
    fn attachment_ids_resolve_to_disk_truth() {
        let dir = temp_dir();
        let s = ua_session(&dir);
        let att = store_attachment(&dir, &s.session_id, "u1", "note.txt", "text/plain", b"hello")
            .expect("store");
        assert_eq!(att.size, 5);
        // A row referencing the id stores the *resolved* metadata; the
        // transient id list never persists.
        let mut m = msg("u1", KIND_USER_MESSAGE, "see attached", "");
        m.attachment_ids = vec![att.id.clone()];
        let (stored, _) = append_message(&dir, &s.session_id, &m, 2).unwrap();
        assert_eq!(stored.attachments.len(), 1);
        assert_eq!(stored.attachments[0].id, att.id);
        assert!(stored.attachment_ids.is_empty());
        // An unknown id refuses the row — no dangling references.
        let mut bad = msg("u1", KIND_USER_MESSAGE, "ghost", "m-bad");
        bad.attachment_ids = vec![uuid::Uuid::new_v4().to_string()];
        assert!(append_message(&dir, &s.session_id, &bad, 3).is_err());
    }

    #[test]
    fn attachment_ids_are_session_scoped() {
        let dir = temp_dir();
        let s1 = ua_session(&dir);
        // A *different* session — the same (u1, ag) pair would resolve to
        // the idempotent pair index entry and be the same session.
        let s2 = ensure_pair(
            &dir,
            Participant::new("u1", ParticipantIdentity::Human),
            Participant::new("ag2", ParticipantIdentity::Agent),
            SessionKind::UserAgent,
            1,
        )
        .unwrap();
        let att = store_attachment(&dir, &s1.session_id, "u1", "a.png", "image/png", b"px")
            .expect("store");
        // The id names a file of *session 1*; referencing it from session 2
        // is an unknown attachment, not a cross-session read.
        let mut m = msg("u1", KIND_USER_MESSAGE, "stolen ref", "");
        m.attachment_ids = vec![att.id.clone()];
        assert!(append_message(&dir, &s2.session_id, &m, 2).is_err());
    }

    #[test]
    fn attachment_rules_at_the_write_entry() {
        let dir = temp_dir();
        let s = ua_session(&dir);
        let att = store_attachment(&dir, &s.session_id, "u1", "x.txt", "text/plain", b"x")
            .expect("store");
        // Cards and notices cannot carry attachments (producer bug shape).
        let mut notice = msg("sys", KIND_SYSTEM_NOTICE, "", "n-1");
        notice.meta.notice_type = "turn_started".into();
        notice.attachment_ids = vec![att.id.clone()];
        assert!(append_message(&dir, &s.session_id, &notice, 2).is_err());
        // An attachment-only user_message is a valid message (§4.2.1).
        let mut photo = msg("u1", KIND_USER_MESSAGE, "", "");
        photo.attachment_ids = vec![att.id.clone()];
        let (stored, _) = append_message(&dir, &s.session_id, &photo, 3).unwrap();
        assert_eq!(stored.attachments.len(), 1);
        // Only participants may upload.
        assert!(
            store_attachment(&dir, &s.session_id, "outsider", "x", "text/plain", b"x").is_err()
        );
    }

    /// ADR-100 D12: the display names an execution composer sent with its
    /// message are part of the durable row. `IntentMeta` is a *typed* struct, so
    /// a key the store does not declare is dropped silently on the way in — this
    /// test is the guard that the one D12 adds is actually declared, and that it
    /// comes back out of the JSONL rather than only surviving in memory.
    #[test]
    fn exec_attachment_labels_are_durable_on_a_user_message() {
        let dir = temp_dir();
        let s = ua_session(&dir);
        let mut m = msg("u1", KIND_USER_MESSAGE, "take a look", "pinned-1");
        m.meta.exec_attachments = vec![
            ExecAttachmentLabel {
                filename: "src/main.rs".into(),
                kind: "attached_file".into(),
            },
            ExecAttachmentLabel {
                filename: "notes.md".into(),
                kind: "upload".into(),
            },
        ];
        let (stored, _) = append_message(&dir, &s.session_id, &m, 2).unwrap();
        assert_eq!(stored.meta.exec_attachments, m.meta.exec_attachments);
        let (rows, _) = read_messages(&dir, &s.session_id, 0, 10).unwrap();
        let row = rows
            .iter()
            .find(|r| r.meta.message_id == "pinned-1")
            .expect("the row is readable");
        assert_eq!(row.meta.exec_attachments.len(), 2);
        assert_eq!(row.meta.exec_attachments[0].filename, "src/main.rs");
        // A row that sent none stays clean: the key is skipped rather than
        // written as an empty array, so ordinary transcript rows grow no noise.
        append_message(
            &dir,
            &s.session_id,
            &msg("ag", KIND_AGENT_MESSAGE, "a reply", "plain-1"),
            3,
        )
        .unwrap();
        let (rows, _) = read_messages(&dir, &s.session_id, 0, 10).unwrap();
        let plain = rows
            .iter()
            .find(|r| r.meta.message_id == "plain-1")
            .expect("the second row");
        assert!(plain.meta.exec_attachments.is_empty());
    }

    /// ADR-100 D12: an execution composer that hands over a file without
    /// writing anything still gets its head row. The attachment bytes live on
    /// the execution side, so the labels in meta are the only content the
    /// transcript has, and the emptiness rule must see them.
    #[test]
    fn an_attachment_only_user_message_is_accepted_from_the_execution_surface() {
        let dir = temp_dir();
        let s = ua_session(&dir);
        // No body, no chat-side attachment ids — only the execution labels.
        let mut labels_only = msg("u1", KIND_USER_MESSAGE, "", "pinned-2");
        labels_only.meta.exec_attachments = vec![ExecAttachmentLabel {
            filename: "diff.patch".into(),
            kind: "upload".into(),
        }];
        assert!(append_message(&dir, &s.session_id, &labels_only, 2).is_ok());
        // With no content of any kind the row is still refused: an empty,
        // label-less human message is a producer bug, not a turn.
        assert!(append_message(&dir, &s.session_id, &msg("u1", KIND_USER_MESSAGE, "  ", "pinned-3"), 3).is_err());
    }

    // ── Unread (§4.6, plan 3.6) ───────────────────────────────────────

    #[test]
    fn unread_increments_for_others_and_clears_on_read() {
        let dir = temp_dir();
        let s = ua_session(&dir);
        append_message(&dir, &s.session_id, &msg("u1", KIND_USER_MESSAGE, "hi", ""), 2).unwrap();
        append_message(&dir, &s.session_id, &msg("ag", KIND_AGENT_MESSAGE, "yo", ""), 3).unwrap();
        let s = load(&dir, &s.session_id).unwrap();
        assert_eq!(s.unread.get("ag").copied(), Some(1));
        assert_eq!(s.unread.get("u1").copied(), Some(1));
        mark_read(&dir, &s.session_id, "u1").unwrap();
        let s = load(&dir, &s.session_id).unwrap();
        assert!(!s.unread.contains_key("u1"), "cleared entries are removed, not zeroed");
        assert_eq!(s.unread.get("ag").copied(), Some(1));
    }

    #[test]
    fn add_and_remove_participant_are_idempotent() {
        let dir = temp_dir();
        let s = create_session(
            &dir,
            SessionKind::UserGroup,
            &[Participant::new("u1", ParticipantIdentity::Human)],
            "g",
            None,
            None,
            1,
        )
        .unwrap();
        // Add: new id lands, repeat add changes nothing.
        let s = add_participant(
            &dir,
            &s.session_id,
            &Participant::new("ag1", ParticipantIdentity::Agent),
            2,
        )
        .unwrap();
        assert_eq!(s.participants.len(), 2);
        let again = add_participant(
            &dir,
            &s.session_id,
            &Participant::new("ag1", ParticipantIdentity::Agent),
            3,
        )
        .unwrap();
        assert_eq!(again.participants.len(), 2, "repeat add is a no-op");
        // Remove: listed id goes, unknown id is a no-op. `0` disables the
        // floor here, exercising the raw store path independent of the
        // user_group ≥2 policy (which the HTTP route enforces via min=2).
        let s = remove_participant(&dir, &s.session_id, "ag1", 0, false, None).unwrap();
        assert_eq!(s.participants.len(), 1);
        let again = remove_participant(&dir, &s.session_id, "ag1", 0, false, None).unwrap();
        assert_eq!(again.participants.len(), 1, "repeat remove is a no-op");
        // The removal is durable — the revoked id must not reappear on load.
        let s = load(&dir, &s.session_id).unwrap();
        assert!(s.participants.iter().all(|p| p.id != "ag1"));
    }

    #[test]
    fn remove_participant_enforces_the_min_floor_atomically() {
        // The ≥2 floor lives in the store's locked RMW, not the route's
        // lock-free snapshot, so a removal that would breach it fails closed.
        let dir = temp_dir();
        let s = create_session(
            &dir,
            SessionKind::UserGroup,
            &[
                Participant::new("u1", ParticipantIdentity::Human),
                Participant::new("u2", ParticipantIdentity::Human),
            ],
            "g",
            None,
            None,
            1,
        )
        .unwrap();
        // min=2: dropping one of two would leave one → refused, roster intact.
        let err = remove_participant(&dir, &s.session_id, "u2", 2, false, None).unwrap_err();
        assert!(err.contains("at least two members"), "{err}");
        assert_eq!(load(&dir, &s.session_id).unwrap().participants.len(), 2);
        // min=0: the same removal is allowed (the PM path has no floor).
        let s = remove_participant(&dir, &s.session_id, "u2", 0, false, None).unwrap();
        assert_eq!(s.participants.len(), 1);
    }

    #[test]
    fn remove_participant_refuses_the_owner() {
        // `protect_owner` makes the current owner permanent: even the GOVERNING
        // actor (the owner themselves) cannot drop the owner id until
        // ownership moves, checked on the locked snapshot.
        let dir = temp_dir();
        let s = create_session(
            &dir,
            SessionKind::UserGroup,
            &[
                Participant::new("u1", ParticipantIdentity::Human),
                Participant::new("u2", ParticipantIdentity::Human),
                Participant::new("u3", ParticipantIdentity::Human),
            ],
            "g",
            Some("u1"),
            None,
            1,
        )
        .unwrap();
        let err = remove_participant(&dir, &s.session_id, "u1", 2, true, Some(("u1", false)))
            .unwrap_err();
        assert!(err.contains("keep an owner"), "{err}");
        assert_eq!(load(&dir, &s.session_id).unwrap().participants.len(), 3);
        // A plain member removes fine for the owner (3 → 2, floor respected).
        let s = remove_participant(&dir, &s.session_id, "u2", 2, true, Some(("u1", false))).unwrap();
        assert_eq!(s.participants.len(), 2);
    }

    #[test]
    fn remove_participant_gates_removals_of_others_on_owner_or_admin() {
        // ADR-098 governance (revised): a plain member may only remove
        // themselves (leave); removing anyone else is owner-or-admin, checked
        // atomically in the lock — and BEFORE owner-permanence, so a plain
        // member probing an owner removal hears `not_authorized` (403), not
        // the transfer hint (422).
        let dir = temp_dir();
        let s = create_session(
            &dir,
            SessionKind::UserGroup,
            &[
                Participant::new("u1", ParticipantIdentity::Human), // owner
                Participant::new("u2", ParticipantIdentity::Human),
                Participant::new("u3", ParticipantIdentity::Human),
                Participant::new("u4", ParticipantIdentity::Human),
            ],
            "g",
            Some("u1"),
            None,
            1,
        )
        .unwrap();
        let sid = s.session_id.clone();
        // 1. Plain member u2 removing another member u3 → refused, roster 4.
        let err = remove_participant(&dir, &sid, "u3", 2, true, Some(("u2", false))).unwrap_err();
        assert_eq!(err, "not_authorized");
        assert_eq!(load(&dir, &sid).unwrap().participants.len(), 4);
        // 2. The same plain member may leave (self-removal skips the gate).
        let s = remove_participant(&dir, &sid, "u2", 2, true, Some(("u2", false))).unwrap();
        assert_eq!(s.participants.len(), 3, "u1,u3,u4");
        // 3. The owner may remove another member (gate passes, floor respected).
        let s = remove_participant(&dir, &sid, "u3", 2, true, Some(("u1", false))).unwrap();
        assert_eq!(s.participants.len(), 2, "u1,u4");
        // 4. An admin caller clears the gate, but owner-permanence still
        //    refuses to drop the owner — proving the gate ran, not blocked.
        let err = remove_participant(&dir, &sid, "u1", 2, true, Some(("u4", true))).unwrap_err();
        assert!(err.contains("keep an owner"), "{err}");
        assert_eq!(load(&dir, &sid).unwrap().participants.len(), 2);
    }

    #[test]
    fn rename_session_requires_owner_or_admin() {
        let dir = temp_dir();
        let s = create_session(
            &dir,
            SessionKind::UserGroup,
            &[
                Participant::new("u1", ParticipantIdentity::Human),
                Participant::new("u2", ParticipantIdentity::Human),
            ],
            "team",
            Some("u1"),
            None,
            1,
        )
        .unwrap();
        // Owner renames — persisted.
        let s = rename_session(&dir, &s.session_id, "renamed", "u1", false).unwrap();
        assert_eq!(s.title, "renamed");
        assert_eq!(load(&dir, &s.session_id).unwrap().title, "renamed");
        // A non-owner member is refused.
        let err = rename_session(&dir, &s.session_id, "hijack", "u2", false).unwrap_err();
        assert_eq!(err, "not_authorized");
        // An outsider is hidden as a non-participant.
        let err = rename_session(&dir, &s.session_id, "nope", "zz", false).unwrap_err();
        assert_eq!(err, "not_a_participant");
        // An admin may rename even without owning.
        rename_session(&dir, &s.session_id, "by admin", "u2", true).unwrap();
    }

    #[test]
    fn transfer_owner_requires_a_human_member() {
        let dir = temp_dir();
        let s = create_session(
            &dir,
            SessionKind::UserGroup,
            &[
                Participant::new("u1", ParticipantIdentity::Human),
                Participant::new("u2", ParticipantIdentity::Human),
                Participant::new("ag", ParticipantIdentity::Agent),
            ],
            "g",
            Some("u1"),
            None,
            1,
        )
        .unwrap();
        // Owner transfers to a human member.
        let s = transfer_owner(&dir, &s.session_id, "u2", "u1", false).unwrap();
        assert_eq!(s.owner.as_deref(), Some("u2"));
        // The old owner can no longer transfer (not_authorized).
        let err = transfer_owner(&dir, &s.session_id, "u1", "u1", false).unwrap_err();
        assert_eq!(err, "not_authorized");
        // The new owner may not hand the group to an agent.
        let err = transfer_owner(&dir, &s.session_id, "ag", "u2", false).unwrap_err();
        assert!(err.contains("agent cannot own"), "{err}");
        // ...nor to a non-member.
        let err = transfer_owner(&dir, &s.session_id, "zz", "u2", false).unwrap_err();
        assert!(err.contains("not a member"), "{err}");
    }

    /// Regression for the §8 atomic-RMW rule: `sync_group` /
    /// `add_participant` mutate `session.json` too, and without the
    /// per-session lock they could overwrite a concurrent
    /// `append_message` save (losing unread / last_active updates).
    #[test]
    fn participant_changes_never_clobber_concurrent_appends() {
        let dir = temp_dir();
        let s = create_session(
            &dir,
            SessionKind::ProjectGroup,
            &[
                Participant::new("ag1", ParticipantIdentity::Agent),
                Participant::new("ag2", ParticipantIdentity::Agent),
            ],
            "g",
            None,
            Some("proj-lock"),
            1,
        )
        .unwrap();
        let sid = s.session_id.clone();
        let dir = Arc::new(dir);
        let writers: Vec<_> = (0..4)
            .map(|i| {
                let dir = Arc::clone(&dir);
                let sid = sid.clone();
                std::thread::spawn(move || {
                    for j in 0..5 {
                        let m = SessionMessage {
                            seq: 0,
                            ts: 10 + j,
                            from: if i % 2 == 0 { "ag1" } else { "ag2" }.to_string(),
                            kind: KIND_AGENT_MESSAGE.to_string(),
                            body: format!("w{i}m{j}"),
                            to: String::new(),
                            meta: IntentMeta {
                                message_id: format!("w{i}-m{j}"),
                                ..Default::default()
                            },
                            attachment_ids: Vec::new(),
                            attachments: Vec::new(),
                        };
                        append_message(&dir, &sid, &m, 10 + j).unwrap();
                    }
                })
            })
            .collect();
        let syncer = {
            let dir = Arc::clone(&dir);
            std::thread::spawn(move || {
                for j in 0..5 {
                    sync_group(
                        &dir,
                        "proj-lock",
                        &[
                            Participant::new("ag1", ParticipantIdentity::Agent),
                            Participant::new("ag2", ParticipantIdentity::Agent),
                            Participant::new(format!("ag{j}"), ParticipantIdentity::Agent),
                        ],
                        100 + j,
                    )
                    .unwrap();
                }
            })
        };
        for w in writers {
            w.join().unwrap();
        }
        syncer.join().unwrap();
        let s = load(&dir, &sid).unwrap();
        assert_eq!(s.message_count, 20, "no append lost its counter save");
        // Every non-sender gained an unread per row (3.6): the aggregate
        // is exactly the rows that did not come from that reader.
        let ag1 = s.unread.get("ag1").copied().unwrap_or(0);
        assert_eq!(ag1, 10, "sync_group save must not clobber append unread");
        assert!(s.participants.iter().any(|p| p.id == "ag4"));
    }

    #[test]
    fn sync_group_converges_the_participant_set() {
        let dir = temp_dir();
        // First sync for an unknown project creates the group (the
        // `ensure_group` case).
        let s = sync_group(
            &dir,
            "proj-9",
            &[
                Participant::new("ag1", ParticipantIdentity::Agent),
                Participant::new("ag2", ParticipantIdentity::Agent),
            ],
            1,
        )
        .unwrap();
        let first_sid = s.session_id.clone();
        assert_eq!(s.participants.len(), 2);
        // Second sync: one joins, one is removed — the set ends up exactly
        // as requested (§8.3: removal is the revocation half).
        let s = sync_group(
            &dir,
            "proj-9",
            &[
                Participant::new("ag1", ParticipantIdentity::Agent),
                Participant::new("ag3", ParticipantIdentity::Agent),
            ],
            2,
        )
        .unwrap();
        assert_eq!(s.session_id, first_sid, "same group session, not a new one");
        let ids: Vec<&str> = s.participants.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["ag1", "ag3"]);
        // The removed participant no longer lists the session — this is
        // exactly what the Gateway scope injection will see, so the bound
        // execution sessions become unreadable for `ag2` on the next hop.
        assert_eq!(list_for(&dir, "ag2").unwrap().len(), 0);
        assert_eq!(list_for(&dir, "ag3").unwrap().len(), 1);
        // Syncing the same set again writes nothing new.
        let s2 = sync_group(
            &dir,
            "proj-9",
            &[
                Participant::new("ag1", ParticipantIdentity::Agent),
                Participant::new("ag3", ParticipantIdentity::Agent),
            ],
            3,
        )
        .unwrap();
        assert_eq!(s.participants, s2.participants);
    }

    #[test]
    fn sync_group_wakes_human_participants_on_change() {
        // The member-add doorbell (backend drives the frontend refresh): when
        // a project member is synced into the group's participant set, every
        // human surface must re-read — the newcomer leaves the read-only
        // state, the owner's header count refreshes. Typed agents are skipped
        // and a no-op re-sync stays silent (same rules as `append_message`).
        use acowork_core::mqtt_proto::user_chat_changed_topic;

        let dir = temp_dir();
        // Seed the group: a human owner plus an agent.
        let s = sync_group(
            &dir,
            "proj-doorbell",
            &[
                Participant::new("owner-h", ParticipantIdentity::Human),
                Participant::new("ag1", ParticipantIdentity::Agent),
            ],
            1,
        )
        .unwrap();
        assert_eq!(s.participants.len(), 2);

        let before = crate::mqtt_publisher::test_signals().len();
        // A human joins (admin added as a project member).
        let _ = sync_group(
            &dir,
            "proj-doorbell",
            &[
                Participant::new("owner-h", ParticipantIdentity::Human),
                Participant::new("admin-h", ParticipantIdentity::Human),
                Participant::new("ag1", ParticipantIdentity::Agent),
            ],
            2,
        )
        .unwrap();
        let sent = crate::mqtt_publisher::test_signals();
        let window = &sent[before..];
        assert!(
            window.contains(&user_chat_changed_topic("admin-h")),
            "the newcomer must be signalled: {window:?}"
        );
        assert!(
            window.contains(&user_chat_changed_topic("owner-h")),
            "the still-listed member refreshes the count: {window:?}"
        );
        assert!(
            !window.contains(&user_chat_changed_topic("ag1")),
            "typed agent must not be signalled: {window:?}"
        );
        // The owner is in both the old and new set, yet fires exactly once.
        assert_eq!(
            window
                .iter()
                .filter(|t| **t == user_chat_changed_topic("owner-h"))
                .count(),
            1,
            "an unchanged member must not be double-signalled: {window:?}"
        );

        // A later sync removes a human: the dropped party is gone from the new
        // set but must still be woken so its surface drops the group.
        let mid = crate::mqtt_publisher::test_signals().len();
        let _ = sync_group(
            &dir,
            "proj-doorbell",
            &[
                Participant::new("owner-h", ParticipantIdentity::Human),
                Participant::new("ag1", ParticipantIdentity::Agent),
            ],
            3,
        )
        .unwrap();
        let sent2 = crate::mqtt_publisher::test_signals();
        assert!(
            sent2[mid..].contains(&user_chat_changed_topic("admin-h")),
            "a removed human must be signalled: {:?}",
            &sent2[mid..]
        );

        // Re-syncing the same set converges to no change → no new signal.
        let mid2 = crate::mqtt_publisher::test_signals().len();
        let _ = sync_group(
            &dir,
            "proj-doorbell",
            &[
                Participant::new("owner-h", ParticipantIdentity::Human),
                Participant::new("ag1", ParticipantIdentity::Agent),
            ],
            4,
        )
        .unwrap();
        let sent3 = crate::mqtt_publisher::test_signals();
        assert!(
            !sent3[mid2..].contains(&user_chat_changed_topic("owner-h")),
            "a no-op re-sync must not re-signal"
        );
    }

    // ── Wave 6.2 legacy layout migration ──────────────────────────────

    const LO: &str = "aaaaaaaa-0000-4000-8000-000000000001";
    const HI: &str = "bbbbbbbb-0000-4000-8000-000000000002";

    /// Build an old-layout pair directory by hand (the old store's code is
    /// gone; the on-disk *format* is the fixture's contract).
    fn seed_legacy_pair(dir: &Path, att_id: Option<&str>) {
        let pair = dir.join("users").join(LO).join("chats").join(HI);
        fs::create_dir_all(pair.join("files")).unwrap();
        fs::write(
            pair.join("conversation.json"),
            serde_json::json!({
                "chat_id": format!("{LO}{}{HI}", crate::chat::SEP),
                "participants": [LO, HI],
                "created_at": 100,
                "last_active_at": 160,
                "last_message_preview": "see attachment",
                "unread": { HI: 2 },
                "version": 7,
                "message_count": 3
            })
            .to_string(),
        )
        .unwrap();
        let mut lines = String::new();
        for (seq, from, body, ts) in [
            (1u64, LO, "hello", 120i64),
            (2, HI, "hi back", 140),
            (3, LO, "see attachment", 160),
        ] {
            let attachments: Vec<serde_json::Value> = att_id
                .filter(|_| seq == 3)
                .map(|id| {
                    serde_json::json!({
                        "id": id, "filename": "shot.png",
                        "mime": "image/png", "size": 3, "thumb": null
                    })
                })
                .into_iter()
                .collect();
            lines.push_str(
                &serde_json::json!({
                    "seq": seq, "ts": ts, "from": from,
                    "kind": if seq == 3 { "image" } else { "text" },
                    "body": body, "attachments": attachments
                })
                .to_string(),
            );
            lines.push('\n');
        }
        fs::write(pair.join("messages.jsonl"), lines).unwrap();
        if let Some(id) = att_id {
            fs::write(pair.join("files").join(id), b"png").unwrap();
            fs::write(
                pair.join("files").join(format!("{id}.json")),
                serde_json::json!({
                    "id": id, "filename": "shot.png", "mime": "image/png",
                    "size": 3, "thumb": null
                })
                .to_string(),
            )
            .unwrap();
        }
    }

    #[test]
    fn legacy_pair_directories_fold_into_user_user_sessions() {
        let dir = temp_dir();
        let att = uuid::Uuid::new_v4().to_string();
        seed_legacy_pair(&dir, Some(&att));

        let report = migrate_legacy(&dir).expect("migration runs");
        assert_eq!(report.pairs, 1);
        assert_eq!(report.messages, 3);

        // The wire chat_id resolves through the pair index, and the legacy
        // directory is gone (staging removed on success).
        let session = pair_lookup(&dir, LO, HI).expect("pair index maps the old chat_id");
        assert_eq!(session.kind, SessionKind::UserUser);
        assert!(!dir.join("users").join(LO).join("chats").join(HI).exists());

        // Rows replay in order, with the old timestamps and the attachment
        // metadata carried onto the row.
        let (rows, total) = read_messages(&dir, &session.session_id, 0, 10).unwrap();
        assert_eq!(total, 3);
        assert_eq!(rows[0].body, "hello");
        assert_eq!(rows[0].ts, 120);
        assert_eq!(rows[2].attachments[0].id, att);
        // The blob and its sidecar moved with it: the download surface works.
        let (meta, blob) = load_attachment(&dir, &session.session_id, &att)
            .unwrap()
            .expect("attachment resolves in the session");
        assert_eq!(meta.filename, "shot.png");
        assert_eq!(fs::read(&blob).unwrap(), b"png");

        // The sidecar badge state carries over (HI had 2 unread).
        assert_eq!(session.unread.get(HI).copied(), Some(2));
        assert_eq!(session.created_at, 100);
    }

    #[test]
    fn migration_replays_without_duplicating_rows() {
        let dir = temp_dir();
        seed_legacy_pair(&dir, None);
        migrate_legacy(&dir).unwrap();

        // Simulate the crash window: the pair index and rows are committed,
        // but the staging directory outlived the run.
        let staged = dir.join("users").join(LO).join("chats").join(format!("{HI}.migrating"));
        fs::create_dir_all(&staged).unwrap();
        fs::write(
            staged.join("conversation.json"),
            "{\"created_at\":100,\"last_active_at\":160,\"unread\":{}}",
        )
        .unwrap();
        let (rows_before, total_before) = {
            let s = pair_lookup(&dir, LO, HI).unwrap();
            read_messages(&dir, &s.session_id, 0, 10).unwrap()
        };
        let report = migrate_legacy(&dir).unwrap();
        assert_eq!(report.messages, 0, "deterministic ids refuse the replay");
        let s = pair_lookup(&dir, LO, HI).unwrap();
        let (rows_after, total_after) = read_messages(&dir, &s.session_id, 0, 10).unwrap();
        assert_eq!(total_after, total_before);
        assert_eq!(rows_after.len(), rows_before.len());
        assert!(!staged.exists(), "the stale staging dir is consumed");
    }

    #[test]
    fn migration_refuses_to_forge_attribution() {
        let dir = temp_dir();
        seed_legacy_pair(&dir, None);
        // A row claiming a third id inside the pair's log must not migrate
        // as speech of a participant.
        let pair_log = dir.join("users").join(LO).join("chats").join(HI).join("messages.jsonl");
        let mut raw = fs::read_to_string(&pair_log).unwrap();
        raw.push_str(
            &serde_json::json!({
                "seq": 4, "ts": 170,
                "from": "cccccccc-0000-4000-8000-000000000003",
                "kind": "text", "body": "forged", "attachments": []
            })
            .to_string(),
        );
        fs::write(&pair_log, raw).unwrap();

        let report = migrate_legacy(&dir).unwrap();
        assert_eq!(report.messages, 3, "the forged line is dropped");
        let s = pair_lookup(&dir, LO, HI).unwrap();
        let (rows, _) = read_messages(&dir, &s.session_id, 0, 10).unwrap();
        assert!(rows.iter().all(|r| r.from == LO || r.from == HI));
    }

    #[test]
    fn migrated_sessions_serve_the_legacy_adapter_surface() {
        // The pair index is the only join: `chat_id` on the wire keeps
        // resolving after convergence, and new sends append to the same
        // session the migration created.
        let dir = temp_dir();
        seed_legacy_pair(&dir, Some(&uuid::Uuid::new_v4().to_string()));
        migrate_legacy(&dir).unwrap();
        let session = pair_lookup(&dir, LO, HI).unwrap();
        let m = msg(LO, KIND_USER_MESSAGE, "post-migration", "");
        let (stored, fresh) = append_message(&dir, &session.session_id, &m, 200).unwrap();
        assert!(fresh);
        assert_eq!(stored.seq, 4);
        let (rows, total) = read_messages(&dir, &session.session_id, 0, 10).unwrap();
        assert_eq!(total, 4);
        assert_eq!(rows[3].body, "post-migration");
    }

    // ── The line index (no-whole-log reads) ───────────────────────

    #[test]
    fn windowed_reads_cover_long_corrupt_and_blank_lines() {
        let dir = temp_dir();
        let g = create_session(
            &dir,
            SessionKind::UserGroup,
            &[Participant::new("u1", ParticipantIdentity::Human)],
            "w",
            None,
            None,
            1,
        )
        .unwrap();
        // Empty log: an empty page, not an error.
        let (rows, total) = read_messages(&dir, &g.session_id, 0, 10).unwrap();
        assert!(rows.is_empty() && total == 0);

        append_message(&dir, &g.session_id, &msg("u1", KIND_USER_MESSAGE, "first", "w-1"), 2)
            .unwrap();
        // A line well past any chunk size reads back exactly through its span.
        let long = "x".repeat(100 * 1024);
        append_message(&dir, &g.session_id, &msg("u1", KIND_USER_MESSAGE, &long, "w-2"), 3)
            .unwrap();

        // A corrupt line and a blank line land in the log behind the
        // write entry's back (an operator edit, a torn write): the corrupt
        // line keeps a physical number but is skipped from pages, the
        // blank one keeps neither.
        {
            let mut f = OpenOptions::new()
                .append(true)
                .open(messages_path(&session_dir(&dir, &g.session_id)))
                .unwrap();
            f.write_all(b"{ not json}\n\n").unwrap();
        }
        let (stored, _) = append_message(
            &dir,
            &g.session_id,
            &msg("u1", KIND_USER_MESSAGE, "tail", "w-3"),
            4,
        )
        .unwrap();
        // The counter reconciled from the index, not from a stale meta:
        // the corrupt line consumed seq 3, so the new row lands on 4.
        assert_eq!(stored.seq, 4);

        let (rows, total) = read_messages(&dir, &g.session_id, 0, 10).unwrap();
        assert_eq!(total, 4, "corrupt line counts, blank line does not");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].body, long, "the long span survives byte-exactly");

        // Middle window: two oldest rows, newest-back offset honoured.
        let (rows, _) = read_messages(&dir, &g.session_id, 2, 2).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].body, "first");
        assert_eq!(rows[1].body, long);

        // Offset past the end: empty window, total still reported.
        let (rows, total) = read_messages(&dir, &g.session_id, 50, 5).unwrap();
        assert!(rows.is_empty());
        assert_eq!(total, 4);

        // The corrupt line holds its seq slot but no id; neighbours stay
        // fetchable through the same index.
        let tail = message_by_id(&dir, &g.session_id, "w-3").unwrap();
        assert_eq!(tail.map(|m| m.seq), Some(4));
    }

    #[test]
    fn idempotency_and_lookup_survive_a_restart() {
        // Evicting the in-memory index simulates a fresh process: the
        // sidecar alone must serve idempotency, fetch-by-id and the
        // counter — without a whole-log pass (the file is complete, so
        // the reconcile only compares lengths).
        let dir = temp_dir();
        let g = create_session(
            &dir,
            SessionKind::UserGroup,
            &[Participant::new("u1", ParticipantIdentity::Human)],
            "r",
            None,
            None,
            1,
        )
        .unwrap();
        append_message(&dir, &g.session_id, &msg("u1", KIND_USER_MESSAGE, "first", "r-1"), 2)
            .unwrap();
        let sess = session_dir(&dir, &g.session_id);
        assert!(line_index_path(&sess).exists(), "the sidecar is persisted");

        lock(&LINE_INDEXES).remove(&sess);
        let (again, fresh) =
            append_message(&dir, &g.session_id, &msg("u1", KIND_USER_MESSAGE, "first", "r-1"), 3)
                .unwrap();
        assert!(!fresh, "the redelivered id is not appended twice");
        assert_eq!(again.seq, 1);
        let (rows, total) = read_messages(&dir, &g.session_id, 0, 10).unwrap();
        assert_eq!((rows.len(), total), (1, 1));

        lock(&LINE_INDEXES).remove(&sess);
        assert_eq!(message_by_id(&dir, &g.session_id, "r-1").unwrap().map(|m| m.seq), Some(1));
    }

    #[test]
    fn list_previews_never_read_the_log() {
        // The stored preview answers the list even when the log lies:
        // deleting the tail line must not change what the sidebar shows
        // (the write path derived it once, at commit time).
        let dir = temp_dir();
        let g = create_session(
            &dir,
            SessionKind::ProjectGroup,
            &[
                Participant::new("u1", ParticipantIdentity::Human),
                Participant::new("ag", ParticipantIdentity::Agent),
            ],
            "p",
            None,
            Some("proj-1"),
            1,
        )
        .unwrap();
        append_message(&dir, &g.session_id, &msg("u1", KIND_USER_MESSAGE, "old tail", "l-1"), 2)
            .unwrap();

        let sess = session_dir(&dir, &g.session_id);
        // The tail line is gone from the log entirely (rotation, operator
        // edit) — the stored preview is unaffected.
        fs::write(messages_path(&sess), "").unwrap();
        lock(&LINE_INDEXES).remove(&sess);

        let mut rows = vec![load(&dir, &g.session_id).unwrap()];
        attach_group_previews(&dir, &mut rows);
        assert_eq!(rows[0].last_message_preview.as_deref(), Some("old tail"));
    }

    #[test]
    fn pre_field_sessions_self_heal_the_preview_once() {
        // A session written before the preview field existed: the first
        // list read derives it from the indexed tail line and persists
        // it; from then on the answer is pure metadata (log removal
        // cannot take it away).
        let dir = temp_dir();
        let g = create_session(
            &dir,
            SessionKind::ProjectGroup,
            &[
                Participant::new("u1", ParticipantIdentity::Human),
                Participant::new("ag", ParticipantIdentity::Agent),
            ],
            "h",
            None,
            Some("proj-1"),
            1,
        )
        .unwrap();
        append_message(&dir, &g.session_id, &msg("u1", KIND_USER_MESSAGE, "first", "h-1"), 2)
            .unwrap();
        append_message(
            &dir,
            &g.session_id,
            &msg("ag", KIND_AGENT_MESSAGE, "second tail", "h-2"),
            3,
        )
        .unwrap();

        let sess = session_dir(&dir, &g.session_id);
        let mut stale = load(&dir, &g.session_id).unwrap();
        stale.last_message_preview = None;
        save(&sess, &stale).unwrap();
        lock(&LINE_INDEXES).remove(&sess);

        let mut rows = vec![load(&dir, &g.session_id).unwrap()];
        attach_group_previews(&dir, &mut rows);
        assert_eq!(
            rows[0].last_message_preview.as_deref(),
            Some("second tail"),
            "derived from the tail line once"
        );
        // The heal was persisted, and the second read takes the no-log
        // path even after the log is gone entirely.
        fs::write(messages_path(&sess), "").unwrap();
        lock(&LINE_INDEXES).remove(&sess);
        let mut rows = vec![load(&dir, &g.session_id).unwrap()];
        attach_group_previews(&dir, &mut rows);
        assert_eq!(rows[0].last_message_preview.as_deref(), Some("second tail"));
    }
}
