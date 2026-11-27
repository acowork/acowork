//! Chunked, resumable upload store — the storage half of ADR-122 §3.
//!
//! One fixed-size raw-binary chunk channel, four verbs:
//!
//! | verb | store call | on-disk effect |
//! |---|---|---|
//! | `POST {store}/uploads` | [`UploadStore::create`] | `.uploads/<id>/meta.json` |
//! | `PUT …/{id}/{offset}` | [`UploadStore::put`] | `.uploads/<id>/<offset>` |
//! | `HEAD …/{id}` | [`UploadStore::head`] | — (reads chunk lengths) |
//! | `POST …/{id}/complete` | [`UploadStore::complete`] | merges → staging file |
//! | `DELETE …/{id}` | [`UploadStore::discard`] | removes `.uploads/<id>` |
//!
//! This module owns *bytes and state only*: it has no HTTP, no auth and no
//! knowledge of where a completed blob lands. The owners of the four stores
//! (Runtime: session attachments + workspaces; the user service: chat files
//! and the agent→agent transfer area) wrap it with their own routes and hand
//! the staging file to their own landing code.
//!
//! ## Why the merge is not "concatenate the chunks in memory"
//!
//! ADR-122 D5: `complete` streams offset-ascending appends into a staging file
//! while hashing it. A 1 GiB upload therefore costs one pass of disk I/O and
//! one 64 KiB buffer, not 1 GiB of heap — the old single-request path buffered
//! the whole file at every hop, which is half of what this ADR exists to fix.
//!
//! The staging file lives inside `.uploads/<id>/` so the landing code can
//! `rename` it onto its target (same filesystem ⇒ atomic replace, ADR-122 §3
//! "原子替换"), instead of copying the bytes a second time.
//!
//! ## Deliberate ceilings (ADR-122 §6)
//!
//! - **Sequential, single-writer.** One `uploadId` accepts one writer at a
//!   time; out-of-order concurrent chunks are rejected, not scheduled.
//! - **No per-caller quota.** Disk is governed by the Gateway's unified cache
//!   management (ADR-123) plus the 24 h TTL sweep below.
//!
//! A checksum failure keeps the chunks (ADR-122 D10 + Q3) and returns the id to
//! a writable state, so the client may re-`PUT` any offset and complete again.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Fixed chunk size (ADR-122 D2). MUST stay well below the smallest single-hop
/// request-body cap — every hop today is 64 MiB (`GLOBAL_BODY_LIMIT`).
pub const CHUNK_BYTES: u64 = 2 * 1024 * 1024;

/// Default single-file ceiling (ADR-122 D6). Configurable per deployment via
/// [`max_file_bytes`]; disk, not this number, is the real ceiling.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024;

/// Unfinished uploads are swept after this long (ADR-122 D10).
pub const UPLOAD_TTL_SECS: i64 = 24 * 60 * 60;

/// Streaming buffer used by the merge pass. Also bounds how much of a chunk
/// sits in memory at once.
const MERGE_BUF_BYTES: usize = 64 * 1024;

/// The single-file ceiling in force for this process.
///
/// `ACOWORK_MAX_UPLOAD_BYTES` overrides [`DEFAULT_MAX_FILE_BYTES`] (ADR-122 D6:
/// the ceiling is decoupled from the transport cap and is per-deployment
/// configurable). A malformed value is ignored rather than fatal — a typo must
/// not take the service down, and the default is always safe.
pub fn max_file_bytes() -> u64 {
    std::env::var("ACOWORK_MAX_UPLOAD_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_FILE_BYTES)
}

/// Errors a caller of [`UploadStore`] can receive; each maps onto the HTTP
/// status in the doc comment.
#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    /// No such `uploadId` (never created, expired, or discarded) → 404.
    #[error("upload not found: {0}")]
    NotFound(String),
    /// Malformed request (size, offset, digest, chunk length) → 400.
    #[error("invalid upload request: {0}")]
    BadRequest(String),
    /// The declared single-file ceiling was exceeded → 413.
    #[error("upload exceeds {limit} bytes (declared {declared})")]
    TooLarge { declared: u64, limit: u64 },
    /// Wrong writer, out of order, or interleaved with `complete` → 409.
    #[error("upload conflict: {0}")]
    Conflict(String),
    /// Merge produced the wrong length or digest → 422. Chunks are kept.
    #[error("upload checksum mismatch: expected {expected}, got {actual} ({size} bytes)")]
    Checksum {
        expected: String,
        actual: String,
        size: u64,
    },
    /// Filesystem failure → 500.
    #[error("upload storage error: {0}")]
    Io(String),
}

impl From<std::io::Error> for UploadError {
    fn from(e: std::io::Error) -> Self {
        UploadError::Io(e.to_string())
    }
}

/// What a `HEAD` answer carries (ADR-122 D4/D8).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadProgress {
    pub name: String,
    /// Declared total, not yet a fact (ADR-122 §3 "声明值不当事实").
    pub size: u64,
    /// Bytes confirmed on disk — the offset the client resumes from.
    pub received: u64,
    pub chunk_bytes: u64,
    /// `true` once `complete` succeeded: a second `complete` replays the
    /// recorded result instead of re-merging.
    pub completed: bool,
}

/// Chunk-stream metadata, persisted as `.uploads/<id>/meta.json`.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct Meta {
    name: String,
    size: u64,
    sha256: String,
    /// Unix seconds; drives the TTL sweep without relying on directory mtime
    /// (which every chunk write would touch).
    created_at: i64,
}

/// One process-local store root.
///
/// Cheap to construct and safe to hold in an `Arc`: the only mutable state is
/// the set of ids currently merging.
pub struct UploadStore {
    root: PathBuf,
    /// Ids with a writer or a merge in flight — the "single writer" rule of
    /// ADR-122 §3, in-process because each store has exactly one owning
    /// process. `std::sync::Mutex` (not `tokio`'s) so the marker can also be
    /// cleared from `Drop`: a caller that drops a [`Completer`] without
    /// committing must not wedge the id forever.
    busy: Arc<Mutex<HashSet<String>>>,
}

impl UploadStore {
    /// `root` is the directory that will hold `<id>/` per in-flight upload,
    /// created on first use (e.g. `<work_dir>/.uploads/sessions`).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            busy: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `POST {store}/uploads` — reserve an id for a declared total + digest.
    ///
    /// The id is server-generated (ADR-122 §3): a client-chosen id would be a
    /// path component under `.uploads/`, i.e. a traversal primitive.
    pub async fn create(
        &self,
        name: &str,
        size: u64,
        sha256: &str,
    ) -> Result<String, UploadError> {
        if size == 0 {
            return Err(UploadError::BadRequest("size must be positive".into()));
        }
        let limit = max_file_bytes();
        if size > limit {
            return Err(UploadError::TooLarge { declared: size, limit });
        }
        let sha = sha256.trim().to_ascii_lowercase();
        if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(UploadError::BadRequest(
                "sha256 must be 64 hex characters".into(),
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let dir = self.dir(&id);
        fs::create_dir_all(&dir).await?;
        let meta = Meta {
            name: name.to_string(),
            size,
            sha256: sha,
            created_at: chrono::Utc::now().timestamp(),
        };
        write_json(&dir.join(META), &meta).await?;
        Ok(id)
    }

    /// `PUT {store}/uploads/{id}/{offset}` — append (or re-send) one chunk.
    ///
    /// Returns the offset now confirmed on disk. Re-sending an already-received
    /// offset overwrites it and succeeds (idempotent retry, and the repair path
    /// after a checksum failure); skipping ahead is a 409.
    pub async fn put(&self, id: &str, offset: u64, chunk: &[u8]) -> Result<u64, UploadError> {
        if chunk.is_empty() {
            return Err(UploadError::BadRequest("empty chunk".into()));
        }
        if chunk.len() as u64 > CHUNK_BYTES {
            return Err(UploadError::BadRequest(format!(
                "chunk exceeds {CHUNK_BYTES} bytes"
            )));
        }
        if !offset.is_multiple_of(CHUNK_BYTES) {
            return Err(UploadError::BadRequest(format!(
                "offset must be a multiple of {CHUNK_BYTES}"
            )));
        }
        let dir = self.dir(id);
        // Single writer (ADR-122 §3): a second writer — another session
        // resuming the same id, or a `complete` in flight — is a 409, not a
        // queue.
        self.begin_busy(id, "another writer is active for this upload")?;
        let written = self.put_locked(&dir, offset, chunk).await;
        self.end_busy(id);
        written
    }

    async fn put_locked(&self, dir: &Path, offset: u64, chunk: &[u8]) -> Result<u64, UploadError> {
        let meta = self.read_meta(dir).await?;
        if offset.saturating_add(chunk.len() as u64) > meta.size {
            return Err(UploadError::BadRequest(
                "chunk runs past the declared size".into(),
            ));
        }
        if dir.join(RESULT).exists() {
            return Err(UploadError::Conflict("upload already completed".into()));
        }
        let received = received_bytes(dir, meta.size).await?;
        if offset > received {
            return Err(UploadError::Conflict(format!(
                "chunk is out of order: offset {offset} but only {received} bytes received"
            )));
        }
        fs::write(dir.join(offset.to_string()), chunk).await?;
        received_bytes(dir, meta.size).await
    }

    /// `HEAD {store}/uploads/{id}` — how far the client got, for resume (D8).
    pub async fn head(&self, id: &str) -> Result<UploadProgress, UploadError> {
        let dir = self.dir(id);
        let meta = self.read_meta(&dir).await?;
        let completed = dir.join(RESULT).exists();
        Ok(UploadProgress {
            name: meta.name,
            size: meta.size,
            received: if completed {
                meta.size
            } else {
                received_bytes(&dir, meta.size).await?
            },
            chunk_bytes: CHUNK_BYTES,
            completed,
        })
    }

    /// `POST {store}/uploads/{id}/complete` — merge, verify, hand off.
    ///
    /// Returns either the recorded result of an earlier successful merge
    /// (idempotent replay, ADR-122 §3) or a [`Completer`] whose staging file
    /// the caller lands itself.
    pub async fn complete(&self, id: &str) -> Result<Completion, UploadError> {
        let dir = self.dir(id);
        if let Some(result) = read_json::<serde_json::Value>(&dir.join(RESULT)).await? {
            return Ok(Completion::Replay(result));
        }
        {
            self.begin_busy(id, "upload is already being completed")?;
        }
        let merged = self.merge(&dir).await;
        match merged {
            Ok(()) => {
                // `merge` verified length + digest, so the staging file is the
                // declared content.
                let meta = self.read_meta(&dir).await?;
                Ok(Completion::Staged(Completer {
                    path: dir.join(STAGING),
                    dir,
                    id: id.to_string(),
                    meta,
                    busy: self.busy.clone(),
                    cleared: false,
                }))
            }
            Err(e) => {
                self.end_busy(id);
                let _ = fs::remove_file(dir.join(STAGING)).await;
                Err(e)
            }
        }
    }

    /// `DELETE {store}/uploads/{id}` — drop every chunk of this upload.
    ///
    /// Idempotent: an unknown id is a no-op, so a client retrying an abort
    /// never has to distinguish "already gone" from "never existed".
    pub async fn discard(&self, id: &str) -> Result<(), UploadError> {
        let dir = self.dir(id);
        match fs::remove_dir_all(&dir).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Remove uploads older than `ttl_secs` (ADR-122 D10). Called at startup
    /// and periodically so an abandoned 1 GiB upload cannot sit on disk
    /// forever. Returns how many were removed.
    pub async fn sweep(&self, ttl_secs: i64) -> usize {
        let now = chrono::Utc::now().timestamp();
        let mut entries = match fs::read_dir(&self.root).await {
            Ok(e) => e,
            Err(_) => return 0, // nothing uploaded yet
        };
        let mut removed = 0usize;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let expired = match read_json::<Meta>(&dir.join(META)).await {
                Ok(Some(meta)) => now.saturating_sub(meta.created_at) > ttl_secs,
                // A directory without a readable meta is half-created garbage
                // or left over from a crash; judge it by its own age.
                _ => match fs::metadata(&dir).await {
                    Ok(m) => match m.modified() {
                        Ok(t) => t
                            .elapsed()
                            .map(|age| age.as_secs() as i64 > ttl_secs)
                            .unwrap_or(false),
                        Err(_) => false,
                    },
                    Err(_) => false,
                },
            };
            if expired {
                self.end_busy(&entry.file_name().to_string_lossy());
                if fs::remove_dir_all(&dir).await.is_ok() {
                    removed += 1;
                    tracing::info!(upload = %dir.display(), "swept expired upload");
                }
            }
        }
        removed
    }

    /// Merge every chunk, offset-ascending, into the staging file; verify the
    /// length and digest the client declared (ADR-122 D5).
    async fn merge(&self, dir: &Path) -> Result<(), UploadError> {
        let meta = self.read_meta(dir).await?;
        let received = received_bytes(dir, meta.size).await?;
        if received != meta.size {
            return Err(UploadError::Conflict(format!(
                "upload incomplete: {received} of {} bytes",
                meta.size
            )));
        }
        let staging = dir.join(STAGING);
        let mut out = fs::File::create(&staging).await?;
        let mut hasher = Sha256::new();
        let mut total = 0u64;
        let mut buf = vec![0u8; MERGE_BUF_BYTES];
        let mut offset = 0u64;
        while offset < meta.size {
            let chunk_path = dir.join(offset.to_string());
            let mut chunk = fs::File::open(&chunk_path).await.map_err(|_| {
                UploadError::Conflict(format!("missing chunk at offset {offset}"))
            })?;
            loop {
                let n = chunk.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                out.write_all(&buf[..n]).await?;
                total += n as u64;
            }
            offset += CHUNK_BYTES;
        }
        out.sync_all().await?;
        drop(out);

        let actual = hex(&hasher.finalize());
        if total != meta.size || actual != meta.sha256 {
            return Err(UploadError::Checksum {
                expected: meta.sha256,
                actual,
                size: total,
            });
        }
        Ok(())
    }

    fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    /// 409 when another writer or a merge already holds this id (ADR-122 §3:
    /// `complete` and a concurrent `PUT` may not interleave).
    fn begin_busy(&self, id: &str, busy_message: &str) -> Result<(), UploadError> {
        let mut guard = self.busy.lock().expect("upload mutex poisoned");
        if !guard.insert(id.to_string()) {
            return Err(UploadError::Conflict(busy_message.to_string()));
        }
        Ok(())
    }

    fn end_busy(&self, id: &str) {
        self.busy
            .lock()
            .expect("upload mutex poisoned")
            .remove(id);
    }

    async fn read_meta(&self, dir: &Path) -> Result<Meta, UploadError> {
        read_json::<Meta>(&dir.join(META))
            .await?
            .ok_or_else(|| UploadError::NotFound(dir.display().to_string()))
    }
}

/// Outcome of [`UploadStore::complete`].
pub enum Completion {
    /// An earlier merge succeeded; this is its recorded result (idempotent
    /// replay).
    Replay(serde_json::Value),
    /// Freshly merged and verified — the caller lands the staging file.
    Staged(Completer),
}

/// A verified staging file plus the metadata needed to land it.
///
/// The caller MUST either [`Completer::commit`] the landing result or
/// [`Completer::abandon`]; dropping it clears the in-flight marker so the id
/// stays usable, and leaves the chunks on disk for the TTL sweep.
pub struct Completer {
    dir: PathBuf,
    id: String,
    meta: Meta,
    path: PathBuf,
    busy: Arc<Mutex<HashSet<String>>>,
    cleared: bool,
}

impl Completer {
    /// Declared chunk-stream metadata. Only trustworthy *after* the merge
    /// verified the digest (ADR-122 §3).
    pub fn name(&self) -> &str {
        &self.meta.name
    }

    pub fn size(&self) -> u64 {
        self.meta.size
    }

    pub fn sha256(&self) -> &str {
        &self.meta.sha256
    }

    /// The staging file. Landing code should `rename` it into place.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record the landed metadata and drop the chunks. A repeat `complete`
    /// then replays `result` instead of re-merging.
    pub async fn commit(mut self, result: serde_json::Value) -> Result<serde_json::Value, UploadError> {
        write_json(&self.dir.join(RESULT), &result).await?;
        remove_chunks(&self.dir).await;
        let _ = fs::remove_file(&self.path).await; // already renamed away ⇒ ENOENT
        self.clear();
        Ok(result)
    }

    /// Landing failed: keep the chunks so the client can re-`PUT` and retry,
    /// drop only the half-written staging file (ADR-122 Q3).
    pub async fn abandon(mut self) {
        let _ = fs::remove_file(&self.path).await;
        self.clear();
    }

    fn clear(&mut self) {
        if !self.cleared {
            self.cleared = true;
            self.busy
                .lock()
                .expect("upload mutex poisoned")
                .remove(&self.id);
        }
    }
}

impl Drop for Completer {
    fn drop(&mut self) {
        self.clear();
    }
}

const META: &str = "meta.json";
const STAGING: &str = "staging.bin";
const RESULT: &str = "result.json";

/// Bytes confirmed on disk: the largest contiguous run of chunks starting at
/// offset 0. A hole (a client that re-`PUT`s an arbitrary offset after a
/// checksum failure) therefore reports the hole, not the sum — `complete`
/// stays honest about what it can merge.
async fn received_bytes(dir: &Path, size: u64) -> Result<u64, UploadError> {
    let mut received = 0u64;
    let mut offset = 0u64;
    while offset < size {
        let path = dir.join(offset.to_string());
        let Ok(m) = fs::metadata(&path).await else {
            break;
        };
        received += m.len();
        offset += CHUNK_BYTES;
    }
    Ok(received)
}

async fn remove_chunks(dir: &Path) {
    let Ok(mut entries) = fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        // Chunks are named by their decimal offset; meta/result/staging stay.
        if !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()) {
            let _ = fs::remove_file(entry.path()).await;
        }
    }
}

async fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), UploadError> {
    let json = serde_json::to_vec(value).map_err(|e| UploadError::Io(e.to_string()))?;
    fs::write(path, json).await?;
    Ok(())
}

async fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, UploadError> {
    let raw = match fs::read(path).await {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    serde_json::from_slice(&raw)
        .map(Some)
        .map_err(|e| UploadError::Io(format!("{}: {e}", path.display())))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tag: &str) -> (UploadStore, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "acowork-upload-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
        (UploadStore::new(root.join("uploads")), root)
    }

    fn digest(bytes: &[u8]) -> String {
        hex(&Sha256::digest(bytes))
    }

    /// A chunk-sized payload so the multi-chunk path is the one under test.
    fn payload(chunks: usize) -> Vec<u8> {
        let mut v = Vec::new();
        for i in 0..chunks {
            let mut chunk = vec![b'a' + i as u8; CHUNK_BYTES as usize];
            v.append(&mut chunk);
        }
        v
    }

    #[tokio::test]
    async fn create_put_head_complete_round_trip() {
        let (s, root) = store("roundtrip");
        let bytes = payload(3);
        let id = s.create("report.pdf", bytes.len() as u64, &digest(&bytes))
            .await
            .unwrap();

        // Two chunks land, the third is missing ⇒ HEAD reports a resumable
        // offset and complete refuses instead of merging a short file.
        for (n, chunk) in bytes.chunks(CHUNK_BYTES as usize).take(2).enumerate() {
            let received = s.put(&id, n as u64 * CHUNK_BYTES, chunk).await.unwrap();
            assert_eq!(received, (n as u64 + 1) * CHUNK_BYTES);
        }
        let head = s.head(&id).await.unwrap();
        assert_eq!((head.received, head.size, head.completed), (2 * CHUNK_BYTES, bytes.len() as u64, false));
        assert!(matches!(s.complete(&id).await, Err(UploadError::Conflict(_))));

        // Resume from where HEAD said, then complete.
        s.put(&id, head.received, &bytes[head.received as usize..])
            .await
            .unwrap();
        let Completion::Staged(c) = s.complete(&id).await.unwrap() else {
            panic!("expected a fresh merge");
        };
        assert_eq!(std::fs::read(c.path()).unwrap(), bytes);
        assert_eq!(c.sha256(), digest(&bytes));
        c.commit(serde_json::json!({"documentId": "d1"})).await.unwrap();

        // Idempotent replay: same result, no re-merge, chunks gone.
        let Completion::Replay(v) = s.complete(&id).await.unwrap() else {
            panic!("expected the recorded result");
        };
        assert_eq!(v["documentId"], "d1");
        assert!(!root.join("uploads").join(&id).join("0").exists());
        assert!(s.head(&id).await.unwrap().completed);
    }

    #[tokio::test]
    async fn checksum_failure_keeps_chunks_and_allows_repair() {
        let (s, _root) = store("checksum");
        let good = payload(2);
        // Declare the digest of different bytes: the merge must reject.
        let id = s.create("a.bin", good.len() as u64, &digest(b"not-the-bytes"))
            .await
            .unwrap();
        for (n, chunk) in good.chunks(CHUNK_BYTES as usize).enumerate() {
            s.put(&id, n as u64 * CHUNK_BYTES, chunk).await.unwrap();
        }
        let err = s.complete(&id).await.err().expect("checksum must fail");
        assert!(matches!(err, UploadError::Checksum { .. }), "got {err:?}");

        // D10/Q3: chunks survive, the id is writable again, and it is not
        // stuck in the "completing" state.
        assert_eq!(s.head(&id).await.unwrap().received, good.len() as u64);
        s.put(&id, 0, &good[..CHUNK_BYTES as usize]).await.unwrap();
        assert!(matches!(s.complete(&id).await, Err(UploadError::Checksum { .. })));
    }

    #[tokio::test]
    async fn rejects_bad_offsets_and_oversized_chunks() {
        let (s, _root) = store("offsets");
        let bytes = payload(2);
        let id = s.create("a.bin", bytes.len() as u64, &digest(&bytes))
            .await
            .unwrap();
        // Not a chunk multiple.
        assert!(matches!(
            s.put(&id, 1, b"x").await,
            Err(UploadError::BadRequest(_))
        ));
        // Skipping a chunk (the first one never arrived).
        assert!(matches!(
            s.put(&id, CHUNK_BYTES, b"x").await,
            Err(UploadError::Conflict(_))
        ));
        // Runs past the declared size.
        assert!(matches!(
            s.put(&id, 0, &vec![0u8; CHUNK_BYTES as usize + 1]).await,
            Err(UploadError::BadRequest(_))
        ));
        // Unknown id.
        assert!(matches!(
            s.head("nope").await,
            Err(UploadError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn declared_size_over_the_ceiling_is_refused() {
        let (s, _root) = store("ceiling");
        let err = s
            .create("huge.bin", max_file_bytes() + 1, &digest(b"x"))
            .await
            .expect_err("must refuse");
        assert!(matches!(err, UploadError::TooLarge { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn sweep_removes_only_expired_uploads() {
        let (s, root) = store("sweep");
        let old = s.create("old.bin", 4, &digest(b"abcd")).await.unwrap();
        let fresh = s.create("new.bin", 4, &digest(b"abcd")).await.unwrap();
        // Age the "old" one past the TTL without waiting 24 h.
        let meta_path = root.join("uploads").join(&old).join(META);
        let mut meta: Meta =
            serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
        meta.created_at -= UPLOAD_TTL_SECS + 1;
        std::fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();

        assert_eq!(s.sweep(UPLOAD_TTL_SECS).await, 1);
        assert!(!root.join("uploads").join(&old).exists());
        assert!(root.join("uploads").join(&fresh).exists());
    }

    #[tokio::test]
    async fn a_dropped_completer_does_not_wedge_the_id() {
        let (s, _root) = store("drop");
        let bytes = payload(1);
        let id = s.create("a.bin", bytes.len() as u64, &digest(&bytes))
            .await
            .unwrap();
        s.put(&id, 0, &bytes).await.unwrap();
        let Completion::Staged(c) = s.complete(&id).await.unwrap() else {
            panic!("expected staging");
        };
        drop(c); // landing code bailed out
        // The id is writable/complete-able again rather than 409 forever.
        assert!(matches!(s.complete(&id).await, Ok(Completion::Staged(_))));
    }
}