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
//! `output_path` is a **node-local** path. In a single-machine (`local`)
//! deployment that is this very filesystem and the copy is possible; in
//! `remote`/`relay` mode the artifact lives on another machine, so the wizard
//! reports which node holds it and copies nothing. Pulling the file over the
//! network is an explicit non-goal (PRD §14.6), as is teaching the node to
//! write into a Desktop-owned directory.

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
/// 1. **Decide from the deployment mode** (see [`plan_save`]): remote/relay
///    never touches the filesystem, local mode must pass the guard below.
/// 2. **Validate** the path is a direct `.agent` file inside the node's
///    `packages_dir` — the frontend hands this value straight back from an IPC
///    response, so it must not become an arbitrary-file-copy primitive.
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
    // Resolved only in local mode: `plan_save` short-circuits before using it
    // otherwise, and asking the Gateway where its packages dir is has no
    // meaning for an artifact that lives on another machine.
    let packages_dir = match mode {
        GatewayMode::Local => {
            let client = state.gateway.read().await;
            resolve_packages_dir(&client).await
        }
        GatewayMode::Remote | GatewayMode::Relay => PathBuf::new(),
    };

    let plan = plan_save(mode, &package_path, &packages_dir)?;
    let SavePlan::Copy(artifact) = plan else {
        tracing::info!(
            "[PUBLISH] write-back skipped for {}: the gateway is not local ({})",
            package_path,
            mode.as_str()
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
/// two acceptance-critical rules are unit-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SavePlan {
    /// Local mode and the artifact passed the guard; canonicalized absolute
    /// path to copy.
    Copy(PathBuf),
    /// Remote/relay mode: the artifact is on another machine (D12 / B5).
    RemoteSkip,
}

/// Decide what to do with `package_path` without performing any I/O in the
/// remote case.
///
/// `remote`/`relay` is answered from the mode alone — prove that by passing a
/// root that does not exist: the function still returns [`SavePlan::RemoteSkip`]
/// instead of failing, because it must not even try to read a path that cannot
/// be on this filesystem.
pub(crate) fn plan_save(
    mode: GatewayMode,
    package_path: &str,
    packages_dir: &Path,
) -> Result<SavePlan, String> {
    match mode {
        GatewayMode::Remote | GatewayMode::Relay => Ok(SavePlan::RemoteSkip),
        GatewayMode::Local => Ok(SavePlan::Copy(validate_publish_artifact(
            Path::new(package_path),
            packages_dir,
        )?)),
    }
}

/// Guard: `candidate` must be an existing `.agent` file **directly inside**
/// `packages_dir`.
///
/// Both sides are canonicalized first, so `..` segments, symlinks/junctions and
/// Windows' verbatim prefix cannot disguise an escape — a path that merely
/// looks contained after string concatenation is rejected. "Direct child"
/// (rather than any descendant) mirrors what the node actually writes
/// (`output_dir.join(filename)`) and keeps the accepted set as small as the
/// real one.
pub(crate) fn validate_publish_artifact(
    candidate: &Path,
    packages_dir: &Path,
) -> Result<PathBuf, String> {
    let root = std::fs::canonicalize(packages_dir).map_err(|e| {
        format!(
            "The node package directory {} is not readable on this machine: {e}",
            packages_dir.display()
        )
    })?;
    let path = std::fs::canonicalize(candidate).map_err(|e| {
        format!(
            "The published package {} is not readable on this machine: {e}",
            candidate.display()
        )
    })?;
    if !path.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    if path.parent() != Some(root.as_path()) {
        return Err(format!(
            "Refusing to copy {}: only files directly inside the node package directory {} are accepted",
            path.display(),
            root.display()
        ));
    }
    if !has_package_extension(&path) {
        return Err(format!(
            "Refusing to copy {}: not an .agent package",
            path.display()
        ));
    }
    Ok(path)
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

    /// B1: the file the node just wrote, as the Gateway reports it, is accepted.
    #[test]
    fn a_built_package_in_the_node_dir_is_accepted() {
        let packages = Scratch::new("packages");
        let built = packages.write("com.example.demo-1.0.0.agent");

        let accepted = validate_publish_artifact(&built, packages.path()).unwrap();

        assert_eq!(accepted, std::fs::canonicalize(&built).unwrap());
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

        let err = validate_publish_artifact(&outside, &packages).unwrap_err();
        assert!(err.contains("only files directly inside"), "{err}");

        // `..` traversal: canonicalization resolves it before the comparison,
        // so a path that is textually "inside" cannot pass.
        let sibling = scratch.write("com.example.demo-1.0.0.agent");
        let sneaky = packages.join("..").join(
            sibling.file_name().expect("file name"),
        );
        let err = validate_publish_artifact(&sneaky, &packages).unwrap_err();
        assert!(err.contains("only files directly inside"), "{err}");

        // A nested file is not a build artifact (`output_dir.join(name)`).
        std::fs::create_dir_all(packages.join("nested")).unwrap();
        let nested = packages.join("nested").join("com.example.demo-1.0.0.agent");
        std::fs::write(&nested, b"PK\x03\x04").unwrap();
        let err = validate_publish_artifact(&nested, &packages).unwrap_err();
        assert!(err.contains("only files directly inside"), "{err}");

        // The directory itself is not a package.
        let err = validate_publish_artifact(&packages, &packages).unwrap_err();
        assert!(err.contains("is not a file"), "{err}");
    }

    #[test]
    fn non_agent_files_are_refused() {
        let packages = Scratch::new("ext");
        let other = packages.write("notes.txt");

        let err = validate_publish_artifact(&other, packages.path()).unwrap_err();
        assert!(err.contains("not an .agent package"), "{err}");
    }

    #[test]
    fn a_missing_artifact_reports_the_path_instead_of_panicking() {
        let packages = Scratch::new("missing");
        let gone = packages.child("com.example.demo-1.0.0.agent");

        let err = validate_publish_artifact(&gone, packages.path()).unwrap_err();
        assert!(err.contains("is not readable on this machine"), "{err}");
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
