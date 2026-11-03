//! ADR-106 D4 — the round projection read.
//!
//! The chat surface holds a *reference* to a round and asks this endpoint for its
//! content: `{ user, answer, card, terminal, status }`, resolved out of the
//! execution session's own transcript. "What a round is" is defined by the
//! execution session, never by the chat — that is the boundary this ADR exists to
//! enforce, and it is why the projection lives here in the Runtime.
//!
//! Two properties make this safe to put on a request path:
//!
//! * **Bounded** (ADR-096): the side table holds ADR-050 data-index coordinates,
//!   so a projection reads exactly the referenced rows plus one bounded window
//!   after the user row. Nothing here scans the transcript.
//! * **Disk-only**: both files are read straight from the session's own directory,
//!   so a session that is not resident — `Closed`, pruned, or one this process
//!   never opened — is still readable. That is what lets a chat block render
//!   hours after its run ended, and it is why no service slot or live session
//!   handle is needed.
//!
//! A round that does not exist is an explicit hole, not a lie: `found: false` in
//! the batch shape, 404 in the single shape. Callers cache the difference.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use std::path::{Path as FsPath, PathBuf};

use crate::http::server::HttpState;
use crate::http::session_control::{authorize_read, scope_from_headers};

/// Upper bound on the round ids one batch request may name. The chat's first
/// render pulls the visible blocks at once; beyond this the caller pages.
const BATCH_MAX_IDS: usize = 64;

/// Longest accepted session id. Matches the Runtime's own generated ids with room
/// for caller-supplied ones; it is also the guard against a path-traversal id.
const MAX_SESSION_ID_BYTES: usize = 128;

/// `GET /sessions/{sid}/rounds/{round_id}` — one round's conversation-visible
/// projection.
///
/// 404 covers three distinct situations on purpose (ADR-076 §决策 4): the session
/// does not exist, the caller may not read it, or the session has no such round.
/// Distinguishing them would make this endpoint an existence oracle.
pub(crate) async fn get_session_round(
    State(state): State<HttpState>,
    Path((sid, round_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let (jsonl, rounds) = session_paths(&state, &sid)?;
    let meta = authorize_read(&state, &sid, &scope_from_headers(&headers))?;
    if !valid_round_id(&round_id) {
        return Err(err(StatusCode::BAD_REQUEST, "malformed round id"));
    }
    let rec = read_round(&rounds, &jsonl, &round_id)?.ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            format!("round not found: {round_id}"),
        )
    })?;
    let projection = crate::rounds::project(&jsonl, &rec).map_err(|e| {
        tracing::warn!(
            session_id = %sid,
            round_id = %round_id,
            error = %e,
            "round projection read failed"
        );
        err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    // A round the transcript can no longer vouch for is reported exactly as one that
    // was never there: this endpoint must not become an oracle for *why* content is
    // missing, and the chat renders both as the same explicit hole.
    let projection = projection.ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            format!("round not projectable: {round_id}"),
        )
    })?;
    let mut body = serde_json::to_value(projection).map_err(ser_err)?;
    stamp_chat_binding(&mut body, &meta);
    Ok(Json(body))
}

/// Query for `GET /sessions/{sid}/rounds`.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct ListRoundsQuery {
    /// Comma-separated round ids. Required: an unbounded "all rounds" read has no
    /// place on a request path, and the caller always knows which rounds the chat
    /// references (those are the rows it is rendering).
    #[serde(default)]
    ids: Option<String>,
}

/// `GET /sessions/{sid}/rounds?ids=a,b` — the same projections, one request.
///
/// The reason this exists is the first render of a chat transcript: it names many
/// blocks at once, and per-block round-trips across machines would make the pull
/// the slowest thing in the feature (ADR-106 D5's cache is what amortises it, but
/// the miss still has to be cheap).
///
/// An id the session does not know comes back as `{"round_id": …, "found": false}`
/// rather than being dropped: a dropped entry lets a caller mistake "no such round"
/// for "not asked".
pub(crate) async fn list_session_rounds(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ListRoundsQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let (jsonl, rounds) = session_paths(&state, &sid)?;
    let meta = authorize_read(&state, &sid, &scope_from_headers(&headers))?;
    let ids: Vec<String> = query
        .ids
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    if ids.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "ids is required (comma-separated round ids)",
        ));
    }
    if ids.len() > BATCH_MAX_IDS {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("too many round ids: {} (max {BATCH_MAX_IDS})", ids.len()),
        ));
    }
    // Fold once for the whole batch: the side table is a handful of short lines per
    // round, and re-reading it per id would multiply the same small file. Rebuilding it
    // from the transcript when it answers nothing is what keeps a session with a lost
    // side table readable (ADR-106 D4's first touch, applied to the read path).
    let index = crate::rounds::fold_or_rebuild(&rounds, &jsonl).map_err(|e| {
        tracing::warn!(session_id = %sid, error = %e, "rounds index fold failed");
        err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    let mut rounds_out = Vec::with_capacity(ids.len());
    for id in ids {
        if !valid_round_id(&id) {
            // A malformed id is the caller's bug, not this session's outage: answer it
            // as a hole and keep serving the rest of the batch. Rejecting the request
            // would let one bad ref turn every other block in the chunk into an
            // "unreachable" retry that can never succeed.
            rounds_out.push(serde_json::json!({ "round_id": id, "found": false }));
            continue;
        }
        let Some(rec) = index.get(&id) else {
            rounds_out.push(serde_json::json!({ "round_id": id, "found": false }));
            continue;
        };
        match crate::rounds::project(&jsonl, rec) {
            Ok(Some(p)) => match serde_json::to_value(p) {
                Ok(mut v) => {
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("found".to_string(), serde_json::Value::Bool(true));
                    }
                    stamp_chat_binding(&mut v, &meta);
                    rounds_out.push(v);
                }
                Err(e) => return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
            },
            // The side table names the round but its rows no longer resolve to it.
            // Say "not found" rather than serving whatever sits at those positions: a
            // stale coordinate would otherwise be cached as this block's content with
            // no way to invalidate it (ADR-106 D5).
            Ok(None) => {
                tracing::warn!(
                    session_id = %sid,
                    round_id = %id,
                    "round projection: recorded coordinates do not resolve to this round"
                );
                rounds_out.push(serde_json::json!({ "round_id": id, "found": false }));
            }
            Err(e) => {
                // The round exists but its rows could not be read. Say so: an
                // explicit hole is what the chat renders, a dropped entry is what
                // it would invent.
                tracing::warn!(
                    session_id = %sid,
                    round_id = %id,
                    error = %e,
                    "round projection read failed"
                );
                rounds_out.push(serde_json::json!({
                    "round_id": id,
                    "found": false,
                    "error": e.to_string(),
                }));
            }
        }
    }
    Ok(Json(serde_json::json!({ "session_id": sid, "rounds": rounds_out })))
}

/// Resolve one session's transcript and round side table.
///
/// The id is validated before it reaches the filesystem: it arrives from a URL
/// segment, and a `..` or a separator in it would let the caller name any `.jsonl`
/// under the Runtime's data directory.
fn session_paths(
    state: &HttpState,
    sid: &str,
) -> Result<(PathBuf, PathBuf), (StatusCode, Json<serde_json::Value>)> {
    if !valid_session_id(sid) {
        return Err(err(StatusCode::NOT_FOUND, "session not found"));
    }
    let dir = state.work_dir.join("conversations");
    Ok((
        dir.join(format!("{sid}.jsonl")),
        crate::rounds::rounds_path(&dir, sid),
    ))
}

fn read_round(
    rounds: &FsPath,
    jsonl: &FsPath,
    round_id: &str,
) -> Result<Option<crate::rounds::RoundRecord>, (StatusCode, Json<serde_json::Value>)> {
    let mut index = crate::rounds::fold_or_rebuild(rounds, jsonl).map_err(|e| {
        tracing::warn!(path = %rounds.display(), error = %e, "rounds index read failed");
        err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?;
    // `None` is the honest hole; the caller turns it into 404 or `found: false`.
    Ok(index.remove(round_id))
}

/// Session ids are Runtime-generated (`YYYYMMDD_HHMMSS_xxxx`) but callers may pass
/// their own; either way the id is only ever used to name a file, so restrict it to
/// the characters a file name may legitimately contain.
fn valid_session_id(sid: &str) -> bool {
    !sid.is_empty()
        && sid.len() <= MAX_SESSION_ID_BYTES
        && sid
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `round_id` is `r:{inbound message id}` (ADR-106 D2), so it carries a colon the
/// session id never may. The character set is the Runtime's own minting predicate —
/// addressing and minting must be the same test or a round can be born unreadable.
/// Bounded by [`crate::rounds::ROUND_ID_MAX_BYTES`], with headroom for a caller-named
/// id; a hash-folded oversized id stays inside it by construction.
fn valid_round_id(id: &str) -> bool {
    id.len() <= crate::rounds::ROUND_ID_MAX_BYTES + 8 && crate::rounds::is_addressable(id)
}

fn err(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": message.into() })))
}

/// Attach the session's Chat binding to a projection.
///
/// This is what makes the chat-side projection cache safe to write (ADR-106
/// D10). The cache lives under a chat, and the only thing tying a fetched
/// round to that chat is the session's own `chat_session_id`; without it a
/// caller participating in two chats could smuggle one chat's execution
/// content into the other chat's cache. The Gateway refuses to store a
/// projection whose binding names a different chat.
fn stamp_chat_binding(body: &mut serde_json::Value, meta: &acowork_core::session_meta::SessionMeta) {
    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "chat_session_id".to_string(),
            serde_json::json!(meta.chat_session_id),
        );
    }
}

fn ser_err(e: serde_json::Error) -> (StatusCode, Json<serde_json::Value>) {
    err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rounds::{append_record, read_record, RoundRecord};

    fn rec(round_id: &str, user: u64, answer: Option<u64>, terminal: bool) -> RoundRecord {
        RoundRecord {
            round_id: round_id.to_string(),
            user,
            answer,
            card: None,
            terminal,
            status: Some("completed".to_string()),
            ts: "2026-11-02T00:00:00.000Z".to_string(),
        }
    }

    fn entry(role: &str, content: &str) -> crate::conversation::ConversationEntry {
        crate::conversation::ConversationEntry {
            id: format!("{role}-{content}"),
            ts: "2026-11-02T00:00:00.000Z".to_string(),
            role: role.to_string(),
            content: content.to_string(),
            // ADR-106 D2: membership is a stamp on the row, so a projection can prove
            // the coordinate it was given still names this round.
            metadata: Some(serde_json::json!({ crate::rounds::META_KEY_ROUND_ID: "r:1" })),
            kind: None,
        }
    }

    #[test]
    fn session_id_shape_rejects_path_traversal() {
        assert!(valid_session_id("20260503_100000_test01"));
        for bad in [
            "",
            "..",
            "../other",
            "a/b",
            "a\\b",
            "a.b",
            &"x".repeat(MAX_SESSION_ID_BYTES + 1),
        ] {
            assert!(!valid_session_id(bad), "must reject {bad:?}");
        }
    }

    #[test]
    fn round_id_shape_allows_the_derived_form() {
        assert!(valid_round_id("r:msg-1"));
        assert!(valid_round_id("r:2026-11-02T00:00:00.000Z:user"));
        assert!(valid_round_id(&crate::rounds::mint_round_id(&"x".repeat(400))));
        for bad in ["", "r:with space", "r:with#hash", "r:with\\backslash"] {
            assert!(!valid_round_id(bad), "must reject {bad:?}");
        }
    }

    #[test]
    fn read_round_degrades_on_a_missing_table() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("s.jsonl.rounds");
        let jsonl = dir.path().join("s.jsonl");
        // Absent side table is not an error: the round simply is not there, which
        // is the explicit hole ADR-106 D5 says the chat must render.
        assert!(read_round(&path, &jsonl, "r:1").unwrap().is_none());

        append_record(&path, &rec("r:1", 0, Some(1), true)).unwrap();
        assert_eq!(read_round(&path, &jsonl, "r:1").unwrap().unwrap().answer, Some(1));
        assert!(read_round(&path, &jsonl, "r:2").unwrap().is_none());
    }

    #[test]
    fn projection_resolves_the_rounds_own_rows() {
        let dir = tempfile::TempDir::new().unwrap();
        let jsonl = dir.path().join("s.jsonl");
        let rounds = dir.path().join("s.jsonl.rounds");
        let body: String = [
            entry("user", "hello"),
            entry("tool_call", "not in the projection"),
            entry("assistant", "the answer"),
        ]
        .iter()
        .map(|e| format!("{}\n", serde_json::to_string(e).unwrap()))
        .collect();
        std::fs::write(&jsonl, body).unwrap();
        append_record(&rounds, &rec("r:1", 0, Some(2), true)).unwrap();

        let rec = read_record(&rounds, "r:1").unwrap().unwrap();
        let p = crate::rounds::project(&jsonl, &rec)
            .unwrap()
            .expect("round is projectable");
        assert_eq!(p.user.as_ref().unwrap().body, "hello");
        assert_eq!(p.answer.as_ref().unwrap().body, "the answer");
        assert!(p.terminal);
        let rendered = serde_json::to_string(&p).unwrap();
        assert!(!rendered.contains("not in the projection"));
    }
}
