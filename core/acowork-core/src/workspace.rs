//! Workspace layout — where an agent instance keeps its private state.
//!
//! ADR-082 D1: memory nodes, the conversation index and session meta all live
//! in a single SQLite file inside the workspace. Every component that has to
//! find (or copy) that state — the memory backend, the conversation index,
//! session meta, package cloning — must agree on the path, so it is defined
//! once here instead of being re-spelled as a literal in each crate.

use std::path::{Path, PathBuf};

/// Directory holding agent-private storage inside a workspace.
pub const MEMORY_DIR: &str = "memory";

/// Directory holding an instance's runtime workspace inside its install
/// directory: `{install_dir}/{instance_id}/{WORKSPACE_DIR}/`.
///
/// Everything the Runtime persists per instance — `config/`,
/// `conversations/`, `memory/`, `files/`, `logs/` — lives under here,
/// not directly under the install directory. Spelled once here because
/// clone, spawn and log-tail each need the same answer (the spawn path
/// passes this directory to the Runtime as its work dir).
pub const WORKSPACE_DIR: &str = "workspace";

/// The instance's workspace directory: `{install_dir}/{WORKSPACE_DIR}`.
pub fn workspace_dir(install_dir: impl AsRef<Path>) -> PathBuf {
    install_dir.as_ref().join(WORKSPACE_DIR)
}

/// SQLite file inside [`MEMORY_DIR`] holding memory nodes, the conversation
/// index and session meta (ADR-082).
pub const STORE_FILE: &str = "private.sqlite";

/// Sidecar files SQLite may leave next to an open database.
pub const STORE_SIDECAR_SUFFIXES: [&str; 2] = ["-wal", "-shm"];

/// Sentinel written beside a legacy store once the Node migration pass has
/// handed its memory rows to the memory service (ADR-101 decision 10).
///
/// The store file itself is NOT renamed: since ADR-082 the same
/// `private.sqlite` also carries the session meta and the conversation index
/// the Runtime still reads, so only the memory rows leave. The sentinel is what
/// stops every later Node start from re-uploading a store that was already
/// dealt with, and it records the outcome (see `acowork-node`'s
/// `memory_migration`) for the operator to read.
pub const MIGRATED_MARKER_FILE: &str = ".migrated";

/// Path of the migration sentinel: `{work_dir}/memory/.migrated`.
pub fn migrated_marker_path(work_dir: impl AsRef<Path>) -> PathBuf {
    work_dir.as_ref().join(MEMORY_DIR).join(MIGRATED_MARKER_FILE)
}

/// Ceiling on the legacy store a migration may hand over in one request.
///
/// Spelled once here because both halves of the transfer enforce it and must
/// not drift: the Node refuses to read a bigger file (and tells the operator to
/// copy it back by hand), and the memory service refuses a bigger body. It is
/// deliberately equal to the Gateway's global reverse-proxy body limit — a
/// larger value would be rejected by the Gateway before it ever reached the
/// service, so the service-side check would be unreachable code.
pub const STORE_IMPORT_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Path of the workspace store: `{work_dir}/memory/private.sqlite`.
pub fn store_path(work_dir: impl AsRef<Path>) -> PathBuf {
    work_dir.as_ref().join(MEMORY_DIR).join(STORE_FILE)
}

/// Every file that currently makes up the workspace store, in copy order: the
/// database itself followed by whichever sidecars exist.
///
/// Callers copying a store (e.g. full package clone) must copy all of them
/// while the source is closed; a `-wal` left behind is unreplayed WAL content.
pub fn store_files(work_dir: impl AsRef<Path>) -> Vec<PathBuf> {
    let db = store_path(work_dir);
    let mut files = vec![db.clone()];
    for suffix in STORE_SIDECAR_SUFFIXES {
        let sidecar = db.with_file_name(format!("{STORE_FILE}{suffix}"));
        if sidecar.exists() {
            files.push(sidecar);
        }
    }
    files
}

/// Workspace subdirectories that are machine-local or runtime artifacts
/// and must NEVER be cloned, even in [`CloneMode::Full`].
///
/// The default `Full` clone iterates every workspace subdir and copies it
/// — symmetric to how the package-files step uses
/// [`PACKAGE_ALWAYS_EXCLUDE_DIRS`] — so adding a new user-meaningful
/// subdir (e.g. `config/`) needs no clone code change. Skipping this list
/// keeps a future agent from accidentally cloning 50MB of log files or a
/// snapshot of this machine's `/tmp` uploads.
///
/// `files/` holds user-uploaded attachments in some deployments; those
/// are also machine-local — re-uploading is faster and saner than copying
/// a path that may not exist on the target host.
///
/// `conversations/` is the per-instance session history (JSONL + meta):
/// a clone is meant to start a fresh dialogue, not inherit the source's
/// past exchanges. Session meta + the conversation index that lives in
/// `memory/private.sqlite` are wiped in lock-step — see
/// `acowork_sqlite::SqliteStore::purge_conversation_state`.
///
/// [`CloneMode::Full`]: crate::CloneMode::Full
/// [`PACKAGE_ALWAYS_EXCLUDE_DIRS`]: crate::packaging::PACKAGE_ALWAYS_EXCLUDE_DIRS
pub const WORKSPACE_LOCAL_DIRS: &[&str] = &[
    "files",         // user-uploaded attachments — host-specific paths
    "logs",          // runtime log files — reset on the target
    "conversations", // per-instance session history — clone starts fresh
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_dir_is_install_dir_plus_workspace() {
        let install = Path::new("/tmp/agent");
        assert_eq!(workspace_dir(install), Path::new("/tmp/agent").join("workspace"));
        // The store lives INSIDE the workspace, never beside it.
        assert_eq!(
            store_path(workspace_dir(install)),
            Path::new("/tmp/agent/workspace/memory/private.sqlite")
        );
    }

    #[test]
    fn store_path_is_memory_dir_plus_store_file() {
        let path = store_path("/tmp/agent/workspace");
        assert_eq!(
            path,
            Path::new("/tmp/agent/workspace")
                .join(MEMORY_DIR)
                .join(STORE_FILE)
        );
        assert!(path.ends_with("memory/private.sqlite"));
    }

    #[test]
    fn migration_sentinel_sits_beside_the_store() {
        let marker = migrated_marker_path("/tmp/agent/workspace");
        assert!(marker.ends_with("memory/.migrated"));
        // Same directory as the store it describes — one `ls` shows both.
        assert_eq!(marker.parent(), store_path("/tmp/agent/workspace").parent());
    }

    #[test]
    fn store_files_includes_existing_sidecars_only() {
        let dir = std::env::temp_dir().join(format!("acowork-ws-layout-{}", std::process::id()));
        let memory_dir = dir.join(MEMORY_DIR);
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::write(memory_dir.join(STORE_FILE), b"db").unwrap();
        std::fs::write(memory_dir.join(format!("{STORE_FILE}-wal")), b"wal").unwrap();

        let files = store_files(&dir);
        assert_eq!(files.len(), 2, "db + -wal, -shm absent: {files:?}");
        assert!(files[0].ends_with(STORE_FILE));
        assert!(files[1].ends_with(format!("{STORE_FILE}-wal")));

        std::fs::remove_dir_all(&dir).ok();
    }
}

