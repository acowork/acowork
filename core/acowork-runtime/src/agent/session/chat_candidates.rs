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
//! Ordering is most recently active first, then title, then session id — fully
//! determined. Block C is re-rendered every turn, so an unstable order would
//! churn the cached tail for no reason. There is no "current" session any more
//! (ADR-106 D11): nothing maps a Chat to one execution session, so recency is
//! the honest statement of what this chat last worked on.
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

/// Read the intent-routing index (ADR-106 D11): which sessions own an intent
/// context, and which of those contexts are over.
///
/// A second tiny metadata read next to [`scan_rows`], deliberately outside the
/// `SessionManager` lock for the same reason. An unavailable store reads as "no
/// bindings": the candidate list loses its `task` origin marks and its terminal
/// exclusions, which costs a turn one stale candidate — never a wrong route.
pub fn scan_bindings(work_dir: &str) -> Vec<acowork_core::session_meta::IntentBinding> {
    match crate::conversation::session_meta_store(
        &std::path::Path::new(work_dir).join("conversations"),
    ) {
        Ok(store) => store.intent_bindings().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "intent bindings unavailable — candidate list built without them");
            Vec::new()
        }),
        Err(e) => {
            tracing::warn!(error = %e, "intent bindings unreadable — candidate list built without them");
            Vec::new()
        }
    }
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
}

impl SessionCandidate {
    /// The option row a Chat choice card shows for this candidate (ADR-106 D6
    /// levels 3 and 5).
    ///
    /// Shaped here rather than at each card writer because the card's promise is
    /// "these are the sessions you may continue": the id a human clicks is the
    /// same id the permission filter admitted, and a writer that renamed or
    /// dropped a field would be handing back an address nothing can resolve.
    pub fn choice_option(&self) -> serde_json::Value {
        serde_json::json!({
            "session_id": self.session_id,
            "label": self.title,
            "status": self.status.label(),
            "origin": self.origin.label(),
        })
    }
}

/// The option row a `confirm_continue` card shows (ADR-106 D6 level 5).
///
/// The card proposes exactly one session, and the human's click is a delivery —
/// so the row has to carry what the decision needs: the candidate's own title
/// (`alias` is the last-resort handle for a session that is not in this turn's
/// candidate list), the router's one-line reason, and the state the dot paints
/// ("this one is mid-round" is a reason to think twice). Absent candidate means
/// absent status: a card must not claim a state it was not told.
pub fn proposal_option(
    session_id: &str,
    alias: &str,
    why: &str,
    candidate: Option<&SessionCandidate>,
) -> serde_json::Value {
    serde_json::json!({
        "session_id": session_id,
        "label": candidate.map(|c| c.title.clone()).unwrap_or_else(|| alias.to_string()),
        "status": candidate.map(|c| c.status.label()),
        "why": why,
    })
}

/// Everything the filter needs besides the meta rows.
pub struct CandidateQuery<'a> {
    pub chat_session_id: &'a str,
    /// The speaking human (the initiator of this turn). The candidate list
    /// only ever offers their own sessions — on every surface.
    pub owner_user_id: Option<&'a str>,
    pub surface: CandidateSurface,
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
        })
        .collect();

    out.sort_by(|a, b| {
        b.last_active_secs
            .cmp(&a.last_active_secs)
            .then_with(|| a.title.cmp(&b.title))
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    out.truncate(query.max.max(1));
    out
}

/// The routable sessions of one chat (ADR-106 D6 level 3).
///
/// Exactly the candidate filter — visibility, initiator ownership, terminal
/// exclusion — without the live-status pass: a session that exists only on disk
/// is still addressable, the dispatcher reopens it. `SessionManager::chat_continuation`
/// asks this so the deterministic route can say "there is one session here and no
/// decision to make" instead of keeping a pointer to "the last one", and the
/// level-3 choice card asks it for the options to show when there are several —
/// which is why the candidates come back whole, not as ids.
///
/// `task_session_ids` is only an origin label, so an empty set is harmless and
/// `max` is left unbounded — the caller needs the true count, not a truncated
/// one.
pub fn deliverable_candidates(
    rows: &[(String, SessionMeta)],
    chat_session_id: &str,
    owner_user_id: Option<&str>,
    surface: CandidateSurface,
    terminal_session_ids: &HashSet<String>,
) -> Vec<SessionCandidate> {
    let task_session_ids = HashSet::new();
    let query = CandidateQuery {
        chat_session_id,
        owner_user_id,
        surface,
        task_session_ids: &task_session_ids,
        terminal_session_ids,
        max: usize::MAX,
    };
    build_candidates(rows, &HashMap::new(), &query)
}

/// The same read rule as an owned value, for a caller that filters rows outside
/// the router (ADR-106 D7: `GET /sessions?chat_session_id=&surface=`).
///
/// [`CandidateQuery`] is the router's shape — it borrows the sets it needs and
/// carries the PM-origin labels a listing never renders. This owns its inputs so
/// the session scan can move it into a blocking task as a row predicate without
/// borrowing the HTTP request or a lock. The rule itself stays `is_visible`:
/// one implementation for both entry points, because a second copy of a
/// permission filter is how the two drift apart.
#[derive(Debug, Clone)]
pub struct ChatScopeRule {
    pub chat_session_id: String,
    /// The reading human, taken from the Gateway-injected scope headers — never
    /// from the query string. `None` = local mode (no accounts to attribute).
    pub owner_user_id: Option<String>,
    pub surface: CandidateSurface,
    /// Sessions whose PM task already reached a terminal state: not addressable
    /// again, so not listable either.
    pub terminal_session_ids: HashSet<String>,
}

impl ChatScopeRule {
    /// May this session be listed for this chat, to this reader?
    ///
    /// Exactly the candidate filter — terminal exclusion, chat binding, initiator
    /// ownership — with no display policy on top. Ordering, paging, and how far
    /// back to look are the listing's business; this only says what is legal.
    pub fn allows(&self, meta: &SessionMeta) -> bool {
        if self.terminal_session_ids.contains(&meta.session_id) {
            return false;
        }
        // The PM-origin set is a label, never a gate, so an empty one is
        // harmless here — and `HashSet::new` allocates nothing until inserted
        // into, which is the point of building it per call.
        let task_session_ids = HashSet::new();
        let query = CandidateQuery {
            chat_session_id: &self.chat_session_id,
            owner_user_id: self.owner_user_id.as_deref(),
            surface: self.surface,
            task_session_ids: &task_session_ids,
            terminal_session_ids: &self.terminal_session_ids,
            max: usize::MAX,
        };
        is_visible(meta, &query)
    }
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
    // The cut is at a character boundary, never inside one, so the string stays
    // valid UTF-8 — but a title that simply stops mid-word reads to a human as
    // broken text ("...一直没有创建执行sessi"), and this string is not only the
    // router's input any more: the choice card renders it as the option a human
    // clicks. The marker says "there was more", not "the title ends here".
    let mut truncated = false;
    for ch in raw.chars() {
        if out.len() + ch.len_utf8() > TITLE_MAX_BYTES {
            truncated = true;
            break;
        }
        out.push(if ch.is_control() { ' ' } else { ch });
    }
    let cleaned = out.trim();
    if cleaned.is_empty() {
        "untitled".to_string()
    } else if truncated {
        format!("{cleaned}…")
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
            aware_users: Vec::new(),
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
            tool_policy: None,
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
        tasks: &'a HashSet<String>,
        terminal: &'a HashSet<String>,
        max: usize,
    ) -> CandidateQuery<'a> {
        CandidateQuery {
            chat_session_id: CHAT,
            owner_user_id: Some(ALICE),
            surface,
            task_session_ids: tasks,
            terminal_session_ids: terminal,
            max,
        }
    }

    /// The title is shown to a human as the card option's label, so a byte cut
    /// has to be marked and must never split a character.
    #[test]
    fn an_overlong_title_is_cut_on_a_char_boundary_and_marked() {
        let long = "整理".repeat(60); // 360 bytes of 3-byte characters
        let cut = title_of(&meta("s", &long, "2026-10-21T10:00:00+00:00", None, Some(ALICE)));
        assert!(cut.ends_with('…'), "a truncated title must be marked: {cut}");
        let body = cut.trim_end_matches('…');
        assert!(body.len() <= TITLE_MAX_BYTES, "kept {body_len} bytes", body_len = body.len());
        assert!(body.chars().all(|c| c == '整' || c == '理'), "no partial character survived");
        // A title that fits is returned verbatim, marker and all absent.
        let short = title_of(&meta("s", "整理报表", "2026-10-21T10:00:00+00:00", None, Some(ALICE)));
        assert_eq!(short, "整理报表");
    }

    fn ids(candidates: &[SessionCandidate]) -> Vec<&str> {
        candidates.iter().map(|c| c.session_id.as_str()).collect()
    }

    /// The single-candidate card's row is built in one place, so the title, the
    /// reason and the status cannot drift apart between the router and the card.
    #[test]
    fn a_proposal_row_names_the_candidate_and_its_state() {
        let rows = vec![(
            "s-cur".to_string(),
            meta("s-cur", "整理报表", "2026-10-21T10:00:00+00:00", Some(CHAT), Some(ALICE)),
        )];
        let live: HashMap<String, CandidateStatus> =
            [("s-cur".to_string(), CandidateStatus::Working)].into_iter().collect();
        let built = build_candidates(&rows, &live, &query(CandidateSurface::Direct, &empty_set(), &empty_set(), 5));
        let option = proposal_option("s-cur", "S1", "最近活跃", built.first());
        assert_eq!(option["label"], "整理报表");
        assert_eq!(option["status"], "working");
        assert_eq!(option["why"], "最近活跃");

        // A session the filter did not admit is named by its alias and claims no
        // status — the card cannot paint a state it was never told.
        let orphan = proposal_option("s-gone", "S2", "别名解析所得", None);
        assert_eq!(orphan["label"], "S2");
        assert!(orphan["status"].is_null());
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
            &query(CandidateSurface::Direct, &tasks, &terminal, 10),
        );
        assert_eq!(ids(&out), vec!["s-cur", "s-old", "s-manual"]);
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
        let mut q = query(CandidateSurface::Group, &tasks, &terminal, 10);
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
            &query(CandidateSurface::Direct, &tasks, &terminal, 10),
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
            &query(CandidateSurface::Group, &tasks, &terminal, 10),
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
        let mut q = query(CandidateSurface::Group, &tasks, &terminal, 10);
        q.owner_user_id = None;
        let out = build_candidates(&rows, &HashMap::new(), &q);
        assert_eq!(ids(&out), vec!["g-legacy"]);
    }

    /// Recency is the only ordering claim left (ADR-106 D11 retired the
    /// "current" mapping), and equal timestamps break on title, then id.
    #[test]
    fn most_recently_active_first_then_title() {
        let rows = vec![
            ("s-a".to_string(), meta("s-a", "Alpha", "2026-10-21T09:00:00+00:00", Some(CHAT), Some(ALICE))),
            ("s-b".to_string(), meta("s-b", "Beta", "2026-10-21T12:00:00+00:00", Some(CHAT), Some(ALICE))),
            ("s-c".to_string(), meta("s-c", "Gamma", "2026-10-21T12:00:00+00:00", Some(CHAT), Some(ALICE))),
            ("s-old".to_string(), meta("s-old", "Zeta", "2026-10-01T00:00:00+00:00", Some(CHAT), Some(ALICE))),
        ];
        let tasks = empty_set();
        let terminal = empty_set();
        let out = build_candidates(
            &rows,
            &HashMap::new(),
            &query(CandidateSurface::Direct, &tasks, &terminal, 10),
        );
        assert_eq!(ids(&out), vec!["s-b", "s-c", "s-a", "s-old"]);
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
            &query(CandidateSurface::Direct, &tasks, &terminal, 10),
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
            &query(CandidateSurface::Direct, &tasks, &terminal, 2),
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
            &query(CandidateSurface::Direct, &tasks, &terminal, 10),
        );
        assert_eq!(out[0].title, "untitled");
    }

    fn rule(surface: CandidateSurface, terminal: &[&str]) -> ChatScopeRule {
        ChatScopeRule {
            chat_session_id: CHAT.to_string(),
            owner_user_id: Some(ALICE.to_string()),
            surface,
            terminal_session_ids: terminal.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// ADR-106 D7: `GET /sessions?chat_session_id=` filters through
    /// [`ChatScopeRule`], so it must agree with the router's own filter row for
    /// row. A listing that admits one extra session is a picker offering an
    /// address the router would refuse; one that drops a session hides work the
    /// human may legally continue.
    #[test]
    fn chat_scope_rule_admits_exactly_what_the_candidate_filter_does() {
        let rows = vec![
            ("s-cur".to_string(), meta("s-cur", "Current", "2026-10-21T10:00:00+00:00", Some(CHAT), Some(ALICE))),
            ("s-old".to_string(), meta("s-old", "Predecessor", "2026-10-20T10:00:00+00:00", Some(CHAT), Some(ALICE))),
            ("s-manual".to_string(), meta("s-manual", "Manual", "2026-10-19T10:00:00+00:00", None, Some(ALICE))),
            ("s-other-chat".to_string(), meta("s-other-chat", "Elsewhere", "2026-10-21T11:00:00+00:00", Some("chat-2"), Some(ALICE))),
            ("s-bob".to_string(), meta("s-bob", "Bob", "2026-10-21T11:00:00+00:00", None, Some("bob"))),
            ("s-agent".to_string(), meta("s-agent", "Agent", "2026-10-21T11:00:00+00:00", None, None)),
        ];
        for surface in [CandidateSurface::Direct, CandidateSurface::Group] {
            for terminal in [&[][..], &["s-old"][..]] {
                let tasks = empty_set();
                let terminal_set: HashSet<String> =
                    terminal.iter().map(|s| s.to_string()).collect();
                let through_query_rows = build_candidates(
                    &rows,
                    &HashMap::new(),
                    &CandidateQuery {
                        chat_session_id: CHAT,
                        owner_user_id: Some(ALICE),
                        surface,
                        task_session_ids: &tasks,
                        terminal_session_ids: &terminal_set,
                        max: 10,
                    },
                );
                let mut through_query = ids(&through_query_rows);
                let rule = rule(surface, terminal);
                let mut through_rule: Vec<&str> = rows
                    .iter()
                    .filter(|(_, meta)| rule.allows(meta))
                    .map(|(sid, _)| sid.as_str())
                    .collect();
                through_query.sort();
                through_rule.sort();
                assert_eq!(through_rule, through_query, "surface={surface:?} terminal={terminal:?}");
            }
        }
    }

    /// The rule is the permission, so the two answers it must never give are
    /// "somebody else's run" and "work that is already over".
    #[test]
    fn chat_scope_rule_keeps_other_people_and_finished_work_out() {
        let rows = [
            ("mine".to_string(), meta("mine", "Mine", "2026-10-21T10:00:00+00:00", Some(CHAT), Some(ALICE))),
            ("bobs".to_string(), meta("bobs", "Bob", "2026-10-21T10:00:00+00:00", Some(CHAT), Some("bob"))),
            ("finished".to_string(), meta("finished", "Done", "2026-10-21T10:00:00+00:00", Some(CHAT), Some(ALICE))),
        ];
        let rule = rule(CandidateSurface::Direct, &["finished"]);
        assert!(rule.allows(&rows[0].1));
        assert!(!rule.allows(&rows[1].1));
        assert!(!rule.allows(&rows[2].1));
    }
}
