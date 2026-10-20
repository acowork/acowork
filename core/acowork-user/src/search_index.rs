//! Per-session, disposable chat-history search index (`search.sqlite`).
//!
//! The WeChat-style "browse this conversation by 文本 / 图片 / 文件" feature
//! needs a queryable index over `messages.jsonl`. The project red line
//! ("no whole-log read on the request path", see [`crate::chat_sessions`])
//! forbids scanning that log to answer a search, so this module keeps a
//! small SQLite sidecar per session, maintained as messages are appended
//! and read *only* by the search endpoint.
//!
//! Design points that keep this safe to add to the chat storage:
//!
//! - **Disposable & non-load-bearing.** `search.sqlite` is a derived index.
//!   `messages.jsonl` stays the single source of truth; the paging spine
//!   (`lines.index`) is untouched; the read path never opens this file. A
//!   corrupt or missing `search.sqlite` degrades search alone — the worst
//!   case is a one-time rebuild, never a lost or unreadable message.
//! - **Self-healing.** Every append bumps a `meta.indexed_lines` watermark.
//!   On open, if the watermark disagrees with the authoritative physical
//!   line count (a failed best-effort write, a crash window, a replayed
//!   migration row, a copied/absent file), the whole index is rebuilt from
//!   the log. That rebuild is the *only* sanctioned whole-log read — the
//!   same first-touch/self-heal exemption `ensure_index` uses, never per
//!   request and never per write.
//! - **No new global lock.** The append-time index write runs inside the
//!   caller's existing per-session `session_lock` (ADR-092's single writer).
//!   The process-wide registry here guards only the map of open handles,
//!   never a hot write path, so different sessions stay parallel.
//!
//! `line_pos` is the same 0-based ascending physical-line coordinate
//! `lines.index` assigns (blank lines occupy bytes but no position), so a
//! hit maps straight onto the paging offset math the frontend uses to
//! locate the message.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};

use crate::chat::Attachment;
use crate::chat_sessions::SessionMessage;
use crate::chat_sessions::{KIND_AGENT_MESSAGE, KIND_USER_MESSAGE, normalize_kind};

/// Minimum pattern length the FTS5 `trigram` tokenizer can match. Kept
/// identical to `acowork-sqlite::TRIGRAM_MIN` so the two services agree on
/// when a query has to fall back to `LIKE`. Re-declared locally (rather
/// than depending on acowork-sqlite) so this crate does not pull the whole
/// memory/retrieval stack in for one constant.
const TRIGRAM_MIN: usize = 3;

/// Category bit flags stored per row (`cat` column). A row can carry
/// several — a text message with an image is `TEXT | IMAGE`.
const CAT_TEXT: i64 = 1;
const CAT_IMAGE: i64 = 2;
const CAT_FILE: i64 = 4;

/// The three browse categories the search surface exposes (WeChat-style).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Text,
    Image,
    File,
}

impl Category {
    /// Parse the `category` query param (`text|image|file`). Anything the
    /// client did not spell correctly defaults to `Text` (the primary tab).
    pub fn from_param(s: &str) -> Category {
        match s {
            "image" => Category::Image,
            "file" => Category::File,
            _ => Category::Text,
        }
    }

    fn bit(self) -> i64 {
        match self {
            Category::Text => CAT_TEXT,
            Category::Image => CAT_IMAGE,
            Category::File => CAT_FILE,
        }
    }
}

/// One search result row. `line_pos` is the locate coordinate the frontend
/// converts into a `read_messages` offset; `attachments` are the rows this
/// message carries (the client renders image thumbnails / file chips from
/// them via the existing `files/{id}` route).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub seq: u64,
    pub line_pos: i64,
    pub ts: i64,
    pub from: String,
    pub kind: String,
    pub body: String,
    pub attachments: Vec<Attachment>,
}

/// Open `search.sqlite` handles, one per session dir, created on first
/// use and dropped with the directory (mirrors `LINE_INDEXES`).
static SEARCH_DBS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<Connection>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn db_path(dir: &Path) -> PathBuf {
    dir.join("search.sqlite")
}

fn messages_path(dir: &Path) -> PathBuf {
    dir.join("messages.jsonl")
}

/// The speech kinds whose non-empty body counts as searchable text. Legacy
/// `intent_*` rows are folded in through [`normalize_kind`] the same way the
/// read path labels them (write new, read old); `system_notice` and
/// `interaction_request` bodies are indexed for the file/image tabs' context
/// but never flagged `CAT_TEXT`.
fn is_speech(kind: &str) -> bool {
    let normalized = normalize_kind(kind, false);
    normalized == KIND_USER_MESSAGE || normalized == KIND_AGENT_MESSAGE
}

/// Category bit mask for a row: text (speech with a body) and/or the
/// attachment buckets it carries.
fn row_category(m: &SessionMessage) -> i64 {
    let mut cat = 0i64;
    if is_speech(&m.kind) && !m.body.trim().is_empty() {
        cat |= CAT_TEXT;
    }
    for a in &m.attachments {
        if a.mime.starts_with("image/") {
            cat |= CAT_IMAGE;
        } else {
            cat |= CAT_FILE;
        }
    }
    cat
}

// ── Connection lifecycle ──────────────────────────────────────────────

/// Get (or lazily open) this session's `search.sqlite` handle, ensuring the
/// schema exists. Opens fresh on failure: a corrupt sidecar is deleted and
/// reopened so the caller's reconcile rebuilds it — the log is untouched,
/// so this never risks message data.
fn open_handle(dir: &Path) -> Result<Arc<Mutex<Connection>>, String> {
    {
        let reg = lock(&SEARCH_DBS);
        if let Some(a) = reg.get(dir) {
            return Ok(a.clone());
        }
    }
    let path = db_path(dir);
    let conn = match connect(&path) {
        Ok(c) => c,
        // Open/schema failed (corrupt file, interrupted create): drop the
        // sidecar and retry once from a clean slate.
        Err(first) => {
            tracing::warn!(path = %path.display(), error = %first, "search index unreadable, recreating");
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(format!("{}-journal", path.display()));
            connect(&path).map_err(|e| format!("{}: {e}", path.display()))?
        }
    };
    let arc = Arc::new(Mutex::new(conn));
    lock(&SEARCH_DBS).insert(dir.to_path_buf(), arc.clone());
    Ok(arc)
}

fn connect(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS search USING fts5(
             body,
             seq         UNINDEXED,
             ts          UNINDEXED,
             from_id     UNINDEXED,
             kind        UNINDEXED,
             cat         UNINDEXED,
             attachments UNINDEXED,
             tokenize = 'trigram'
           );
           CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, val INTEGER);",
    )
    .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(conn)
}

/// Drop a session's cached handle (called from `delete_session` alongside
/// the line-index removal, so a deleted id never leaves a leaked connection).
pub fn forget(dir: &Path) {
    let _ = lock(&SEARCH_DBS).remove(dir);
}

/// Reconcile the index with the authoritative physical line count and return
/// a handle ready for querying. If the watermark disagrees with
/// `log_line_count` (as it does on a fresh or lying index), rebuild the whole
/// sidecar from `messages.jsonl` — the one sanctioned full read.
pub fn ensure(dir: &Path, log_line_count: u64) -> Result<Arc<Mutex<Connection>>, String> {
    let arc = open_handle(dir)?;
    {
        let mut conn = lock(&arc);
        if meta_indexed(&conn)? != log_line_count as i64 {
            rebuild(&mut conn, dir, log_line_count)?;
        }
    }
    Ok(arc)
}

fn meta_indexed(conn: &Connection) -> Result<i64, String> {
    // A fresh sidecar has no watermark row yet: report it as "-1" (never
    // equal to a real line count) so the caller rebuilds rather than erroring.
    conn.query_row(
        "SELECT val FROM meta WHERE key = 'indexed_lines'",
        [],
        |r| r.get::<_, i64>(0),
    )
    .optional()
    .map_err(|e| format!("meta: {e}"))
    .map(|o| o.unwrap_or(-1))
}

fn set_meta(conn: &Connection, val: i64) -> Result<(), String> {
    conn.execute(
        "INSERT INTO meta(key, val) VALUES('indexed_lines', ?1)
         ON CONFLICT(key) DO UPDATE SET val = excluded.val",
        params![val],
    )
    .map_err(|e| format!("meta: {e}"))?;
    Ok(())
}

fn insert_row(conn: &Connection, line_pos: i64, m: &SessionMessage) -> Result<(), String> {
    let attachments =
        serde_json::to_string(&m.attachments).map_err(|e| format!("serialize attachments: {e}"))?;
    conn.execute(
        "INSERT INTO search(rowid, body, seq, ts, from_id, kind, cat, attachments)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            line_pos,
            m.body,
            m.seq as i64,
            m.ts,
            m.from,
            m.kind,
            row_category(m),
            attachments
        ],
    )
    .map_err(|e| format!("insert row {line_pos}: {e}"))?;
    Ok(())
}

/// Rebuild the sidecar from the whole log (called under the session lock and
/// the connection lock). A corrupt-but-non-blank line occupies a position
/// (matching `lines.index`) but yields no searchable row; blank lines occupy
/// neither.
fn rebuild(conn: &mut Connection, dir: &Path, log_line_count: u64) -> Result<(), String> {
    let path = messages_path(dir);
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        // A brand-new session legitimately has no log yet.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let tx = conn
        .transaction()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    tx.execute("DELETE FROM search", [])
        .map_err(|e| e.to_string())?;
    let mut pos: i64 = 0;
    for line in raw.split('\n') {
        if line.trim().is_empty() {
            continue; // blank: occupies bytes, not a message number
        }
        match serde_json::from_str::<SessionMessage>(line) {
            Ok(m) => {
                let attachments = serde_json::to_string(&m.attachments)
                    .map_err(|e| format!("serialize attachments: {e}"))?;
                tx.execute(
                    "INSERT INTO search(rowid, body, seq, ts, from_id, kind, cat, attachments)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        pos,
                        m.body,
                        m.seq as i64,
                        m.ts,
                        m.from,
                        m.kind,
                        row_category(&m),
                        attachments
                    ],
                )
                .map_err(|e| format!("rebuild row {pos}: {e}"))?;
            }
            // Corrupt line: keep its position, index nothing.
            Err(e) => {
                tracing::warn!(path = %path.display(), line = pos, error = %e, "search rebuild skipped a corrupt line");
            }
        }
        pos += 1;
    }
    // Trust the authoritative line count over the loop's own tally (they
    // agree unless the log changed mid-read, which the session lock prevents).
    set_meta(&tx, log_line_count as i64)?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

/// Best-effort incremental index of the newest row, called by the write entry
/// right after `commit_row`. `line_pos` must be the row's physical line index
/// (== `spans.len() - 1` after the commit). Any disagreement with the
/// watermark falls back to a full rebuild so the index can never silently
/// drift behind the log; a returned `Err` must never fail the message write
/// (the caller only logs it — the lazy rebuild on next open heals).
pub fn index_append(dir: &Path, line_pos: i64, m: &SessionMessage) -> Result<(), String> {
    let arc = open_handle(dir)?;
    let mut conn = lock(&arc);
    // The watermark before this row must have covered exactly `line_pos`
    // rows; anything else (skipped write, crash, replay, copied dir) rebuilds.
    if meta_indexed(&conn)? != line_pos {
        return rebuild(&mut conn, dir, (line_pos + 1) as u64);
    }
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    insert_row(&tx, line_pos, m)?;
    set_meta(&tx, line_pos + 1)?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

// ── Query ─────────────────────────────────────────────────────────────

/// Escape a `LIKE` operand (`%`, `_` and the `\` escape char), matching the
/// `ESCAPE '\'` clause used below.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '%' || c == '_' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Select matching rows, newest first, bounded by an optional `before_line`
/// cursor (rows strictly older than it). Never touches `messages.jsonl`.
pub fn query(
    conn: &Connection,
    category: Category,
    q: &str,
    before_line: Option<i64>,
    limit: usize,
) -> Result<Vec<SearchHit>, String> {
    let before = before_line.unwrap_or(i64::MAX);
    let bit = category.bit();
    let q = q.trim();
    let limit = limit.max(1) as i64;

    // Assemble WHERE clauses and bind values in one deterministic order so
    // each `?` maps to exactly one parameter, left to right. `cat` is inlined
    // (a server-controlled constant), not bound.
    let mut clauses: Vec<String> = Vec::new();
    let mut binds: Vec<rusqlite::types::Value> = Vec::new();

    match category {
        Category::Text if !q.is_empty() => {
            if q.chars().count() >= TRIGRAM_MIN {
                // Trigram phrase search == substring containment for >=3ch.
                clauses.push("search MATCH ?".to_string());
                binds.push(rusqlite::types::Value::Text(format!(
                    "\"{}\"",
                    q.replace('"', "\"\"")
                )));
            } else {
                clauses.push("body LIKE ? ESCAPE '\\'".to_string());
                binds.push(rusqlite::types::Value::Text(format!(
                    "%{}%",
                    escape_like(q)
                )));
            }
        }
        Category::File if !q.is_empty() => {
            // Filename substring over the stored attachment JSON.
            clauses.push("attachments LIKE ? ESCAPE '\\'".to_string());
            binds.push(rusqlite::types::Value::Text(format!(
                "%{}%",
                escape_like(q)
            )));
        }
        _ => {}
    }
    clauses.push(format!("(cat & {bit}) <> 0"));
    clauses.push("rowid < ?".to_string());
    binds.push(rusqlite::types::Value::Integer(before));

    let sql = format!(
        "SELECT rowid, seq, ts, from_id, kind, body, attachments FROM search \
         WHERE {} ORDER BY rowid DESC LIMIT ?",
        clauses.join(" AND ")
    );
    binds.push(rusqlite::types::Value::Integer(limit));

    let refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
    let mut stmt = conn.prepare(&sql).map_err(|e| format!("prepare: {e}"))?;
    let rows = stmt
        .query_map(refs.as_slice(), map_hit)
        .map_err(|e| format!("query: {e}"))?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| format!("row: {e}"))?);
    }
    Ok(out)
}

fn map_hit(r: &rusqlite::Row<'_>) -> rusqlite::Result<SearchHit> {
    let attachments: String = r.get(6)?;
    let attachments = serde_json::from_str(&attachments).unwrap_or_default();
    Ok(SearchHit {
        line_pos: r.get(0)?,
        seq: r.get::<_, i64>(1)? as u64,
        ts: r.get(2)?,
        from: r.get(3)?,
        kind: r.get(4)?,
        body: r.get(5)?,
        attachments,
    })
}
