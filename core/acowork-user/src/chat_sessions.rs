//! Chat session persistence (design doc 27 §2.2 / §3 / §8, Phase 1).
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
//!                    last_active_at, message_count (physical-line counter)
//!   messages.jsonl   append-only, each line carries a session-local
//!                    monotonic `seq` and a globally unique `message_id`
//! ```
//!
//! Concurrency follows the `chat.rs` model (ADR-092): one mutex per session
//! directory; the whole load → dedup → assign seq → append → save sequence
//! is one atomic read-modify-write under that lock. Different sessions stay
//! parallel (§8: per-session single writer, not a global serial point).
//!
//! Idempotency: a write carrying a `message_id` already present in the log
//! returns the existing message instead of appending a duplicate — QoS 1
//! redelivery and client retries must never double-write (§8-4).
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

/// Fixed message vocabulary (design 27 §2.2). Anything else is an agent
/// execution detail and has no business in a chat log.
pub const KIND_USER_MESSAGE: &str = "user_message";
pub const KIND_INTENT_REQUEST: &str = "intent_request";
pub const KIND_INTENT_RESPONSE: &str = "intent_response";

/// `project_group` sessions are keyed by project id: the same project can
/// never get two group chats (§3.3, §14 Phase 2 "幂等创建").
const KINDS: &[&str] = &[
    KIND_USER_MESSAGE,
    KIND_INTENT_REQUEST,
    KIND_INTENT_RESPONSE,
];

/// Chat session kinds (design 27 §3.2).
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

/// Intent-direction metadata every chat message carries (§4). All fields
/// are empty on a `user_message`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentMeta {
    /// Stable message identity (§4 `message_id`). Required on every
    /// intent message; the idempotency key of the write entry.
    #[serde(default)]
    pub message_id: String,
    /// The conversation this message belongs to (§6.5). Requests create
    /// it; responses reuse the request's value.
    #[serde(default)]
    pub conversation_id: String,
    /// The sender's execution session (design 27 `source_agent_session_id`).
    #[serde(default)]
    pub sender_session_id: String,
    /// For a response: the request this answers (§4, system-generated).
    #[serde(default)]
    pub in_reply_to: String,
    /// The execution session that actually handled the request, once
    /// routing resolved it (`target_agent_session_id`).
    #[serde(default)]
    pub agent_session_id: String,
    /// Repo the request concerns (§4.1). A chat log records the
    /// normalised remote URL, never a filesystem path.
    #[serde(default)]
    pub repo: String,
    /// Response outcome (§7.1): completed / failed / aborted / timeout.
    #[serde(default)]
    pub status: String,
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
    /// One of [`KIND_USER_MESSAGE`] / [`KIND_INTENT_REQUEST`] /
    /// [`KIND_INTENT_RESPONSE`].
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
}

/// `session.json` — session metadata, rewritten atomically on change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatSession {
    /// Session identity (UUID v4, server-generated).
    pub session_id: String,
    pub kind: SessionKind,
    /// Participant ids (users and/or agent instances). Deduplicated,
    /// order preserved as given.
    pub participants: Vec<String>,
    #[serde(default)]
    pub title: String,
    /// Project binding for `project_group` sessions — the idempotency key
    /// for "one project, exactly one group chat".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub created_at: i64,
    pub last_active_at: i64,
    /// Derived physical-line counter of `messages.jsonl` (same role as
    /// `Conversation::message_count` in `chat.rs`: read-path total and
    /// next `seq` come from it; recomputed from the file when it drifts).
    #[serde(default)]
    pub message_count: u64,
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
/// for the same project returns the existing session (design 27 §3.3 /
/// Phase 2). Other kinds are created as given; duplicate participant ids are
/// rejected, not silently deduped — a caller that passes the same id twice
/// is confused about the membership it asked for.
pub fn create_session(
    data_dir: &Path,
    kind: SessionKind,
    participants: &[String],
    title: &str,
    project_id: Option<&str>,
    now: i64,
) -> Result<ChatSession, String> {
    if participants.is_empty() {
        return Err("a chat session needs at least one participant".to_string());
    }
    let unique: HashSet<&String> = participants.iter().collect();
    if unique.len() != participants.len() {
        return Err("duplicate participant id in session membership".to_string());
    }
    if participants.iter().any(|p| p.is_empty() || p.contains('/') || p.contains('\\')) {
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
        let s = write_new(data_dir, kind, participants, title, Some(&pid), now)?;
        index.insert(pid, s.session_id.clone());
        write_projects_index(data_dir, &index)?;
        return Ok(s);
    }
    write_new(data_dir, kind, participants, title, None, now)
}

fn write_new(
    data_dir: &Path,
    kind: SessionKind,
    participants: &[String],
    title: &str,
    project_id: Option<&str>,
    now: i64,
) -> Result<ChatSession, String> {
    let session_id = uuid::Uuid::new_v4().to_string();
    let dir = session_dir(data_dir, &session_id);
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let s = ChatSession {
        session_id,
        kind,
        participants: participants.to_vec(),
        title: title.to_string(),
        project_id: project_id.map(str::to_string),
        created_at: now,
        last_active_at: now,
        message_count: 0,
    };
    save(&dir, &s)?;
    Ok(s)
}

/// Load `session.json`.
pub fn load(data_dir: &Path, session_id: &str) -> Result<ChatSession, String> {
    validate_session_id(session_id)?;
    let path = session_path(&session_dir(data_dir, session_id));
    let raw = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
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
/// Guarantees (design 27 §8):
/// 1. the sender must be a session participant (participant relation is the
///    permission, not the session id itself — §5.3);
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
    match msg.kind.as_str() {
        KIND_USER_MESSAGE => {
            if msg.body.trim().is_empty() {
                return Err("a user_message needs a body".to_string());
            }
        }
        KIND_INTENT_REQUEST | KIND_INTENT_RESPONSE if msg.meta.message_id.is_empty() => {
            return Err("an intent message needs a message_id".to_string());
        }
        KIND_INTENT_REQUEST | KIND_INTENT_RESPONSE => {}
        _ => unreachable!(),
    }
    if msg.kind == KIND_INTENT_RESPONSE && msg.meta.in_reply_to.is_empty() {
        return Err("an intent_response needs in_reply_to".to_string());
    }

    let dir = session_dir(data_dir, session_id);
    let tlock = session_lock(&dir);
    let _g = lock(&tlock);

    let mut session = load(data_dir, session_id)?;
    if !session.participants.iter().any(|p| p == &msg.from) {
        return Err("sender is not a participant of this session".to_string());
    }

    // Idempotency scan. ponytail: O(messages) per intent write — chat logs
    // stay small by design (§6.5 R4 caps a conversation at 6 requests) and
    // the scan shares the per-session lock with the append anyway; move to
    // an id index file if a session ever grows past tens of thousands.
    let path = messages_path(&dir);
    if !msg.meta.message_id.is_empty()
        && let Some(existing) = find_by_message_id(&path, &msg.meta.message_id)?
    {
        return Ok((existing, false));
    }

    // Reconcile the counter before allocating a seq (crash window: a line
    // durable but metadata save failed must not hand out a used seq).
    let physical = count_lines(&path)?;
    if session.message_count < physical {
        session.message_count = physical;
    }
    let seq = session.message_count + 1;

    let stored = SessionMessage {
        seq,
        ts: if msg.ts == 0 { now } else { msg.ts },
        from: msg.from.clone(),
        kind: msg.kind.clone(),
        body: msg.body.clone(),
        to: msg.to.clone(),
        meta: msg.meta.clone(),
    };
    let mut line = serde_json::to_string(&stored).map_err(|e| format!("serialize message: {e}"))?;
    line.push('\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    file.write_all(line.as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))?;

    session.message_count = seq;
    session.last_active_at = stored.ts;
    save(&dir, &session)?;
    Ok((stored, true))
}

/// Scan the log for a message id. Returns the parsed message.
fn find_by_message_id(path: &Path, message_id: &str) -> Result<Option<SessionMessage>, String> {
    let Ok(raw) = fs::read_to_string(path) else {
        return Ok(None); // no log yet
    };
    for line in raw.lines() {
        if !line.contains(message_id) {
            continue; // cheap prefilter before the parse
        }
        if let Ok(m) = serde_json::from_str::<SessionMessage>(line)
            && m.meta.message_id == message_id
        {
            return Ok(Some(m));
        }
    }
    Ok(None)
}

fn count_lines(path: &Path) -> Result<u64, String> {
    let Ok(raw) = fs::read_to_string(path) else {
        return Ok(0);
    };
    Ok(raw.lines().filter(|l| !l.trim().is_empty()).count() as u64)
}

// ── Reads ─────────────────────────────────────────────────────────────

/// A page of messages plus the total count. `offset` counts from the
/// newest message backwards (the chat UI reads its tail), `limit` is
/// capped by the caller.
pub fn read_messages(
    data_dir: &Path,
    session_id: &str,
    offset: usize,
    limit: usize,
) -> Result<(Vec<SessionMessage>, u64), String> {
    validate_session_id(session_id)?;
    let dir = session_dir(data_dir, session_id);
    let path = messages_path(&dir);
    let raw = match fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    let total = lines.len() as u64;
    let end = total.saturating_sub(offset as u64) as usize;
    let start = end.saturating_sub(limit.max(1));
    let mut out = Vec::with_capacity(end - start);
    for line in &lines[start..end] {
        // One corrupt line must not brick the history (same rule as chat.rs).
        match serde_json::from_str::<SessionMessage>(line) {
            Ok(m) => out.push(m),
            Err(e) => tracing::warn!(
                path = %path.display(), error = %e, "skipping corrupt message line"
            ),
        }
    }
    Ok((out, total))
}

/// List the sessions `participant` belongs to, newest activity first.
///
/// ponytail: full scan of `chat-sessions/*/session.json`. The session count
/// per deployment is small (groups + agent pairs, not per-message); add an
/// index file if it ever shows up in a profile.
pub fn list_for(data_dir: &Path, participant: &str) -> Result<Vec<ChatSession>, String> {
    let root = data_dir.join("chat-sessions");
    let mut out = Vec::new();
    let entries = match fs::read_dir(&root) {
        Ok(e) => e,
        Err(_) => return Ok(out), // no sessions yet
    };
    for entry in entries.flatten() {
        let path = entry.path().join("session.json");
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(s) = serde_json::from_str::<ChatSession>(&raw)
            && s.participants.iter().any(|p| p == participant)
        {
            out.push(s);
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.last_active_at));
    Ok(out)
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
        }
    }

    #[test]
    fn create_and_load_roundtrip() {
        let dir = temp_dir();
        let s = create_session(&dir, SessionKind::UserGroup, &["u1".into(), "u2".into()], "team", None, 100)
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
            Some("proj-1"),
            1,
        )
        .unwrap();
        let b = create_session(
            &dir,
            SessionKind::ProjectGroup,
            &["owner".into()],
            "p",
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
            create_session(&dir, SessionKind::UserGroup, &["u".into(), "u".into()], "", None, 1)
                .is_err()
        );
        assert!(
            create_session(&dir, SessionKind::ProjectGroup, &["u".into()], "", None, 1).is_err()
        );
    }

    #[test]
    fn append_assigns_monotonic_seq_and_validates_participation() {
        let dir = temp_dir();
        let s =
            create_session(&dir, SessionKind::UserUser, &["a".into(), "b".into()], "", None, 1)
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
            1,
        )
        .unwrap();
        let m = msg("inst-a", KIND_INTENT_REQUEST, "do it", "mid-1");
        let (first, wrote1) = append_message(&dir, &s.session_id, &m, 5).unwrap();
        let (second, wrote2) = append_message(&dir, &s.session_id, &m, 9).unwrap();
        assert!(wrote1 && !wrote2, "redelivery must not double-write");
        assert_eq!(first.seq, second.seq);
        assert_eq!(second.ts, first.ts, "the stored row is returned verbatim");
        let (_, total) = read_messages(&dir, &s.session_id, 0, 10).unwrap();
        assert_eq!(total, 1);
    }

    #[test]
    fn intent_kind_schema_is_enforced() {
        let dir = temp_dir();
        let s = create_session(&dir, SessionKind::UserAgent, &["u".into(), "ag".into()], "", None, 1)
            .unwrap();
        // request without message_id
        assert!(append_message(&dir, &s.session_id, &msg("u", KIND_INTENT_REQUEST, "x", ""), 1)
            .is_err());
        // response without in_reply_to
        let mut r = msg("ag", KIND_INTENT_RESPONSE, "x", "mid-2");
        assert!(append_message(&dir, &s.session_id, &r, 1).is_err());
        r.meta.in_reply_to = "mid-0".into();
        assert!(append_message(&dir, &s.session_id, &r, 1).is_ok());
        // unknown kind
        assert!(append_message(&dir, &s.session_id, &msg("u", "agent_thought", "x", ""), 1).is_err());
    }

    #[test]
    fn seq_survives_metadata_counter_crash_window() {
        // Simulate: JSONL line durable, session.json save failed → counter
        // behind. The next append must reconcile, not reuse the seq.
        let dir = temp_dir();
        let s = create_session(&dir, SessionKind::UserUser, &["a".into(), "b".into()], "", None, 1)
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
        let s1 = create_session(&dir, SessionKind::UserGroup, &["x".into(), "y".into()], "", None, 1)
            .unwrap();
        let _s2 = create_session(&dir, SessionKind::UserGroup, &["y".into(), "z".into()], "", None, 2)
            .unwrap();
        let xs = list_for(&dir, "x").unwrap();
        assert_eq!(xs.len(), 1);
        assert_eq!(xs[0].session_id, s1.session_id);
        assert_eq!(list_for(&dir, "nobody").unwrap().len(), 0);
    }
}
