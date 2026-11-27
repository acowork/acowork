//! RuntimeAttachmentService — implements [`AttachmentService`] (ADR-046).
//!
//! Backed by the unified blob store at `<work_dir>/files/<document_id>`.
//! One physical directory for **all** uploads across all sessions (PDF,
//! DOCX, PNG, JPG, …); per-session indexing lives in the JSONL as
//! `AttachmentMeta` system entries (see
//! [`crate::conversation::AttachmentMeta`]).
//!
//! ## State
//!
//! Holds the boot-time `work_dir` (no async resource dependencies), so
//! the service can be constructed immediately after the workspace
//! services in `session_init.rs` Phase B and published to the
//! `attachment_slot` for HTTP handlers to consume.
//!
//! ## On-disk layout
//!
//! ```text
//! <work_dir>/
//! ├── conversations/
//! │   └── <sid>.jsonl       # each upload / attach is a system entry
//! └── files/
//!     └── <sanitized_stem>_<document_id>.<ext>
//!         # blob — `<sanitized_stem>` is the user-supplied filename
//!         # cleaned up by `sanitize_stem` (so the workspace file tree
//!         # stays readable), `document_id` is the content hash that
//!         # keys the dedup invariant, and `<ext>` is a whitelisted
//!         # suffix chosen from the user's `format` via `safe_extension`.
//! ```
//!
//! Both the safe-extension whitelist and the stem sanitiser live in the
//! shared [`crate::usecases::attachment`] module so the write path here,
//! the read predicate below, and the LLM hint builder in
//! `session_task::build_attachment_hint` all agree on the on-disk name.
//!
//! Pre-change files written as bare `<document_id>` (no extension) or
//! `<document_id>.<ext>` (the previous default) still exist on some
//! developer disks. `read_file` falls back to those names so historic
//! blobs remain readable; new writes always pick the
//! `<name>_<document_id>.<ext>` shape and reuse a pre-existing blob with
//! the same content as a dedup hit (its bytes are identical because
//! `document_id` is content-derived).
//!
//! ## `document_id` format
//!
//! 12-hex content-hash prefix + 4-hex content-hash suffix
//! (`[0-9a-f]{12}-[0-9a-f]{4}`). Both halves are sliced from a single
//! `SHA-256(bytes)` digest, so the id is **fully content-derived**:
//! identical bytes always yield the same `document_id`, regardless of
//! process, host, or wall-clock time. This is the cornerstone of the
//! implicit-dedup invariant — same bytes ⇒ same id ⇒ `tokio::fs::rename`
//! onto an existing `<document_id>.<ext>` is a no-op.
//!
//! Why SHA-256 (not `std::collections::hash_map::DefaultHasher`):
//! `DefaultHasher` is `SipHash` with `RandomState`-derived keys that
//! are reseeded per process. Across a Runtime restart the same bytes
//! would hash to a **different** prefix, defeating dedup across
//! sessions. SHA-256 is collision-resistant and deterministic — the
//! only choice for a stable content address.
//!
//! Why 48+16 bits (and not, say, 256 bits): the storage layer can only
//! sanity-check the prefix against on-disk filenames; longer ids would
//! just inflate every JSONL metadata entry and HTTP URL without
//! improving practical collision odds (a user-side file library of 2^24
//! ≈ 16M blobs still has ~50% odds of a 48-bit collision, but no single
//! agent reaches that volume). If a collision ever does happen, the
//! read path's "ambiguous on-disk match" guard surfaces it as an
//! error rather than silently picking one.
//!
//! Read paths validate the charset before any filesystem access to
//! defang path-traversal probes.
//!
//! ## Atomicity
//!
//! Writes use the write-tmp-rename pattern: bytes land in
//! `<work_dir>/files/<document_id>.<ext>.tmp`, then `rename` swaps into
//! place. Concurrent uploads of the same content race harmlessly on
//! the rename — the second rename is a no-op because the file already
//! has the same content (and we never mutate a placed blob).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::usecases::attachment::{
    AttachmentError, AttachmentService, StagedUploadParams, UploadFileParams, UploadedFileResponse,
    max_upload_bytes, name_matches_document_id, on_disk_name,
};

/// Concrete [`AttachmentService`] backed by `<work_dir>/files/`.
pub struct RuntimeAttachmentService {
    work_dir: PathBuf,
    /// Serialises [`write_blob_atomic`] so two concurrent uploads of
    /// identical bytes under different filenames cannot race past the
    /// dedup scan and produce two on-disk files for one `document_id`.
    /// The runtime is a single process, so a process-local mutex is
    /// sufficient — and `tokio::sync::Mutex` is what we need here
    /// because the protected section awaits on `fs::*` calls.
    write_lock: tokio::sync::Mutex<()>,
}

impl RuntimeAttachmentService {
    pub fn new(work_dir: PathBuf) -> Self {
        Self {
            work_dir,
            write_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Resolve `<work_dir>/files`. Created on demand by
    /// [`RuntimeAttachmentService::ensure_files_dir`].
    fn files_dir(&self) -> PathBuf {
        self.work_dir.join("files")
    }

    async fn ensure_files_dir(&self) -> Result<PathBuf, AttachmentError> {
        let dir = self.files_dir();
        if !dir.exists() {
            fs::create_dir_all(&dir).await.map_err(|e| {
                AttachmentError::Persistence(format!("create_dir_all {}: {}", dir.display(), e))
            })?;
        }
        Ok(dir)
    }

    /// Validate `document_id` matches the canonical charset. Cheap; no
    /// filesystem access. Used by every read path and (defensively) by
    /// write paths that accept a precomputed id.
    fn validate_document_id(document_id: &str) -> Result<(), AttachmentError> {
        // 12-hex prefix + `-` + 1..=4 hex suffix.
        let bytes = document_id.as_bytes();
        let len = bytes.len();
        if !(14..=17).contains(&len) {
            return Err(AttachmentError::InvalidDocumentId(document_id.to_string()));
        }
        if bytes[12] != b'-' {
            return Err(AttachmentError::InvalidDocumentId(document_id.to_string()));
        }
        let hex_ok = |c: &u8| c.is_ascii_hexdigit();
        if !bytes[..12].iter().all(hex_ok) || !bytes[13..].iter().all(hex_ok) {
            return Err(AttachmentError::InvalidDocumentId(document_id.to_string()));
        }
        Ok(())
    }

    /// Compute the canonical `document_id` for a content payload.
    ///
    /// `document_id = format!("{prefix_hex}-{suffix_hex}")` where both
    /// halves are taken from `SHA-256(bytes)`:
    ///
    /// - `prefix_hex` = first 6 bytes → 12 lowercase hex chars (48 bits).
    /// - `suffix_hex` = next 2 bytes → 4 lowercase hex chars (16 bits).
    ///
    /// Total id length: 12 + 1 (dash) + 4 = 17 chars, which satisfies the
    /// 14..=17 length window accepted by [`Self::validate_document_id`].
    ///
    /// Stability: identical bytes ⇒ identical id, regardless of process,
    /// wall-clock time, or system time-jitter. Verified by
    /// `compute_document_id_is_content_deterministic`.
    fn compute_document_id(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(bytes);
        Self::document_id_from_digest(&digest).expect("fresh digest is well-formed")
    }

    /// Derive `document_id` from an already-computed `SHA-256` digest.
    ///
    /// This is the derivation point for the chunked route (ADR-122 §3): the
    /// upload store verifies the client's declared digest while streaming the
    /// merge, so the landing path must not read the bytes a second time just
    /// to hash them.
    fn document_id_from_digest(digest: &[u8]) -> Result<String, AttachmentError> {
        if digest.len() < 8 {
            return Err(AttachmentError::Persistence(
                "content digest is too short".to_string(),
            ));
        }
        let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
        Ok(format!("{}-{}", &hex[..12], &hex[12..16]))
    }

    /// Same derivation, from the lowercase-hex `sha256` a chunked upload
    /// verified (`acowork_core::upload::Completer::sha256`).
    fn document_id_from_sha256_hex(sha256: &str) -> Result<String, AttachmentError> {
        let bytes = sha256.as_bytes();
        if bytes.len() != 64 || !bytes.iter().all(|b| b.is_ascii_hexdigit()) {
            return Err(AttachmentError::Persistence(format!(
                "malformed sha256: {sha256:?}"
            )));
        }
        Ok(format!("{}-{}", &sha256[..12], &sha256[12..16]))
    }

    /// List every UTF-8-valid filename in `dir`. Used by both the
    /// write-path dedup scan and the read-path matching loop, so they
    /// iterate the directory the same way.
    async fn list_dir_names(dir: &Path) -> Result<Vec<String>, AttachmentError> {
        let mut entries = fs::read_dir(dir).await.map_err(|e| {
            AttachmentError::Persistence(format!("read_dir {}: {}", dir.display(), e))
        })?;
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await.map_err(|e| {
            AttachmentError::Persistence(format!("iter dir {}: {}", dir.display(), e))
        })? {
            // Bind `file_name` first — `OsString::to_str` borrows the
            // `OsString` which is dropped at end of statement otherwise.
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_string());
            }
        }
        Ok(names)
    }

    /// Count how many on-disk entries in `dir` address `document_id`
    /// (under any of the supported legacy / current on-disk shapes).
    /// Drives both the write-side dedup check and the post-rename
    /// race guard; the read path uses the matching predicate directly
    /// to also report ambiguity.
    async fn count_matches(dir: &Path, document_id: &str) -> Result<usize, AttachmentError> {
        let names = Self::list_dir_names(dir).await?;
        Ok(names
            .iter()
            .filter(|n| name_matches_document_id(n, document_id))
            .count())
    }

    /// Atomic write-tmp-rename of `bytes` to
    /// `<dir>/<sanitized_stem>_<document_id>.<safe_ext>`.
    ///
    /// `format` selects the on-disk suffix and `filename` seeds the
    /// readable stem — both flow through the shared helpers in
    /// [`crate::usecases::attachment`] so the path produced here is
    /// identical to the hint string the LLM / `doc_reader` receive.
    ///
    /// Returns `Ok(true)` if a new file was created and `Ok(false)` on
    /// a dedup hit (a blob with the same `document_id` was already on
    /// disk — the existing file is left untouched, and the caller's
    /// `filename` is echoed back to the user as-is without rewriting
    /// disk). The dedup check accepts all three on-disk shapes
    /// (legacy bare / legacy suffixed / new `<stem>_<id>.<ext>`) so
    /// pre-existing data is honoured.
    ///
    /// Serialised by `self.write_lock` so two concurrent uploads of
    /// identical bytes under different filenames can't race past the
    /// dedup scan and produce two on-disk files for the same id.
    async fn write_blob_atomic(
        &self,
        dir: &Path,
        document_id: &str,
        format: &str,
        filename: &str,
        bytes: &[u8],
    ) -> Result<bool, AttachmentError> {
        // Hold the per-service lock for the whole critical section.
        // Uploads are user-initiated and infrequent, so process-wide
        // serialisation is fine — and necessary because the section
        // awaits on `fs::*` calls (a plain `std::sync::Mutex` would
        // not be `Send` across `.await`).
        let _guard = self.write_lock.lock().await;

        // (1) Dedup: a blob with this `document_id` may already exist
        // — under the legacy bare shape, the legacy suffixed shape,
        // or a new `<stem>_<id>.<ext>` shape from a prior upload whose
        // user filename was different. Content is identical by
        // construction, so any existing file is authoritative — never
        // overwrite. The first-uploaded on-disk name wins.
        if Self::count_matches(dir, document_id).await? > 0 {
            return Ok(false);
        }

        let final_name = on_disk_name(document_id, format, filename);
        let final_path = dir.join(&final_name);
        let tmp_path = dir.join(format!("{final_name}.tmp"));

        {
            let mut f = fs::File::create(&tmp_path).await.map_err(|e| {
                AttachmentError::Persistence(format!("create tmp {}: {}", tmp_path.display(), e))
            })?;
            f.write_all(bytes).await.map_err(|e| {
                AttachmentError::Persistence(format!("write tmp {}: {}", tmp_path.display(), e))
            })?;
            f.sync_all().await.map_err(|e| {
                AttachmentError::Persistence(format!("sync tmp {}: {}", tmp_path.display(), e))
            })?;
        }

        // rename is atomic on the same filesystem on Unix; Windows
        // would need ReplaceFileW semantics but Desktop targets macOS
        // and Windows is not in scope for this runtime path.
        fs::rename(&tmp_path, &final_path).await.map_err(|e| {
            AttachmentError::Persistence(format!(
                "rename {} -> {}: {}",
                tmp_path.display(),
                final_path.display(),
                e
            ))
        })?;

        // (2) Post-rename invariant check. With the write_lock held
        // above, the answer should always be `1`; the re-scan is a
        // belt-and-suspenders guard against any future change that
        // drops the lock. If a second match ever appears (e.g. from a
        // manual file drop), the dedup hold is restored by removing
        // our newly-written file — the other, earlier-uploaded blob
        // remains the canonical one.
        if Self::count_matches(dir, document_id).await? > 1 {
            // Best-effort cleanup; if remove fails the read path's
            // ambiguity guard will surface the situation.
            let _ = fs::remove_file(&final_path).await;
            return Ok(false);
        }

        Ok(true)
    }
}

#[async_trait]
impl AttachmentService for RuntimeAttachmentService {
    async fn upload_file(
        &self,
        params: UploadFileParams,
    ) -> Result<UploadedFileResponse, AttachmentError> {
        let limit = max_upload_bytes();
        if params.bytes.len() as u64 > limit {
            return Err(AttachmentError::PayloadTooLarge {
                size: params.bytes.len() as u64,
                limit,
            });
        }

        let dir = self.ensure_files_dir().await?;
        let document_id = Self::compute_document_id(&params.bytes);

        // validate_document_id is a sanity check on the computed id —
        // compute_document_id always produces a well-formed id by
        // construction, but a defensive call here keeps the audit
        // log uniform with the read path.
        Self::validate_document_id(&document_id)?;

        self.write_blob_atomic(
            &dir,
            &document_id,
            &params.format,
            &params.filename,
            &params.bytes,
        )
        .await?;

        Ok(UploadedFileResponse {
            document_id,
            filename: params.filename,
            format: params.format,
            size_bytes: params.bytes.len() as u64,
            width: params.width,
            height: params.height,
        })
    }

    async fn adopt_staged_file(
        &self,
        params: StagedUploadParams,
    ) -> Result<UploadedFileResponse, AttachmentError> {
        let dir = self.ensure_files_dir().await?;
        let document_id = Self::document_id_from_sha256_hex(&params.sha256)?;
        Self::validate_document_id(&document_id)?;

        // Single audit point, same lock as `write_blob_atomic`: two
        // concurrent adoptions of identical bytes must not both land.
        let _guard = self.write_lock.lock().await;

        if Self::count_matches(&dir, &document_id).await? > 0 {
            // Dedup hit: the earlier blob is authoritative (identical bytes by
            // construction), so the staging file is dropped instead of renamed.
            let _ = fs::remove_file(&params.staged).await;
        } else {
            let final_path = dir.join(on_disk_name(&document_id, &params.format, &params.filename));
            // The staging file lives under `<work_dir>/.uploads/`, i.e. the same
            // filesystem, so this is the atomic replace ADR-122 §3 requires.
            fs::rename(&params.staged, &final_path).await.map_err(|e| {
                AttachmentError::Persistence(format!(
                    "rename {} -> {}: {}",
                    params.staged.display(),
                    final_path.display(),
                    e
                ))
            })?;
        }

        Ok(UploadedFileResponse {
            document_id,
            filename: params.filename,
            format: params.format,
            size_bytes: params.size,
            width: params.width,
            height: params.height,
        })
    }

    async fn read_file(&self, document_id: &str) -> Result<Vec<u8>, AttachmentError> {
        Self::validate_document_id(document_id)?;

        // Ensure the `<work_dir>/files/` directory exists before
        // listing — matches the write-path invariant and turns "no
        // uploads yet" into a clean 404 instead of a filesystem
        // ENOENT that surfaces as a 500. `ensure_files_dir` is a no-op
        // when the directory is already present.
        let dir = self.ensure_files_dir().await?;
        let names = Self::list_dir_names(&dir).await?;
        let mut matched: Option<&String> = None;
        for name in &names {
            if !name_matches_document_id(name, document_id) {
                continue;
            }
            match matched {
                None => matched = Some(name),
                Some(_) => {
                    return Err(AttachmentError::Persistence(format!(
                        "ambiguous on-disk match for document_id {document_id:?} in {}",
                        dir.display()
                    )));
                }
            }
        }
        let Some(path) = matched.map(|n| dir.join(n)) else {
            return Err(AttachmentError::NotFound(document_id.to_string()));
        };
        match fs::read(&path).await {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(AttachmentError::NotFound(document_id.to_string()))
            }
            Err(e) => Err(AttachmentError::Persistence(format!(
                "read {}: {}",
                path.display(),
                e
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an `AttachmentService` rooted in `tempfile::TempDir` so the
    /// test owns the directory and cleans up on drop.
    async fn fresh_service() -> (tempfile::TempDir, RuntimeAttachmentService) {
        let dir = tempfile::tempdir().expect("tempdir");
        let svc = RuntimeAttachmentService::new(dir.path().to_path_buf());
        (dir, svc)
    }

    fn params(format: &str, payload: &[u8]) -> UploadFileParams {
        UploadFileParams {
            filename: format!("sample.{format}"),
            format: format.to_string(),
            bytes: payload.to_vec(),
            width: None,
            height: None,
        }
    }

    fn params_with_name(filename: &str, format: &str, payload: &[u8]) -> UploadFileParams {
        UploadFileParams {
            filename: filename.to_string(),
            format: format.to_string(),
            bytes: payload.to_vec(),
            width: None,
            height: None,
        }
    }

    /// End-to-end coverage that docx/pptx/xlsx blobs land with their
    /// real Office extensions — this is the regression test for the bug
    /// where every document format silently degraded to `.bin`, which
    /// then made the `doc_reader` tool reject them as unsupported.
    #[tokio::test]
    async fn upload_writes_office_docs_with_real_extensions() {
        for (format, expected_ext) in [
            ("pdf", "pdf"),
            ("docx", "docx"),
            ("pptx", "pptx"),
            ("xlsx", "xlsx"),
        ] {
            let (dir, svc) = fresh_service().await;
            let payload = format!("hello-{format}-bytes").into_bytes();
            let r = svc.upload_file(params(format, &payload)).await.unwrap();
            let on_disk = dir
                .path()
                .join("files")
                .join(format!("sample_{}.{expected_ext}", r.document_id));
            assert!(
                on_disk.exists(),
                "upload({format}) must land at {} (got doc_id={})",
                on_disk.display(),
                r.document_id
            );
        }
    }

    /// ADR-122 D5: the chunked route hands over an already-verified staging
    /// file. It must land exactly like an upload — digest-derived id, real
    /// extension — and a second staging file of identical bytes must dedup to
    /// the first blob instead of storing a copy.
    #[tokio::test]
    async fn adopt_staged_file_lands_by_digest_and_dedups_identical_bytes() {
        use sha2::{Digest, Sha256};

        let (dir, svc) = fresh_service().await;
        let payload = b"chunked-merge-payload".to_vec();
        let sha256: String = Sha256::digest(&payload)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        let staged_a = dir.path().join("staging-a.bin");
        std::fs::write(&staged_a, &payload).unwrap();
        let landed = svc
            .adopt_staged_file(StagedUploadParams {
                filename: "report.pdf".to_string(),
                format: "pdf".to_string(),
                staged: staged_a.clone(),
                sha256: sha256.clone(),
                size: payload.len() as u64,
                width: None,
                height: None,
            })
            .await
            .unwrap();
        assert!(!staged_a.exists(), "the staging file must be moved, not copied");
        let blob = dir
            .path()
            .join("files")
            .join(format!("report_{}.pdf", landed.document_id));
        assert!(blob.exists(), "expected blob at {}", blob.display());

        // Same bytes, different name: the earlier blob is authoritative.
        let staged_b = dir.path().join("staging-b.bin");
        std::fs::write(&staged_b, &payload).unwrap();
        let again = svc
            .adopt_staged_file(StagedUploadParams {
                filename: "copy.pdf".to_string(),
                format: "pdf".to_string(),
                staged: staged_b.clone(),
                sha256,
                size: payload.len() as u64,
                width: None,
                height: None,
            })
            .await
            .unwrap();
        assert_eq!(again.document_id, landed.document_id);
        assert!(!staged_b.exists(), "a dedup hit drops the staging file");
        assert_eq!(std::fs::read_dir(dir.path().join("files")).unwrap().count(), 1);
    }

    /// `upload_file` writes to `<dir>/<sanitized_stem>_<document_id>.<safe_ext>`.
    /// Dedup behaviour (same bytes ⇒ same id) is covered by
    /// [`upload_then_reupload_same_bytes_dedups`] below.
    #[tokio::test]
    async fn upload_writes_with_whitelisted_extension() {
        let (dir, svc) = fresh_service().await;
        let payload = b"hello-image-bytes";
        let r = svc.upload_file(params("jpg", payload)).await.unwrap();
        assert!(r.document_id.contains('-'));
        let on_disk = dir
            .path()
            .join("files")
            .join(format!("sample_{}.jpg", r.document_id));
        if !on_disk.exists() {
            // Build a helpful diagnostic list of files for the failure message.
            let mut names = Vec::new();
            let mut entries = fs::read_dir(dir.path().join("files")).await.unwrap();
            while let Some(e) = entries.next_entry().await.unwrap() {
                names.push(e.file_name().to_string_lossy().into_owned());
            }
            panic!(
                "expected blob at {}; dir contents: {names:?}",
                on_disk.display(),
            );
        }
    }

    /// Non-whitelisted formats fall back to `.bin` so an accidental
    /// raw string cannot surface as a Finder-rendered extension.
    #[tokio::test]
    async fn upload_unknown_format_falls_back_to_bin_extension() {
        let (dir, svc) = fresh_service().await;
        let r = svc
            .upload_file(params("exe", b"binary-blob"))
            .await
            .unwrap();
        let on_disk = dir
            .path()
            .join("files")
            .join(format!("sample_{}.bin", r.document_id));
        assert!(on_disk.exists(), "expected {} to exist", on_disk.display());
    }

    /// Original filename appears on disk so the workspace file tree
    /// stays readable. `sanitize_stem` trims the extension (we always
    /// append our own whitelisted suffix) and Unicode characters
    /// pass through unchanged.
    #[tokio::test]
    async fn upload_writes_with_readable_original_name() {
        let (dir, svc) = fresh_service().await;
        let r = svc
            .upload_file(params_with_name(
                "2024年度报告.pdf",
                "pdf",
                b"annual-report",
            ))
            .await
            .unwrap();
        let on_disk = dir
            .path()
            .join("files")
            .join(format!("2024年度报告_{}.pdf", r.document_id));
        assert!(
            on_disk.exists(),
            "Unicode filename must survive sanitisation: {}",
            on_disk.display()
        );
    }

    /// `read_file` resolves the new `<stem>_<id>.<ext>` shape AND the
    /// pre-change bare `<id>` legacy blob to the same bytes; ambiguous
    /// collisions return a persistence error rather than guessing.
    #[tokio::test]
    async fn read_resolves_new_and_legacy_blobs() {
        let (dir, svc) = fresh_service().await;
        let payload = b"read-me";
        let r = svc.upload_file(params("png", payload)).await.unwrap();

        // New <stem>_<id>.<ext> shape is what `read_file` finds.
        let got = svc.read_file(&r.document_id).await.unwrap();
        assert_eq!(got, payload);

        // Now simulate a legacy bare file: delete the new file and
        // write a legacy <doc_id> blob with the same content. read_file
        // must still find it via the shared matching predicate.
        tokio::fs::remove_file(
            dir.path()
                .join("files")
                .join(format!("sample_{}.png", r.document_id)),
        )
        .await
        .unwrap();
        tokio::fs::write(dir.path().join("files").join(&r.document_id), payload)
            .await
            .unwrap();
        let got_legacy = svc.read_file(&r.document_id).await.unwrap();
        assert_eq!(got_legacy, payload);
    }

    /// `read_file` also resolves the pre-change suffixed
    /// `<document_id>.<ext>` shape (the previous default) for backward
    /// compatibility with developer disks that already have such
    /// blobs on them.
    #[tokio::test]
    async fn read_resolves_pre_change_suffixed_blob() {
        let (dir, svc) = fresh_service().await;
        let payload = b"legacy-suffixed-bytes";
        let id = RuntimeAttachmentService::compute_document_id(payload);

        // Pre-create the files directory so we can drop a legacy blob
        // outside of `upload_file` (which would normally ensure it).
        tokio::fs::create_dir_all(dir.path().join("files"))
            .await
            .unwrap();
        // Drop a legacy `<id>.png` file directly (no upload).
        tokio::fs::write(dir.path().join("files").join(format!("{id}.png")), payload)
            .await
            .unwrap();

        let got = svc.read_file(&id).await.unwrap();
        assert_eq!(got, payload);
    }

    /// When two on-disk files accidentally address the same
    /// `document_id` (shouldn't happen under the dedup invariant, but
    /// may happen via manual filesystem intervention), `read_file`
    /// MUST return an `ambiguous on-disk match` error rather than
    /// silently picking one and reading corrupted state.
    #[tokio::test]
    async fn read_returns_ambiguous_error_when_two_matches_exist() {
        let (dir, svc) = fresh_service().await;
        let payload = b"ambiguous";
        let id = RuntimeAttachmentService::compute_document_id(payload);

        // Pre-create the files directory; we drop blobs directly
        // without going through `upload_file` to fabricate the
        // ambiguous-on-disk state.
        tokio::fs::create_dir_all(dir.path().join("files"))
            .await
            .unwrap();

        // Two files for the same id: one in the legacy suffixed
        // shape, one in the new shape.
        tokio::fs::write(dir.path().join("files").join(format!("{id}.png")), payload)
            .await
            .unwrap();
        tokio::fs::write(
            dir.path().join("files").join(format!("duplicate_{id}.png")),
            payload,
        )
        .await
        .unwrap();

        let err = svc.read_file(&id).await.unwrap_err();
        match err {
            AttachmentError::Persistence(msg) => assert!(
                msg.contains("ambiguous on-disk match"),
                "expected ambiguity error, got {msg:?}"
            ),
            other => panic!("expected Persistence ambiguity error, got {other:?}"),
        }
    }

    /// Regression guard for the dedup invariant: uploading identical
    /// bytes twice (whether back-to-back or "after a restart" — same
    /// process is enough since the hash is content-only) MUST collapse
    /// to a single on-disk file and a single `document_id`. Before the
    /// SHA-256 rewrite this failed because the old implementation XOR'd
    /// `SystemTime::subsec_nanos()` into the suffix.
    #[tokio::test]
    async fn upload_then_reupload_same_bytes_dedups() {
        let (dir, svc) = fresh_service().await;
        let payload = b"identical-image-bytes-for-dedup-check";

        let first = svc.upload_file(params("png", payload)).await.unwrap();
        let second = svc.upload_file(params("png", payload)).await.unwrap();

        assert_eq!(
            first.document_id, second.document_id,
            "identical bytes must hash to the same document_id"
        );

        // And there must be exactly one file on disk matching that id
        // — i.e. the second upload was a true dedup hit, not a
        // co-located duplicate.
        let suffixed = dir
            .path()
            .join("files")
            .join(format!("sample_{}.png", first.document_id));
        assert!(suffixed.exists(), "suffixed blob must exist");
        let names: Vec<String> = {
            let mut entries = tokio::fs::read_dir(dir.path().join("files")).await.unwrap();
            let mut v = Vec::new();
            while let Some(e) = entries.next_entry().await.unwrap() {
                v.push(e.file_name().to_string_lossy().into_owned());
            }
            v
        };
        let matches = names
            .iter()
            .filter(|n| name_matches_document_id(n, &first.document_id))
            .count();
        assert_eq!(
            matches, 1,
            "exactly one on-disk file must carry this document_id (dir: {names:?})"
        );
    }

    /// Same content uploaded under two DIFFERENT original filenames
    /// MUST still dedup to one on-disk file. The first-uploaded name
    /// wins (the response still echoes whatever filename the current
    /// upload sent, independent of disk). This is the load-bearing
    /// test for the new dedup-by-id scan in `write_blob_atomic` —
    /// the previous `<id>.<ext>` scheme implicitly deduped via path
    /// equality; we now scan for any file addressing the id.
    #[tokio::test]
    async fn upload_same_bytes_different_filenames_dedups_to_one_blob() {
        let (dir, svc) = fresh_service().await;
        let payload = b"identical-bytes-two-names";

        let first = svc
            .upload_file(params_with_name("report.pdf", "pdf", payload))
            .await
            .unwrap();
        let second = svc
            .upload_file(params_with_name("annual-report-final.pdf", "pdf", payload))
            .await
            .unwrap();
        let third = svc
            .upload_file(params_with_name("年度报告.pdf", "pdf", payload))
            .await
            .unwrap();

        assert_eq!(first.document_id, second.document_id);
        assert_eq!(second.document_id, third.document_id);

        // The first-uploaded on-disk name should still be present and
        // be the only one — first-uploaded-wins.
        let first_path = dir
            .path()
            .join("files")
            .join(format!("report_{}.pdf", first.document_id));
        let second_path = dir
            .path()
            .join("files")
            .join(format!("annual-report-final_{}.pdf", first.document_id));
        let third_path = dir
            .path()
            .join("files")
            .join(format!("年度报告_{}.pdf", first.document_id));
        assert!(
            first_path.exists(),
            "first-uploaded blob must remain on disk"
        );
        assert!(
            !second_path.exists(),
            "second upload must NOT have created a second blob ({})",
            second_path.display()
        );
        assert!(
            !third_path.exists(),
            "third upload must NOT have created a third blob ({})",
            third_path.display()
        );

        // And exactly one match exists in the directory.
        let names: Vec<String> = {
            let mut entries = tokio::fs::read_dir(dir.path().join("files")).await.unwrap();
            let mut v = Vec::new();
            while let Some(e) = entries.next_entry().await.unwrap() {
                v.push(e.file_name().to_string_lossy().into_owned());
            }
            v
        };
        let count = names
            .iter()
            .filter(|n| name_matches_document_id(n, &first.document_id))
            .count();
        assert_eq!(
            count, 1,
            "exactly one on-disk blob for one document_id (dir: {names:?})"
        );
    }

    /// Distinct bytes that share a sanitised stem but different ids
    /// (because the bytes differ) MUST land as distinct files —
    /// dedup must not over-match on the stem portion.
    #[tokio::test]
    async fn upload_distinct_bytes_with_same_stem_keeps_separate_blobs() {
        let (dir, svc) = fresh_service().await;

        let r1 = svc
            .upload_file(params_with_name("report.pdf", "pdf", b"alpha"))
            .await
            .unwrap();
        let r2 = svc
            .upload_file(params_with_name("report.pdf", "pdf", b"beta"))
            .await
            .unwrap();

        assert_ne!(r1.document_id, r2.document_id);
        let p1 = dir
            .path()
            .join("files")
            .join(format!("report_{}.pdf", r1.document_id));
        let p2 = dir
            .path()
            .join("files")
            .join(format!("report_{}.pdf", r2.document_id));
        assert!(
            p1.exists() && p2.exists(),
            "distinct ids must land as distinct files"
        );
    }

    /// Regression guard at the pure-function level: identical bytes
    /// must always hash to the same id, even when called many times in
    /// a row (the old buggy implementation drifted each call because of
    /// the per-call `SystemTime::now()` read). Also asserts that
    /// distinct bytes almost certainly produce distinct ids — we use
    /// a tiny-but-different payload and rely on the 48-bit prefix
    /// making a collision astronomically unlikely.
    #[test]
    fn compute_document_id_is_content_deterministic() {
        let a = b"alpha-bytes";
        let b = b"beta-bytes";

        // Stability across repeated calls.
        let id_a_1 = RuntimeAttachmentService::compute_document_id(a);
        let id_a_2 = RuntimeAttachmentService::compute_document_id(a);
        let id_a_3 = RuntimeAttachmentService::compute_document_id(a);
        assert_eq!(id_a_1, id_a_2);
        assert_eq!(id_a_2, id_a_3);

        // Format: 12 hex + `-` + 4 hex (17 chars total), all lowercase.
        assert_eq!(id_a_1.len(), 17, "expected 17-char id, got {id_a_1:?}");
        let (prefix, suffix) = id_a_1.split_once('-').expect("id contains dash");
        assert_eq!(prefix.len(), 12);
        assert_eq!(suffix.len(), 4);
        assert!(
            prefix
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "prefix must be lowercase hex: {prefix:?}"
        );
        assert!(
            suffix
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "suffix must be lowercase hex: {suffix:?}"
        );

        // Distinct content → distinct id (collision odds ≈ 2^-48).
        let id_b = RuntimeAttachmentService::compute_document_id(b);
        assert_ne!(id_a_1, id_b, "distinct bytes must hash differently");

        // Validate against the charset guard.
        RuntimeAttachmentService::validate_document_id(&id_a_1).expect("computed id must validate");
        RuntimeAttachmentService::validate_document_id(&id_b).expect("computed id must validate");
    }

    /// Distinct bytes that happen to share the first 6 hash bytes
    /// (i.e. a 48-bit collision) would be indistinguishable by `read_file`
    /// under the existing prefix-based filename scheme. With SHA-256
    /// first-6-byte slicing the only way to hit this is for the 6 bytes
    /// to be byte-identical, which is a 1-in-2^48 event; we don't try
    /// to craft such an input here. This test documents the
    /// expectation with a much weaker sanity check: the helper must
    /// produce valid ids for an empty payload too (boundary case that
    /// previously fell into the `{:x}` zero-padding branch).
    #[test]
    fn compute_document_id_handles_empty_payload() {
        let id = RuntimeAttachmentService::compute_document_id(b"");
        assert_eq!(id.len(), 17);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        // The empty-string SHA-256 prefix is e3b0c442… — verify the
        // first 12 chars are exactly that, pinning the implementation
        // against accidental algorithm swaps.
        assert_eq!(
            &id[..12],
            "e3b0c44298fc",
            "empty-payload id must match SHA-256(\"\") prefix"
        );
    }

    /// `read_file` rejects malformed `document_id` values before any
    /// filesystem access (defang path-traversal probes).
    #[tokio::test]
    async fn read_rejects_path_traversal_probe() {
        let (_dir, svc) = fresh_service().await;
        for bad in ["../../../etc/passwd", "abc", ""] {
            let err = svc.read_file(bad).await.unwrap_err();
            assert!(
                matches!(err, AttachmentError::InvalidDocumentId(_)),
                "expected InvalidDocumentId for {bad:?}, got {err:?}"
            );
        }
    }
}
