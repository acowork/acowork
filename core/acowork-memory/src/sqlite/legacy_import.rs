//! One-shot import of a legacy agent-private store (ADR-101 decision 10).
//!
//! Before ADR-101 an instance's memories lived in its own workspace,
//! `{work_dir}/memory/private.sqlite`, opened by the Runtime. The memory
//! service keeps them at `{data_dir}/instances/{instance_id}/memory.db`, so the
//! rows have to move exactly once. The Node owns the node-local filesystem
//! (ADR-055) and uploads the old file's bytes; this module turns those bytes
//! into rows in one instance namespace.
//!
//! # What "import" means here
//!
//! **Memory rows only.** Since ADR-082 the same file also carries the session
//! meta (`sessions`) and the conversation index (`ConversationMessage` nodes)
//! that the Runtime *still reads today* — those are not memories, and copying
//! them into a service namespace would make the service a second source of
//! session truth. The filter is exactly [`labels::ALL`], in both directions:
//! what is copied in, and what [`SqliteStore::purge_memory_rows`] deletes from
//! the legacy file afterwards.
//!
//! **Verbatim, ids included.** `props` cross-references other nodes by
//! id (`source_episode_id`, `source_episode_ids`), so an imported node keeps
//! its primary key or the graph edges silently point at the wrong memory.
//! Embedding blobs and their recorded `dim` are copied as bytes, never
//! recomputed: a store that was embedded at a width the service does not use
//! then shows up in `AdminStats::vectors_of_other_dim`, which is precisely the
//! signal the panel's "Rebuild Index" banner reads. Re-embedding is the user's
//! choice, made with a model in front of them, not a side effect of a migration.
//!
//! **All or nothing.** One transaction: a partially imported namespace is worse
//! than an un-migrated one, because the panel would show half a life history
//! and the second attempt would be refused as "already has memories".

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};

use super::{Error, Result, SqliteStore, schema};
use crate::labels;

/// First 16 bytes of every SQLite database file.
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Whether `bytes` is (the head of) a SQLite database.
///
/// A cheap pre-flight for both halves of the transfer: the Node refuses to
/// upload anything else, and the service refuses to open it. A pre-ADR-082
/// grafeo store does not match and must be exported by hand — inventing a
/// second reader for a format that is already gone was rejected (ADR-101
/// "no compatibility code").
pub fn has_sqlite_magic(bytes: &[u8]) -> bool {
    bytes.starts_with(SQLITE_MAGIC)
}

/// What one legacy import moved, per table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportCounts {
    /// `nodes` rows carrying a memory label.
    pub nodes: usize,
    /// `vectors` rows of those nodes (any width).
    pub vectors: usize,
    /// Rows copied into the per-label FTS5 tables.
    pub lexical_rows: usize,
    /// `purge_log` rows (the forgetting archive of memory nodes).
    pub purge_log_rows: usize,
}

impl ImportCounts {
    /// True when the legacy file held no memory data at all.
    pub fn is_empty(&self) -> bool {
        self.nodes == 0 && self.purge_log_rows == 0
    }
}

/// Unique-ish suffix for the spool file, so two concurrent imports of two
/// instances never share a path (the service is multi-tenant).
static SPOOL_SEQ: AtomicU64 = AtomicU64::new(0);

/// The uploaded bytes, written to a throwaway file so SQLite can open them.
///
/// SQLite cannot read a database out of memory without a VFS, and a WAL-mode
/// database cannot be opened read-only unless its `-shm` already exists — so
/// the upload lands in a temp file that is opened read-write and deleted when
/// the import ends, on every path out (including `?`).
struct Spool(PathBuf);

impl Spool {
    fn write(bytes: &[u8]) -> Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "acowork-memory-import-{}-{nanos}-{}.sqlite",
            std::process::id(),
            SPOOL_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, bytes)?;
        Ok(Self(path))
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        // Best-effort: a failed delete leaves a stale temp file, never a
        // wrong answer. Opening it made SQLite create sidecars.
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", self.0.display(), suffix));
        }
    }
}

impl SqliteStore {
    /// Number of memory rows in this store (`labels::ALL`, any status).
    ///
    /// The migration's idempotency gate: a namespace that already holds
    /// memories is not a migration target — merging two memory histories is a
    /// different, much harder problem and is explicitly out of scope.
    pub fn memory_node_count(&self) -> Result<u64> {
        let conn = self.lock();
        let placeholders: Vec<String> =
            (1..=labels::ALL.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT COUNT(*) FROM nodes WHERE label IN ({})",
            placeholders.join(", ")
        );
        let mut stmt = conn.prepare(&sql)?;
        let args: Vec<&dyn rusqlite::types::ToSql> =
            labels::ALL.iter().map(|l| l as &dyn rusqlite::types::ToSql).collect();
        let n: i64 = stmt.query_row(args.as_slice(), |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Copy the memory rows of a legacy workspace store into this namespace.
    ///
    /// `bytes` is the whole uploaded `private.sqlite`. Returns what moved;
    /// `(ImportCounts::default(), nothing written)` is a legitimate answer for
    /// a store that had no memories, and the caller still gets to record that
    /// (the Node writes its sentinel on it, so the scan does not repeat).
    ///
    /// **Precondition**: this store holds no nodes at all. Ids are preserved on
    /// the way in (see the module docs), so an existing row with the same id
    /// would abort the transaction — the caller checks
    /// [`Self::memory_node_count`] first, and a non-memory row in a service
    /// namespace is a state the service itself never creates.
    pub fn import_legacy_store(&self, bytes: &[u8]) -> Result<ImportCounts> {
        if !has_sqlite_magic(bytes) {
            return Err(Error::Memory(
                "upload is not a SQLite database file".to_string(),
            ));
        }
        if self.node_count()? > 0 {
            return Err(Error::Memory(
                "target store is not empty; a legacy import preserves node ids".to_string(),
            ));
        }
        let spool = Spool::write(bytes)?;
        let legacy = Connection::open(&spool.0)?;
        let counts = {
            let mut conn = self.lock();
            let tx = conn.transaction()?;
            let counts = copy_memory_rows(&tx, &legacy)?;
            tx.commit()?;
            counts
        };
        // `close` order matters only for the temp file: the connection must be
        // dropped before the file goes away (the `Spool` guard does that).
        drop(legacy);
        drop(spool);
        if !counts.is_empty() {
            tracing::info!(
                nodes = counts.nodes,
                vectors = counts.vectors,
                lexical_rows = counts.lexical_rows,
                purge_log_rows = counts.purge_log_rows,
                "imported memory rows from a legacy workspace store"
            );
        }
        Ok(counts)
    }

    /// Drop every **memory** row from this store — the mirror image of
    /// [`SqliteStore::purge_conversation_state`] and the second half of the
    /// migration.
    ///
    /// Called by the Node on its *local* legacy file after the service has
    /// confirmed the import: the memory rows have moved, so they must stop
    /// existing here or the next upgrade-time scan would upload them again and
    /// the (now service-side) history would have a stale twin on disk.
    ///
    /// Deliberately leaves the file itself, its `meta` row and everything the
    /// Runtime still owns — `sessions`, `fts_sessions`, `conversation_state` and
    /// the `ConversationMessage` index — untouched. Vectors of deleted nodes
    /// cascade through the `vectors.node_id` foreign key; the per-label FTS
    /// tables have no FK, so each is cleared explicitly (same reasoning as
    /// [`SqliteStore::purge_conversation_state`]). `purge_log` is the archive of
    /// deleted memories and travels with them.
    ///
    /// Returns the number of `nodes` rows removed.
    pub fn purge_memory_rows(&self) -> Result<usize> {
        let conn = self.lock();
        let placeholders: Vec<String> =
            (1..=labels::ALL.len()).map(|i| format!("?{i}")).collect();
        let in_list = placeholders.join(", ");
        let args: Vec<&dyn rusqlite::types::ToSql> =
            labels::ALL.iter().map(|l| l as &dyn rusqlite::types::ToSql).collect();
        let nodes = conn.execute(
            &format!("DELETE FROM nodes WHERE label IN ({in_list})"),
            args.as_slice(),
        )?;
        // FTS5 shadow tables are keyed by rowid == node id, so a `WHERE label`
        // delete on `nodes` cannot reach them: clear the whole table per memory
        // label. `fts_conversation` is NOT in that set — it is the Runtime's.
        let mut lexical = 0usize;
        for label in labels::ALL {
            if let Some(fts) = schema::fts_table(label) {
                lexical += conn.execute(&format!("DELETE FROM {fts}"), [])?;
            }
        }
        let archive = conn.execute(
            &format!("DELETE FROM purge_log WHERE label IN ({in_list})"),
            args.as_slice(),
        )?;
        tracing::info!(
            nodes,
            lexical_rows = lexical,
            purge_log_rows = archive,
            "removed migrated memory rows from the legacy workspace store"
        );
        Ok(nodes)
    }
}

/// Copy the memory-scoped rows of `legacy` into `target`.
///
/// Both connections are live at once on purpose: reads stream from the legacy
/// spool, writes go to the target's open transaction. `target` is a
/// `&Connection` rather than a `&Transaction` because the statements are the
/// same either way and the commit stays with the caller.
fn copy_memory_rows(target: &Connection, legacy: &Connection) -> Result<ImportCounts> {
    let mut counts = ImportCounts::default();
    if !table_exists(legacy, "nodes")? {
        // Not a workspace store at all (or a file created by a Runtime that
        // never opened the memory backend): nothing to move, not an error —
        // the Node records the sentinel and stops asking.
        return Ok(counts);
    }
    let placeholders: Vec<String> =
        (1..=labels::ALL.len()).map(|i| format!("?{i}")).collect();
    let in_list = placeholders.join(", ");
    let args: Vec<&dyn rusqlite::types::ToSql> =
        labels::ALL.iter().map(|l| l as &dyn rusqlite::types::ToSql).collect();

    // 1. nodes — explicit id, so `props` cross-references survive.
    {
        let mut stmt = legacy.prepare(&format!(
            "SELECT id, label, status, props, created_at, updated_at \
             FROM nodes WHERE label IN ({in_list}) ORDER BY id"
        ))?;
        let mut rows = stmt.query(args.as_slice())?;
        while let Some(row) = rows.next()? {
            target.execute(
                "INSERT INTO nodes(id, label, status, props, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ],
            )?;
            counts.nodes += 1;
        }
    }

    // 2. vectors — bytes verbatim, `dim` verbatim (see the module docs).
    if table_exists(legacy, "vectors")? {
        let mut stmt = legacy.prepare(&format!(
            "SELECT v.node_id, v.dim, v.embedding FROM vectors v \
             JOIN nodes n ON n.id = v.node_id WHERE n.label IN ({in_list})"
        ))?;
        let mut rows = stmt.query(args.as_slice())?;
        while let Some(row) = rows.next()? {
            target.execute(
                "INSERT INTO vectors(node_id, dim, embedding) VALUES (?1, ?2, ?3)",
                params![
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ],
            )?;
            counts.vectors += 1;
        }
    }

    // 3. lexical index — one FTS5 table per memory label, rowid == node id.
    //    Filtered by the label's own table *and* by `nodes`, so a legacy row
    //    whose node was already deleted does not come back as a dangling hit.
    for label in labels::ALL {
        let Some(fts) = schema::fts_table(label) else {
            continue;
        };
        if !table_exists(legacy, fts)? {
            continue;
        }
        let mut stmt = legacy.prepare(&format!(
            "SELECT rowid, content, node_id FROM {fts} \
             WHERE rowid IN (SELECT id FROM nodes WHERE label = ?1)"
        ))?;
        let mut rows = stmt.query(params![label])?;
        while let Some(row) = rows.next()? {
            target.execute(
                &format!("INSERT INTO {fts}(rowid, content, node_id) VALUES (?1, ?2, ?3)"),
                params![
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ],
            )?;
            counts.lexical_rows += 1;
        }
    }

    // 4. forgetting archive — re-inserted without its id (it is a plain log,
    //    nothing references it) but with its `node_id`, which still resolves
    //    because memory ids are preserved.
    if table_exists(legacy, "purge_log")? {
        let mut stmt = legacy.prepare(&format!(
            "SELECT node_id, label, props, content, embedding, reason, purged_at \
             FROM purge_log WHERE label IN ({in_list})"
        ))?;
        let mut rows = stmt.query(args.as_slice())?;
        while let Some(row) = rows.next()? {
            target.execute(
                "INSERT INTO purge_log(node_id, label, props, content, embedding, reason, purged_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<Vec<u8>>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ],
            )?;
            counts.purge_log_rows += 1;
        }
    }

    Ok(counts)
}

/// Whether a table (ordinary or FTS5 virtual) exists in this database.
///
/// Guards the copy against a store file from an unexpected generation: the
/// answer is "nothing to import", not "the migration crashed".
fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1 LIMIT 1",
            params![name],
            |r| r.get(0),
        )
        .unwrap_or(None);
    Ok(exists.is_some())
}

