//! Per-chat, disposable round-projection cache (`rounds.sqlite`) — ADR-106 D5.
//!
//! A Chat block is a *reference* to one round of an execution session, and the
//! block's content lives on whichever node hosts that session. Rendering a
//! transcript would then need a cross-host round trip per block, and history
//! would stop being readable the moment its Runtime stopped. So the Gateway
//! pulls each round's projection once and hands it here.
//!
//! This file is the reason that is safe rather than a second source of truth:
//!
//! - **Only terminal rounds are stored.** A terminal round's transcript rows are
//!   immutable, so a cached copy can never be stale — which is why there is no
//!   TTL, no version column and no invalidation path (ADR-106 D5). An open
//!   round is refused at write time, not expired later: the caller's bug cannot
//!   turn this cache into a volatile fact.
//! - **Disposable and non-load-bearing.** `messages.jsonl` stays the chat's only
//!   truth; nothing else in this crate reads this file. A missing, corrupt or
//!   lying sidecar costs exactly one re-fetch and never a lost message, so
//!   `delete_session` takes the directory with it and a failed write is a `warn`.
//! - **Bounded.** Row and payload ceilings cap what one chat can accumulate;
//!   eviction is by `fetched_at`, which is the closest thing to LRU a cache with
//!   immutable values can express.
//!
//! Unlike `search_index`, there is no watermark to reconcile: this cache's source
//! of truth is *not* on this machine, so agreement with a local log is neither
//! checkable nor meaningful. A miss is simply a miss.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};

/// Longest accepted id segment (instance, session or round). Every one of them
/// is far shorter in practice; the ceiling exists so a caller cannot make a key
/// out of an arbitrary blob.
const MAX_ID_BYTES: usize = 128;

/// Rows kept per chat. A block per turn, a few hundred turns per chat, plus
/// slack: beyond this the oldest rows go and a re-fetch covers them.
const MAX_ROWS: i64 = 4000;

/// Largest projection payload accepted for storage. A projection is the user
/// row and the answer row of one round, so anything this large is not a block.
const MAX_PROJECTION_BYTES: usize = 256 * 1024;

/// Most refs one request may name. The chat renderer asks for the blocks on
/// screen; a caller needing more is paging and should send several requests.
pub const MAX_REFS: usize = 200;

/// Which round a cached projection belongs to: the triple a Chat block
/// references, and the cache key (ADR-106 D5).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RoundKey {
    pub agent_instance_id: String,
    pub agent_session_id: String,
    pub round_id: String,
}

impl RoundKey {
    /// Wire form used in query strings: `instance|session|round`, several joined
    /// by `,`. The field separator is `|` because a round id legitimately
    /// contains `:` (ADR-106 D2 mints `r:{message_id}`) and an instance id may
    /// contain `.` or `-`.
    pub fn encode(&self) -> String {
        format!(
            "{}|{}|{}",
            self.agent_instance_id, self.agent_session_id, self.round_id
        )
    }

    /// Parse one encoded ref. Shape-checked rather than trusted: the parts end
    /// up in a SQL bind and in a cache key, never in a path.
    pub fn parse(s: &str) -> Result<RoundKey, String> {
        let mut parts = s.split('|');
        let (Some(instance), Some(session), Some(round), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return Err(format!("malformed ref {s:?}: expected instance|session|round"));
        };
        for (name, part) in [
            ("agent_instance_id", instance),
            ("agent_session_id", session),
            ("round_id", round),
        ] {
            if !valid_id(part) {
                return Err(format!("malformed ref {s:?}: bad {name}"));
            }
        }
        Ok(RoundKey {
            agent_instance_id: instance.to_string(),
            agent_session_id: session.to_string(),
            round_id: round.to_string(),
        })
    }

    /// Parse a `,`-separated list, enforcing [`MAX_REFS`] as a whole.
    pub fn parse_list(s: &str) -> Result<Vec<RoundKey>, String> {
        let mut out = Vec::new();
        for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            if out.len() >= MAX_REFS {
                return Err(format!("too many refs (max {MAX_REFS})"));
            }
            out.push(RoundKey::parse(part)?);
        }
        Ok(out)
    }
}

impl fmt::Display for RoundKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode())
    }
}

/// One cached block projection: the ADR-106 D4 payload as fetched, plus what
/// the Gateway needs to decide whether to trust it (terminal) and when it
/// stopped being current (fetched_at, kept for eviction only — never as a TTL).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedRound {
    #[serde(flatten)]
    pub key: RoundKey,
    pub projection: serde_json::Value,
    pub terminal: bool,
    pub fetched_at: i64,
}

/// Open `rounds.sqlite` handles, one per chat dir — the same registry shape
/// `search_index` uses, so the delete path has exactly one thing to forget.
static ROUND_DBS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<Connection>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn db_path(dir: &Path) -> PathBuf {
    dir.join("rounds.sqlite")
}

/// Id segments allow-list: the characters the three id spaces actually use
/// (UUID, Runtime session id, `r:{message_id}`). Anything else is refused
/// before it reaches SQL or a key. `pub(crate)` so the HTTP boundary can
/// answer a malformed ref with a 422 rather than a stored-then-lost row.
///
/// A leading `.` is refused too: none of the three id spaces ever starts with
/// one, and `..` is precisely the value that would stop being harmless the day
/// this key names something on disk.
pub(crate) fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && !s.contains("..")
        && s.len() <= MAX_ID_BYTES
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':' | b'.'))
}

// ── Connection lifecycle ──────────────────────────────────────────────

/// Get (or lazily open) this chat's handle, ensuring the schema exists. A
/// corrupt sidecar is deleted and reopened once from a clean slate: the worst
/// case that can happen to this cache is that it is empty.
fn open_handle(dir: &Path) -> Result<Arc<Mutex<Connection>>, String> {
    {
        let reg = lock(&ROUND_DBS);
        if let Some(a) = reg.get(dir) {
            return Ok(a.clone());
        }
    }
    let path = db_path(dir);
    let conn = match connect(&path) {
        Ok(c) => c,
        Err(first) => {
            tracing::warn!(path = %path.display(), error = %first, "round cache unreadable, recreating");
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(format!("{}-journal", path.display()));
            connect(&path).map_err(|e| format!("{}: {e}", path.display()))?
        }
    };
    let arc = Arc::new(Mutex::new(conn));
    lock(&ROUND_DBS).insert(dir.to_path_buf(), arc.clone());
    Ok(arc)
}

fn connect(path: &Path) -> Result<Connection, String> {
    // The chat directory always exists by the time a block is rendered (the
    // session record is on disk), but a fresh checkout of an older data dir
    // may not have it — create rather than fail the read.
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let conn = Connection::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS rounds(
             agent_instance_id TEXT NOT NULL,
             agent_session_id  TEXT NOT NULL,
             round_id          TEXT NOT NULL,
             projection        TEXT NOT NULL,
             terminal          INTEGER NOT NULL,
             fetched_at        INTEGER NOT NULL,
             PRIMARY KEY (agent_instance_id, agent_session_id, round_id)
           );",
    )
    .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(conn)
}

/// Release this chat's cached handle. MUST be called before `remove_dir_all`
/// of the session directory: on Windows a SQLite file is opened without
/// `FILE_SHARE_DELETE`, so a live handle turns "a disposable side table" into
/// "a chat that cannot be deleted" (ADR-106 D5's ordering contract).
pub fn forget(dir: &Path) {
    let _ = lock(&ROUND_DBS).remove(dir);
}

// ── Read ──────────────────────────────────────────────────────────────

/// Batch lookup for the refs a render names. A ref with no row is simply
/// absent from the map — the caller treats "absent" as "go ask the Runtime",
/// which is the whole point of the cache being disposable.
///
/// An unreadable cache degrades to `Err`, and the caller turns that into "all
/// refs missed": a broken side table must never become a broken chat read.
pub fn read_many(dir: &Path, keys: &[RoundKey]) -> Result<HashMap<RoundKey, CachedRound>, String> {
    let arc = open_handle(dir)?;
    let conn = lock(&arc);
    let mut out = HashMap::new();
    for key in keys {
        let row = conn
            .query_row(
                "SELECT projection, terminal, fetched_at FROM rounds
                 WHERE agent_instance_id = ?1 AND agent_session_id = ?2 AND round_id = ?3",
                params![key.agent_instance_id, key.agent_session_id, key.round_id],
                |r| {
                    let projection: String = r.get(0)?;
                    Ok((projection, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
                },
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let Some((projection, terminal, fetched_at)) = row else {
            continue;
        };
        // A row that no longer parses is a corrupt cache, not a corrupt chat:
        // drop it and let the caller re-fetch.
        match serde_json::from_str::<serde_json::Value>(&projection) {
            Ok(projection) => {
                out.insert(
                    key.clone(),
                    CachedRound {
                        key: key.clone(),
                        projection,
                        terminal: terminal != 0,
                        fetched_at,
                    },
                );
            }
            Err(e) => {
                tracing::warn!(ref_ = %key, error = %e, "cached round payload unreadable, treating as miss");
            }
        }
    }
    Ok(out)
}

// ── Write ─────────────────────────────────────────────────────────────

/// Store freshly fetched projections. Returns how many were written.
///
/// Two refusals are enforced here rather than left to the caller, because each
/// is a durability guarantee of the design: a non-terminal round is not a fact
/// yet (ADR-106 D5 "what is never cached"), and an oversized payload is not a
/// block. Both are skipped with a `warn` — this function never fails a fetch.
pub fn write_many(dir: &Path, rows: &[CachedRound]) -> Result<usize, String> {
    let mut pending: Vec<(&CachedRound, String)> = Vec::with_capacity(rows.len());
    for row in rows {
        if !row.terminal {
            tracing::warn!(
                ref_ = %row.key,
                "refusing to cache an open round (ADR-106 D5: terminal-only)"
            );
            continue;
        }
        if !valid_id(&row.key.agent_instance_id)
            || !valid_id(&row.key.agent_session_id)
            || !valid_id(&row.key.round_id)
        {
            return Err(format!("malformed cache key {:?}", row.key));
        }
        // Serialised once here: the same string is the size check and the bind.
        let encoded = row.projection.to_string();
        if encoded.len() > MAX_PROJECTION_BYTES {
            tracing::warn!(
                ref_ = %row.key,
                bytes = encoded.len(),
                "cached round exceeds the payload ceiling, not stored"
            );
            continue;
        }
        pending.push((row, encoded));
    }
    if pending.is_empty() {
        return Ok(0);
    }
    let arc = open_handle(dir)?;
    let mut conn = lock(&arc);
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let mut written = 0usize;
    for (row, encoded) in &pending {
        // Last-writer-wins on the same key: re-writing a terminal round is a
        // no-op in content (it cannot change), so this only refreshes
        // `fetched_at` for eviction.
        tx.execute(
            "INSERT INTO rounds(agent_instance_id, agent_session_id, round_id, projection, terminal, fetched_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5)
             ON CONFLICT(agent_instance_id, agent_session_id, round_id)
             DO UPDATE SET projection = excluded.projection,
                           terminal   = excluded.terminal,
                           fetched_at = excluded.fetched_at",
            params![
                row.key.agent_instance_id,
                row.key.agent_session_id,
                row.key.round_id,
                encoded,
                row.fetched_at
            ],
        )
        .map_err(|e| format!("insert {}: {e}", row.key))?;
        written += 1;
    }
    prune(&tx)?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(written)
}

/// Evict oldest-first past [`MAX_ROWS`]. Within one transaction with the
/// inserts so a write is never visible alongside an over-cap file.
fn prune(tx: &rusqlite::Transaction<'_>) -> Result<(), String> {
    let count: i64 = tx
        .query_row("SELECT count(*) FROM rounds", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if count <= MAX_ROWS {
        return Ok(());
    }
    tx.execute(
        "DELETE FROM rounds WHERE rowid NOT IN (
             SELECT rowid FROM rounds ORDER BY fetched_at DESC LIMIT ?1
         )",
        params![MAX_ROWS],
    )
    .map_err(|e| format!("prune: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A chat directory under the shared temp-root naming the crate already
    /// uses for storage tests.
    fn chat_dir(name: &str) -> PathBuf {
        crate::test_support::temp_dir(name).join("chat-sessions").join("chat-1")
    }

    fn key(round: &str) -> RoundKey {
        RoundKey {
            agent_instance_id: "7a7a7a7a-1111-4bbb-8ccc-000000000001".to_string(),
            agent_session_id: "20261102_100000_abcd".to_string(),
            round_id: format!("r:{round}"),
        }
    }

    fn row(round: &str, terminal: bool, fetched_at: i64) -> CachedRound {
        CachedRound {
            key: key(round),
            projection: json!({ "round_id": format!("r:{round}"), "answer": { "body": "hi" } }),
            terminal,
            fetched_at,
        }
    }

    fn cleanup(dir: &Path) {
        forget(dir);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn ref_wire_form_round_trips_the_colon_in_a_round_id() {
        let k = key("m-1");
        let encoded = k.encode();
        assert!(encoded.contains('|'), "field separator: {encoded}");
        assert!(encoded.contains("r:m-1"), "the colon survives unescaped");
        assert_eq!(RoundKey::parse(&encoded).unwrap(), k);

        let list = RoundKey::parse_list(&format!("{encoded},{encoded}")).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(RoundKey::parse_list("  ").unwrap(), vec![]);
    }

    #[test]
    fn ref_parsing_refuses_anything_that_is_not_an_id() {
        // The parts reach a SQL bind and a cache key, so the shape check is
        // the boundary — a `|` smuggling a fourth field, a path separator, or
        // a blank is refused rather than stored.
        for bad in [
            "a|b",
            "a|b|c|d",
            "a|b|",
            "|b|c",
            "a../x|b|c",
            "a b|b|c",
            "..|b|c",
            ".hidden|b|c",
            &format!("{}|b|c", "x".repeat(MAX_ID_BYTES + 1)),
        ] {
            assert!(RoundKey::parse(bad).is_err(), "must reject {bad:?}");
        }
    }

    #[test]
    fn a_miss_is_absent_not_an_error() {
        let dir = chat_dir("rounds-cache-miss");
        let hits = read_many(&dir, &[key("nope")]).unwrap();
        assert!(hits.is_empty(), "empty cache reads as empty");
        cleanup(&dir);
    }

    #[test]
    fn write_then_read_returns_the_projection_whole() {
        let dir = chat_dir("rounds-cache-hit");
        let written = write_many(&dir, &[row("m-1", true, 1000)]).unwrap();
        assert_eq!(written, 1);

        let hits = read_many(&dir, &[key("m-1"), key("m-2")]).unwrap();
        assert_eq!(hits.len(), 1, "only the stored ref is a hit");
        let hit = hits.get(&key("m-1")).unwrap();
        assert!(hit.terminal);
        assert_eq!(hit.fetched_at, 1000);
        assert_eq!(hit.projection["answer"]["body"], "hi");
        cleanup(&dir);
    }

    /// The design's one hard rule: an open round is not a fact yet. Refusing
    /// it here means a caller bug cannot silently put volatile state in a
    /// cache that has no invalidation path.
    #[test]
    fn an_open_round_is_never_stored() {
        let dir = chat_dir("rounds-cache-open");
        assert_eq!(write_many(&dir, &[row("m-1", false, 1000)]).unwrap(), 0);
        assert!(read_many(&dir, &[key("m-1")]).unwrap().is_empty());
        cleanup(&dir);
    }

    /// Deleting the sidecar must change nothing about the chat and cost only
    /// a re-fetch — the property that makes "disposable" true rather than a
    /// claim in a comment.
    #[test]
    fn a_deleted_cache_only_costs_a_refetch() {
        let dir = chat_dir("rounds-cache-disposable");
        write_many(&dir, &[row("m-1", true, 1000)]).unwrap();
        assert_eq!(read_many(&dir, &[key("m-1")]).unwrap().len(), 1);

        forget(&dir);
        fs::remove_file(db_path(&dir)).unwrap();
        assert!(read_many(&dir, &[key("m-1")]).unwrap().is_empty(), "gone, and readable as gone");

        // Re-populating works with no further setup: the handle reopened and
        // the schema came back.
        write_many(&dir, &[row("m-1", true, 2000)]).unwrap();
        let hit = read_many(&dir, &[key("m-1")]).unwrap();
        assert_eq!(hit.get(&key("m-1")).unwrap().fetched_at, 2000);
        cleanup(&dir);
    }

    #[test]
    fn a_corrupt_sidecar_is_replaced_not_inherited() {
        let dir = chat_dir("rounds-cache-corrupt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(db_path(&dir), b"this is not a database").unwrap();
        // The handle registry must not be holding the poisoned connection.
        forget(&dir);

        assert!(read_many(&dir, &[key("m-1")]).is_ok(), "a corrupt cache reads as empty");
        write_many(&dir, &[row("m-1", true, 1000)]).unwrap();
        assert_eq!(read_many(&dir, &[key("m-1")]).unwrap().len(), 1, "and heals");
        cleanup(&dir);
    }

    #[test]
    fn an_oversized_payload_is_skipped() {
        let dir = chat_dir("rounds-cache-oversize");
        let mut fat = row("m-1", true, 1000);
        fat.projection = json!({ "answer": { "body": "x".repeat(MAX_PROJECTION_BYTES + 1) } });
        assert_eq!(write_many(&dir, &[fat, row("m-2", true, 1000)]).unwrap(), 1, "the sound row still lands");
        assert!(read_many(&dir, &[key("m-1")]).unwrap().is_empty());
        assert_eq!(read_many(&dir, &[key("m-2")]).unwrap().len(), 1);
        cleanup(&dir);
    }

    /// Eviction drops the oldest `fetched_at` rows once the ceiling is passed.
    /// One batch write: the prune runs inside that transaction, so an
    /// over-capacity file is never observable.
    #[test]
    fn the_cache_is_bounded() {
        let dir = chat_dir("rounds-cache-prune");
        let rows: Vec<CachedRound> = (0..=MAX_ROWS)
            .map(|i| row(&format!("m-{i}"), true, i))
            .collect();
        assert_eq!(write_many(&dir, &rows).unwrap(), rows.len());

        let hits = read_many(&dir, &[key("m-0"), key("m-1"), key(&format!("m-{MAX_ROWS}"))]).unwrap();
        assert_eq!(hits.len(), 2, "the single oldest row went: {hits:?}");
        assert!(
            !hits.contains_key(&key("m-0")),
            "eviction is oldest-first by fetched_at"
        );
        assert!(hits.contains_key(&key("m-1")));
        assert!(hits.contains_key(&key(&format!("m-{MAX_ROWS}"))));
        cleanup(&dir);
    }

    #[test]
    fn rewriting_a_round_refreshes_it_instead_of_duplicating() {
        let dir = chat_dir("rounds-cache-upsert");
        write_many(&dir, &[row("m-1", true, 1000)]).unwrap();
        let mut newer = row("m-1", true, 2000);
        newer.projection = json!({ "round_id": "r:m-1", "answer": { "body": "edited" } });
        write_many(&dir, &[newer]).unwrap();
        let hit = read_many(&dir, &[key("m-1")]).unwrap();
        assert_eq!(hit.len(), 1);
        let got = hit.get(&key("m-1")).unwrap();
        assert_eq!(got.fetched_at, 2000);
        assert_eq!(got.projection["answer"]["body"], "edited");
        cleanup(&dir);
    }
}
