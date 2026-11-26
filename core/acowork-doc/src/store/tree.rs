//! Filesystem walk + startup reconciliation (design §3.3).
//!
//! Split into two passes:
//! - `scan_all_docs` — list every `.md` file in the library root, returning
//!   paths relative to the root (e.g. `["项目A/PRD.md", "doc.md"]`).
//! - `reconcile_on_startup` — compare the disk view against every
//!   `library.json` and heal drift:
//!
//! | disk vs `files[]`                  | action                        |
//! |-----------------------------------|-------------------------------|
//! | disk only                        | synthesize entry (version=1)  |
//! | `files[]` only (disk missing)    | mark `deleted = true`         |
//! | both, name matches               | leave alone                   |
//! | both, name differs               | trust filename (per §3.3)     |

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::{DocError, Result};
use crate::store::library::LibraryStore;
use crate::types::{generate_doc_id, DocMeta, LibraryIndex, ROOT_DIR_ID};

/// Walk `root` and return relative paths of every regular `.md` file,
/// skipping hidden directories (`.trash`, `.requests`, …).
///
/// Imported files (pdf / docx / images) are deliberately excluded: they have
/// no Markdown body, and `reconcile_on_startup` matches them by full file
/// name via [`scan_all_files`].
pub async fn scan_all_docs(root: &Path) -> Result<Vec<String>> {
    Ok(scan_all_files(root)
        .await?
        .into_iter()
        .filter(|rel| rel.ends_with(".md"))
        .collect())
}

/// Walk `root` and return relative paths of **every** regular file outside
/// hidden directories — Markdown documents and imported files alike.
pub async fn scan_all_files(root: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries = fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let ft = entry.file_type().await?;
            let name = entry.file_name();
            let name_lossy = name.to_string_lossy();
            if ft.is_dir() {
                // Skip hidden system directories + the root itself (we re-enter via stack).
                if name_lossy.starts_with('.') {
                    continue;
                }
                stack.push(entry.path());
                continue;
            }
            if ft.is_file() {
                let rel = entry
                    .path()
                    .strip_prefix(root)
                    .map_err(|e| DocError::CorruptIndex(format!("strip_prefix: {e}")))?
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push(rel);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Summary of one reconciliation pass (used by tests + structured logs).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReconcileReport {
    /// `files[]` entries that gained a synthetic doc on disk (new doc, no entry yet).
    pub added: Vec<String>,
    /// `files[]` entries that had `deleted` flipped to `true` (disk missing).
    pub orphaned: Vec<String>,
    /// Entries whose on-disk filename no longer matched `name` — name rewritten to filename.
    pub renamed: Vec<String>,
}

/// Reconcile every `library.json` against the disk view, fixing drift in-place.
///
/// Policy (design §3.3 "filename is authoritative"):
/// - On-disk `.md` file without a matching `files[]` entry → add (new doc).
/// - `files[]` entry without a matching `.md` file → mark `deleted = true`.
/// - Both present but `files[].name != disk filename (sans .md)` → rewrite `name`.
pub async fn reconcile_on_startup(store: &LibraryStore) -> Result<ReconcileReport> {
    let mut report = ReconcileReport::default();
    let on_disk = scan_all_files(store.root()).await?;
    // index by (dir_id, filename-without-.md) — Markdown documents only.
    let mut disk_index: std::collections::HashMap<(String, String), String> =
        std::collections::HashMap::new();
    // Imported files are keyed by their FULL file name (they keep their own
    // extension), and only importable ones participate.
    let mut disk_files: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    let mut disk_importables: std::collections::HashMap<(String, String), (String, String)> =
        std::collections::HashMap::new();
    for rel in &on_disk {
        let (dir_part, file_part) = split_relative(rel);
        let dir_id = match dir_part {
            Some(p) => p.to_string(),
            None => ROOT_DIR_ID.to_string(),
        };
        if let Some(stem) = file_part.strip_suffix(".md") {
            disk_index.insert((dir_id.clone(), stem.to_string()), rel.clone());
            continue;
        }
        disk_files.insert((dir_id.clone(), file_part.to_string()));
        let Some((stem, ext)) = file_part.rsplit_once('.') else {
            continue;
        };
        if crate::types::import_kind(ext).is_some() {
            disk_importables.insert(
                (dir_id, stem.to_string()),
                (file_part.to_string(), ext.to_ascii_lowercase()),
            );
        }
    }

    let all = store.list_all_indexes().await?;
    for (_path, mut idx) in all {
        let dir_id = idx.dir_id.clone();
        let mut changed = false;
        let original_len = idx.files.len();

        // Helper: stems on disk within this dir that are not yet matched by any live entry.
        let unmatched_stems = |idx: &LibraryIndex| -> Vec<String> {
            disk_index
                .keys()
                .filter(|(d, stem)| {
                    *d == dir_id
                        && !idx
                            .files
                            .iter()
                            .any(|g| !g.deleted && &g.name == stem)
                })
                .map(|(_, stem)| stem.clone())
                .collect()
        };

        // Pass 1: rename — for each entry whose name has no disk match, claim
        // one unmatched stem (filename wins, design §3.3). Only claim when
        // the count matches (1-to-1 rename) — multi-entry collision is left
        // for manual recovery and reports as `orphaned` instead.
        let unmatched = unmatched_stems(&idx);
        if unmatched.len() == 1 {
            let target = unmatched[0].clone();
            let mut renamed_one = false;
            for f in idx.files.iter_mut() {
                if renamed_one || f.deleted {
                    continue;
                }
                if f.is_file() {
                    continue;
                }
                let key = (dir_id.clone(), f.name.clone());
                if !disk_index.contains_key(&key) {
                    report.renamed.push(f.doc_id.clone());
                    f.name = target.clone();
                    f.updated_at = chrono::Utc::now();
                    changed = true;
                    renamed_one = true;
                }
            }
        }

        // Pass 2: orphan — entries still without a disk match (and not renamed above).
        for f in idx.files.iter_mut() {
            if f.deleted {
                continue;
            }
            let alive = if f.is_file() {
                disk_files.contains(&(dir_id.clone(), f.file_name()))
            } else {
                disk_index.contains_key(&(dir_id.clone(), f.name.clone()))
            };
            if !alive {
                report.orphaned.push(f.doc_id.clone());
                f.deleted = true;
                f.updated_at = chrono::Utc::now();
                changed = true;
            }
        }

        // Pass 3: add — disk files without any live entry get synthesised.
        let unmatched_after = unmatched_stems(&idx);
        for stem in unmatched_after {
            let now = chrono::Utc::now();
            let new_meta = DocMeta::new(generate_doc_id(), stem.clone(), None, now);
            report.added.push(new_meta.doc_id.clone());
            idx.files.push(new_meta);
            changed = true;
        }
        // Imported files dropped into the library folder by hand get an
        // entry too — a file the tree cannot show is a file the user thinks
        // is lost.
        for ((d, stem), (_, ext)) in disk_importables.iter() {
            if *d != dir_id {
                continue;
            }
            if idx.files.iter().any(|g| !g.deleted && &g.name == stem) {
                continue;
            }
            let now = chrono::Utc::now();
            let mut new_meta = DocMeta::new(generate_doc_id(), stem.clone(), None, now);
            new_meta.ext = Some(ext.clone());
            report.added.push(new_meta.doc_id.clone());
            idx.files.push(new_meta);
            changed = true;
        }

        if changed || idx.files.len() != original_len {
            store.save(&idx).await?;
        }
        let _ = (dir_id, original_len);
    }
    Ok(report)
}

/// Split "项目A/PRD.md" or "PRD.md" into (dir_path, filename).
fn split_relative(rel: &str) -> (Option<&str>, &str) {
    match rel.rsplit_once('/') {
        Some((dir, file)) => (Some(dir), file),
        None => (None, rel),
    }
}

#[allow(dead_code)]
fn _unused_silence() -> LibraryIndex {
    LibraryIndex::root()
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn new_store() -> (TempDir, LibraryStore) {
        let tmp = TempDir::new().unwrap();
        let store = LibraryStore::new(tmp.path().to_path_buf());
        (tmp, store)
    }

    #[tokio::test]
    async fn scan_all_docs_finds_md_files_one_level() {
        let (_tmp, store) = new_store();
        store.ensure_root().await.unwrap();
        fs::write(store.root().join("a.md"), "x").await.unwrap();
        fs::write(store.root().join("b.md"), "y").await.unwrap();
        fs::write(store.root().join("c.txt"), "not md").await.unwrap();
        let docs = scan_all_docs(store.root()).await.unwrap();
        assert_eq!(docs.len(), 2);
        assert!(docs.contains(&"a.md".to_string()));
        assert!(docs.contains(&"b.md".to_string()));
    }

    #[tokio::test]
    async fn scan_all_docs_skips_dot_dirs() {
        let (_tmp, store) = new_store();
        store.ensure_root().await.unwrap();
        fs::create_dir(store.root().join(".trash")).await.unwrap();
        fs::write(store.root().join(".trash").join("old.md"), "x")
            .await
            .unwrap();
        fs::write(store.root().join("live.md"), "y").await.unwrap();
        let docs = scan_all_docs(store.root()).await.unwrap();
        assert_eq!(docs, vec!["live.md".to_string()]);
    }

    #[tokio::test]
    async fn scan_all_docs_walks_into_subdirs() {
        let (_tmp, store) = new_store();
        store.ensure_root().await.unwrap();
        let sub = store.root().join("dir-abcdef012345");
        fs::create_dir(&sub).await.unwrap();
        fs::write(sub.join("nested.md"), "x").await.unwrap();
        let docs = scan_all_docs(store.root()).await.unwrap();
        assert_eq!(docs, vec!["dir-abcdef012345/nested.md".to_string()]);
    }

    #[tokio::test]
    async fn reconcile_adds_missing_disk_files() {
        let (_tmp, store) = new_store();
        store.ensure_root().await.unwrap();
        fs::write(store.root().join("ghost.md"), "x").await.unwrap();
        let report = reconcile_on_startup(&store).await.unwrap();
        assert_eq!(report.added.len(), 1);
        assert_eq!(report.orphaned.len(), 0);
        let idx = store.load(ROOT_DIR_ID).await.unwrap();
        assert!(idx.files.iter().any(|f| f.name == "ghost"));
    }

    #[tokio::test]
    async fn reconcile_marks_missing_disk_files_orphaned() {
        let (_tmp, store) = new_store();
        store.ensure_root().await.unwrap();
        let mut idx = LibraryIndex::root();
        idx.files.push(DocMeta::new(
            "doc-abcdef012345".into(),
            "missing".into(),
            None,
            chrono::Utc::now(),
        ));
        store.save(&idx).await.unwrap();
        let report = reconcile_on_startup(&store).await.unwrap();
        assert_eq!(report.added.len(), 0);
        assert_eq!(report.orphaned.len(), 1);
        let after = store.load(ROOT_DIR_ID).await.unwrap();
        assert!(after.files[0].deleted);
    }

    #[tokio::test]
    async fn reconcile_renames_entry_to_match_filename() {
        let (_tmp, store) = new_store();
        store.ensure_root().await.unwrap();
        fs::write(store.root().join("real.md"), "x").await.unwrap();
        let mut idx = LibraryIndex::root();
        idx.files.push(DocMeta::new(
            "doc-abcdef012345".into(),
            "fake".into(),
            None,
            chrono::Utc::now(),
        ));
        store.save(&idx).await.unwrap();
        let report = reconcile_on_startup(&store).await.unwrap();
        assert_eq!(report.renamed.len(), 1);
        let after = store.load(ROOT_DIR_ID).await.unwrap();
        assert_eq!(after.files[0].name, "real");
        assert!(!after.files[0].deleted);
    }

    #[tokio::test]
    async fn reconcile_no_change_returns_empty_report() {
        let (_tmp, store) = new_store();
        store.ensure_root().await.unwrap();
        fs::write(store.root().join("a.md"), "x").await.unwrap();
        let mut idx = LibraryIndex::root();
        idx.files.push(DocMeta::new(
            "doc-abcdef012345".into(),
            "a".into(),
            None,
            chrono::Utc::now(),
        ));
        store.save(&idx).await.unwrap();
        let report = reconcile_on_startup(&store).await.unwrap();
        assert_eq!(report, ReconcileReport::default());
    }

    // ── imported files ────────────────────────────────────────────

    #[tokio::test]
    async fn reconcile_keeps_live_imported_files_and_orphans_missing_ones() {
        let (_tmp, store) = new_store();
        store.ensure_root().await.unwrap();
        fs::write(store.root().join("spec.pdf"), b"%PDF").await.unwrap();
        let mut pdf = DocMeta::new("doc-aaaaaaaaaaaa".into(), "spec".into(), None, chrono::Utc::now());
        pdf.ext = Some("pdf".into());
        let mut gone = DocMeta::new("doc-bbbbbbbbbbbb".into(), "gone".into(), None, chrono::Utc::now());
        gone.ext = Some("xlsx".into());
        let mut idx = LibraryIndex::root();
        idx.files.push(pdf);
        idx.files.push(gone);
        store.save(&idx).await.unwrap();

        let report = reconcile_on_startup(&store).await.unwrap();
        assert_eq!(report.added.len(), 0, "the pdf is already indexed: {report:?}");
        assert_eq!(report.orphaned, vec!["doc-bbbbbbbbbbbb".to_string()]);
        let after = store.load(ROOT_DIR_ID).await.unwrap();
        let live = after.files.iter().find(|f| f.name == "spec").unwrap();
        assert!(!live.deleted);
        assert_eq!(live.ext.as_deref(), Some("pdf"));
    }

    #[tokio::test]
    async fn reconcile_synthesises_entries_for_stray_importable_files() {
        let (_tmp, store) = new_store();
        store.ensure_root().await.unwrap();
        fs::write(store.root().join("dropped.png"), b"img").await.unwrap();
        // Not an accepted type: stays out of the tree (no phantom entries).
        fs::write(store.root().join("notes.bak"), b"x").await.unwrap();
        let report = reconcile_on_startup(&store).await.unwrap();
        assert_eq!(report.added.len(), 1);
        let idx = store.load(ROOT_DIR_ID).await.unwrap();
        assert_eq!(idx.files.len(), 1);
        assert_eq!(idx.files[0].name, "dropped");
        assert_eq!(idx.files[0].ext.as_deref(), Some("png"));
    }
}