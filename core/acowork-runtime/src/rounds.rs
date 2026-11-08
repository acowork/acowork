//! ADR-106 D2 / D4 — the round index and the round projection.
//!
//! A **round** is one turn of work handed to an execution session: the human's
//! message, the agent's reply, and whatever card the round is waiting on. It is
//! the only join key between the chat model and the execution model, and the chat
//! holds nothing but a *reference* to it (D3) — the content is projected from this
//! side at read time.
//!
//! Two durable artefacts live here:
//!
//! 1. `round_id` in the metadata of every entry the round produces, injected at the
//!    write funnel ([`crate::conversation::ConversationSession::append_message_with_id`]).
//!    D2 forbids inferring a round from message order, so the stamp is explicit.
//! 2. `{conversations_dir}/{session_id}.jsonl.rounds`, an append-only **disposable
//!    side table** in the ADR-097 family: one line per round state change, folded on
//!    read, last line per `round_id` wins.
//!
//! The side table stores **coordinates, not content** (D4): `user` / `answer` are
//! ADR-050 data-index positions into the session JSONL, so a projection reads two
//! lines plus one bounded window instead of scanning the transcript — the property
//! that keeps this off the ADR-096 redline.
//!
//! Disposability contract (ADR-097 D3/D4): the file is derived state. A read failure
//! degrades to "rebuild" or to an empty index; it must never make a session
//! unreadable, and a write failure is logged and ignored.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::conversation::{ConversationEntry, read_messages_paginated};
use crate::error::Result;

/// Metadata key carrying the round coordinate on a JSONL entry (D2).
pub const META_KEY_ROUND_ID: &str = "round_id";

/// Deterministic `round_id` prefix (D2: `r:{inbound_message_id}`).
pub const ROUND_ID_PREFIX: &str = "r:";

/// Upper bound on the opaque id, per D2 ("≤ 128 bytes").
pub const ROUND_ID_MAX_BYTES: usize = 128;

/// Status written for a round that was still open when its session reloaded — the
/// crash-repair terminal (D4). A killed process therefore cannot leave a round
/// stuck "in progress" forever, which is the property ADR-100 D10 lacked.
pub const STATUS_INTERRUPTED: &str = "interrupted";

/// Status of an open round that is not waiting on a human.
pub const STATUS_IN_PROGRESS: &str = "in_progress";

/// Status of a round parked on a card the human can still answer (ADR-106 D8).
///
/// The only terminal status that keeps its `card` (see [`ActiveRound::forced_closure`]):
/// a round abandoned while its question was open is *waiting*, not interrupted, and
/// the answer path still exists.
pub const STATUS_WAITING: &str = "waiting";

/// Status a rebuilt round carries when the transcript proves the round is over but
/// not *how* it ended. A rebuilt side table has no terminal records to read, so
/// `unknown` is the honest label — the projected content (user row, answer row)
/// still comes from the main store verbatim.
pub const STATUS_UNKNOWN: &str = "unknown";

/// Ceiling on the transcript size the first-touch rebuild is allowed to walk.
/// Above it the index is left empty, so a projection on a legacy session degrades
/// to an explicit hole instead of paying an unbounded read at open time.
const REBUILD_MAX_ENTRIES: u64 = 20_000;

/// Page size used while folding the transcript in [`rebuild_from_log`].
const REBUILD_PAGE: u32 = 512;

/// Bounded window read after the user row to fold attachment rows (D4).
const ATTACHMENT_WINDOW: u32 = 32;

/// The five `AttachmentMeta` tag values (ADR-046). An entry in the window after the
/// user row counts as an attachment only when its metadata `type` is one of these —
/// a `system` row that is not an attachment must not leak into the block.
const ATTACHMENT_TYPES: [&str; 5] = [
    "file_upload",
    "image_upload",
    "attached_file",
    "attached_selection",
    "attached_folder",
];

/// Mint the round coordinate for an inbound message (D2).
///
/// Deterministic by contract: the same `message_id` re-delivered (MQTT QoS
/// redelivery, an HTTP retry) maps to the same round. That is why no protocol field
/// is needed and why the chat-side reference row deduplicates by construction.
pub fn mint_round_id(inbound_message_id: &str) -> String {
    let trimmed = inbound_message_id.trim();
    if trimmed.is_empty() {
        return format!("{ROUND_ID_PREFIX}{}", uuid::Uuid::new_v4());
    }
    let id = format!("{ROUND_ID_PREFIX}{trimmed}");
    // Two independent reasons to fold, handled the same way: too long to store, or
    // carrying characters no reader can address. The inbound id is a free-form client
    // field, and a `round_id` that cannot be written into a URL, a query string or a
    // ref is a round that exists but can never be read — so the mint never emits one.
    if id.len() <= ROUND_ID_MAX_BYTES && is_addressable(&id) {
        return id;
    }
    // Oversized or unaddressable inbound id (a hand-typed client): hash it so the
    // coordinate stays bounded and addressable while remaining a pure function of the
    // input.
    //
    // A stable digest, not `DefaultHasher`: this value is written into every row of the
    // round, into the side table, and (from P4) into the chat's reference row as its
    // idempotency key — so it must survive a toolchain upgrade. A hash whose algorithm
    // is an implementation detail would mint a *second* round for a message that was
    // already delivered, which is precisely the failure D2 folds against.
    let digest = <sha2::Sha256 as sha2::Digest>::digest(trimmed.as_bytes());
    let head: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("{ROUND_ID_PREFIX}{head}-{}", trimmed.len())
}

/// Can this string be used as a `round_id` by every hop that carries one?
///
/// The set is the intersection of what the three readers accept: the Runtime's URL
/// path/query, the Gateway's `instance|session|round` ref, and the chat cache's SQL key
/// — alphanumerics plus `_ - : .`, with `..` excluded because it names a parent
/// directory wherever a path is built from the id. Minting and addressing MUST test
/// against this one predicate; two hand-rolled character sets on the two ends is how a
/// round becomes unaddressable at birth.
pub fn is_addressable(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('.')
        && !id.contains("..")
        && id.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':' | b'.'))
}

/// Path of one session's round side table.
///
/// Named *after* the transcript rather than as another `*.jsonl`: inside
/// `conversations/` that suffix **is** session identity (ADR-050 / ADR-082 — the
/// conversation index discovers sessions by it, and reconciles them by it), so a side
/// table called `{sid}.rounds.jsonl` would be swept forever as a session whose lines
/// never parse, and never be reconciled away. The derived file has to stay outside
/// that namespace.
pub fn rounds_path(conversations_dir: &Path, session_id: &str) -> PathBuf {
    conversations_dir.join(format!("{session_id}.jsonl.rounds"))
}

/// One line of the side table: the full snapshot of a round at the moment it was
/// appended. Fold-on-read takes the **last** line per `round_id`, so an update is an
/// append rather than a rewrite — and the single writer thread is the only party
/// that ever touches the file (ADR-092).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundRecord {
    pub round_id: String,
    /// Data index of the round's user entry, once the transcript has one.
    ///
    /// `None` means the round is **recorded but has written nothing yet**: it was
    /// announced at the turn boundary the moment its Chat reference row was about
    /// to become visible (see [`announce_round`]), and a turn can also die before
    /// writing its opener. The distinction matters because a coordinate is a claim
    /// about a row, and a round with no row has no claim to make — not a false one.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub user: Option<u64>,
    /// Data index of the round's latest assistant entry, once there is one.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub answer: Option<u64>,
    /// Card the round is blocked on (`{kind, request_id, …}`), if any.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub card: Option<Value>,
    /// `true` once the round reached a terminal state. A terminal record is never
    /// rewritten — the immutability the projection cache (D5) rests on.
    pub terminal: bool,
    /// `completed` / `aborted` / `failed` / `waiting` / `interrupted`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub status: Option<String>,
    pub ts: String,
}

/// The writer's live tracking of the round currently being appended.
///
/// `status` / `card` carry the last state the turn code published, so a snapshot
/// taken for a coordinate update does not silently drop them (the fold is
/// last-wins — a dropped field would erase the card from the index).
#[derive(Debug, Clone, Default)]
pub struct ActiveRound {
    pub round_id: String,
    pub user: Option<u64>,
    pub answer: Option<u64>,
    pub status: Option<String>,
    pub card: Option<Value>,
}

impl ActiveRound {
    /// Fold one committed entry into the active round.
    ///
    /// Returns the records that must be appended, in order: at most one forced
    /// closure of the *previous* round (one that never reached `end_round` — a crash,
    /// or a pre-empting turn) plus the snapshot of the current round. Called on the
    /// writer thread after the entry is on disk, so `data_index` is that entry's
    /// ADR-050 coordinate.
    pub fn note_entry(&mut self, entry: &ConversationEntry, data_index: u64) -> Vec<RoundRecord> {
        let Some(round_id) = entry
            .metadata
            .as_ref()
            .and_then(|m| m.get(META_KEY_ROUND_ID))
            .and_then(|v| v.as_str())
        else {
            return Vec::new();
        };
        let round_id = round_id.to_string();
        let ts = entry.ts.clone();
        let mut out = Vec::new();

        if self.round_id != round_id {
            if let Some(closed) = self.forced_closure(&ts) {
                out.push(closed);
            }
            *self = ActiveRound {
                round_id,
                ..Default::default()
            };
        }

        let mut changed = false;
        if entry.role == "user" && !entry.is_internal() && self.user.is_none() {
            self.user = Some(data_index);
            changed = true;
        } else if entry.role == "assistant" && !entry.is_internal() {
            // Keep the LATEST assistant entry: a round's answer is its final message,
            // a mid-round partial is not the block's content.
            self.answer = Some(data_index);
            changed = true;
        }
        if changed && self.user.is_some() {
            out.push(self.snapshot(false, None, None, &ts));
        }
        out
    }

    /// Close the tracked round because another round started without an `end_round`.
    ///
    /// A round that had already published a state keeps it: a round parked on a
    /// `pause` card which the human answered by starting something else was not
    /// *interrupted*, it was abandoned while waiting, and `waiting` plus its card is
    /// the honest rendering. Only a round that never published anything — one cut off
    /// mid-flight — gets the `interrupted` terminal.
    ///
    /// A round with no recorded coordinate is closed the same way: announcing a round
    /// (see [`announce_round`]) can precede its first row, so "abandoned before it
    /// wrote anything" is a real state and must not stay open forever — an open
    /// record is what the projection reports as `pending`, and a pending block that
    /// can never finish is a different lie from the hole it replaced.
    ///
    /// The card survives only a [`STATUS_WAITING`] park. Any other terminal status
    /// means the interaction that owned the card is gone, and a terminal record that
    /// still carries a question freezes a dead card into the block: a terminal
    /// projection is cached with no invalidation path (D5), so the surface would keep
    /// painting an answerable question nobody can answer any more. `waiting` is
    /// exempt because that card is still live — a `pause` is answered through the
    /// composer, against the reference row, not against the request id.
    pub fn forced_closure(&self, ts: &str) -> Option<RoundRecord> {
        if self.round_id.is_empty() {
            return None;
        }
        let status = Some(match self.status.as_deref() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => STATUS_INTERRUPTED.to_string(),
        });
        let card = if status.as_deref() == Some(STATUS_WAITING) {
            None
        } else {
            Some(Value::Null)
        };
        Some(self.snapshot(true, card, status, ts))
    }

    /// Build the snapshot line for the tracked round.
    ///
    /// `card` / `status` default to what the tracker already holds — pass `None` to
    /// leave them alone, `Some(Value::Null)` to state them explicitly. For the card,
    /// "explicitly" means *no card*: a round whose interaction was resolved
    /// mid-turn has nothing left to be parked on, and a block still showing a
    /// clickable question would be lying about a live turn.
    pub fn snapshot(
        &self,
        terminal: bool,
        card: Option<Value>,
        status: Option<String>,
        ts: &str,
    ) -> RoundRecord {
        let card = match card {
            Some(v) if v.is_null() => None,
            Some(v) => Some(v),
            None => self.card.clone(),
        };
        RoundRecord {
            round_id: self.round_id.clone(),
            user: self.user,
            answer: self.answer,
            card,
            terminal,
            status: status.or_else(|| self.status.clone()),
            ts: ts.to_string(),
        }
    }

    pub fn round_id(&self) -> Option<&str> {
        if self.round_id.is_empty() {
            None
        } else {
            Some(self.round_id.as_str())
        }
    }
}

/// Append one snapshot line. Best-effort by contract: the caller logs and moves on.
pub fn append_record(path: &Path, rec: &RoundRecord) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let mut line = serde_json::to_string(rec)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    file.write_all(line.as_bytes())?;
    file.flush()
}

/// Fold the side table into the current state of each round (last line wins).
///
/// Reads only the side file — a handful of short lines per round — so it is bounded
/// by construction. A malformed line is skipped the way the JSONL readers skip a
/// corrupt line: the index must never make a session unreadable.
pub fn fold(path: &Path) -> std::io::Result<BTreeMap<String, RoundRecord>> {
    let mut map: BTreeMap<String, RoundRecord> = BTreeMap::new();
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(map),
        Err(e) => return Err(e),
    };
    use std::io::BufRead;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<RoundRecord>(&line) {
            Ok(rec) => {
                map.insert(rec.round_id.clone(), rec);
            }
            Err(e) => tracing::warn!(
                path = %path.display(),
                error = %e,
                "rounds index: skipping malformed line"
            ),
        }
    }
    Ok(map)
}

/// Make a round addressable in the index before anything can reference it.
///
/// D3 makes the Chat's `execution_round` reference row the gate a round must pass
/// through, and that row is written *before* the round runs. The index, by contrast,
/// only gained a record once the round's first stamped row was **committed** — so
/// between the two there was a window (tens of milliseconds) in which a reader that
/// could already see the reference got `not found` for the round it names. That is
/// not a harmless transient: D4's reader is triggered by exactly that reference row,
/// and D5 caches the hole as the block's permanent content, so the block said "this
/// run is no longer available" for a run that completed moments later.
///
/// Announcing the round at the turn boundary closes the window: once this returns,
/// every round the Chat can see is in the index, and a projection read answers
/// "open, nothing yet" (`terminal: false`, no coordinates) instead of a hole. The
/// surface re-reads that (D9), so the block fills in as the turn produces its rows.
///
/// Idempotent and non-destructive by construction: a round already recorded keeps its
/// coordinates untouched, which is what makes it safe to call for a continuation, and
/// a redelivery of the same round must never erase what the transcript already says.
/// Best-effort like the rest of the index (ADR-097 disposability): a disk fault costs
/// the block a transient hole, never the turn its content.
pub fn announce_round(path: &Path, round_id: &str, ts: &str) -> Option<RoundRecord> {
    if round_id.is_empty() {
        return None;
    }
    // A fold failure means the index cannot be trusted this moment; announcing on top
    // of it could shadow a coordinate that is still there. Leave it to the read path's
    // rebuild instead of writing a record we cannot confirm is new.
    let known = match fold(path) {
        Ok(map) => map,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                round_id = %round_id,
                error = %e,
                "rounds index: cannot announce a round over an unreadable index"
            );
            return None;
        }
    };
    if let Some(existing) = known.get(round_id) {
        return Some(existing.clone());
    }
    // No `status`: whether the round is running, waiting or finished is a fact of the
    // turn, not of the index. The marker only says the round exists.
    let record = RoundRecord {
        round_id: round_id.to_string(),
        user: None,
        answer: None,
        card: None,
        terminal: false,
        status: None,
        ts: ts.to_string(),
    };
    if let Err(e) = append_record(path, &record) {
        tracing::warn!(
            path = %path.display(),
            round_id = %round_id,
            error = %e,
            "rounds index: announce failed (the round itself is unaffected)"
        );
        return None;
    }
    Some(record)
}

/// Rebuild the side table from the transcript's `round_id` stamps.
///
/// This is the ONE sanctioned full read (ADR-097 first touch, ADR-096's stated
/// exception): it runs only when the side table is missing, it pages through the
/// canonical reader so the coordinates match [`read_messages_paginated`] exactly,
/// and it refuses transcripts above [`REBUILD_MAX_ENTRIES`].
///
/// Every rebuilt round is historical, hence terminal. The *outcome* cannot be
/// recovered from the transcript — that was a fact of the closing `end_round`, not
/// of the entries — so a round with an answer is labelled [`STATUS_UNKNOWN`] and
/// one without is labelled [`STATUS_INTERRUPTED`]. Inventing `completed` here would
/// be the mirror's old failure mode wearing a different hat.
fn rebuild_from_log(jsonl: &Path) -> Result<Vec<RoundRecord>> {
    // Cheap size probe first: one entry tells us `total` without a content walk.
    let probe = read_messages_paginated(jsonl, 0, 1, false)?;
    if probe.total > REBUILD_MAX_ENTRIES {
        tracing::info!(
            session_log = %jsonl.display(),
            total = probe.total,
            "rounds index: transcript too large to rebuild, leaving the index empty"
        );
        return Ok(Vec::new());
    }

    let mut order: Vec<String> = Vec::new();
    let mut states: BTreeMap<String, (u64, Option<u64>)> = BTreeMap::new();
    let mut cursor = 0u64;
    // Coordinates come from the reader, never from arithmetic on a page: a corrupt or
    // truncated line is skipped by pagination, and deriving `offset + index_in_page`
    // would then shift every coordinate after it into somebody else's row (D5 caches
    // the result with no way to invalidate it).
    while cursor < probe.total {
        let end = (cursor + REBUILD_PAGE as u64).min(probe.total);
        for (data_index, entry) in crate::conversation::read_entries_indexed(jsonl, cursor, end)? {
            let Some(round_id) = entry
                .metadata
                .as_ref()
                .and_then(|m| m.get(META_KEY_ROUND_ID))
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            if entry.is_internal() {
                continue;
            }
            let is_opener = entry.role == "user" && !states.contains_key(round_id);
            let is_answer = entry.role == "assistant";
            if !is_opener && !is_answer {
                continue;
            }
            if is_opener {
                order.push(round_id.to_string());
                states.insert(round_id.to_string(), (data_index, None));
            } else if let Some(state) = states.get_mut(round_id) {
                state.1 = Some(data_index);
            }
        }
        cursor = end;
    }

    let ts = now_ts();
    Ok(order
        .iter()
        .filter_map(|round_id| {
            let (user, answer) = states.get(round_id)?;
            Some(RoundRecord {
                round_id: round_id.clone(),
                user: Some(*user),
                answer: *answer,
                card: None,
                terminal: true,
                status: Some(match answer {
                    Some(_) => STATUS_UNKNOWN.to_string(),
                    None => STATUS_INTERRUPTED.to_string(),
                }),
                ts: ts.clone(),
            })
        })
        .collect())
}

/// Open-time index maintenance: make sure the side table exists, rebuild it once if
/// it does not, then close any round left open by a crash (D4).
///
/// Returns the number of rounds that were force-closed. Every failure mode here is
/// survivable — an unreadable index degrades to "no rounds projected", never to
/// "session cannot open".
pub fn repair_on_open(path: &Path, jsonl: &Path) -> usize {
    if !path.exists() {
        let records = match rebuild_from_log(jsonl) {
            Ok(records) => records,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "rounds index: rebuild from transcript failed"
                );
                return 0;
            }
        };
        for rec in &records {
            if let Err(e) = append_record(path, rec) {
                tracing::warn!(path = %path.display(), error = %e, "rounds index: rebuild append failed");
                return 0;
            }
        }
        // Rebuilt rounds are historical and already terminal, so this path needs no
        // crash closure.
        // Touch the file so "absent" and "present but empty" stop meaning the same
        // thing: a later open must not rebuild a second time.
        if let Err(e) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            tracing::warn!(path = %path.display(), error = %e, "rounds index: create failed");
        }
        return 0;
    }

    let map = match fold(path) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "rounds index: fold failed");
            return 0;
        }
    };
    let ts = now_ts();
    let mut closed = 0usize;
    for rec in map.values().filter(|r| !r.terminal) {
        let mut repaired = rec.clone();
        repaired.terminal = true;
        repaired.status = Some(STATUS_INTERRUPTED.to_string());
        // The interaction died with the process that held it: the request id in a
        // crash-repaired round can no longer be answered by anybody, so the card
        // must not survive into the terminal record. This is the one repair that
        // used to leave `terminal: true` + a live-looking question on disk, which
        // the projection cache then kept forever (D5) — the Chat's projected card
        // was clickable, did nothing, and never went away.
        repaired.card = None;
        repaired.ts = ts.clone();
        if let Err(e) = append_record(path, &repaired) {
            tracing::warn!(path = %path.display(), error = %e, "rounds index: crash repair append failed");
            return closed;
        }
        closed += 1;
    }
    if closed > 0 {
        tracing::debug!(
            count = closed,
            path = %path.display(),
            "rounds index: closed rounds left open by a crash"
        );
    }
    closed
}

/// Look up one round. `Ok(None)` means "this session has no such round" — the honest
/// 404 shape D5 turns into an explicit hole in the block.
pub fn read_record(path: &Path, round_id: &str) -> std::io::Result<Option<RoundRecord>> {
    Ok(fold(path)?.remove(round_id))
}

/// Fold the side table, rebuilding it from the transcript when it answers nothing.
///
/// D4's first-touch rebuild originally fires only from session open, but the
/// projection read deliberately does **not** open a session — that is what makes a
/// `Closed`, pruned-away or never-resident round readable. Without this, a session
/// whose side table was deleted, lost to a truncated write, or simply never created
/// (a Runtime upgraded mid-life, a workspace copied without the derived files) would
/// answer `found: false` for every round forever, even though each transcript row
/// still carries its `round_id` stamp and every fact needed to rebuild is on disk.
///
/// The cost is bounded and deliberately lazy: a rebuild only happens in exactly the
/// case where the read was going to return nothing anyway, it is capped by
/// [`REBUILD_MAX_ENTRIES`], and it persists what it derived so the next read is cheap.
pub fn fold_or_rebuild(path: &Path, jsonl: &Path) -> crate::error::Result<BTreeMap<String, RoundRecord>> {
    let folded = fold(path)?;
    if !folded.is_empty() || !jsonl.exists() {
        return Ok(folded);
    }
    let rebuilt = rebuild_from_log(jsonl)?;
    if rebuilt.is_empty() {
        return Ok(folded);
    }
    tracing::info!(
        path = %path.display(),
        rounds = rebuilt.len(),
        "rounds index: projection read rebuilt the side table (ADR-106 D4 first touch)"
    );
    for rec in &rebuilt {
        if let Err(e) = append_record(path, rec) {
            // Persisting is a convenience, not a condition: the caller still gets an
            // answer, and a side table that cannot be written stays disposable.
            tracing::warn!(path = %path.display(), error = %e, "rounds index: rebuild persist failed");
            break;
        }
    }
    Ok(rebuilt
        .into_iter()
        .map(|rec| (rec.round_id.clone(), rec))
        .collect())
}

/// The conversation-visible projection of one round (D4).
///
/// Deliberately narrow: the human's row, the agent's reply, the card the round waits
/// on. Tool calls, thinking and streaming never enter it — ADR-100 D3's "execution
/// detail never enters the chat" becomes "never enters the projection".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundProjection {
    pub round_id: String,
    pub user: Option<RoundUserRow>,
    pub answer: Option<RoundAnswerRow>,
    pub card: Option<Value>,
    pub terminal: bool,
    pub status: String,
}

/// The human half of a round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundUserRow {
    /// The row as stored. For an agent-origin turn this is the **machine
    /// envelope** (`[intent:…] {params}` + reply coordinates) — the model reads
    /// it, and a human surface must not print it directly.
    pub body: String,
    pub ts: String,
    #[serde(default)]
    pub attachments: Vec<Value>,
    /// The same turn for the other audience: the display projection the producer
    /// wrote beside the body (`metadata.intent`, design 27 §5.4.5).
    ///
    /// `None` on every human turn (its body already is the person's words) and on
    /// every row written before the projection existed — a surface that gets
    /// `None` renders `body`, which is the honest fallback rather than a blank.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<RoundUserDisplay>,
}

/// The human rendering of an agent-origin turn, as the Chat surfaces read it.
///
/// Shape owned by the producer (`intent_turn::IntentDisplay` serialises these
/// fields); the extra keys it carries (`origin`, `action`) are not needed here and
/// are ignored on read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundUserDisplay {
    /// One line: a block header, a round row, a list preview.
    #[serde(default)]
    pub summary: String,
    /// Markdown the reader gets in place of the envelope.
    #[serde(default)]
    pub body: String,
    /// `body` was cut by the producer; the raw `RoundUserRow::body` still holds
    /// everything.
    #[serde(default)]
    pub truncated: bool,
}

impl RoundUserDisplay {
    /// The display on a round's user row, when the row carries a usable one.
    ///
    /// A row with an empty body and no summary is treated as absent: painting an
    /// empty panel is worse than falling back to the stored row.
    fn from_entry(entry: &ConversationEntry) -> Option<Self> {
        let raw = entry
            .metadata
            .as_ref()
            .and_then(|m| m.get(crate::agent::intent_turn::META_KEY_INTENT_DISPLAY))?;
        let display: Self = serde_json::from_value(raw.clone()).ok()?;
        (!display.body.trim().is_empty() || !display.summary.trim().is_empty()).then_some(display)
    }
}

/// The agent half of a round. Its terminal state is carried by the enclosing
/// projection's `status`, not duplicated here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundAnswerRow {
    pub body: String,
    pub ts: String,
}

/// Assemble the projection for `rec` out of the session transcript.
///
/// Reads exactly the two referenced lines plus one bounded window after the user row
/// — no range scan, no full pass (ADR-096).
///
/// `Ok(None)` means *this round cannot be projected as it is recorded*: the transcript
/// is gone, or a named coordinate no longer resolves to the round's own row. The caller
/// must render an explicit hole. Returning whatever happens to sit at that position is
/// the one outcome this function may not produce, because a terminal projection is
/// cached with no invalidation path (D5) — a wrong row would then be a permanent one.
/// A genuine read failure stays `Err`, so `unreachable` and "no content" stay distinct.
///
/// A round that is *recorded but has no coordinate* is not a hole: it was announced
/// when its Chat reference became visible, and the turn has not written its opener
/// yet (or died before it could). It projects as an empty, non-terminal round.
pub fn project(jsonl: &Path, rec: &RoundRecord) -> Result<Option<RoundProjection>> {
    // Pruned or never-copied transcript: the content is not on this host. Not a server
    // fault, and not something a retry fixes — an explicit hole is.
    if !jsonl.exists() {
        tracing::warn!(
            transcript = %jsonl.display(),
            round_id = %rec.round_id,
            "round projection: transcript absent"
        );
        return Ok(None);
    }
    let Some(user_index) = rec.user else {
        // Nothing has been written under this round yet. Answering "not found" here is
        // what made a live block say its run was gone: the reader is triggered by the
        // reference row that announcing precedes, and D5 caches the hole forever.
        return Ok(Some(RoundProjection {
            round_id: rec.round_id.clone(),
            user: None,
            answer: None,
            card: rec.card.clone(),
            terminal: rec.terminal,
            status: projection_status(rec),
        }));
    };
    let Some(user) = read_entry_at(jsonl, user_index)? else {
        tracing::warn!(
            transcript = %jsonl.display(),
            round_id = %rec.round_id,
            user = user_index,
            "round projection: user coordinate past the end of the transcript"
        );
        return Ok(None);
    };
    if !row_is_the_rounds(&user, "user", &rec.round_id) {
        tracing::warn!(
            round_id = %rec.round_id,
            user = user_index,
            found_role = %user.role,
            "round projection: coordinate drifted to another entry — rendering a hole"
        );
        return Ok(None);
    }
    let attachments = fold_attachments(jsonl, user_index, rec.answer, &rec.round_id)?;
    let answer = match rec.answer {
        Some(idx) => {
            let Some(entry) = read_entry_at(jsonl, idx)? else {
                tracing::warn!(
                    round_id = %rec.round_id,
                    answer = idx,
                    "round projection: answer coordinate past the end of the transcript"
                );
                return Ok(None);
            };
            if !row_is_the_rounds(&entry, "assistant", &rec.round_id) {
                // Half a round is not a round: an answer-less `completed` block would
                // claim the agent replied. The hole is the honest rendering.
                tracing::warn!(
                    round_id = %rec.round_id,
                    answer = idx,
                    found_role = %entry.role,
                    "round projection: coordinate drifted to another entry — rendering a hole"
                );
                return Ok(None);
            }
            Some(entry)
        }
        None => None,
    };

    Ok(Some(RoundProjection {
        round_id: rec.round_id.clone(),
        user: Some(RoundUserRow {
            display: RoundUserDisplay::from_entry(&user),
            body: user.content,
            ts: user.ts,
            attachments,
        }),
        answer: answer.map(|e| RoundAnswerRow {
            body: e.content,
            ts: e.ts,
        }),
        card: rec.card.clone(),
        terminal: rec.terminal,
        status: projection_status(rec),
    }))
}

/// The state a recorded round projects to when it published no status of its own.
///
/// A terminal record without a status can only come from an abandoned round (a crash
/// repaired at open, or a pre-empted turn that never published), so `interrupted` is
/// the fact; a non-terminal one is the live round the Chat is watching.
fn projection_status(rec: &RoundRecord) -> String {
    rec.status.clone().unwrap_or_else(|| {
        if rec.terminal {
            STATUS_INTERRUPTED.to_string()
        } else {
            STATUS_IN_PROGRESS.to_string()
        }
    })
}

/// Does the entry sitting at a recorded coordinate actually belong to this round?
///
/// The side table stores *coordinates*, and a coordinate is a claim about a file that
/// compaction, a truncated tail, or a rewritten transcript can invalidate. D2 makes
/// every row of a round carry its own `round_id` stamp, which is the only fact able to
/// contradict that claim — so the projection checks the role and the stamp instead of
/// trusting the position.
fn row_is_the_rounds(entry: &ConversationEntry, role: &str, round_id: &str) -> bool {
    entry.role == role
        && entry
            .metadata
            .as_ref()
            .and_then(|m| m.get(META_KEY_ROUND_ID))
            .and_then(|v| v.as_str())
            == Some(round_id)
}

/// Fold the attachment rows that trail the user row (ADR-046 §2.1).
///
/// The window is bounded twice over: by [`ATTACHMENT_WINDOW`] and by the round's own
/// answer position, so a long round never turns this into a range scan.
fn fold_attachments(
    jsonl: &Path,
    user: u64,
    answer: Option<u64>,
    round_id: &str,
) -> Result<Vec<Value>> {
    let span = match answer {
        Some(answer) if answer > user => answer - user - 1,
        Some(_) => 0,
        None => u64::from(ATTACHMENT_WINDOW),
    };
    let span = span.min(u64::from(ATTACHMENT_WINDOW));
    if span == 0 {
        return Ok(Vec::new());
    }
    let page = read_messages_paginated(jsonl, user + 1, span as u32, false)?;
    Ok(page
        .messages
        .iter()
        .filter(|e| e.role == "system" && !e.is_internal() && is_attachment(e))
        // The window is a *range*, and a range is the one coordinate this projection
        // must not trust: a round cut off before its answer (crash, interrupt) has no
        // answer position to bound the window, so the rows trailing it belong to the
        // next round. Stamping the window rows the same way as the two named rows
        // (D2) keeps the block from inheriting another round's attachments — and from
        // caching that as immutable content (D5).
        .filter(|e| row_is_the_rounds(e, "system", round_id))
        .map(|e| {
            serde_json::json!({
                "type": e.metadata.as_ref().and_then(|m| m.get("type")).and_then(|v| v.as_str()),
                "content": e.content,
                "metadata": e.metadata,
            })
        })
        .collect())
}

/// Read the single entry sitting at an ADR-050 data index.
fn read_entry_at(jsonl: &Path, index: u64) -> Result<Option<ConversationEntry>> {
    let page = read_messages_paginated(jsonl, index, 1, false)?;
    Ok(page.messages.into_iter().next())
}

fn is_attachment(entry: &ConversationEntry) -> bool {
    entry
        .metadata
        .as_ref()
        .and_then(|m| m.get("type"))
        .and_then(|v| v.as_str())
        .is_some_and(|t| ATTACHMENT_TYPES.contains(&t))
}

pub fn now_ts() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn entry(role: &str, content: &str, round: Option<&str>) -> ConversationEntry {
        let metadata = round.map(|r| serde_json::json!({ META_KEY_ROUND_ID: r }));
        ConversationEntry {
            id: format!("{role}-{content}"),
            ts: "2026-11-02T00:00:00.000Z".to_string(),
            role: role.to_string(),
            content: content.to_string(),
            metadata,
            kind: None,
        }
    }

    fn write_jsonl(dir: &TempDir, entries: &[ConversationEntry]) -> PathBuf {
        let path = dir.path().join("s.jsonl");
        let mut body = String::new();
        for e in entries {
            body.push_str(&serde_json::to_string(e).unwrap());
            body.push('\n');
        }
        std::fs::write(&path, body).unwrap();
        path
    }

    fn rec(round_id: &str, user: u64, terminal: bool) -> RoundRecord {
        RoundRecord {
            round_id: round_id.to_string(),
            user: Some(user),
            answer: None,
            card: None,
            terminal,
            status: None,
            ts: "2026-11-02T00:00:00.000Z".to_string(),
        }
    }

    #[test]
    fn mint_is_deterministic_and_bounded() {
        assert_eq!(mint_round_id("msg-1"), "r:msg-1");
        assert_eq!(
            mint_round_id("msg-1"),
            mint_round_id("msg-1"),
            "a re-delivery must not mint a second round"
        );
        let long = mint_round_id(&"x".repeat(400));
        assert!(
            long.len() <= ROUND_ID_MAX_BYTES,
            "oversized id stays bounded: {}",
            long.len()
        );
        assert!(long.starts_with(ROUND_ID_PREFIX));
        assert_eq!(long, mint_round_id(&"x".repeat(400)), "still a pure function");
        assert_ne!(long, mint_round_id(&"y".repeat(400)));
        assert!(!mint_round_id("").eq(&mint_round_id("")));
    }

    #[test]
    fn the_mint_never_emits_an_unaddressable_round_id() {
        // `message_id` is a free-form client field. A `round_id` outside the reader's
        // character set is a round that exists but can never be fetched — and from P4
        // on, one such ref in a transcript would fail the whole block render.
        for dirty in ["turn 1", "\u{56de}\u{5408}-1", "a/b", "..", "a..b", "x#y"] {
            let id = mint_round_id(dirty);
            assert!(is_addressable(&id), "{dirty:?} minted an unaddressable {id:?}");
            assert!(id.len() <= ROUND_ID_MAX_BYTES, "{dirty:?} minted an oversized id");
            assert_eq!(id, mint_round_id(dirty), "still a pure function");
        }
        // The clean form is untouched — no hashing, so the id stays readable in logs.
        assert_eq!(mint_round_id("msg-1"), "r:msg-1");
    }

    #[test]
    fn a_waiting_round_keeps_its_state_when_pre_empted() {
        // A round parked on a pause card published `waiting` + the card. When a later
        // round starts without an `end_round`, closing it must not rewrite that state
        // as `interrupted` — the human abandoned the question, nothing interrupted it.
        let mut active = ActiveRound::default();
        active.note_entry(&entry("user", "u", Some("r:1")), 0);
        active.status = Some("waiting".to_string());
        active.card = Some(serde_json::json!({ "kind": "pause", "request_id": "q1" }));

        let closed = active.forced_closure("t2").unwrap();
        assert!(closed.terminal);
        assert_eq!(closed.status.as_deref(), Some("waiting"));
        assert_eq!(closed.card.unwrap()["request_id"], "q1");
    }

    /// A terminal record may carry a card only for a `waiting` park.
    ///
    /// Regression (2026-11-06): a round pre-empted, or closed by crash repair, kept
    /// its `ask_question` card into the terminal record. The block projection then
    /// served `terminal: true` + a question, the Gateway cached it as `ok` with no
    /// invalidation path (D5), and the Chat's projected card was clickable, did
    /// nothing (the request id was dead) and never went away until the window
    /// changed. The card must die with the interaction.
    #[test]
    fn a_terminal_round_drops_a_card_nobody_can_answer_any_more() {
        let mut active = ActiveRound {
            round_id: "r:1".to_string(),
            status: Some(STATUS_IN_PROGRESS.to_string()),
            card: Some(serde_json::json!({ "kind": "ask_question", "request_id": "q1" })),
            ..Default::default()
        };
        active.user = Some(0);
        let closed = active.forced_closure("t2").unwrap();
        assert!(closed.terminal);
        assert_eq!(closed.status.as_deref(), Some(STATUS_IN_PROGRESS));
        assert!(
            closed.card.is_none(),
            "a dead question must not survive into the terminal record"
        );

        // No published status at all is the same fact: interrupted, no card.
        let abandoned = ActiveRound {
            round_id: "r:2".to_string(),
            card: Some(serde_json::json!({ "kind": "ask_question", "request_id": "q2" })),
            ..Default::default()
        };
        let closed = abandoned.forced_closure("t3").unwrap();
        assert_eq!(closed.status.as_deref(), Some(STATUS_INTERRUPTED));
        assert!(closed.card.is_none());
    }

    /// Crash repair closes what the dead process left open — and with it the card
    /// whose request id died with the process.
    #[test]
    fn crash_repair_drops_the_card_of_the_open_round() {
        let dir = TempDir::new().unwrap();
        let jsonl = write_jsonl(&dir, &[entry("user", "u1", Some("r:1"))]);
        let rounds = dir.path().join("s.jsonl.rounds");
        let mut open = RoundRecord {
            round_id: "r:1".to_string(),
            user: Some(0),
            answer: None,
            card: Some(serde_json::json!({ "kind": "ask_question", "request_id": "q1" })),
            terminal: false,
            status: Some(STATUS_WAITING.to_string()),
            ts: "t1".to_string(),
        };
        append_record(&rounds, &open).unwrap();

        assert_eq!(repair_on_open(&rounds, &jsonl), 1);

        open = read_record(&rounds, "r:1").unwrap().unwrap();
        assert!(open.terminal);
        assert_eq!(open.status.as_deref(), Some(STATUS_INTERRUPTED));
        assert!(
            open.card.is_none(),
            "the repaired round must not keep a question nobody can answer"
        );
    }

    #[test]
    fn coordinate_snapshot_does_not_erase_the_published_state() {
        // The fold is last-wins over every record of a round, so a snapshot taken for
        // a new answer coordinate has to carry the status and the card forward.
        let active = ActiveRound {
            round_id: "r:1".to_string(),
            user: Some(0),
            answer: Some(3),
            status: Some("waiting".to_string()),
            card: Some(serde_json::json!({ "kind": "pause" })),
        };
        let rec = active.snapshot(false, None, None, "t3");
        assert_eq!(rec.status.as_deref(), Some("waiting"));
        assert!(rec.card.is_some());
        assert_eq!(rec.answer, Some(3));
    }

    #[test]
    fn fold_takes_the_last_snapshot_per_round() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("s.jsonl.rounds");
        let mut open = rec("r:1", 3, false);
        open.ts = "t1".into();
        append_record(&path, &open).unwrap();
        let mut closed = rec("r:1", 3, true);
        closed.answer = Some(7);
        closed.card = Some(serde_json::json!({ "kind": "pause", "request_id": "q1" }));
        closed.status = Some("completed".into());
        closed.ts = "t2".into();
        append_record(&path, &closed).unwrap();

        let got = read_record(&path, "r:1").unwrap().unwrap();
        assert_eq!(got.answer, Some(7));
        assert!(got.terminal);
        assert_eq!(got.status.as_deref(), Some("completed"));
        assert!(read_record(&path, "r:missing").unwrap().is_none());
    }

    /// Announcing is what closes the window between "the Chat can see this round" and
    /// "the index knows it". The announced record projects as *open and empty*, never
    /// as a hole: a hole is what the chat caches as the block's content (D5), and it
    /// is how a running block came to say its run was gone.
    #[test]
    fn an_announced_round_projects_as_open_instead_of_a_hole() {
        let dir = TempDir::new().unwrap();
        let jsonl = write_jsonl(&dir, &[]);
        let path = dir.path().join("s.jsonl.rounds");

        let rec = announce_round(&path, "r:fresh", "2026-11-05T00:00:00.000Z")
            .expect("announcing a fresh round succeeds");
        assert_eq!(rec.user, None, "the opener has not been written yet");
        assert!(!rec.terminal);

        let p = project(&jsonl, &rec)
            .unwrap()
            .expect("an announced round is not a hole");
        assert!(p.user.is_none(), "no row is invented for a round that has none");
        assert!(p.answer.is_none());
        assert!(!p.terminal);
        assert_eq!(p.status, STATUS_IN_PROGRESS);
    }

    /// A re-delivery announces the round the transcript already produced: the
    /// coordinate the index holds must survive, so this can never be a rewrite.
    #[test]
    fn announcing_a_known_round_keeps_its_coordinate() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("s.jsonl.rounds");
        append_record(&path, &rec("r:1", 3, true)).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let known = announce_round(&path, "r:1", "2026-11-05T00:00:00.000Z").unwrap();
        assert_eq!(known.user, Some(3), "the recorded coordinate wins");
        assert!(known.terminal);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "announcing a known round appends nothing"
        );
    }

    /// A round abandoned before it wrote anything is closed as interrupted, not left
    /// open forever: a permanently `pending` block is a different lie from the hole.
    #[test]
    fn a_round_closed_before_its_opener_is_not_a_hole() {
        let active = ActiveRound {
            round_id: "r:1".to_string(),
            ..Default::default()
        };
        let closed = active
            .forced_closure("2026-11-05T00:00:01.000Z")
            .expect("an abandoned round is closed");
        assert!(closed.terminal);
        assert_eq!(closed.user, None, "no coordinate to invent");
        assert_eq!(closed.status.as_deref(), Some(STATUS_INTERRUPTED));

        let dir = TempDir::new().unwrap();
        let jsonl = write_jsonl(&dir, &[]);
        let p = project(&jsonl, &closed)
            .unwrap()
            .expect("a recorded round is never a hole");
        assert!(p.user.is_none());
        assert!(p.terminal);
        assert_eq!(p.status, STATUS_INTERRUPTED);
    }

    #[test]
    fn pre_emption_closes_the_previous_round_as_interrupted() {
        // D2 counter-example 2: a queued message pre-empts and opens a new round
        // while the previous one is still tracked.
        let mut active = ActiveRound::default();
        let recs = active.note_entry(&entry("user", "a", Some("r:1")), 0);
        assert_eq!(recs.len(), 1);
        assert!(!recs[0].terminal);

        let recs = active.note_entry(&entry("user", "b", Some("r:2")), 5);
        assert_eq!(recs.len(), 2, "closure of r:1 plus the snapshot of r:2");
        assert_eq!(recs[0].round_id, "r:1");
        assert!(recs[0].terminal);
        assert_eq!(recs[0].status.as_deref(), Some(STATUS_INTERRUPTED));
        assert_eq!(recs[1].round_id, "r:2");
        assert_eq!(recs[1].user, Some(5));
        assert!(!recs[1].terminal);
    }

    #[test]
    fn compaction_and_unstamped_rows_do_not_move_the_boundary() {
        // D2 counter-example 1: compaction rewrites the representation of what came
        // in; injected rows carry no stamp and must not open or move a round.
        let mut active = ActiveRound::default();
        active.note_entry(&entry("user", "u", Some("r:1")), 0);
        active.note_entry(&entry("assistant", "partial", Some("r:1")), 1);
        assert!(active.note_entry(&entry("system", "injected", None), 2).is_empty());
        assert!(active
            .note_entry(
                &entry("user", "internal nudge", Some("r:1")),
                3
            )
            .is_empty());
        assert_eq!(active.round_id(), Some("r:1"));
        assert_eq!(active.user, Some(0), "the FIRST user row is the round's opener");
        assert_eq!(active.answer, Some(1), "the latest assistant entry wins");
    }

    #[test]
    fn replay_of_the_same_round_reuses_the_coordinate() {
        // D2 counter-example 3: DevMode resume/step replays the same turn. The stamp
        // is identical, so no second round is opened.
        let mut active = ActiveRound::default();
        let first = active.note_entry(&entry("user", "u", Some("r:1")), 0);
        let replayed = active.note_entry(&entry("user", "u", Some("r:1")), 9);
        assert!(!first[0].terminal);
        assert!(
            replayed.iter().all(|r| !r.terminal),
            "replaying a round must not close it: {replayed:?}"
        );
        assert_eq!(active.user, Some(0));
    }

    #[test]
    fn rebuild_from_transcript_labels_outcomes_as_unknown() {
        let dir = TempDir::new().unwrap();
        let jsonl = write_jsonl(
            &dir,
            &[
                entry("user", "u1", Some("r:1")),
                entry("assistant", "a1", Some("r:1")),
                entry("user", "u2", Some("r:2")),
                entry("tool_call", "tc", Some("r:2")),
            ],
        );
        let rounds = dir.path().join("s.jsonl.rounds");
        assert_eq!(
            repair_on_open(&rounds, &jsonl),
            0,
            "a rebuild yields historical rounds, not crash closures"
        );

        let r1 = read_record(&rounds, "r:1").unwrap().unwrap();
        assert_eq!((r1.user, r1.answer), (Some(0), Some(1)));
        assert!(r1.terminal, "a rebuilt round cannot still be in progress");
        assert_eq!(r1.status.as_deref(), Some(STATUS_UNKNOWN));
        let r2 = read_record(&rounds, "r:2").unwrap().unwrap();
        assert_eq!(r2.user, Some(2));
        assert_eq!(r2.status.as_deref(), Some(STATUS_INTERRUPTED), "no answer row");

        // A second open must not rebuild, and must not add records.
        assert_eq!(repair_on_open(&rounds, &jsonl), 0);
        assert_eq!(fold(&rounds).unwrap().len(), 2);
    }

    #[test]
    fn projection_reads_only_the_referenced_rows() {
        let dir = TempDir::new().unwrap();
        let mut attachment = entry("system", "Uploaded file: a.pdf", Some("r:1"));
        attachment.metadata = Some(serde_json::json!({
            META_KEY_ROUND_ID: "r:1",
            "type": "file_upload",
            "document_id": "d1",
        }));
        let jsonl = write_jsonl(
            &dir,
            &[
                entry("user", "hello", Some("r:1")),
                attachment,
                entry("tool_call", "secret detail", Some("r:1")),
                entry("tool_result", "secret output", Some("r:1")),
                entry("assistant", "the answer", Some("r:1")),
            ],
        );
        let mut r = rec("r:1", 0, true);
        r.answer = Some(4);
        r.status = Some("completed".into());
        r.card = Some(serde_json::json!({ "kind": "pause", "request_id": "q1" }));
        let p = project(&jsonl, &r).unwrap().expect("round is projectable");
        assert_eq!(p.user.as_ref().unwrap().body, "hello");
        assert_eq!(p.user.as_ref().unwrap().attachments.len(), 1, "attachment folded");
        assert_eq!(p.answer.as_ref().unwrap().body, "the answer");
        assert_eq!(p.status, "completed");
        assert!(p.terminal);
        // ADR-100 D3: execution detail never enters the projection.
        let rendered = serde_json::to_string(&p).unwrap();
        assert!(!rendered.contains("secret detail"));
        assert!(!rendered.contains("secret output"));
    }

    #[test]
    fn a_peer_round_projects_its_human_display_beside_the_envelope() {
        // Two audiences, two formats, one row: `body` stays the machine envelope
        // the model reads, `display` is what a Chat surface prints. A row without
        // the projection (a human turn, or any row written before it existed)
        // projects `display: None` so the surface keeps its old rendering.
        let dir = TempDir::new().unwrap();
        let mut agent_turn = entry("user", "[intent:inst-a:analyze] {\"title\":\"x\"}", Some("r:1"));
        agent_turn.metadata = Some(serde_json::json!({
            META_KEY_ROUND_ID: "r:1",
            "intent": {
                "origin": "request",
                "action": "analyze",
                "summary": "审计 usecase 分层",
                "body": "### 审计 usecase 分层

检查接口",
                "truncated": false,
            }
        }));
        let jsonl = write_jsonl(
            &dir,
            &[agent_turn, entry("assistant", "done", Some("r:1"))],
        );
        let mut r = rec("r:1", 0, true);
        r.answer = Some(1);
        r.status = Some("completed".into());
        let p = project(&jsonl, &r).unwrap().expect("round is projectable");
        let user = p.user.as_ref().unwrap();
        assert!(user.body.starts_with("[intent:inst-a:analyze]"), "the row is untouched");
        let display = user.display.as_ref().expect("display projected");
        assert_eq!(display.summary, "审计 usecase 分层");
        assert!(display.body.starts_with("### "));
        assert!(!display.truncated);
    }

    #[test]
    fn a_row_without_a_usable_display_projects_none() {
        let dir = TempDir::new().unwrap();
        let mut broken = entry("user", "[intent:…] {}", Some("r:1"));
        broken.metadata = Some(serde_json::json!({
            META_KEY_ROUND_ID: "r:1",
            // Not the shape the producer writes: an unusable projection must read
            // as absent, never as a failure of the whole round (D9's "a broken
            // side table is never an outage", applied to the row).
            "intent": 7,
        }));
        let jsonl = write_jsonl(&dir, &[broken]);
        let p = project(&jsonl, &rec("r:1", 0, false)).unwrap().expect("projectable");
        assert!(p.user.as_ref().unwrap().display.is_none());

        // A human turn carries no projection at all.
        let jsonl = write_jsonl(&dir, &[entry("user", "修复分页", Some("r:2"))]);
        let p = project(&jsonl, &rec("r:2", 0, false)).unwrap().expect("projectable");
        assert!(p.user.as_ref().unwrap().display.is_none());
    }

    #[test]
    fn an_answerless_round_does_not_inherit_the_next_rounds_attachments() {
        // The window after the user row is bounded by the round's own answer position,
        // so a round cut off before it answered has nothing to stop the window at its
        // own boundary. Without the row's own stamp the next round's upload would be
        // served as this block's attachment — and cached as immutable content (D5).
        let dir = TempDir::new().unwrap();
        let attached = |round: &str, name: &str| {
            let mut e = entry("system", name, Some(round));
            e.metadata = Some(serde_json::json!({
                META_KEY_ROUND_ID: round,
                "type": "file_upload",
            }));
            e
        };
        let jsonl = write_jsonl(
            &dir,
            &[
                entry("user", "one", Some("r:1")),
                attached("r:1", "mine.pdf"),
                entry("user", "two", Some("r:2")),
                attached("r:2", "theirs.pdf"),
            ],
        );
        let r = rec("r:1", 0, true);
        let p = project(&jsonl, &r).unwrap().expect("round is projectable");
        let user = p.user.unwrap();
        let names: Vec<&str> = user
            .attachments
            .iter()
            .map(|a| a["content"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(names, vec!["mine.pdf"], "only this round's own rows");
    }

    #[test]
    fn open_round_projects_without_a_status_and_is_not_terminal() {
        let dir = TempDir::new().unwrap();
        let jsonl = write_jsonl(&dir, &[entry("user", "hello", Some("r:1"))]);
        let r = rec("r:1", 0, false);
        let p = project(&jsonl, &r).unwrap().expect("round is projectable");
        assert!(!p.terminal);
        assert_eq!(p.status, STATUS_IN_PROGRESS);
        assert!(p.answer.is_none());
    }

    #[test]
    fn a_drifted_coordinate_is_a_hole_not_someone_elses_message() {
        // The side table names positions; positions can go stale (compaction, a
        // rewritten transcript). The stamp on the row is what says so, and the only
        // honest answer is a hole — D5 caches a terminal projection forever, so a row
        // borrowed from another round would be pinned into this block permanently.
        let dir = TempDir::new().unwrap();
        let jsonl = write_jsonl(
            &dir,
            &[
                entry("user", "mine", Some("r:1")),
                entry("assistant", "my answer", Some("r:1")),
                entry("user", "someone else's", Some("r:2")),
                entry("assistant", "their answer", Some("r:2")),
            ],
        );
        // The user coordinate points at another round's opener.
        let mut borrowed = rec("r:1", 2, true);
        borrowed.answer = Some(1);
        assert!(project(&jsonl, &borrowed).unwrap().is_none());
        // The user row is right, the answer coordinate belongs to another round.
        let mut mixed = rec("r:1", 0, true);
        mixed.answer = Some(3);
        assert!(project(&jsonl, &mixed).unwrap().is_none());
        // A coordinate past the end is the same fact, arrived at differently.
        let mut past_end = rec("r:1", 0, true);
        past_end.answer = Some(99);
        assert!(project(&jsonl, &past_end).unwrap().is_none());
        // The honest case still projects.
        let mut good = rec("r:1", 0, true);
        good.answer = Some(1);
        let p = project(&jsonl, &good).unwrap().expect("projectable");
        assert_eq!(p.user.as_ref().unwrap().body, "mine");
        assert_eq!(p.answer.as_ref().unwrap().body, "my answer");
    }

    #[test]
    fn an_absent_transcript_is_a_hole_not_a_failure() {
        // A pruned session (or one whose transcript never reached this host) must not
        // look like a server fault: `unreachable` promises a retry that can never work.
        let dir = TempDir::new().unwrap();
        let jsonl = dir.path().join("gone.jsonl");
        assert!(project(&jsonl, &rec("r:1", 0, true)).unwrap().is_none());
    }

    #[test]
    fn corrupt_side_table_degrades_instead_of_failing_the_read() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("s.jsonl.rounds");
        std::fs::write(
            &path,
            "not json\n{\"round_id\":\"r:1\",\"user\":0,\"terminal\":true,\"ts\":\"t\"}\n",
        )
        .unwrap();
        let map = fold(&path).unwrap();
        assert_eq!(map.len(), 1, "the malformed line is skipped, the good one survives");
    }

    #[test]
    fn missing_side_table_is_not_an_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("absent.jsonl.rounds");
        assert!(fold(&path).unwrap().is_empty());
        assert!(read_record(&path, "r:1").unwrap().is_none());
    }

    #[test]
    fn the_read_path_rebuilds_a_lost_side_table() {
        // The projection read never opens a session, so "first touch" cannot mean only
        // "open": a closed session whose derived file is gone would otherwise answer
        // `found: false` forever while the transcript still stamps every row.
        let dir = TempDir::new().unwrap();
        let jsonl = write_jsonl(
            &dir,
            &[
                entry("user", "u1", Some("r:1")),
                entry("assistant", "a1", Some("r:1")),
                entry("user", "u2", Some("r:2")),
            ],
        );
        let rounds = dir.path().join("s.jsonl.rounds");
        assert!(fold(&rounds).unwrap().is_empty(), "nothing on disk yet");

        let map = fold_or_rebuild(&rounds, &jsonl).unwrap();
        assert_eq!(map.len(), 2, "both rounds resolved from the transcript");
        assert_eq!(map["r:1"].answer, Some(1));
        assert!(map["r:2"].terminal, "a rebuilt round is historical");
        assert!(
            rounds.exists(),
            "the rebuild is persisted, so the next read is a cheap fold"
        );
        // A second read folds what it persisted: no second walk, no duplicated records.
        let again = fold_or_rebuild(&rounds, &jsonl).unwrap();
        assert_eq!(again.len(), 2);
        assert_eq!(fold(&rounds).unwrap().len(), 2);
    }

    #[test]
    fn the_read_path_does_not_rebuild_over_records_it_already_has() {
        let dir = TempDir::new().unwrap();
        let jsonl = write_jsonl(&dir, &[entry("user", "u1", Some("r:1"))]);
        let rounds = dir.path().join("s.jsonl.rounds");
        append_record(&rounds, &rec("r:mine", 7, true)).unwrap();

        let map = fold_or_rebuild(&rounds, &jsonl).unwrap();
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("r:mine"), "the live table wins");
        assert_eq!(fold(&rounds).unwrap().len(), 1, "nothing was appended");
    }
}
