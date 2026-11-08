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
use crate::chat_sessions::{
    KIND_AGENT_MESSAGE, KIND_EXECUTION_ROUND, KIND_USER_MESSAGE, normalize_kind,
};

/// Schema version of the `search` table. Bumped whenever the column set
/// changes so an older sidecar is dropped and rebuilt rather than queried
/// against a schema it does not have (the sidecar is disposable — a rebuild
/// is the sanctioned heal, never a data loss). v2 added the `exec_ref` and
/// `brief` columns that carry an `execution_round` reference's coordinates
/// and one-line name to the search surface.
const SCHEMA_VERSION: i64 = 2;

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
    /// For an `execution_round` hit: the projection address
    /// `instance|session|round` the client resolves through
    /// `GET /api/chat-blocks` to render the round's answer fragment. Empty on
    /// every other kind (ADR-106 D3: the reference carries coordinates, never
    /// content — so the search sidecar stores the coordinate, and the content
    /// is read from the execution session exactly as the transcript reads it).
    #[serde(default)]
    pub exec_ref: String,
    /// For an `execution_round` hit: the row's durable one-line task name
    /// (`assignment_brief`, ADR-106 D13). It is the row's own searchable text —
    /// the `brief` column is FTS-indexed, so a keyword search reaches a round
    /// through the very label the history row shows — and the fallback label
    /// when the projection is not (yet) readable. Empty on every other kind.
    #[serde(default)]
    pub brief: String,
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
///
/// An `execution_round` is deliberately given `CAT_TEXT` even though it
/// carries no body: it is the chat's durable record of one round of execution
/// work, and the history surface must show it so a run of turns does not
/// render as consecutive user messages with no assistant reply between them.
/// Its searchable text is its own `brief` (FTS-indexed, ADR-106 D13), and the
/// answer it shows comes from the projection (`exec_ref`) with `brief` as the
/// fallback — never a body copied into the chat log (ADR-106 D3's zero-write
/// rule).
fn row_category(m: &SessionMessage) -> i64 {
    let mut cat = 0i64;
    if (is_speech(&m.kind) && !m.body.trim().is_empty())
        || normalize_kind(&m.kind, false) == KIND_EXECUTION_ROUND
    {
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

/// The projection address an `execution_round` hit carries to the client, in
/// the exact `instance|session|round` form `GET /api/chat-blocks` and the
/// frontend `blockRefOf` use. Empty (and so unresolvable) when any coordinate
/// is missing — a reference without its triple addresses nothing, and the
/// surface degrades to `brief` rather than inventing a fourth.
fn exec_ref(m: &SessionMessage) -> String {
    if normalize_kind(&m.kind, false) != KIND_EXECUTION_ROUND {
        return String::new();
    }
    let instance = m.meta.agent_instance_id.as_str();
    let session = m.meta.agent_session_id.as_str();
    let round = m.meta.round_id.as_str();
    if instance.is_empty() || session.is_empty() || round.is_empty() {
        return String::new();
    }
    format!("{instance}|{session}|{round}")
}

/// The row's durable one-line task name, for an `execution_round` hit only.
/// It is this row's own text column (`brief` is FTS-indexed): a reference row
/// has no body, so this is what a keyword search reaches it by.
fn exec_brief(m: &SessionMessage) -> String {
    if normalize_kind(&m.kind, false) != KIND_EXECUTION_ROUND {
        return String::new();
    }
    m.meta.assignment_brief.clone().unwrap_or_default()
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
        "CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, val INTEGER);",
    )
    .map_err(|e| format!("{}: {e}", path.display()))?;
    // A sidecar written by an older schema (no `exec_ref`/`brief`) cannot be
    // queried against the new column set. The index is disposable and
    // rebuildable from the log, so drop it and let the reconcile pass rebuild:
    // `indexed_lines` is reset to a value no real line count equals, which is
    // exactly the signal `ensure` uses to rebuild.
    let stored = conn
        .query_row(
            "SELECT val FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let fresh = stored != Some(SCHEMA_VERSION);
    if fresh {
        conn.execute("DROP TABLE IF EXISTS search", [])
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS search USING fts5(
             body,
             seq         UNINDEXED,
             ts          UNINDEXED,
             from_id     UNINDEXED,
             kind        UNINDEXED,
             cat         UNINDEXED,
             attachments UNINDEXED,
             exec_ref    UNINDEXED,
             brief,
             tokenize = 'trigram'
           );",
    )
    .map_err(|e| format!("{}: {e}", path.display()))?;
    if fresh {
        conn.execute(
            "INSERT INTO meta(key, val) VALUES('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET val = excluded.val",
            params![SCHEMA_VERSION],
        )
        .map_err(|e| format!("{}: {e}", path.display()))?;
        // Force a rebuild: no watermark can equal -1.
        conn.execute(
            "INSERT INTO meta(key, val) VALUES('indexed_lines', -1)
             ON CONFLICT(key) DO UPDATE SET val = excluded.val",
            [],
        )
        .map_err(|e| format!("{}: {e}", path.display()))?;
    }
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
        "INSERT INTO search(rowid, body, seq, ts, from_id, kind, cat, attachments, exec_ref, brief)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            line_pos,
            m.body,
            m.seq as i64,
            m.ts,
            m.from,
            m.kind,
            row_category(m),
            attachments,
            exec_ref(m),
            exec_brief(m)
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
    // The session directory layout is `chat_sessions`' property, not ours.
    let path = crate::chat_sessions::messages_path(dir);
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
                    "INSERT INTO search(rowid, body, seq, ts, from_id, kind, cat, attachments, exec_ref, brief)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        pos,
                        m.body,
                        m.seq as i64,
                        m.ts,
                        m.from,
                        m.kind,
                        row_category(&m),
                        attachments,
                        exec_ref(&m),
                        exec_brief(&m)
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
/// (== `spans.len() - 1` after the commit). A returned `Err` must never fail
/// the message write (the caller only logs it — the lazy rebuild on next open
/// heals).
pub fn index_append(dir: &Path, line_pos: i64, m: &SessionMessage) -> Result<(), String> {
    let arc = open_handle(dir)?;
    let mut conn = lock(&arc);
    // The watermark before this row must have covered exactly `line_pos` rows.
    // Anything else — a skipped write, a crash window, a replayed row, a copied
    // dir, or a column-set bump (see `SCHEMA_VERSION`) — means the index cannot
    // be extended incrementally, and reconciling it needs the WHOLE log.
    //
    // Do not pay that here: this runs inside `append_message`, i.e. on the
    // message-send request path under the session lock, where ADR-096 forbids a
    // whole-session read. The index is disposable and `ensure` — the first
    // search, once per process per session — is already the sanctioned place to
    // rebuild it. Leaving the watermark stale is what makes that happen: every
    // later search compares it against the authoritative line count.
    if meta_indexed(&conn)? != line_pos {
        tracing::debug!(
            dir = %dir.display(),
            line_pos,
            "chat search index is stale — rebuild deferred to the next search"
        );
        return Ok(());
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
                // Matches every indexed column: `body` for speech, `brief` for
                // an `execution_round` (whose body is empty by contract), so a
                // round is found by the same label the history row shows.
                clauses.push("search MATCH ?".to_string());
                binds.push(rusqlite::types::Value::Text(format!(
                    "\"{}\"",
                    q.replace('"', "\"\"")
                )));
            } else {
                // Below the trigram floor: same two columns, scanned instead.
                clauses.push(
                    "(body LIKE ? ESCAPE '\\' OR brief LIKE ? ESCAPE '\\')".to_string(),
                );
                let pattern = rusqlite::types::Value::Text(format!("%{}%", escape_like(q)));
                binds.push(pattern.clone());
                binds.push(pattern);
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
        "SELECT rowid, seq, ts, from_id, kind, body, attachments, exec_ref, brief FROM search \
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
        exec_ref: r.get(7)?,
        brief: r.get(8)?,
    })
}
