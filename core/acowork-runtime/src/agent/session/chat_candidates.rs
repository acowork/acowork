//! Chat Router candidate construction (ADR-099 D5 / design 29 §5.2).
//!
//! A router turn can only route to execution sessions the Chat surface is
//! allowed to know about, so the filter — not the prompt — is where isolation
//! lives. It is a pure function over the session-meta rows, kept apart from
//! `SessionManager` so the two rules below can be tested against synthetic
//! rows and never drift:
//!
//! - **1:1** — the chat belongs to one human. Candidates are the sessions
//!   bound to this chat (the current one plus any predecessor bound to it) and
//!   the sessions that human started by hand (unbound, same owner). Another
//!   account's sessions are invisible, whatever their binding.
//! - **group** — membership *is* the permission (design 27 §8.2). Candidates
//!   are the sessions bound to this group chat **and initiated by the
//!   speaker**. A group executing session belongs to the participant who
//!   started it (ADR-076 §决策 4: one initiator, no multi-user session), so
//!   "the work *I* have running here" is the honest list; another member's
//!   session is not routable from my turn and would otherwise swallow my
//!   message into their conversation. A 1:1 chat's session must never list in
//!   a group, and a group's sessions must never list in a 1:1 chat.
//!
//! Ordering is `current` first, then most recently active, then title, then
//! session id — fully determined. Block C is re-rendered every turn, so an
//! unstable order would churn the cached tail for no reason.
//!
//! Only metadata is read (ADR-096): title, activity stamp, binding, origin.
//! The session JSONL is never touched here.

use std::collections::{HashMap, HashSet};

use crate::agent::session_state::SessionStatus;
use crate::conversation::SessionMeta;

/// Longest title carried into the candidate line, in bytes.
const TITLE_MAX_BYTES: usize = 80;

/// Which surface the Chat session is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateSurface {
    /// 1:1 chat between one human and this agent.
    Direct,
    /// Group chat (`user_group` / `project_group`).
    Group,
}

/// Status vocabulary shown to the router (design 29 §5.2). Deliberately
/// coarser than [`crate::agent::session_state::SessionStatus`]: the router
/// only needs to know whether a session is busy, ready, broken, or needs
/// reopening.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateStatus {
    Working,
    Idle,
    Errored,
    Closed,
}

impl CandidateStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Errored => "errored",
            Self::Closed => "closed",
        }
    }
}

/// Map the Runtime's fine-grained status onto this vocabulary.
fn candidate_status(status: &SessionStatus) -> Option<CandidateStatus> {
    match status {
        SessionStatus::Idle => Some(CandidateStatus::Idle),
        SessionStatus::LlmAwaitingFirstChunk
        | SessionStatus::Thinking
        | SessionStatus::LlmStreaming { .. }
        | SessionStatus::ToolExecuting
        | SessionStatus::WaitingApproval { .. }
        | SessionStatus::Paused { .. } => Some(CandidateStatus::Working),
        SessionStatus::Errored { .. } => Some(CandidateStatus::Errored),
    }
}

/// Status of a live session, in the candidate vocabulary (one mapping table,
/// shared by `SessionManager::resolve_candidates` and the router prompt).
pub fn live_status(status: &SessionStatus) -> Option<CandidateStatus> {
    candidate_status(status)
}

/// Read the candidate rows out of the session-meta store.
///
/// A separate entry point so the Chat Router can do this scan **outside** the
/// global `SessionManager` lock: it is the slowest step of a turn's setup and
/// holding that lock across a filesystem walk would stall session creation
/// (design 29 §5).
pub fn scan_rows(work_dir: &str) -> Vec<(String, SessionMeta)> {
    crate::conversation::scan_sessions_from_meta(
        &std::path::Path::new(work_dir).join("conversations"),
    )
}

/// Where the execution session came from, as far as the router may know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateOrigin {
    /// Bound to a Chat session (this one, or a predecessor bound to it).
    Chat,
    /// A PM task chain (`conv:task:…` mapping). Design 27 §6.2: the task is
    /// the lifecycle boundary — the mapping lives until the task closes.
    Task,
}

impl CandidateOrigin {
    pub fn label(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Task => "task",
        }
    }
}

/// One routable execution session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCandidate {
    pub session_id: String,
    pub title: String,
    pub status: CandidateStatus,
    pub origin: CandidateOrigin,
    /// `meta.last_active_at` as epoch seconds (0 when unparseable).
    pub last_active_secs: i64,
    /// The session the Chat session is currently mapped to.
    pub current: bool,
}

/// Everything the filter needs besides the meta rows.
pub struct CandidateQuery<'a> {
    pub chat_session_id: &'a str,
    /// The speaking human (the initiator of this turn). The candidate list
    /// only ever offers their own sessions — on every surface.
    pub owner_user_id: Option<&'a str>,
    pub surface: CandidateSurface,
    /// The session the chat is currently mapped to (marked `current`).
    pub current_session_id: Option<&'a str>,
    /// Session ids reachable through a `conv:task:` mapping.
    pub task_session_ids: &'a HashSet<String>,
    /// Sessions whose PM task already reached a terminal state. Never
    /// "revived" by routing a message back into them.
    pub terminal_session_ids: &'a HashSet<String>,
    pub max: usize,
}

/// Build the candidate list. `live` carries the status of the sessions that
/// are currently open; anything else exists only on disk and reads `closed`
/// (which the dispatcher reopens before delivering).
pub fn build_candidates(
    rows: &[(String, SessionMeta)],
    live: &HashMap<String, CandidateStatus>,
    query: &CandidateQuery<'_>,
) -> Vec<SessionCandidate> {
    let mut out: Vec<SessionCandidate> = rows
        .iter()
        .filter(|(sid, _)| !query.terminal_session_ids.contains(sid))
        .filter(|(_, meta)| is_visible(meta, query))
        .map(|(sid, meta)| SessionCandidate {
            session_id: sid.clone(),
            title: title_of(meta),
            status: live.get(sid).copied().unwrap_or(CandidateStatus::Closed),
            origin: if query.task_session_ids.contains(sid) {
                CandidateOrigin::Task
            } else {
                CandidateOrigin::Chat
            },
            last_active_secs: active_secs(meta),
            current: query.current_session_id == Some(sid.as_str()),
        })
        .collect();

    out.sort_by(|a, b| {
        b.current
            .cmp(&a.current)
            .then_with(|| b.last_active_secs.cmp(&a.last_active_secs))
            .then_with(|| a.title.cmp(&b.title))
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    out.truncate(query.max.max(1));
    out
}

/// The two visibility rules (module docs). A session bound to another chat is
/// invisible on every surface, which is what keeps a group's `Members`-visible
/// sessions out of an unrelated private chat's candidate list.
///
/// Everything is also gated on the initiator: a session somebody else started
/// is not this turn's business on any surface. An ownerless session
/// (pre-accounts, local mode) stays visible — there is nobody it could belong
/// to instead.
fn is_visible(meta: &SessionMeta, query: &CandidateQuery<'_>) -> bool {
    let bound = meta.chat_session_id.as_deref();
    if bound == Some(query.chat_session_id) {
        return is_own(meta, query);
    }
    match query.surface {
        CandidateSurface::Group => false,
        CandidateSurface::Direct => {
            // Unbound sessions are the human's own manual sessions, so an
            // unowned one is not theirs to list: responder sessions the
            // agent started on its own have no initiator to match, and
            // handing those to every account is what made history look
            // shared.
            bound.is_none() && meta.initiator.is_some() && is_own(meta, query)
        }
    }
}

/// ADR-076 §决策 4: the session was initiated by the speaking human (or by
/// nobody, which predates the notion).
fn is_own(meta: &SessionMeta, query: &CandidateQuery<'_>) -> bool {
    match (meta.initiator.as_deref(), query.owner_user_id) {
        (None, _) => true,
        (Some(initiator), Some(speaker)) => initiator == speaker,
        // An owned session with no known speaker fails CLOSED: "we cannot
        // tell who is asking" must not mean "show them everybody's runs". A
        // local-mode deployment never reaches this arm — with no accounts,
        // no session carries an initiator — so the only turns affected are
        // the degenerate ones (an older Gateway that sent no `from`), and for
        // those the router opens a session of its own instead of guessing.
        (Some(_), None) => false,
    }
}

/// One-line, bounded title. A title is free text and may contain newlines;
/// the candidate block is one line per session, so control characters must
/// not forge a second line.
fn title_of(meta: &SessionMeta) -> String {
    let raw = meta
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("untitled");
    let mut out = String::with_capacity(raw.len().min(TITLE_MAX_BYTES));
    for ch in raw.chars() {
        if out.len() + ch.len_utf8() > TITLE_MAX_BYTES {
            break;
        }
        out.push(if ch.is_control() { ' ' } else { ch });
    }
    let cleaned = out.trim();
    if cleaned.is_empty() {
        "untitled".to_string()
    } else {
        cleaned.to_string()
    }
}

fn active_secs(meta: &SessionMeta) -> i64 {
    chrono::DateTime::parse_from_rfc3339(&meta.last_active_at)
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every Runtime session state maps to a candidate label — a state left
    /// unmapped would make a live session look `closed` to the router.
    #[test]
    fn live_status_maps_every_runtime_state() {
        assert_eq!(candidate_status(&SessionStatus::Idle), Some(CandidateStatus::Idle));
        assert_eq!(
            candidate_status(&SessionStatus::Thinking),
            Some(CandidateStatus::Working)
        );
        assert_eq!(
            candidate_status(&SessionStatus::LlmStreaming { message_id: None }),
            Some(CandidateStatus::Working)
        );
        assert_eq!(
            candidate_status(&SessionStatus::Errored {
                reason: "x".into(),
                message: "y".into(),
                last_iteration: None,
                recoverable: false,
            }),
            Some(CandidateStatus::Errored)
        );
    }

    const CHAT: &str = "chat-1";
    const ALICE: &str = "alice";

    fn meta(session_id: &str, title: &str, active: &str, bound: Option<&str>, owner: Option<&str>) -> SessionMeta {
        SessionMeta {
            version: 3,
            session_id: session_id.to_string(),
            agent_id: "com.acowork.test".to_string(),
            created_at: "2026-10-01T00:00:00+00:00".to_string(),
            initiator: owner.map(str::to_string),
            visibility: None,
            chat_session_id: bound.map(str::to_string),
            title: Some(title.to_string()),
            workspace_id: None,
            model: None,
            provider: None,
            account_id: None,
            reasoning_effort: None,
            temperature: None,
            context_window: None,
            todos: None,
            message_count: 1,
            last_active_at: active.to_string(),
            tokens: None,
            llm_call_counter: None,
            perf: None,
            model_ratio: None,
            last_compaction_offset: None,
            corrupted: false,
            model_usage: None,
        }
    }

    fn empty_set() -> HashSet<String> {
        HashSet::new()
    }

    fn query<'a>(
        surface: CandidateSurface,
        current: Option<&'a str>,
        tasks: &'a HashSet<String>,
        terminal: &'a HashSet<String>,
        max: usize,
    ) -> CandidateQuery<'a> {
        CandidateQuery {
            chat_session_id: CHAT,
            owner_user_id: Some(ALICE),
            surface,
            current_session_id: current,
            task_session_ids: tasks,
            terminal_session_ids: terminal,
            max,
        }
    }

    fn ids(candidates: &[SessionCandidate]) -> Vec<&str> {
        candidates.iter().map(|c| c.session_id.as_str()).collect()
    }

    #[test]
    fn direct_lists_bound_predecessors_and_the_owners_manual_session() {
        let rows = vec![
            ("s-cur".to_string(), meta("s-cur", "Current", "2026-10-21T10:00:00+00:00", Some(CHAT), Some(ALICE))),
            ("s-old".to_string(), meta("s-old", "Predecessor", "2026-10-20T10:00:00+00:00", Some(CHAT), Some(ALICE))),
            ("s-manual".to_string(), meta("s-manual", "Manual", "2026-10-19T10:00:00+00:00", None, Some(ALICE))),
            // Another chat's session: never visible here.
            ("s-other-chat".to_string(), meta("s-other-chat", "Elsewhere", "2026-10-21T11:00:00+00:00", Some("chat-2"), Some(ALICE))),
            // Same human, but owned by someone else.
            ("s-bob".to_string(), meta("s-bob", "Bob", "2026-10-21T11:00:00+00:00", None, Some("bob"))),
            // Agent-owned responder session with no binding (must not leak).
            ("s-agent".to_string(), meta("s-agent", "Agent", "2026-10-21T11:00:00+00:00", None, None)),
        ];
        let tasks = empty_set();
        let terminal = empty_set();
        let out = build_candidates(
            &rows,
            &HashMap::new(),
            &query(CandidateSurface::Direct, Some("s-cur"), &tasks, &terminal, 10),
        );
        assert_eq!(ids(&out), vec!["s-cur", "s-old", "s-manual"]);
        assert!(out[0].current);
        assert!(!out[1].current);
    }

    #[test]
    fn group_lists_only_sessions_bound_to_that_group() {
        let rows = vec![
            ("g-cur".to_string(), meta("g-cur", "Group task", "2026-10-21T10:00:00+00:00", Some("group-1"), None)),
            ("g-task".to_string(), meta("g-task", "PM chain", "2026-10-20T10:00:00+00:00", Some("group-1"), None)),
            // The same human's private chat session must not appear.
            ("dm".to_string(), meta("dm", "Private", "2026-10-21T11:00:00+00:00", Some("chat-1"), Some(ALICE))),
            // An unbound manual session is not a group candidate either.
            ("manual".to_string(), meta("manual", "Manual", "2026-10-21T11:00:00+00:00", None, Some(ALICE))),
        ];
        let tasks: HashSet<String> = ["g-task".to_string()].into_iter().collect();
        let terminal = empty_set();
        let mut q = query(CandidateSurface::Group, None, &tasks, &terminal, 10);
        q.chat_session_id = "group-1";
        let out = build_candidates(&rows, &HashMap::new(), &q);
        assert_eq!(ids(&out), vec!["g-cur", "g-task"]);
        assert_eq!(out[1].origin, CandidateOrigin::Task);
        assert_eq!(out[0].origin, CandidateOrigin::Chat);
    }

    #[test]
    fn terminal_sessions_are_excluded() {
        let rows = vec![
            ("live".to_string(), meta("live", "Live", "2026-10-21T10:00:00+00:00", Some(CHAT), Some(ALICE))),
            ("done".to_string(), meta("done", "Finished task", "2026-10-21T11:00:00+00:00", Some(CHAT), Some(ALICE))),
        ];
        let tasks = empty_set();
        let terminal: HashSet<String> = ["done".to_string()].into_iter().collect();
        let out = build_candidates(
            &rows,
            &HashMap::new(),
            &query(CandidateSurface::Direct, None, &tasks, &terminal, 10),
        );
        assert_eq!(ids(&out), vec!["live"]);
    }

    /// ADR-076 §决策 4: one execution session per initiator. Two members of
    /// the same group chat each see their own runs there — never each
    /// other's, which is what kept a second person out of a session that
    /// belongs to the first.
    #[test]
    fn group_candidates_are_scoped_to_the_speaking_member() {
        let rows = vec![
            ("g-alice".to_string(), meta("g-alice", "Alice run", "2026-10-21T10:00:00+00:00", Some(CHAT), Some(ALICE))),
            ("g-bob".to_string(), meta("g-bob", "Bob run", "2026-10-21T11:00:00+00:00", Some(CHAT), Some("bob"))),
            // Pre-accounts session: no initiator to compare, stays visible.
            ("g-legacy".to_string(), meta("g-legacy", "Legacy", "2026-10-21T09:00:00+00:00", Some(CHAT), None)),
        ];
        let tasks = empty_set();
        let terminal = empty_set();
        let out = build_candidates(
            &rows,
            &HashMap::new(),
            &query(CandidateSurface::Group, None, &tasks, &terminal, 10),
        );
        assert_eq!(ids(&out), vec!["g-alice", "g-legacy"]);
    }

    /// "We cannot tell who is asking" must not mean "show them every run in
    /// this chat": an owned session drops out of the candidate list, while a
    /// pre-accounts (unowned) one stays — there is nobody it could belong to
    /// instead. Fail closed on the only axis that can leak.
    #[test]
    fn an_unknown_speaker_sees_no_owned_sessions() {
        let rows = vec![
            ("g-alice".to_string(), meta("g-alice", "Alice run", "2026-10-21T10:00:00+00:00", Some(CHAT), Some(ALICE))),
            ("g-legacy".to_string(), meta("g-legacy", "Legacy", "2026-10-21T09:00:00+00:00", Some(CHAT), None)),
        ];
        let tasks = empty_set();
        let terminal = empty_set();
        let mut q = query(CandidateSurface::Group, None, &tasks, &terminal, 10);
        q.owner_user_id = None;
        let out = build_candidates(&rows, &HashMap::new(), &q);
        assert_eq!(ids(&out), vec!["g-legacy"]);
    }

    #[test]
    fn current_first_then_recency_then_title() {
        let rows = vec![
            ("s-a".to_string(), meta("s-a", "Alpha", "2026-10-21T09:00:00+00:00", Some(CHAT), Some(ALICE))),
            ("s-b".to_string(), meta("s-b", "Beta", "2026-10-21T12:00:00+00:00", Some(CHAT), Some(ALICE))),
            ("s-c".to_string(), meta("s-c", "Gamma", "2026-10-21T12:00:00+00:00", Some(CHAT), Some(ALICE))),
            ("s-cur".to_string(), meta("s-cur", "Zeta", "2026-10-01T00:00:00+00:00", Some(CHAT), Some(ALICE))),
        ];
        let tasks = empty_set();
        let terminal = empty_set();
        let out = build_candidates(
            &rows,
            &HashMap::new(),
            &query(CandidateSurface::Direct, Some("s-cur"), &tasks, &terminal, 10),
        );
        // current wins over recency; the equal timestamps break on title.
        assert_eq!(ids(&out), vec!["s-cur", "s-b", "s-c", "s-a"]);
    }

    #[test]
    fn status_comes_from_the_live_map_and_defaults_to_closed() {
        let rows = vec![
            ("busy".to_string(), meta("busy", "Busy", "2026-10-21T10:00:00+00:00", Some(CHAT), Some(ALICE))),
            ("open".to_string(), meta("open", "Open", "2026-10-21T09:00:00+00:00", Some(CHAT), Some(ALICE))),
            ("gone".to_string(), meta("gone", "Gone", "2026-10-21T08:00:00+00:00", Some(CHAT), Some(ALICE))),
        ];
        let live: HashMap<String, CandidateStatus> = [
            ("busy".to_string(), CandidateStatus::Working),
            ("open".to_string(), CandidateStatus::Idle),
        ]
        .into_iter()
        .collect();
        let tasks = empty_set();
        let terminal = empty_set();
        let out = build_candidates(
            &rows,
            &live,
            &query(CandidateSurface::Direct, None, &tasks, &terminal, 10),
        );
        assert_eq!(out[0].status.label(), "working");
        assert_eq!(out[1].status.label(), "idle");
        assert_eq!(out[2].status.label(), "closed");
    }

    #[test]
    fn cap_applies_after_ordering_and_titles_stay_one_line() {
        let mut rows = Vec::new();
        for i in 0..5 {
            rows.push((
                format!("s{i}"),
                meta(
                    &format!("s{i}"),
                    &format!("Title {i}\nsecond line"),
                    &format!("2026-10-2{}T10:00:00+00:00", i),
                    Some(CHAT),
                    Some(ALICE),
                ),
            ));
        }
        let tasks = empty_set();
        let terminal = empty_set();
        let out = build_candidates(
            &rows,
            &HashMap::new(),
            &query(CandidateSurface::Direct, None, &tasks, &terminal, 2),
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].session_id, "s4");
        assert!(!out[0].title.contains('\n'));
        assert_eq!(out[0].title, "Title 4 second line");
    }

    #[test]
    fn untitled_sessions_get_a_stable_placeholder() {
        let mut m = meta("s", "", "2026-10-21T10:00:00+00:00", Some(CHAT), Some(ALICE));
        m.title = Some("\n \n".to_string());
        let rows = vec![("s".to_string(), m)];
        let tasks = empty_set();
        let terminal = empty_set();
        let out = build_candidates(
            &rows,
            &HashMap::new(),
            &query(CandidateSurface::Direct, None, &tasks, &terminal, 10),
        );
        assert_eq!(out[0].title, "untitled");
    }
}
