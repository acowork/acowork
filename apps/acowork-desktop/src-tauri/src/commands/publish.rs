//! Publish commands — prepare, build, export, and the Local-library write-back.
//!
//! The write-back (`save_publish_to_library`) is the only place where the
//! Desktop touches an artifact the Node Agent produced. It does **not** change
//! how the node writes: `build_package` keeps writing
//! `{agent_id}-{version}.agent` into the node's own `packages_dir`
//! (`core/acowork-node/src/package/publish.rs`), the Gateway keeps sending an
//! empty `output_dir` (`core/acowork-gateway/src/http/publish_api.rs`), and the
//! Desktop merely copies that finished file into its own library directory
//! (PRD §14 D12 / REQ-029, plan §3).
//!
//! Scope limit, deliberately encoded below (D12 / B5): the returned
//! `output_path` is a **node-local** path, so whether the write-back can happen
//! is a **filesystem fact**, not a configuration flag: if this Desktop can read
//! the artifact under the node `packages_dir`, it copies it; if it cannot, the
//! artifact is on another machine and the wizard reports which node holds it
//! and copies nothing. The deployment mode only chooses how an unreadable
//! artifact is explained — a `local` Gateway that cannot see its own node's file
//! is a real failure worth retrying, while `remote`/`relay` is the documented
//! limitation. Pulling the file over the network is an explicit non-goal
//! (PRD §14.6), as is teaching the node to write into a Desktop-owned directory.
//!
//! Why not `mode == Local` as the gate: a Desktop can legitimately be
//! configured `remote` with a URL on this very machine (the dev setup, and any
//! user who types their own LAN IP), and then the artifact *is* here and must
//! still reach the Local tab. The mode is a description of intent; reachability
//! is the fact.

use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, State};

use crate::commands::agent_library::{copy_into_library, has_package_extension};
use crate::gateway_client::{BuildPublishResponse, ExportPackageResponse, GatewayClient,
    PreparePublishResponse};
use crate::state::{AppState, GatewayMode};

/// Run publish prepare (checks + optional cleanup)
#[tauri::command]
pub async fn prepare_publish(
    state: State<'_, AppState>,
    agent_id: String,
    clean: Option<bool>,
) -> Result<PreparePublishResponse, String> {
    let client = state.gateway.read().await;
    client
        .prepare_publish(&agent_id, clean.unwrap_or(false))
        .await
        .map_err(|e| format!("Failed to prepare publish: {}", e))
}

/// Build a .agent package
#[tauri::command]
pub async fn build_publish(
    state: State<'_, AppState>,
    agent_id: String,
    sign: Option<bool>,
    key_dir: Option<String>,
) -> Result<BuildPublishResponse, String> {
    let client = state.gateway.read().await;
    client
        .build_publish(&agent_id, sign.unwrap_or(false), key_dir.as_deref())
        .await
        .map_err(|e| format!("Failed to build package: {}", e))
}

/// Export the built .agent file path
#[tauri::command]
pub async fn export_package(
    state: State<'_, AppState>,
    agent_id: String,
) -> Result<ExportPackageResponse, String> {
    let client = state.gateway.read().await;
    client
        .export_package(&agent_id)
        .await
        .map_err(|e| format!("Failed to export package: {}", e))
}

// ── Library write-back (REQ-029 / plan §3 T2) ──────────────────────────────

/// What the write-back actually did — the two legal outcomes of REQ-029.
///
/// `remote_skipped` is **not** an error: D12 scopes the feature to a
/// single-machine deployment, so in remote/relay mode "the artifact stayed on
/// node X" is the correct, successful answer (B5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveToLibraryStatus {
    Saved,
    RemoteSkipped,
}

/// Result of [`save_publish_to_library`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SaveToLibraryResult {
    pub status: SaveToLibraryStatus,
    /// Absolute path inside the library — `None` for `remote_skipped`
    /// (nothing was copied).
    pub path: Option<String>,
    /// The artifact's file name (`{agent_id}-{version}.agent`); known in both
    /// outcomes because the node's output path carries it.
    pub file_name: String,
}

impl SaveToLibraryResult {
    fn remote_skipped(package_path: &str) -> Self {
        Self {
            status: SaveToLibraryStatus::RemoteSkipped,
            path: None,
            file_name: file_name_of(Path::new(package_path)),
        }
    }
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

/// Save the artifact the publish build just produced into the Local library.
///
/// Three steps, in this order (the order is the security property):
/// 1. **Look the artifact up on this machine** (see [`plan_save`]): reachable →
///    it must pass the guard; unreachable → `remote_skipped` (B5) or, when the
///    Desktop believes the Gateway is local, a retryable error.
/// 2. **Guard** the path: it must be a direct `.agent` file inside the node's
///    `packages_dir` — the frontend hands this value straight back from an IPC
///    response, so it must not become an arbitrary-file-copy primitive. A
///    reachable file that fails the guard is always an error, never a silent
///    skip.
/// 3. **Copy** through `agent_library::copy_into_library`, which overwrites a
///    same-named file (B2: republishing one version replaces that row; B3: a
///    new version is a new row — nothing is de-duplicated by `agent_id`).
#[tauri::command]
pub async fn save_publish_to_library(
    state: State<'_, AppState>,
    app: AppHandle,
    package_path: String,
) -> Result<SaveToLibraryResult, String> {
    let mode = *state.gateway_mode.read().await;
    // Always resolved: a `remote`-configured Desktop may still be talking to a
    // Gateway on this very machine, in which case the artifact is right here and
    // the guard has to be checked against the real root.
    let packages_dir = {
        let client = state.gateway.read().await;
        resolve_packages_dir(&client).await
    };

    let plan = plan_save(mode, &package_path, &packages_dir)?;
    let SavePlan::Copy(artifact) = plan else {
        tracing::info!(
            "[PUBLISH] write-back skipped for {}: not readable on this machine (mode {}, node packages dir {})",
            package_path,
            mode.as_str(),
            packages_dir.display()
        );
        return Ok(SaveToLibraryResult::remote_skipped(&package_path));
    };

    let target = copy_into_library(&app, &artifact)?;
    tracing::info!(
        "[PUBLISH] copied {} into the local library as {}",
        artifact.display(),
        target.display()
    );
    Ok(SaveToLibraryResult {
        status: SaveToLibraryStatus::Saved,
        path: Some(target.to_string_lossy().to_string()),
        file_name: file_name_of(&target),
    })
}

/// The decision the command makes, as data — no `AppHandle`, no async, so the
/// acceptance-critical rules are unit-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SavePlan {
    /// The artifact is readable on this machine and passed the guard;
    /// canonicalized absolute path to copy.
    Copy(PathBuf),
    /// The artifact is **not on this filesystem** — the node that built it is
    /// another machine (D12 / B5). Nothing is copied and nothing failed.
    RemoteSkip,
}

/// How far the lookup of the artifact got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Lookup {
    /// Readable here, canonicalized, and inside the node package dir.
    Local(PathBuf),
    /// Readable here, but it violates the guard (wrong directory, not a file,
    /// not a package). Always an error: a reachable file is never "somewhere
    /// else", so it must not be reported as a remote skip.
    Rejected(String),
    /// Not readable here: the file — or the node's package dir — is not on this
    /// filesystem. That is the only honest signal we have for "the artifact is
    /// on another machine", and it cannot be faked by the caller.
    Unreachable(String),
}

fn canonical_or_none(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// Locate `candidate` relative to `packages_dir`, distinguishing "not readable
/// here" from "here, but wrong".
///
/// Canonicalizing both sides first means `..` segments, symlinks/junctions and
/// Windows' verbatim prefix cannot disguise an escape — a path that merely
/// looks contained after string concatenation does not pass.
pub(crate) fn look_up_artifact(candidate: &Path, packages_dir: &Path) -> Lookup {
    let Some(root) = canonical_or_none(packages_dir) else {
        return Lookup::Unreachable(format!(
            "The node package directory {} is not readable on this machine",
            packages_dir.display()
        ));
    };
    let Some(path) = canonical_or_none(candidate) else {
        return Lookup::Unreachable(format!(
            "The published package {} is not readable on this machine",
            candidate.display()
        ));
    };
    match check_artifact(&path, &root) {
        Ok(()) => Lookup::Local(path),
        Err(e) => Lookup::Rejected(e),
    }
}

/// Decide what to do with `package_path`.
///
/// The copy is gated on **reachability**, not on the mode flag: a Desktop
/// configured `remote` may still point at a Gateway on this machine (dev runs,
/// or a user-entered localhost/LAN URL), and then the artifact is right here and
/// belongs in the Local tab. Reachability cannot be faked by the caller — it is
/// a `canonicalize` of the path the Gateway reported — while the mode only
/// decides *how to explain* an unreachable artifact:
///
///   - `local` Gateway that cannot see its own node's file → a real failure the
///     user can retry, never a silent skip;
///   - `remote`/`relay` → the documented D12 limitation (B5).
pub(crate) fn plan_save(
    mode: GatewayMode,
    package_path: &str,
    packages_dir: &Path,
) -> Result<SavePlan, String> {
    match look_up_artifact(Path::new(package_path), packages_dir) {
        Lookup::Local(path) => Ok(SavePlan::Copy(path)),
        Lookup::Rejected(why) => Err(why),
        Lookup::Unreachable(why) => match mode {
            GatewayMode::Local => Err(why),
            GatewayMode::Remote | GatewayMode::Relay => Ok(SavePlan::RemoteSkip),
        },
    }
}

/// Guard on already-canonicalized paths: `path` must be an `.agent` file
/// **directly inside** `root`.
///
/// "Direct child" (rather than any descendant) mirrors what the node actually
/// writes (`output_dir.join(filename)`) and keeps the accepted set as small as
/// the real one.
pub(crate) fn check_artifact(path: &Path, root: &Path) -> Result<(), String> {
    if !path.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    if path.parent() != Some(root) {
        return Err(format!(
            "Refusing to copy {}: only files directly inside the node package directory {} are accepted",
            path.display(),
            root.display()
        ));
    }
    if !has_package_extension(path) {
        return Err(format!(
            "Refusing to copy {}: not an .agent package",
            path.display()
        ));
    }
    Ok(())
}

/// The node's `packages_dir` — where `build_package` wrote the artifact,
/// because the Gateway sends an empty `output_dir`.
///
/// The Gateway is asked first (`GET /api/config`): that is the same value it
/// reads back in `publish/export`, so a Gateway started with an explicit
/// `--packages-dir` keeps the guard aligned with the build. When the call
/// fails we fall back to the documented default (`{node_home}/packages`): in
/// that (already degraded) case the guard can only produce a truthful
/// "not readable" error, never a copy of the wrong file.
async fn resolve_packages_dir(client: &GatewayClient) -> PathBuf {
    match client.get_config().await {
        Ok(config) if !config.packages_dir.trim().is_empty() => PathBuf::from(config.packages_dir),
        Ok(_) => default_packages_dir(),
        Err(e) => {
            tracing::warn!(
                "[PUBLISH] GET /api/config failed ({e}); assuming the default node package dir"
            );
            default_packages_dir()
        }
    }
}

/// `{node_home}/packages` — the Gateway's own default for `packages_dir`
/// (`acowork-gateway/src/config.rs`), shared through `acowork-core` so the
/// Desktop and both node flavours derive one path.
fn default_packages_dir() -> PathBuf {
    acowork_core::node::default_node_home().join("packages")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique scratch directory under the OS temp dir (the desktop crate has
    /// no `tempfile` dependency — same pattern as `agent_library`'s tests).
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!("acowork-publish-{name}-{nanos}"));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn child(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        fn write(&self, name: &str) -> PathBuf {
            let path = self.child(name);
            std::fs::write(&path, b"PK\x03\x04").unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Convenience for the guard tests: the rejection message, or a panic when
    /// the guard unexpectedly accepted the path.
    fn rejection(path: &Path, root: &Path) -> String {
        match look_up_artifact(path, root) {
            Lookup::Rejected(why) => why,
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    /// B1: the file the node just wrote, as the Gateway reports it, is accepted.
    #[test]
    fn a_built_package_in_the_node_dir_is_accepted() {
        let packages = Scratch::new("packages");
        let built = packages.write("com.example.demo-1.0.0.agent");

        match look_up_artifact(&built, packages.path()) {
            Lookup::Local(accepted) => {
                assert_eq!(accepted, std::fs::canonicalize(&built).unwrap())
            }
            other => panic!("expected the artifact to be accepted, got {other:?}"),
        }
    }

    /// The guard exists so the IPC argument cannot become "copy any file".
    /// Each case is a distinct escape attempt.
    #[test]
    fn paths_outside_the_node_package_dir_are_refused() {
        let scratch = Scratch::new("pkg-root");
        let packages = scratch.child("packages");
        std::fs::create_dir_all(&packages).unwrap();
        let elsewhere = Scratch::new("elsewhere");
        let outside = elsewhere.write("com.example.demo-1.0.0.agent");

        let err = rejection(&outside, &packages);
        assert!(err.contains("only files directly inside"), "{err}");

        // `..` traversal: canonicalization resolves it before the comparison,
        // so a path that is textually "inside" cannot pass.
        let sibling = scratch.write("com.example.demo-1.0.0.agent");
        let sneaky = packages.join("..").join(
            sibling.file_name().expect("file name"),
        );
        let err = rejection(&sneaky, &packages);
        assert!(err.contains("only files directly inside"), "{err}");

        // A nested file is not a build artifact (`output_dir.join(name)`).
        std::fs::create_dir_all(packages.join("nested")).unwrap();
        let nested = packages.join("nested").join("com.example.demo-1.0.0.agent");
        std::fs::write(&nested, b"PK\x03\x04").unwrap();
        let err = rejection(&nested, &packages);
        assert!(err.contains("only files directly inside"), "{err}");

        // The directory itself is not a package.
        let err = rejection(&packages, &packages);
        assert!(err.contains("is not a file"), "{err}");
    }

    #[test]
    fn non_agent_files_are_refused() {
        let packages = Scratch::new("ext");
        let other = packages.write("notes.txt");

        let err = rejection(&other, packages.path());
        assert!(err.contains("not an .agent package"), "{err}");
    }

    #[test]
    fn a_missing_artifact_reports_the_path_instead_of_panicking() {
        let packages = Scratch::new("missing");
        let gone = packages.child("com.example.demo-1.0.0.agent");

        // Unreadable here is the *remote* signal, not a guard rejection: the
        // panel must be able to tell the two apart.
        match look_up_artifact(&gone, packages.path()) {
            Lookup::Unreachable(why) => {
                assert!(why.contains("is not readable on this machine"), "{why}")
            }
            other => panic!("expected an unreachable verdict, got {other:?}"),
        }
    }

    /// B5 / D12: remote and relay modes answer "skipped" without touching the
    /// filesystem. The bogus roots make the assertion meaningful: if the
    /// function tried to resolve or read anything it would fail instead.
    #[test]
    fn remote_and_relay_modes_skip_without_copying() {
        let unreachable_root = Path::new("Z:\\acowork-does-not-exist\\packages");
        for mode in [GatewayMode::Remote, GatewayMode::Relay] {
            let plan = plan_save(
                mode,
                "Z:\\acowork-does-not-exist\\packages\\com.example.demo-1.0.0.agent",
                unreachable_root,
            )
            .unwrap();
            assert_eq!(plan, SavePlan::RemoteSkip, "{mode:?}");
        }
    }

    /// Regression (dev-run bug): a Desktop configured `remote` that points at a
    /// Gateway on *this* machine must still copy. The mode flag describes the
    /// deployment, not the filesystem — gating the copy on it silently dropped
    /// every artifact produced by a localhost Gateway (B1 failed in `tauri dev`).
    #[test]
    fn remote_mode_with_a_reachable_artifact_still_copies() {
        let packages = Scratch::new("remote-but-local");
        let built = packages.write("com.example.demo-1.0.0.agent");

        for mode in [GatewayMode::Remote, GatewayMode::Relay] {
            let plan = plan_save(mode, &built.to_string_lossy(), packages.path()).unwrap();
            assert_eq!(
                plan,
                SavePlan::Copy(std::fs::canonicalize(&built).unwrap()),
                "{mode:?}"
            );
        }
    }

    /// Local mode does not degrade quietly: an unreadable artifact is an error
    /// the wizard offers to retry, not a silent skip.
    #[test]
    fn local_mode_with_an_unreadable_artifact_is_an_error() {
        let packages = Scratch::new("local-missing");
        let gone = packages.child("com.example.demo-1.0.0.agent");

        let err = plan_save(GatewayMode::Local, &gone.to_string_lossy(), packages.path())
            .unwrap_err();
        assert!(err.contains("is not readable on this machine"), "{err}");
    }

    /// B2/B3 at the data level: the copy is keyed by **file name only**, so the
    /// second publish of one version replaces it while a new version lands
    /// beside it — no `agent_id` de-duplication anywhere.
    #[test]
    fn plan_keeps_versions_side_by_side_and_overwrites_the_same_name() {
        let packages = Scratch::new("versions");
        let v1 = packages.write("com.example.demo-1.0.0.agent");
        let v1_again = packages.write("com.example.demo-1.0.0.agent");
        let v2 = packages.write("com.example.demo-1.1.0.agent");

        let one = match plan_save(GatewayMode::Local, &v1.to_string_lossy(), packages.path())
            .unwrap()
        {
            SavePlan::Copy(path) => path,
            SavePlan::RemoteSkip => panic!("local mode must copy"),
        };
        let again = match plan_save(
            GatewayMode::Local,
            &v1_again.to_string_lossy(),
            packages.path(),
        )
        .unwrap()
        {
            SavePlan::Copy(path) => path,
            SavePlan::RemoteSkip => panic!("local mode must copy"),
        };
        let two = match plan_save(GatewayMode::Local, &v2.to_string_lossy(), packages.path())
            .unwrap()
        {
            SavePlan::Copy(path) => path,
            SavePlan::RemoteSkip => panic!("local mode must copy"),
        };

        assert_eq!(one, again, "the same version resolves to the same target");
        assert_ne!(one, two, "a new version is a new target");
        assert_eq!(
            one.file_name().map(|n| n.to_string_lossy().to_string()),
            Some("com.example.demo-1.0.0.agent".to_string())
        );
    }

    /// The node's file name convention is what B1 asserts on disk; a rename in
    /// `build_package` would otherwise surface only as a missing library row.
    #[test]
    fn result_reports_the_file_name_for_both_outcomes() {
        let saved = SaveToLibraryResult {
            status: SaveToLibraryStatus::Saved,
            path: Some("/lib/com.example.demo-1.0.0.agent".to_string()),
            file_name: "com.example.demo-1.0.0.agent".to_string(),
        };
        assert_eq!(saved.file_name, "com.example.demo-1.0.0.agent");

        let skipped = SaveToLibraryResult::remote_skipped(
            "/home/u/.acowork/acowork-node/packages/com.example.demo-1.0.0.agent",
        );
        assert_eq!(skipped.status, SaveToLibraryStatus::RemoteSkipped);
        assert!(skipped.path.is_none());
        assert_eq!(skipped.file_name, "com.example.demo-1.0.0.agent");

        // Windows path form is what a remote node may report.
        let skipped = SaveToLibraryResult::remote_skipped(
            "C:\\Users\\u\\.acowork\\acowork-node\\packages\\com.example.demo-1.0.0.agent",
        );
        assert_eq!(skipped.file_name, "com.example.demo-1.0.0.agent");
    }

    // ── B1–B4: the real pipeline on disk ──────────────────────────────────
    //
    // The `AppHandle`-bound command is glue over exactly these three pieces
    // (`validate_publish_artifact` → `copy_into` → `list_rows`), so driving them
    // here proves the acceptance criteria without a Tauri app: the artifact the
    // node writes is what lands in the library, one version replaces itself,
    // and a second version sits beside it as its own row.

    /// Write a real (minimal) `.agent` — ZIP + `manifest.toml`, the same wire
    /// shape `acowork-sign::packager` produces.
    fn write_package(path: &Path, version: &str) {
        use std::io::Write as _;
        let manifest = format!(
            r#"
agent_id = "com.example.demo"
version = "{version}"
name = "Demo"
description = "A demo agent"
author = "acowork"
runtime_version = "0.1.0"
"#
        );
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("manifest.toml", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(manifest.as_bytes()).unwrap();
        zip.finish().unwrap();
    }

    /// Run the command's plan + copy steps for one artifact, returning the
    /// library path. Mirrors `save_publish_to_library` minus the `AppHandle`.
    fn save_once(packages: &Path, library: &Path, file_name: &str) -> PathBuf {
        let source = packages.join(file_name);
        let plan = plan_save(GatewayMode::Local, &source.to_string_lossy(), packages).unwrap();
        match plan {
            SavePlan::Copy(artifact) => {
                crate::commands::agent_library::copy_into(library, &artifact).unwrap()
            }
            SavePlan::RemoteSkip => panic!("local mode must copy"),
        }
    }

    fn library_file_names(library: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(library)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names
    }

    /// B1 + B2 + B3: one version → one row; republishing it overwrites; a new
    /// version is a second row (no `agent_id` de-duplication anywhere).
    #[test]
    fn republishing_overwrites_and_a_new_version_stays_side_by_side() {
        let packages = Scratch::new("flow-packages");
        let library = Scratch::new("flow-library");

        // B1: the name is what `build_package` produced.
        write_package(&packages.child("com.example.demo-1.0.0.agent"), "1.0.0");
        let saved = save_once(packages.path(), library.path(), "com.example.demo-1.0.0.agent");
        assert_eq!(
            saved.file_name().unwrap().to_string_lossy(),
            "com.example.demo-1.0.0.agent"
        );
        assert_eq!(saved, library.child("com.example.demo-1.0.0.agent"));
        assert_eq!(
            library_file_names(library.path()),
            vec!["com.example.demo-1.0.0.agent".to_string()]
        );
        let first_len = std::fs::metadata(&saved).unwrap().len();

        // B2: a rebuilt (larger) package of the SAME version replaces it — the
        // library must not grow a second file for the same version.
        use std::io::Write as _;
        let source = packages.child("com.example.demo-1.0.0.agent");
        let mut file = std::fs::OpenOptions::new().append(true).open(&source).unwrap();
        file.write_all(b"\x00\x00\x00\x00").unwrap();
        drop(file);
        let rebuilt_len = std::fs::metadata(&source).unwrap().len();
        assert!(rebuilt_len > first_len);
        let saved_again = save_once(packages.path(), library.path(), "com.example.demo-1.0.0.agent");
        assert_eq!(saved_again, saved);
        assert_eq!(
            library_file_names(library.path()),
            vec!["com.example.demo-1.0.0.agent".to_string()],
            "republishing one version must not add a file"
        );
        assert_eq!(std::fs::metadata(&saved_again).unwrap().len(), rebuilt_len);

        // B3: a new version is a new file and a new row.
        write_package(&packages.child("com.example.demo-1.1.0.agent"), "1.1.0");
        save_once(packages.path(), library.path(), "com.example.demo-1.1.0.agent");
        assert_eq!(
            library_file_names(library.path()),
            vec![
                "com.example.demo-1.0.0.agent".to_string(),
                "com.example.demo-1.1.0.agent".to_string(),
            ]
        );

        // B4 (list half): the wizard's copy is already visible to the Local tab,
        // which enumerates the directory — two rows, two versions, one
        // `agent_id`, and the newer version's manifest is what is read back.
        let list = crate::commands::agent_library::list_rows(library.path());
        assert!(list.warning.is_none(), "{:?}", list.warning);
        assert_eq!(list.rows.len(), 2);
        let versions: Vec<Option<String>> = list
            .rows
            .iter()
            .map(|r| r.version.clone())
            .collect();
        assert_eq!(
            versions,
            vec![Some("1.0.0".to_string()), Some("1.1.0".to_string())]
        );
        for row in &list.rows {
            assert_eq!(row.error, None, "{} is unreadable", row.file_name);
            assert_eq!(row.agent_id.as_deref(), Some("com.example.demo"));
            // The row hands this back verbatim to `ensure_local_agent`.
            assert!(Path::new(&row.path).is_file(), "{}", row.path);
        }
    }
}
