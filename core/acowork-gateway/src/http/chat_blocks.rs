//! ADR-106 D5/D9/D10 — read-side orchestration for Chat execution blocks.
//!
//! A Chat transcript holds *references* to rounds of execution sessions
//! (`{agent_instance_id, agent_session_id, round_id}`), never copies of their
//! content (D1/D2). Rendering one block therefore needs a second read, and the
//! thing being read lives on whichever node hosts that execution session. Doing
//! that per block, per render, across machines would make the transcript pull
//! the slowest thing in the feature — and history would stop being readable the
//! moment its Runtime stopped. So this module is the one place that composes the
//! two reads:
//!
//! 1. ask the user service for the projections this chat already caches (D5);
//! 2. fetch only what missed, one batched request per execution session;
//! 3. write the terminal ones back, so the next render is a disk read.
//!
//! What this owns, and what it deliberately does not:
//!
//! * **The chat relation is the permission.** The cache read is gated by the
//!   user service (hidden 404 for a non-participant, same rule as the
//!   transcript), and the Runtime fetch is gated by a Chat membership set derived
//!   server-side from the verified token — never from a client header. Either
//!   wall alone would hold; both are kept because they guard different edges,
//!   and the cache is the one an unreachable Runtime cannot re-authorise.
//! * **The binding guard.** Refs arrive from the client, so a ref may name a
//!   round of some *other* chat's execution session. The Runtime stamps every
//!   projection with that session's own `chat_session_id`; a projection not
//!   bound to the chat being read is served as an explicit hole and is never
//!   written back. Without that, one member of two chats could pour the other
//!   chat's content into this chat's cache and have it survive as history.
//! * **No open round is cached.** A round still running is served live and
//!   marked `pending`, never stored (D5): a cache with no invalidation path may
//!   only hold immutable facts.
//! * **Degrade, never outage.** An unreadable cache reads as "every ref missed";
//!   an unreachable Runtime reads as "this block is a hole". Only the chat
//!   relation itself may fail the request, and a lying cache row is distrusted
//!   rather than believed.

use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    http::HeaderMap,
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;

use crate::http::auth_middleware::{AuthContext, SCOPE_ALL, runtime_scope_headers};
use crate::http::routes::{AppState, ApiError};
use crate::intent::chat_client;

/// Most refs one render may name. Mirrors the user service's `MAX_REFS`: the
/// two sides must agree, or a request is half-served at one end and
/// half-rejected at the other.
const MAX_REFS: usize = 200;

/// Round ids per upstream batch — the Runtime's own `BATCH_MAX_IDS`. Refs for
/// one execution session are chunked to this so a long transcript degrades into
/// several requests rather than one rejection.
const RUNTIME_BATCH_MAX: usize = 64;

/// The one round status that keeps its card through the terminal transition
/// (ADR-106 D8): a park the human can still answer. Spelled here rather than
/// imported because the Gateway reads the Runtime's wire vocabulary, never its
/// types.
const STATUS_WAITING: &str = "waiting";

/// Longest accepted id segment. Instance ids are UUIDv4, session ids are
/// `YYYYMMDD_HHMMSS_xxxx`, round ids are `r:{message_id}` — all far under this.
const MAX_ID_BYTES: usize = 128;

/// Which execution session a block points at, and which round of it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BlockRef {
    instance: String,
    session: String,
    round: String,
}

impl BlockRef {
    /// Cache and response key: `instance|session|round`. `|` because a round id
    /// legitimately contains `:` (D2 mints `r:{message_id}`) and an instance id
    /// may contain `.` or `-`.
    fn key(&self) -> String {
        format!("{}|{}|{}", self.instance, self.session, self.round)
    }

    /// Parse one ref. Shape-checked, never trusted: the segments go into a URL
    /// path, a query string, and a SQL bind downstream.
    fn parse(s: &str) -> Result<BlockRef, String> {
        let mut parts = s.split('|');
        let instance = parts.next().unwrap_or("");
        let session = parts.next().unwrap_or("");
        let round = parts.next().unwrap_or("");
        let trailing = parts.next();
        if trailing.is_some() {
            return Err(format!("too many segments in block ref `{s}`"));
        }
        // A session id also names a file upstream, so it is restricted to the
        // character set the Runtime itself accepts there: no `:`, no `.`.
        let ok = valid_id(instance, false)
            && valid_id(session, false)
            && valid_id(round, true)
            // D2 mints `r:{message_id}`, so a bare `r:` is not a round id: it is
            // a client that truncated one.
            && round.len() > 2
            && round.starts_with("r:");
        if !ok {
            return Err(format!("malformed block ref `{s}` (expected instance|session|r:round)"));
        }
        Ok(BlockRef {
            instance: instance.to_string(),
            session: session.to_string(),
            round: round.to_string(),
        })
    }
}

/// Id character set. `allow_loose` admits `:` and `.`, which a round id needs and
/// neither an instance id nor a session id ever carries.
///
/// This MUST stay a subset of what the cache and the Runtime accept
/// (`crate::rounds::is_addressable` upstream, `rounds_cache::valid_id` in
/// acowork-user): one shared predicate is what keeps a minted round readable, and a
/// token this layer waves through but a downstream layer rejects fails **there**, on
/// the whole batch — an over-permissive check here turns one broken ref into a
/// permanently uncacheable page.
fn valid_id(s: &str, allow_loose: bool) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        // `..` names a parent directory wherever an id is used to build a path, and
        // the cache refuses it outright — so refusing it here is the only way a bad
        // token stays the single hole the contract promises instead of a 422 over
        // every ref in the page.
        && !s.contains("..")
        && s.len() <= MAX_ID_BYTES
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || b == b'_'
                || b == b'-'
                || (allow_loose && (b == b':' || b == b'.'))
        })
}

/// The Runtime-side scope pair for this caller, mirroring `auth_middleware`.
///
/// Deliberately the same mapping the agent proxy uses, rather than a narrower
/// one invented here: an administrator acting as themselves reads `*` with their
/// own uid, and an `as_user` request is a plain user. The wide scope is not a
/// wider door for blocks — a projection is still refused unless it is bound to
/// the chat being read — it only stops this endpoint from being *narrower* than
/// the session reads the same caller can already make through the proxy.
fn runtime_scope_pair(ctx: &AuthContext) -> (String, Option<String>) {
    match (ctx.as_user.as_deref(), ctx.is_admin()) {
        (Some(u), true) => (u.to_string(), None),
        (_, true) => (SCOPE_ALL.to_string(), Some(ctx.user_id.clone())),
        _ => (ctx.user_id.clone(), None),
    }
}

/// The per-ref answer, kept distinct because the Chat surface renders each one
/// differently (D9): content, a live partial, an explicit hole, or an outage.
#[derive(Debug, Clone, PartialEq)]
enum Resolution {
    /// Terminal projection served from this chat's own cache.
    Cached(Value),
    /// Projection read from the hosting Runtime just now; its `terminal`
    /// field still decides whether the block is finished or in flight.
    Live(Value),
    /// No such round, or not this chat's round. Rendered as a hole — never as
    /// an error a reader could retry into content.
    Hole,
    /// The hosting Runtime could not be consulted. Distinct from `Hole`
    /// because the surface offers a retry here, and because conflating the two
    /// would let a stopped node rewrite what happened.
    Unreachable(String),
}

/// One requested slot, in the order the client named it: either a ref this endpoint
/// can answer, or a token that never was one.
///
/// Both get a response entry. Rejecting the whole request over one bad token would let
/// a single broken transcript row make every *other* block unreadable, and dropping the
/// token would let the surface invent a row for it — D9's "never silently drop" cuts
/// both ways.
#[derive(Debug, Clone, PartialEq)]
enum RefSlot {
    Valid(BlockRef),
    Malformed(String),
}

impl RefSlot {
    /// The key the answer is reported under — the ref triple, or the rejected token
    /// verbatim so the caller can match it to the row that produced it.
    fn key(&self) -> String {
        match self {
            RefSlot::Valid(r) => r.key(),
            RefSlot::Malformed(raw) => raw.clone(),
        }
    }
}

/// Parse the `refs` query parameter: comma-separated, deduped, request order
/// preserved — the transcript renders in order, and a reordered answer would
/// put blocks under the wrong rows.
fn parse_refs(raw: Option<&str>) -> Result<Vec<RefSlot>, ApiError> {
    let mut seen: HashMap<String, ()> = HashMap::new();
    let mut out: Vec<RefSlot> = Vec::new();
    for part in raw.unwrap_or_default().split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let slot = match BlockRef::parse(part) {
            Ok(parsed) => {
                if seen.insert(parsed.key(), ()).is_some() {
                    continue;
                }
                RefSlot::Valid(parsed)
            }
            Err(e) => {
                if seen.insert(part.to_string(), ()).is_some() {
                    continue;
                }
                tracing::debug!(ref_token = part, error = %e, "malformed block ref — answered as a hole");
                RefSlot::Malformed(part.to_string())
            }
        };
        out.push(slot);
    }
    if out.is_empty() {
        return Err(ApiError::unprocessable_entity(
            "refs is required (comma-separated instance|session|r:round)",
        ));
    }
    if out.len() > MAX_REFS {
        return Err(ApiError::unprocessable_entity(&format!(
            "too many refs: {} (max {MAX_REFS})",
            out.len()
        )));
    }
    Ok(out)
}

/// The answerable subset, in order.
fn valid_refs(slots: &[RefSlot]) -> Vec<BlockRef> {
    slots
        .iter()
        .filter_map(|s| match s {
            RefSlot::Valid(r) => Some(r.clone()),
            RefSlot::Malformed(_) => None,
        })
        .collect()
}

/// Group refs by execution session, first-seen order, so one session costs one
/// batched upstream request however the transcript interleaves them.
fn group_by_session(refs: &[BlockRef]) -> Vec<(String, String, Vec<String>)> {
    let mut order: Vec<(String, String)> = Vec::new();
    let mut groups: HashMap<(String, String), Vec<String>> = HashMap::new();
    for r in refs {
        let id = (r.instance.clone(), r.session.clone());
        if !groups.contains_key(&id) {
            order.push(id.clone());
            groups.insert(id.clone(), Vec::new());
        }
        if let Some(rounds) = groups.get_mut(&id) {
            rounds.push(r.round.clone());
        }
    }
    let mut out = Vec::new();
    for id in order {
        if let Some(rounds) = groups.remove(&id) {
            out.push((id.0, id.1, rounds));
        }
    }
    out
}

/// Turn the chat's cached rows into answers, and report the refs it did not
/// answer.
///
/// A cached row that is not terminal, or whose payload is not an object, is
/// distrusted: this cache is disposable, so the cheap response to a lying row is
/// "ask the Runtime again", never "believe the side table".
fn apply_cache_hits(rows: &[Value], wanted: &[BlockRef]) -> (HashMap<String, Resolution>, Vec<BlockRef>) {
    let mut keys: HashMap<String, ()> = HashMap::new();
    for w in wanted {
        keys.insert(w.key(), ());
    }
    let mut hits: HashMap<String, Resolution> = HashMap::new();
    for row in rows {
        let instance = row
            .get("agent_instance_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let session = row
            .get("agent_session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let round = row.get("round_id").and_then(|v| v.as_str()).unwrap_or("");
        let key = format!("{instance}|{session}|{round}");
        // A row for a ref nobody asked about is not a design hole: it is the
        // bounded cache keeping a round this transcript no longer names. It is
        // not served, and it is not an error.
        if !keys.contains_key(&key) {
            continue;
        }
        let terminal = row.get("terminal").and_then(|v| v.as_bool()).unwrap_or(false);
        if !terminal {
            tracing::warn!(
                block_ref = %key,
                "cached round is marked open — refetching instead of serving it"
            );
            continue;
        }
        match row.get("projection") {
            Some(p) if p.is_object() => {
                hits.insert(key, Resolution::Cached(p.clone()));
            }
            _ => {
                tracing::warn!(block_ref = %key, "cached round has no projection object — refetching");
            }
        }
    }
    let missed: Vec<BlockRef> = wanted
        .iter()
        .filter(|w| !hits.contains_key(&w.key()))
        .cloned()
        .collect();
    (hits, missed)
}

/// Is this projection this chat's own? A session with no chat binding is refused
/// too: an execution session that belongs to no chat has no business appearing in
/// a chat transcript.
fn bound_to(projection: &Value, chat_id: &str) -> bool {
    match projection.get("chat_session_id").and_then(|v| v.as_str()) {
        Some(bound) => bound == chat_id,
        None => false,
    }
}

/// The projection as a surface may see it: sanitized, never aliased.
///
/// Serving is the one edge every block passes through — cached or live — so the
/// terminal-card rule is applied here even though the writer enforces it too.
/// `Cached` rows are the reason it has to be here as well: an earlier binary
/// could store `terminal: true` with a live question, and D5 gives a terminal
/// projection **no** invalidation path, so nothing would ever re-read that round
/// to fix it.
fn served_projection(p: &Value) -> Value {
    let mut out = p.clone();
    sanitize_card(&mut out);
    out
}

/// A terminal round may carry a card only for a `waiting` park (ADR-106 D8).
///
/// The writer enforces this (`rounds::ActiveRound::forced_closure` and
/// `rounds::repair_on_open` drop the card whenever the round did not end
/// *waiting*), but the read boundary has to as well, for a second reason that
/// does not go away on its own: a card names a `request_id` that only the
/// *live* interaction can accept. Once the interaction is gone the answer
/// endpoint still replies `202` (it queues the message; the loop then drops it),
/// so a surface cannot tell "answered" from "that question died twenty minutes
/// ago" — it paints a clickable card that does nothing and never disappears.
///
/// Dropping the field is the honest rendering: the round is over, there is
/// nothing left to answer.
fn sanitize_card(projection: &mut Value) {
    if projection.get("terminal").and_then(|v| v.as_bool()) != Some(true) {
        return;
    }
    if projection.get("status").and_then(|v| v.as_str()) == Some(STATUS_WAITING) {
        return;
    }
    if let Some(map) = projection.as_object_mut() {
        map.remove("card");
    }
}

/// Classify one entry of the Runtime's batch response.
///
/// Four refusals live here because they are four different facts: the session says
/// the round is not there, the rows behind it could not be read, the round belongs to
/// a *different* chat, and the round is still running. Only the third one is a
/// security boundary, only the last one decides whether the answer may be stored, and
/// the second one must stay retryable rather than permanent.
fn classify_fetched(entry: &Value, chat_id: &str) -> (String, Resolution) {
    let round_id = entry
        .get("round_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    match entry.get("found").and_then(|v| v.as_bool()) {
        Some(true) => {}
        // §5.7 keeps the two refusals apart on one item: `found: false` is the
        // permanent hole, `found: false` **plus** `error` is "the round exists but its
        // rows could not be read". Collapsing the second into the first would render a
        // transient disk fault as "this round never happened", with no retry affordance
        // — and the surface cannot recover it, because D5 gives the cache no
        // invalidation path.
        _ => {
            return match entry.get("error").and_then(|v| v.as_str()) {
                Some(detail) => (round_id, Resolution::Unreachable(detail.to_string())),
                None => (round_id, Resolution::Hole),
            };
        }
    }
    if !bound_to(entry, chat_id) {
        // Reachable only through a ref naming another chat's execution session,
        // so it is worth saying loudly: this is either a broken client or an
        // attempt to seed this chat's cache with foreign content.
        tracing::warn!(
            round_id,
            chat_id,
            "refused a projection bound to a different chat (ADR-106 D10)"
        );
        return (round_id, Resolution::Hole);
    }
    (round_id, Resolution::Live(entry.clone()))
}

/// Rows to write back: live, terminal, and bound. `Cached` rows are already
/// there, and rewriting them only churns `fetched_at`, which is the eviction key.
fn select_write_back(resolutions: &HashMap<String, Resolution>) -> Vec<Value> {
    let mut out = Vec::new();
    for (key, res) in resolutions {
        let projection = match res {
            Resolution::Live(p) => p,
            _ => continue,
        };
        if projection.get("terminal").and_then(|v| v.as_bool()) != Some(true) {
            continue;
        }
        let parsed = match BlockRef::parse(key) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(key, error = %e, "cannot write back an unparseable cache key");
                continue;
            }
        };
        out.push(json!({
            "agent_instance_id": parsed.instance,
            "agent_session_id": parsed.session,
            "round_id": parsed.round,
            "projection": projection,
            "terminal": true,
        }));
    }
    out
}

/// Render the answer for every ref requested, in request order.
///
/// `status` is the contract with the Chat surface (D9): `ok` renders the block,
/// `pending` renders it live with a "still running" affordance and should be
/// re-read, `not_found` renders the explicit hole, `unreachable` renders the
/// retry affordance. A ref with no resolution is `unreachable`, never dropped —
/// a dropped ref lets the surface invent a row for it. A token that is not a ref
/// at all is reported as `not_found` under the token itself, which keeps one
/// malformed row from failing the render of the transcript around it.
fn assemble(order: &[RefSlot], resolutions: &HashMap<String, Resolution>) -> Vec<Value> {
    let mut out = Vec::with_capacity(order.len());
    for slot in order {
        // One key for both kinds of slot: every entry of the answer is reported
        // under what the caller named, so an entry can always be matched back.
        let key = slot.key();
        let entry = match slot {
            RefSlot::Malformed(_) => json!({
                "ref": key,
                "status": "not_found",
                "detail": "malformed block ref",
            }),
            RefSlot::Valid(r) => {
                let mut entry = match resolutions.get(&key) {
                    Some(Resolution::Cached(p)) => json!({
                        "status": "ok",
                        "source": "cache",
                        // Enforced again at the serving edge: a cache row written
                        // before the writer invariant existed is terminal and will
                        // never be re-read (D5), so this is the only place its dead
                        // card can still be caught.
                        "projection": served_projection(p),
                    }),
                    Some(Resolution::Live(p)) => {
                        let terminal = p
                            .get("terminal")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        json!({
                            "status": if terminal { "ok" } else { "pending" },
                            "source": "runtime",
                            "projection": served_projection(p),
                        })
                    }
                    Some(Resolution::Hole) => json!({ "status": "not_found" }),
                    Some(Resolution::Unreachable(d)) => {
                        json!({ "status": "unreachable", "detail": d })
                    }
                    None => json!({
                        "status": "unreachable",
                        "detail": "no answer for this ref",
                    }),
                };
                // The ref fields are added, not interleaved by hand: every answer
                // carries the same triple the caller named, so a surface can match a
                // block to its row without parsing `ref`.
                if let Some(map) = entry.as_object_mut() {
                    map.insert("ref".to_string(), Value::String(key.clone()));
                    map.insert(
                        "agent_instance_id".to_string(),
                        Value::String(r.instance.clone()),
                    );
                    map.insert(
                        "agent_session_id".to_string(),
                        Value::String(r.session.clone()),
                    );
                    map.insert("round_id".to_string(), Value::String(r.round.clone()));
                }
                entry
            }
        };
        out.push(entry);
    }
    out
}

pub fn chat_block_routes() -> Router<AppState> {
    Router::new().route("/api/chat-blocks/{chat_id}", get(get_chat_blocks))
}

/// Query for `GET /api/chat-blocks/{chat_id}`.
#[derive(Debug, Deserialize)]
struct BlocksQuery {
    /// Comma-separated refs — the block rows the surface is rendering.
    /// Required: an unbounded "all blocks" read has no place on a request path,
    /// and only the client knows which rows it shows.
    #[serde(default)]
    refs: Option<String>,
}

/// `GET /api/chat-blocks/{chat_id}?refs=` — the block contents behind a Chat
/// transcript's references: from this chat's cache where it has them, from the
/// hosting Runtime otherwise, and written back so the next render is a read.
async fn get_chat_blocks(
    State(state): State<AppState>,
    Path(chat_id): Path<String>,
    auth: Option<Extension<AuthContext>>,
    Query(q): Query<BlocksQuery>,
) -> Result<Json<Value>, ApiError> {
    if !valid_id(&chat_id, true) {
        // The transcript's own hidden-404 discipline: a malformed id is not the
        // absence of this chat, but answering 422 here would tell a probe which
        // ids are well-formed *and* not its caller's to read.
        return Err(ApiError::not_found("chat session not found"));
    }
    // Blocks are a user action, so the token-derived identity is the only
    // permission input (ADR-076 §决策 4). With no account system the chat
    // domain does not exist at all — the user service says so itself for every
    // other chat surface, and this must not be the one that guesses.
    let ctx = match auth {
        Some(Extension(c)) => c,
        None => {
            return Err(ApiError::unauthorized(
                "chat blocks require an authenticated user",
            ));
        }
    };
    let who = ctx.effective_user_id().to_string();
    let slots = parse_refs(q.refs.as_deref())?;
    let refs = valid_refs(&slots);
    let port = chat_client::service_port(&state.gateway_state)
        .await
        .ok_or_else(|| ApiError::service_unavailable("chat blocks require the user service"))?;

    // 1. What this chat already holds.
    let encoded: Vec<String> = refs.iter().map(|r| r.key()).collect();
    let mut resolutions: HashMap<String, Resolution> = HashMap::new();
    let missed: Vec<BlockRef> = match chat_client::rounds_read(port, &chat_id, &who, &encoded).await {
        // The user service's hidden 404: not this caller's chat. Fail here
        // rather than fetching around it — a non-participant must not be able to
        // warm or read a cache they cannot see.
        Ok(None) => return Err(ApiError::not_found("chat session not found")),
        Ok(Some(rows)) => {
            let (hits, gap) = apply_cache_hits(&rows, &refs);
            resolutions = hits;
            gap
        }
        // An unreadable cache is a full miss, not a failed read (D5).
        Err(e) => {
            tracing::warn!(
                chat_id,
                error = %e,
                "round cache unreadable — serving every ref from the source of truth"
            );
            refs.clone()
        }
    };

    // 2. Fetch the gap. The membership set is resolved once, and only when there
    //    is something to fetch: it is the same server-derived set the agent proxy
    //    injects, and it is what makes the Runtime's own authorization wall hold
    //    even on the path where step 1 was skipped by a broken cache.
    if !missed.is_empty() {
        let chats = match chat_client::list_session_ids(port, &who).await {
            Ok(ids) => ids,
            Err(e) => {
                // Fail closed: with no membership set no `Members` session is
                // readable, which is a hole per block, not an error per request.
                tracing::warn!(
                    chat_id,
                    error = %e,
                    "chat scope lookup failed — execution blocks stay unreadable"
                );
                Vec::new()
            }
        };
        let scope_headers = {
            let (scope, admin_uid) = runtime_scope_pair(&ctx);
            runtime_scope_headers(&scope, admin_uid.as_deref(), &chats)
        };
        for (instance, session, rounds) in group_by_session(&missed) {
            for chunk in rounds.chunks(RUNTIME_BATCH_MAX) {
                fetch_chunk(
                    &state,
                    &chat_id,
                    &instance,
                    &session,
                    chunk,
                    &scope_headers,
                    &mut resolutions,
                )
                .await;
            }
        }
    }

    // 3. Write back, best-effort. A cache that refused the rows costs the next
    //    render exactly the fetch this one already paid for.
    let write_back = select_write_back(&resolutions);
    if !write_back.is_empty()
        && let Err(e) = chat_client::rounds_write(port, &chat_id, &who, &write_back).await
    {
        tracing::warn!(chat_id, error = %e, "round cache write-back failed (blocks still served)");
    }

    Ok(Json(json!({
        "chat_session_id": chat_id,
        "blocks": assemble(&slots, &resolutions),
    })))
}

/// Read one upstream batch and merge it into `resolutions`.
///
/// Every failure mode here is per chunk: an agent that is not running, a node
/// that is unreachable, and a response nobody can parse all leave the other
/// chunks' answers served. That is the point of the explicit-hole shape (D9) — a
/// partial transcript is still a transcript.
async fn fetch_chunk(
    state: &AppState,
    chat_id: &str,
    instance: &str,
    session: &str,
    rounds: &[String],
    scope_headers: &HeaderMap,
    resolutions: &mut HashMap<String, Resolution>,
) {
    // Every ref in this chunk gets an answer, so a hole cannot be expressed as
    // an absent entry. Fill it up front and overwrite with what comes back.
    let fill = |resolutions: &mut HashMap<String, Resolution>, detail: String| {
        for round in rounds {
            resolutions.insert(
                format!("{instance}|{session}|{round}"),
                Resolution::Unreachable(detail.clone()),
            );
        }
    };
    let Some(registry) = state.runtime_http_registry.as_ref() else {
        fill(resolutions, "runtime registry unavailable".to_string());
        return;
    };
    let endpoint = {
        let reg = registry.read().await;
        reg.get_endpoint(instance)
    };
    // The Runtime endpoint registry is keyed by instance id and populated from
    // its retained MQTT message (ADR-055 D3): no endpoint means that Runtime is
    // not up, which is exactly the "history stops being readable" case this
    // cache exists to survive. So it is a hole for this render, not an outage.
    let Some(endpoint) = endpoint else {
        tracing::debug!(instance, session, chat_id, "block fetch skipped: runtime not registered");
        fill(resolutions, "agent runtime not reachable".to_string());
        return;
    };
    // Both ids are shape-validated by `BlockRef::parse`, and the accepted
    // character set is exactly the URL-unreserved one — so this string is safe
    // to build by hand, with no encoding step left to get wrong.
    let url = format!("{endpoint}/sessions/{session}/rounds");
    let ids = rounds.join(",");
    let mut request = crate::http::proxy::runtime_http_client()
        .get(&url)
        .query(&[("ids", ids.as_str())])
        .headers(scope_headers.clone());
    if let Some(token) = crate::http::proxy::resolve_node_token(state, instance).await {
        request = request.header("X-ACowork-Node-Token", token);
    }
    let response = match request.send().await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::debug!(instance, session, error = %e, "block fetch failed to connect");
            fill(resolutions, format!("runtime request failed: {e}"));
            return;
        }
    };
    let status = response.status();
    if !status.is_success() {
        // A Runtime 404 conflates "no such session" with "not readable by this
        // scope", and for these refs both are holes. Keeping them distinct would
        // make this endpoint an existence oracle for other users' execution
        // sessions (ADR-076 §决策 4).
        tracing::debug!(instance, session, %status, "block fetch refused by runtime");
        fill(resolutions, format!("runtime refused the read ({status})"));
        return;
    }
    let body: Value = match response.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(instance, session, error = %e, "block fetch response unreadable");
            fill(resolutions, "runtime response was not JSON".to_string());
            return;
        }
    };
    let entries = match body.get("rounds").and_then(|v| v.as_array()) {
        Some(arr) => arr.clone(),
        None => {
            fill(resolutions, "runtime returned no rounds array".to_string());
            return;
        }
    };
    merge_entries(&entries, instance, session, rounds, chat_id, resolutions);
}

/// Fold one upstream batch response into the answers for exactly the refs that
/// chunk asked about.
///
/// Two refusals, both about trusting a response rather than reading it: an entry
/// naming a round nobody requested is ignored (an upstream that writes its own
/// refs must not get to answer for a ref it was not given), and a round the array
/// does not mention becomes an explicit hole rather than an absent key.
fn merge_entries(
    entries: &[Value],
    instance: &str,
    session: &str,
    asked: &[String],
    chat_id: &str,
    resolutions: &mut HashMap<String, Resolution>,
) {
    let wanted: HashMap<&str, ()> = asked.iter().map(|rr| (rr.as_str(), ())).collect();
    for entry in entries {
        let (round_id, resolution) = classify_fetched(entry, chat_id);
        if !wanted.contains_key(round_id.as_str()) {
            tracing::warn!(
                round_id,
                session,
                chat_id,
                "runtime returned an unrequested round — ignored"
            );
            continue;
        }
        resolutions.insert(format!("{instance}|{session}|{round_id}"), resolution);
    }
    // A round that came back absent from the array is an explicit hole, never a
    // hang: the chunk answered for the others.
    for round in asked {
        let key = format!("{instance}|{session}|{round}");
        match resolutions.get(&key) {
            Some(_) => {}
            None => {
                resolutions.insert(key, Resolution::Hole);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::account::Role;
    use axum::extract::Request;
    use axum::http::StatusCode;
    use tower::ServiceExt;

    /// The ids a block row holds: an instance, a Runtime session, and a round
    /// minted from the inbound message id (D2).
    const REF_1: &str = "9f1e2d3c-4b5a-4678-9a0b-c1d2e3f40516|20261102_101500_ab12|r:1762070100000:u-1";
    const REF_2: &str = "9f1e2d3c-4b5a-4678-9a0b-c1d2e3f40516|20261102_101500_ab12|r:1762070200000:u-2";
    const OTHER_SESSION: &str = "9f1e2d3c-4b5a-4678-9a0b-c1d2e3f40516|20261102_102000_cd34|r:1-x";
    const OTHER_INSTANCE: &str = "11111111-2222-4333-8444-555555555555|20261102_101500_ab12|r:1-x";
    const CHAT: &str = "0f6c4a9f-2b1e-4d3a-9c8b-7a6f5e4d3c2b";

    fn brf(key: &str) -> BlockRef {
        BlockRef::parse(key).expect("fixture ref is well-formed")
    }

    /// A projection as the Runtime returns it. `chat` is the binding the Runtime
    /// stamps (D10); `None` is an unbound execution session.
    fn projection(round: &str, chat: Option<&str>, terminal: bool) -> Value {
        json!({
            "round_id": round,
            "found": true,
            "terminal": terminal,
            "status": if terminal { "completed" } else { "running" },
            "user": { "body": "hello", "ts": "2026-11-02T10:15:00Z", "attachments": [] },
            "answer": { "body": "the answer", "ts": "2026-11-02T10:15:05Z" },
            "card": null,
            "chat_session_id": chat,
        })
    }

    fn keys_of(refs: &[BlockRef]) -> Vec<String> {
        refs.iter().map(|r| r.key()).collect()
    }

    /// Regression (2026-11-06): the Chat's projected card outlived its question.
    ///
    /// A round pre-empted mid-question (or crash-repaired) used to keep its card
    /// into the terminal record, and the projection was then cached as `ok` with
    /// no invalidation path. The surface painted a clickable card whose request
    /// id no longer existed: clicking answered `202` and nothing happened, and
    /// the card only left the screen when a newer row pushed it out of view.
    #[test]
    fn a_terminal_round_never_serves_a_card() {
        let mut closed = projection("r:1", Some(CHAT), true);
        closed["status"] = json!("completed");
        closed["card"] = json!({ "kind": "ask_question", "request_id": "q1", "options": ["a"] });
        sanitize_card(&mut closed);
        assert!(
            closed.get("card").is_none(),
            "a finished round has nothing to answer: {closed}"
        );

        let mut interrupted = projection("r:2", Some(CHAT), true);
        interrupted["status"] = json!("interrupted");
        interrupted["card"] = json!({ "kind": "ask_question", "request_id": "q2" });
        sanitize_card(&mut interrupted);
        assert!(interrupted.get("card").is_none());

        // A `waiting` park is the one terminal state whose card is still live:
        // the question was abandoned, not answered, and a `pause` is answered
        // through the composer against the reference row.
        let mut waiting = projection("r:3", Some(CHAT), true);
        waiting["status"] = json!(STATUS_WAITING);
        waiting["card"] = json!({ "kind": "pause", "request_id": "q3" });
        sanitize_card(&mut waiting);
        assert!(waiting.get("card").is_some(), "a pause stays answerable");

        // An open round keeps its card: that is the whole point of the field.
        let mut running = projection("r:4", Some(CHAT), false);
        running["card"] = json!({ "kind": "ask_question", "request_id": "q4" });
        sanitize_card(&mut running);
        assert!(running.get("card").is_some());
    }

    /// The served answer is sanitized for cached rows too — the ones an earlier
    /// binary wrote and that no re-read will ever replace (D5).
    #[test]
    fn a_cached_terminal_card_is_not_served() {
        let mut cached = projection("r:1", Some(CHAT), true);
        cached["status"] = json!("interrupted");
        cached["card"] = json!({ "kind": "ask_question", "request_id": "q1" });
        let resolutions = HashMap::from([(
            REF_1.to_string(),
            Resolution::Cached(cached.clone()),
        )]);
        let order = vec![RefSlot::Valid(brf(REF_1))];
        let out = assemble(&order, &resolutions);
        assert_eq!(out.len(), 1);
        assert!(
            out[0]["projection"].get("card").is_none(),
            "a cache row is sanitized on the way out: {}",
            out[0]
        );
    }

    fn slot_keys(slots: &[RefSlot]) -> Vec<String> {
        slots.iter().map(|s| s.key()).collect()
    }

    /// The answerable slots for a fixture list of refs — what `parse_refs` yields
    /// when every token in the request was well-formed.
    fn slots_of(keys: &[&str]) -> Vec<RefSlot> {
        keys.iter().map(|k| RefSlot::Valid(brf(k))).collect()
    }

    #[test]
    fn a_ref_is_the_triple_the_block_row_holds() {
        let parsed = brf(REF_1);
        assert_eq!(
            parsed,
            BlockRef {
                instance: "9f1e2d3c-4b5a-4678-9a0b-c1d2e3f40516".to_string(),
                session: "20261102_101500_ab12".to_string(),
                round: "r:1762070100000:u-1".to_string(),
            }
        );
        assert_eq!(parsed.key(), REF_1);
        for bad in [
            "",
            "only-one",
            "a|b",
            "a|b|c|d",
            "..|b|r:1",
            "a|../etc|r:1",
            "a|b/c|r:1",
            "a|b|r with space",
            "a|b|not-prefixed",
            "a|b|r:",
            // Rejected here rather than downstream: the cache refuses `..` outright and
            // one token it refuses would fail the whole read, i.e. cost every other
            // block in the page a cross-host refetch on every render.
            "a|b|r:1..2",
            "a|b|r:..",
            "a|:b|r:1",
            "a|b|r:1|",
            &format!("{}|b|r:1", "x".repeat(MAX_ID_BYTES + 1)),
        ] {
            assert!(BlockRef::parse(bad).is_err(), "must reject {bad:?}");
        }
    }

    #[test]
    fn refs_are_deduped_and_keep_request_order() {
        let raw = format!("{REF_2}, {REF_1},{REF_2},,{OTHER_INSTANCE}");
        let slots = parse_refs(Some(&raw)).expect("valid refs");
        assert_eq!(
            slot_keys(&slots),
            vec![REF_2.to_string(), REF_1.to_string(), OTHER_INSTANCE.to_string()]
        );
        assert!(parse_refs(None).is_err());
        assert!(parse_refs(Some("")).is_err());
        assert!(parse_refs(Some(", ,")).is_err());
        // Asking for nothing is a client error. Asking for one nonsense token is not:
        // see `a_malformed_ref_is_answered_in_its_own_slot`.
        assert_eq!(slot_keys(&parse_refs(Some("junk")).expect("one bad token")), vec!["junk"]);
    }

    #[test]
    fn a_malformed_ref_is_answered_in_its_own_slot() {
        // D9's "never silently drop" applies to a ref the client should never have
        // sent too: it gets its own hole, in place, and the valid refs around it are
        // still fetched. 422-ing the request would let one broken transcript row turn
        // an entire history unreadable.
        let slots = parse_refs(Some(&format!("{REF_1},junk,{REF_2}"))).expect("parses");
        assert_eq!(
            slot_keys(&slots),
            vec![REF_1.to_string(), "junk".to_string(), REF_2.to_string()]
        );
        assert_eq!(valid_refs(&slots).len(), 2, "only the real refs are fetched");

        let mut resolutions: HashMap<String, Resolution> = HashMap::new();
        resolutions.insert(REF_1.to_string(), Resolution::Cached(projection("r:1", Some(CHAT), true)));
        let blocks = assemble(&slots, &resolutions);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["status"], "ok");
        assert_eq!(blocks[1]["status"], "not_found");
        assert_eq!(blocks[1]["ref"], "junk");
        assert!(blocks[1].get("projection").is_none());
        assert_eq!(blocks[2]["ref"], REF_2);
    }

    #[test]
    fn refs_are_bounded_at_the_caches_own_ceiling() {
        // The ceiling counts distinct work: 201 identical refs are one fetch, so
        // the bound has to bite on 201 different rounds.
        let many: Vec<String> = (0..=MAX_REFS)
            .map(|i| format!("9f1e2d3c-4b5a-4678-9a0b-c1d2e3f40516|20261102_101500_ab12|r:{i}"))
            .collect();
        let err = parse_refs(Some(&many.join(","))).unwrap_err();
        assert_eq!(err.code, 422);
        assert!(err.error.contains("too many refs"));
        let exact = &many[..MAX_REFS];
        assert_eq!(
            parse_refs(Some(&exact.join(","))).expect("fits").len(),
            MAX_REFS
        );
        // A repeated ref is one block, not two: the dedup happens before the
        // ceiling, or a client could exhaust it by echoing one id.
        let repeated = vec![REF_2.to_string(); MAX_REFS + 1].join(",");
        assert_eq!(parse_refs(Some(&repeated)).expect("deduped").len(), 1);
    }

    #[test]
    fn one_request_per_execution_session_regardless_of_interleaving() {
        let refs = vec![brf(REF_2), brf(OTHER_SESSION), brf(REF_1), brf(OTHER_INSTANCE)];
        let groups = group_by_session(&refs);
        let shape: Vec<(&str, &str, usize)> = groups
            .iter()
            .map(|(i, s, g)| (i.as_str(), s.as_str(), g.len()))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("9f1e2d3c-4b5a-4678-9a0b-c1d2e3f40516", "20261102_101500_ab12", 2),
                ("9f1e2d3c-4b5a-4678-9a0b-c1d2e3f40516", "20261102_102000_cd34", 1),
                ("11111111-2222-4333-8444-555555555555", "20261102_101500_ab12", 1),
            ]
        );
        // A group's rounds stay in ref order, so the batch reads in the order the
        // transcript names them.
        assert_eq!(
            groups[0].2,
            vec!["r:1762070200000:u-2".to_string(), "r:1762070100000:u-1".to_string()]
        );
    }

    #[test]
    fn a_lying_cache_row_is_distrusted_rather_than_served() {
        // The cache may only hold terminal rounds with an object payload. A row
        // that says otherwise is a broken side table, and the cheap answer is a
        // re-fetch — never a stale belief.
        let rows = vec![
            json!({
                "agent_instance_id": "i", "agent_session_id": "s", "round_id": "r:1",
                "terminal": false, "projection": projection("r:1", Some(CHAT), false)
            }),
            json!({
                "agent_instance_id": "i", "agent_session_id": "s", "round_id": "r:2",
                "terminal": true, "projection": "not-an-object"
            }),
            json!({ "agent_instance_id": "i", "agent_session_id": "s", "round_id": "r:3" }),
        ];
        let refs = vec![brf("i|s|r:1"), brf("i|s|r:2"), brf("i|s|r:3")];
        let (hits, missed) = apply_cache_hits(&rows, &refs);
        assert!(hits.is_empty(), "no row above is trustworthy");
        assert_eq!(keys_of(&missed), keys_of(&refs));
    }

    #[test]
    fn a_cache_hit_answers_and_leaves_the_rest_to_fetch() {
        let rows = vec![
            json!({
                "agent_instance_id": "i", "agent_session_id": "s", "round_id": "r:1",
                "terminal": true, "projection": projection("r:1", Some(CHAT), true)
            }),
            // The bounded cache keeps a round this transcript no longer names:
            // not asked, so neither served nor reported as a hole.
            json!({
                "agent_instance_id": "i", "agent_session_id": "s", "round_id": "r:9",
                "terminal": true, "projection": projection("r:9", Some(CHAT), true)
            }),
        ];
        let refs = vec![brf("i|s|r:1"), brf("i|s|r:2")];
        let (hits, missed) = apply_cache_hits(&rows, &refs);
        assert_eq!(hits.len(), 1);
        assert!(matches!(hits.get("i|s|r:1"), Some(Resolution::Cached(_))));
        assert_eq!(keys_of(&missed), vec!["i|s|r:2".to_string()]);
    }

    #[test]
    fn a_projection_bound_to_another_chat_is_a_hole() {
        // The poisoning case: a member of two chats naming a ref from the other
        // one. Answered as "no such block", and never written back.
        let (round, res) = classify_fetched(&projection("r:1", Some("chat-OTHER"), true), CHAT);
        assert_eq!(round, "r:1");
        assert_eq!(res, Resolution::Hole);
        // Unbound is refused for the same reason: a session belonging to no chat
        // has no business in a chat transcript.
        assert_eq!(
            classify_fetched(&projection("r:1", None, true), CHAT).1,
            Resolution::Hole
        );
        assert!(bound_to(&projection("r:1", Some(CHAT), true), CHAT));
    }

    #[test]
    fn an_absent_round_is_an_explicit_hole() {
        let entry = json!({ "round_id": "r:404", "found": false });
        let (round, res) = classify_fetched(&entry, CHAT);
        assert_eq!(round, "r:404");
        assert_eq!(res, Resolution::Hole);
        // A missing `found` reads as not-found, never as content.
        assert_eq!(
            classify_fetched(&json!({ "round_id": "r:5" }), CHAT).1,
            Resolution::Hole
        );
    }

    #[test]
    fn a_round_whose_rows_could_not_be_read_is_an_outage_not_a_hole() {
        // §5.7 answers a per-item read failure as `found: false` **plus** `error`,
        // precisely so a caller can tell "retry may help" from "nothing was ever
        // there". Rendering the first as the second would let a disk fault rewrite
        // history, and the surface offers no retry on a hole.
        let entry = json!({ "round_id": "r:7", "found": false, "error": "disk busy" });
        let (round, res) = classify_fetched(&entry, CHAT);
        assert_eq!(round, "r:7");
        assert_eq!(res, Resolution::Unreachable("disk busy".to_string()));
        // And an outage is never a write-back candidate — there is no projection.
        let mut resolutions: HashMap<String, Resolution> = HashMap::new();
        resolutions.insert(REF_1.to_string(), res);
        assert!(select_write_back(&resolutions).is_empty());
    }

    #[test]
    fn an_unrequested_round_in_a_response_is_ignored_and_absence_is_a_hole() {
        let asked = vec!["r:1".to_string(), "r:2".to_string()];
        let entries = vec![
            projection("r:1", Some(CHAT), true),
            projection("r:INJECTED", Some(CHAT), true),
        ];
        let mut resolutions: HashMap<String, Resolution> = HashMap::new();
        merge_entries(&entries, "i", "s", &asked, CHAT, &mut resolutions);
        assert!(matches!(
            resolutions.get("i|s|r:1"),
            Some(Resolution::Live(_))
        ));
        assert!(!resolutions.contains_key("i|s|r:INJECTED"));
        // Asked and unanswered: a hole, so the render does not stall on it.
        assert!(matches!(resolutions.get("i|s|r:2"), Some(Resolution::Hole)));
    }

    #[test]
    fn only_a_live_terminal_round_is_written_back() {
        let mut resolutions: HashMap<String, Resolution> = HashMap::new();
        resolutions.insert(
            REF_1.to_string(),
            Resolution::Cached(projection("r:1", Some(CHAT), true)),
        );
        resolutions.insert(
            REF_2.to_string(),
            Resolution::Live(projection("r:2", Some(CHAT), true)),
        );
        // D5: an open round is never cached, whatever the caller thinks.
        resolutions.insert(
            OTHER_SESSION.to_string(),
            Resolution::Live(projection("r:1", Some(CHAT), false)),
        );
        resolutions.insert(OTHER_INSTANCE.to_string(), Resolution::Hole);
        resolutions.insert(
            "i|s|r:5".to_string(),
            Resolution::Unreachable("down".to_string()),
        );
        let rows = select_write_back(&resolutions);
        assert_eq!(rows.len(), 1, "the freshly-read terminal round only");
        assert_eq!(rows[0]["round_id"], "r:1762070200000:u-2");
        assert_eq!(rows[0]["terminal"], true);
        assert_eq!(
            rows[0]["agent_session_id"],
            "20261102_101500_ab12".to_string()
        );
        assert_eq!(rows[0]["projection"]["round_id"], "r:2");
    }

    #[test]
    fn the_answer_names_every_ref_requested_in_order() {
        let slots = slots_of(&[REF_1, REF_2, OTHER_SESSION, OTHER_INSTANCE]);
        let mut resolutions: HashMap<String, Resolution> = HashMap::new();
        resolutions.insert(
            REF_1.to_string(),
            Resolution::Cached(projection("r:1", Some(CHAT), true)),
        );
        resolutions.insert(
            REF_2.to_string(),
            Resolution::Live(projection("r:2", Some(CHAT), true)),
        );
        resolutions.insert(
            OTHER_SESSION.to_string(),
            Resolution::Live(projection("r:1", Some(CHAT), false)),
        );
        // OTHER_INSTANCE is left unanswered on purpose: a ref nobody answered is
        // an outage for that block, not a row that disappears.
        let blocks = assemble(&slots, &resolutions);
        let status: Vec<&str> = blocks
            .iter()
            .map(|b| b["status"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(status, vec!["ok", "ok", "pending", "unreachable"]);
        assert_eq!(blocks[0]["source"], "cache".to_string());
        assert_eq!(blocks[1]["source"], "runtime".to_string());
        assert_eq!(blocks[3]["ref"], OTHER_INSTANCE.to_string());
        assert_eq!(blocks[3]["round_id"], "r:1-x".to_string());
        assert!(blocks[3]["projection"].is_null());
        // One hole never swallows the rest: the transcript still renders.
        assert_eq!(blocks[0]["projection"]["answer"]["body"], "the answer");
    }

    #[test]
    fn a_hole_carries_no_projection_to_render() {
        let slots = slots_of(&[REF_1]);
        let mut resolutions: HashMap<String, Resolution> = HashMap::new();
        resolutions.insert(REF_1.to_string(), Resolution::Hole);
        let blocks = assemble(&slots, &resolutions);
        assert_eq!(blocks[0]["status"], "not_found".to_string());
        assert!(blocks[0].get("projection").is_none(), "no invented content");
        assert!(blocks[0].get("source").is_none());
    }

    #[test]
    fn the_runtime_scope_matches_the_agent_proxy_mapping() {
        // A plain user is themselves, with no admin header: the pair travels
        // together or not at all.
        let plain = AuthContext {
            user_id: "u-1".to_string(),
            role: Role::User,
            as_user: Some("u-2".to_string()),
        };
        assert_eq!(runtime_scope_pair(&plain), ("u-1".to_string(), None));
        let admin = AuthContext {
            user_id: "a-1".to_string(),
            role: Role::Admin,
            as_user: None,
        };
        assert_eq!(
            runtime_scope_pair(&admin),
            (SCOPE_ALL.to_string(), Some("a-1".to_string()))
        );
        let viewing = AuthContext {
            user_id: "a-1".to_string(),
            role: Role::Admin,
            as_user: Some("u-2".to_string()),
        };
        assert_eq!(runtime_scope_pair(&viewing), ("u-2".to_string(), None));
        // The header builder refuses to emit `*` without its admin identity, so
        // a mismatched pair would silently degrade to a local-mode caller.
        let headers = runtime_scope_headers(&runtime_scope_pair(&admin).0, Some("a-1"), &[CHAT.to_string()]);
        assert_eq!(
            headers
                .get("x-acowork-scope-admin")
                .and_then(|v| v.to_str().ok()),
            Some("a-1")
        );
        assert_eq!(
            headers
                .get("x-acowork-chat-scope")
                .and_then(|v| v.to_str().ok()),
            Some(CHAT)
        );
    }

    fn test_state() -> AppState {
        let dir = std::env::temp_dir().join(format!("acowork-chatblocks-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&dir);
        let gw = crate::gateway::state::GatewayState::new(&dir.to_string_lossy());
        AppState::new(
            std::sync::Arc::new(tokio::sync::RwLock::new(gw)),
            std::sync::Arc::new(crate::http::auth::HttpAuth::new(false)),
        )
    }

    fn caller(id: &str) -> AuthContext {
        // Inserted as the bare context, which is what `Extension<T>` reads: the
        // middleware stores the value, the extractor rewraps it.
        AuthContext {
            user_id: id.to_string(),
            role: Role::User,
            as_user: None,
        }
    }

    /// Drive the route directly: the guards under test run before any I/O, so no
    /// user service and no Runtime is needed to prove what is refused and in
    /// which order.
    async fn get_status(uri: &str, ctx: Option<AuthContext>) -> StatusCode {
        let app = chat_block_routes().with_state(test_state());
        let mut request = Request::builder()
            .uri(uri)
            .body(axum::body::Body::empty())
            .expect("request builds");
        if let Some(c) = ctx {
            request.extensions_mut().insert(c);
        }
        let resp = app.oneshot(request).await.expect("handler answers");
        resp.status()
    }

    #[tokio::test]
    async fn a_malformed_chat_id_is_the_hidden_404_before_anything_else() {
        // Ordered ahead of the identity check on purpose: answering 422 here
        // would tell an unauthenticated probe which ids are well-formed.
        let uri = format!("/api/chat-blocks/not%20a-chat?refs={REF_1}");
        assert_eq!(get_status(&uri, None).await, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_unauthenticated_caller_is_refused_before_any_io() {
        let uri = format!("/api/chat-blocks/{CHAT}?refs={REF_1}");
        assert_eq!(get_status(&uri, None).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_malformed_ref_does_not_reject_the_page_it_travels_in() {
        // One bad token used to 422 the whole request. It is now answered as a hole in
        // its own slot, so the request gets past ref parsing and stops on what this
        // harness actually lacks — the user service.
        let uri = format!("/api/chat-blocks/{CHAT}?refs=junk");
        assert_eq!(
            get_status(&uri, Some(caller("u-1"))).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        // Asking for nothing at all is still a client error, not an empty page.
        let uri = format!("/api/chat-blocks/{CHAT}");
        assert_eq!(
            get_status(&uri, Some(caller("u-1"))).await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    /// A well-formed request with no user service running says 503 rather than
    /// serving a page of holes: the cache read is the first thing this endpoint
    /// does, and there is nothing to degrade past.
    #[tokio::test]
    async fn no_user_service_is_an_outage_rather_than_a_page_of_holes() {
        let uri = format!("/api/chat-blocks/{CHAT}?refs={REF_1}");
        assert_eq!(
            get_status(&uri, Some(caller("u-1"))).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    // ── Orchestration: the promise the cache exists for ─────────────────────
    //
    // The tests above pin single decisions. These drive the handler against a
    // stand-in user service and a stand-in Runtime, because the property being
    // claimed is a *sequence*: render once, pay a fetch; render twice, pay
    // nothing. That is only observable on the composed path.
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The instance and session the fixture refs point at.
    const INSTANCE: &str = "9f1e2d3c-4b5a-4678-9a0b-c1d2e3f40516";
    const SESSION: &str = "20261102_101500_ab12";

    /// What the stand-in chat service holds: cache key -> cached row.
    type FakeCache = std::sync::Arc<tokio::sync::Mutex<HashMap<String, Value>>>;

    struct FakeBackend {
        user_port: u16,
        runtime_port: u16,
        fetches: std::sync::Arc<AtomicUsize>,
        cache: FakeCache,
    }

    /// Boot both services this endpoint composes. The stand-in cache refuses a
    /// non-terminal row exactly like `rounds_cache::write_many` does, so a test
    /// cannot pass on a cache that quietly stores what it was told not to.
    async fn fake_backend(terminal: bool, bound: &str) -> FakeBackend {
        use axum::response::IntoResponse;

        let cache: FakeCache = Default::default();
        let read_cache = cache.clone();
        let write_cache = cache.clone();
        let user_app = Router::new()
            .route(
                "/internal/chat-sessions/{sid}/rounds",
                get(move |Path(sid): Path<String>, Query(q): Query<HashMap<String, String>>| {
                    let cache = read_cache.clone();
                    async move {
                        if q.get("who").map(|w| w.is_empty()).unwrap_or(true) {
                            return (StatusCode::UNPROCESSABLE_ENTITY, "who is required").into_response();
                        }
                        // One chat exists here: any other id is the hidden 404 a
                        // non-participant gets from the real service.
                        if sid != CHAT {
                            return StatusCode::NOT_FOUND.into_response();
                        }
                        let held = cache.lock().await;
                        let rounds: Vec<Value> = q
                            .get("refs")
                            .cloned()
                            .unwrap_or_default()
                            .split(',')
                            .filter(|k| !k.is_empty())
                            .filter_map(|k| held.get(k).cloned())
                            .collect();
                        Json(json!({ "rounds": rounds })).into_response()
                    }
                })
                .put(move |body: Json<Value>| {
                    let cache = write_cache.clone();
                    async move {
                        let mut stored = 0usize;
                        let mut held = cache.lock().await;
                        if let Some(rows) = body.get("rounds").and_then(|r| r.as_array()) {
                            for row in rows {
                                if row.get("terminal").and_then(|v| v.as_bool()) != Some(true) {
                                    continue;
                                }
                                let key = format!(
                                    "{}|{}|{}",
                                    row["agent_instance_id"].as_str().unwrap_or_default(),
                                    row["agent_session_id"].as_str().unwrap_or_default(),
                                    row["round_id"].as_str().unwrap_or_default(),
                                );
                                held.insert(key, row.clone());
                                stored += 1;
                            }
                        }
                        Json(json!({ "stored": stored })).into_response()
                    }
                }),
            )
            // The membership set every Runtime fetch is authorised with.
            .route(
                "/internal/chat-sessions",
                get(|| async { Json(vec![CHAT]) }),
            );

        let fetches = std::sync::Arc::new(AtomicUsize::new(0));
        let counted = fetches.clone();
        let bound = bound.to_string();
        let runtime_app = Router::new().route(
            "/sessions/{sid}/rounds",
            get(move |Query(q): Query<HashMap<String, String>>| {
                let counted = counted.clone();
                let bound = bound.clone();
                async move {
                    counted.fetch_add(1, Ordering::SeqCst);
                    let rounds: Vec<Value> = q
                        .get("ids")
                        .cloned()
                        .unwrap_or_default()
                        .split(',')
                        .filter(|r| !r.is_empty())
                        .map(|round| projection(round, Some(&bound), terminal))
                        .collect();
                    Json(json!({ "rounds": rounds }))
                }
            }),
        );

        let user_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake user service");
        let user_port = user_listener.local_addr().unwrap().port();
        tokio::spawn(async move { let _ = axum::serve(user_listener, user_app).await; });
        let runtime_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake runtime");
        let runtime_port = runtime_listener.local_addr().unwrap().port();
        tokio::spawn(async move { let _ = axum::serve(runtime_listener, runtime_app).await; });
        FakeBackend {
            user_port,
            runtime_port,
            fetches,
            cache,
        }
    }

    /// The Gateway state a real deployment would have at this moment: a ready
    /// user service and one registered Runtime endpoint.
    async fn orchestrated(backend: &FakeBackend) -> AppState {
        let mut state = test_state();
        let registry = std::sync::Arc::new(tokio::sync::RwLock::new(
            crate::http::proxy::RuntimeHttpRegistry::new(),
        ));
        registry
            .write()
            .await
            .register(INSTANCE, &format!("http://127.0.0.1:{}", backend.runtime_port));
        state.runtime_http_registry = Some(registry);
        let mut gw = state.gateway_state.write().await;
        gw.user_process = Some(crate::lifecycle::user_supervisor::UserProcessState {
            pid: 0,
            port: backend.user_port,
            ready: true,
        });
        drop(gw);
        state
    }

    /// One render: the status and the parsed body, so an assertion can name the
    /// block it could not match rather than printing a status code.
    async fn render(state: AppState, uri: &str) -> (StatusCode, Value) {
        let app = chat_block_routes().with_state(state);
        let mut request = Request::builder()
            .uri(uri)
            .body(axum::body::Body::empty())
            .expect("request builds");
        request.extensions_mut().insert(caller("u-1"));
        let response = app.oneshot(request).await.expect("handler answers");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body reads");
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// The performance property D5 exists for: the first render pays one batched
    /// fetch, the second is answered from this chat's own side table. Without it
    /// every repaint would put the slowest remote round-trip in the feature on
    /// the transcript — and history would stop being readable when its node left.
    #[tokio::test]
    async fn the_first_render_fetches_and_the_second_reads_the_cache() {
        let backend = fake_backend(true, CHAT).await;
        let state = orchestrated(&backend).await;
        assert!(REF_1.starts_with(&format!("{INSTANCE}|{SESSION}|")));
        let uri = format!("/api/chat-blocks/{CHAT}?refs={REF_1},{REF_2}");

        let (status, first) = render(state.clone(), &uri).await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(
            backend.fetches.load(Ordering::SeqCst),
            1,
            "two refs of one session cost one batched request"
        );
        let blocks = first["blocks"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["status"], "ok");
        assert_eq!(blocks[0]["source"], "runtime");
        assert_eq!(blocks[1]["ref"], REF_2, "the answer keeps request order");
        assert_eq!(
            backend.cache.lock().await.len(),
            2,
            "both rounds were written back"
        );

        let (status, second) = render(state, &uri).await;
        assert_eq!(status, StatusCode::OK, "{second}");
        assert_eq!(
            backend.fetches.load(Ordering::SeqCst),
            1,
            "the second render asked the Runtime for nothing"
        );
        let blocks = second["blocks"].as_array().unwrap();
        assert_eq!(blocks[0]["source"], "cache");
        assert_eq!(blocks[0]["projection"]["answer"]["body"], "the answer");
    }

    /// An open round is served and never stored (D5), so a second render of the
    /// same block pays the fetch again — the cost of a cache with no invalidation
    /// path, taken deliberately.
    #[tokio::test]
    async fn an_open_round_is_rendered_and_never_cached() {
        let backend = fake_backend(false, CHAT).await;
        let state = orchestrated(&backend).await;
        let uri = format!("/api/chat-blocks/{CHAT}?refs={REF_1}");

        let (status, first) = render(state.clone(), &uri).await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["blocks"][0]["status"], "pending", "an open round says so");
        assert!(backend.cache.lock().await.is_empty(), "nothing was written back");

        let (_, second) = render(state, &uri).await;
        assert_eq!(
            backend.fetches.load(Ordering::SeqCst),
            2,
            "no cache, so no free second render"
        );
        assert_eq!(second["blocks"][0]["status"], "pending");
    }

    /// A stopped node costs a hole, not a failed page (D9): the transcript still
    /// renders, and nothing unverified is offered to the cache.
    #[tokio::test]
    async fn an_unregistered_runtime_is_a_hole_not_an_outage() {
        let backend = fake_backend(true, CHAT).await;
        let mut state = orchestrated(&backend).await;
        // A registry with no endpoint: the Runtime this ref points at is not up.
        state.runtime_http_registry = Some(std::sync::Arc::new(tokio::sync::RwLock::new(
            crate::http::proxy::RuntimeHttpRegistry::new(),
        )));

        let (status, body) = render(state, &format!("/api/chat-blocks/{CHAT}?refs={REF_1}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["blocks"][0]["status"], "unreachable");
        assert!(
            body["blocks"][0].get("projection").is_none(),
            "an outage carries no content to render"
        );
        assert_eq!(backend.fetches.load(Ordering::SeqCst), 0);
    }

    /// Acceptance 9: a ref naming another chat's execution session yields
    /// neither content nor a cache row. The projection's own binding decides what
    /// this chat may keep, not the caller's scope headers.
    #[tokio::test]
    async fn a_projection_bound_to_another_chat_is_neither_served_nor_stored() {
        let backend = fake_backend(true, "some-other-chat").await;
        let state = orchestrated(&backend).await;

        let (status, body) = render(state, &format!("/api/chat-blocks/{CHAT}?refs={REF_1}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["blocks"][0]["status"], "not_found", "refused as a hole");
        assert!(
            backend.cache.lock().await.is_empty(),
            "and never written back — a foreign projection must not become history"
        );
    }
}
