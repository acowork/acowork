//! Local agent file library — the Local tab's data source.
//!
//! PRD: docs/prd/zh/prd-agent-hub-install-dialog.md §14 (D9–D14, REQ-021–032);
//! plan: docs/plan/zh/local-agent-library-plan.md §2.
//!
//! The library is a **user-owned directory**, not the shipped resource folder:
//! `app_local_data_dir()/agent-library`. The bundled `resource_dir/
//! agent-packages/*.agent` are copied in once (D10) because the resource tree
//! is read-only in per-machine installs, is replaced wholesale on upgrade, and
//! is `target/debug` in dev builds — none of which may hold user data.
//!
//! Layering (kept deliberately thin so the rules are unit-testable without a
//! Tauri `AppHandle`):
//!
//! 1. **Paths** — [`library_dir`] / [`bundled_dir`] resolve the two roots.
//! 2. **Seeding** — [`seed_from`] implements the idempotence contract against
//!    plain `&Path`s (F14: an existing target is skipped, never compared,
//!    never overwritten, so a file the user deleted does not resurrect).
//! 3. **Enumeration** — [`package_files`] + [`read_row`] turn the directory
//!    contents into rows (F12: the directory is the source of truth, there is
//!    no name→file table; F15: no de-duplication by `agent_id`).
//! 4. **Commands** — the two `#[tauri::command]` entry points, which only
//!    resolve roots, delegate, and log.
//!
//! Two facts the row model deliberately carries as `Option` rather than an
//! empty string: a broken file has no manifest, so `agent_id` / `name` /
//! `version` / `description` / `size_bytes` are genuinely unknown. The row is
//! still returned with `error` filled (PRD §14.4) — the user copied that file
//! in, so it is theirs to see and delete, not ours to hide.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, Manager, State};

use acowork_core::manifest::AgentManifest;

use crate::commands::agent::{INSTALL_WAIT_SECS, submit_with_retry};
use crate::gateway_client::EnsureAck;
use crate::state::AppState;

/// Directory name the packaged agents land in inside `resource_dir`
/// (`tauri.conf.json` → `resources { "../../../agents/agent-packages":
/// "agent-packages" }`).
const BUNDLED_DIR: &str = "agent-packages";

/// Directory created under `app_local_data_dir()`.
const LIBRARY_DIR: &str = "agent-library";

/// Only files with this extension are listed (REQ-023).
const PACKAGE_EXT: &str = "agent";

// ── Data model ─────────────────────────────────────────────────────────────

/// The avatar declared by one `.agent` file, as the card can draw it.
///
/// The manifest has two mutually exclusive ways to name an avatar
/// (`AgentManifest::avatar` and `builtin_avatar`, the same pair the Runtime's
/// avatar endpoint reports), and they are **not** interchangeable: one is a file
/// inside the archive, the other is a bare id owned by the Desktop's icon set.
/// Flattening both into one nullable string is what made a published package
/// with `builtin_avatar = "icon-15"` and no `assets/` entry render as a letter
/// block — the row said "no avatar" when the truth was "a builtin one".
///
/// So the row carries the resolved form, tag and all: exactly one of them can be
/// in effect, and the UI renders per tag instead of re-deriving priority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LocalAvatar {
    /// `avatar = "assets/avatar.png"` — the package's own image, inlined as a
    /// `data:` URL. Inlined rather than extracted to a cache directory because
    /// the list is small and a temp-file lifetime would add a second thing to
    /// invalidate.
    Packaged {
        /// `data:image/...;base64,...`
        data_url: String,
    },
    /// `builtin_avatar = "icon-15"` — nothing is inlined: the id indexes the
    /// Desktop's own icon set (`BUILTIN_ICONS`), so the row must hand it over
    /// untouched and let the renderer pick the asset.
    Builtin {
        /// The id as declared, e.g. `icon-15`. Not validated here: an unknown id
        /// is a rendering-capability question, and the card already falls back
        /// to the deterministic icon for one it does not own.
        icon_id: String,
    },
}

/// One row of the Local list.
///
/// `file_name` / `path` are always known; everything else comes from
/// `manifest.toml` inside the ZIP, so a broken package leaves it `None` and
/// explains itself in `error`. Identity is **never** guessed from the filename
/// (REQ-024): `com.acowork.senior-engineer.agent` happens to match its
/// `agent_id` today, but a renamed or re-versioned artifact must not break the
/// row.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LocalAgentRow {
    pub file_name: String,
    /// Absolute path, handed straight back to [`ensure_local_agent`].
    pub path: String,
    pub agent_id: Option<String>,
    pub name: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub size_bytes: Option<u64>,
    /// Package role / job title (`role` in the manifest) — the same field the
    /// Hub card renders next to the name.
    pub role: Option<String>,
    /// Discovery labels (`labels` in the manifest), trimmed.
    pub labels: Vec<String>,
    /// Capability keys (`[capabilities.*]` table names), sorted. Keys only: the
    /// descriptions are prose and the card shows chips.
    pub capabilities: Vec<String>,
    /// Skill names, taken from the package's `skills/<name>/SKILL.md` entries.
    /// The manifest does not list them (`[skills]` is only an injection mode
    /// flag), so the archive is the only truthful source — the Hub index reads
    /// the published artifact the same way.
    pub skills: Vec<String>,
    /// The avatar the manifest declares, resolved to **the one thing the card
    /// can draw** (see [`LocalAvatar`]).
    ///
    /// `None` when the manifest declares neither form, or declares a packaged
    /// image that cannot be used (outside the archive, not an image the webview
    /// renders, too large) — the card then falls back to the same deterministic
    /// icon the Hub card uses.
    pub avatar: Option<LocalAvatar>,
    /// Declared permissions, used **only** to render "N 项权限" (PRD REQ-007
    /// keeps the high-risk confirmation on the Hub card; a file row has no
    /// catalogue classification to confirm against).
    pub permissions: Vec<String>,
    /// `Some(io/manifest text)` when this single file could not be read. The
    /// row is still returned so one bad file does not blank the list.
    pub error: Option<String>,
}

/// Result of [`list_local_agent_packages`].
///
/// The list is returned as an envelope rather than a bare `Vec` because the
/// empty state and the reason for it are different facts (REQ-028): "the
/// directory is empty" and "seeding / the directory itself failed" must not
/// collapse into the same blank screen. `warning` carries the raw IO text —
/// never a rewritten guess — and is `None` on a healthy library.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LocalAgentList {
    pub rows: Vec<LocalAgentRow>,
    pub warning: Option<String>,
}

/// Result of [`ensure_local_agent`].
///
/// Mirrors the Hub path's `HubInstallResult`: `already_present` is the
/// Gateway's answer (D11), not a client-side guess, and it is what tells the
/// UI to say "already on this node" instead of "installed" (REQ-026).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LocalEnsureResult {
    pub agent_id: String,
    pub instance_id: String,
    pub already_present: bool,
}

// ── Paths ──────────────────────────────────────────────────────────────────

/// `app_local_data_dir()/agent-library`
/// (`%LOCALAPPDATA%\com.acowork.desktop\agent-library` on Windows).
pub(crate) fn library_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let base = app
        .path()
        .app_local_data_dir()
        .map_err(|e| format!("Failed to resolve the app local data dir: {e}"))?;
    Ok(base.join(LIBRARY_DIR))
}

/// `resource_dir/agent-packages` — the read-only shipped packages.
fn bundled_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let base = app
        .path()
        .resource_dir()
        .map_err(|e| format!("Failed to resolve the resource dir: {e}"))?;
    Ok(base.join(BUNDLED_DIR))
}

// ── Seeding (F14) ──────────────────────────────────────────────────────────

/// Copy the bundled packages into the library on first use. Returns how many
/// files were copied.
///
/// Idempotence contract (D10 / F14), in two tiers — and the reason it is
/// "exists → skip" instead of "same bytes → skip":
///
/// 1. **Directory gate.** Seeding runs only while the library directory does
///    not exist. This is the tier that makes A3 true: "target exists → skip"
///    alone would copy a file the user deleted straight back (the target is
///    *gone*, so a per-file check says "copy"). Gating on the directory means
///    the shipped content is installed exactly once, at creation; afterwards
///    the directory is the user's — deleted files stay deleted, and an emptied
///    directory shows the empty state (REQ-028 lists "被清空" as one of its
///    causes) instead of silently refilling itself. The cost is deliberate: a
///    later release that ships an *additional* bundled package does not push it
///    into an existing library (REQ-028 keeps a "re-seed" button as P1).
/// 2. **Per-file guard.** Even inside the first seed, a target that already
///    exists is skipped, never compared and never overwritten — so an
///    interrupted seed, or a file another path (publish) created between the
///    gate and the copy, is left alone. A package the user edited is never
///    rolled back to the shipped bytes.
///
/// A missing bundled directory is not an error — dev builds and slimmed-down
/// installers have none, and the library is still perfectly usable.
pub(crate) fn seed_from(bundled: &Path, library: &Path) -> Result<usize, String> {
    if library.exists() {
        return Ok(0);
    }
    std::fs::create_dir_all(library)
        .map_err(|e| format!("Failed to create {}: {e}", library.display()))?;

    let entries = match std::fs::read_dir(bundled) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(format!("Failed to read {}: {e}", bundled.display())),
    };

    let mut copied = 0usize;
    for path in package_paths(entries)? {
        let Some(file_name) = path.file_name() else {
            continue;
        };
        let target = library.join(file_name);
        if target.exists() {
            continue;
        }
        std::fs::copy(&path, &target)
            .map_err(|e| format!("Failed to copy {} to {}: {e}", path.display(), target.display()))?;
        copied += 1;
    }
    Ok(copied)
}

/// `seed_if_missing` for an `AppHandle`: bundled resource dir → library.
pub(crate) fn seed_if_missing(app: &AppHandle) -> Result<usize, String> {
    let bundled = bundled_dir(app)?;
    let library = library_dir(app)?;
    let copied = seed_from(&bundled, &library)?;
    if copied > 0 {
        tracing::info!(
            "[LOCAL_LIBRARY] seeded {copied} package(s) from {} into {}",
            bundled.display(),
            library.display()
        );
    }
    Ok(copied)
}

/// Copy one `.agent` into the library, **overwriting** a same-named file, and
/// return the destination path.
///
/// The publish wizard reuses this (plan §3 T2): a rebuild of
/// `{agent_id}-{version}.agent` is the same artifact and must replace the old
/// one — unlike seeding, "overwrite" is correct here because the caller is
/// producing the file, not restoring a default.
///
/// The only caller is `commands::publish::save_publish_to_library`, which owns
/// the "is this really a publish artifact on this machine" question; this
/// function stays a plain file copy so that policy and I/O cannot drift apart
/// (plan §1: one shared seam, not two implementations).
pub(crate) fn copy_into_library(app: &AppHandle, src: &Path) -> Result<PathBuf, String> {
    let library = library_dir(app)?;
    copy_into(&library, src)
}

/// The path-level half of [`copy_into_library`], without the `AppHandle`.
///
/// `pub(crate)` so the publish write-back's tests can prove the on-disk
/// contract (overwrite one version, keep two versions side by side) without a
/// Tauri app — the `AppHandle` wrapper only adds `library_dir()`.
pub(crate) fn copy_into(library: &Path, src: &Path) -> Result<PathBuf, String> {
    let file_name = src
        .file_name()
        .ok_or_else(|| format!("{} has no file name", src.display()))?;
    std::fs::create_dir_all(library)
        .map_err(|e| format!("Failed to create {}: {e}", library.display()))?;
    let target = library.join(file_name);
    std::fs::copy(src, &target)
        .map_err(|e| format!("Failed to copy {} to {}: {e}", src.display(), target.display()))?;
    Ok(target)
}

// ── Enumeration (F12 / F15) ────────────────────────────────────────────────

/// The `.agent` files of one directory entry iterator, sorted by file name.
///
/// F12: this is the only place that decides "which packages exist" — the
/// directory listing, never a hard-coded match like
/// `bundled_agent_package_path`. Adding a shipped agent must require no code
/// change here.
///
/// The sort is on the file name (REQ-023) so the list is stable and
/// predictable across runs; the trailing "+ add a file" row is rendered by the
/// UI and is not part of this ordering.
fn package_paths(
    entries: impl Iterator<Item = std::io::Result<std::fs::DirEntry>>,
) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("Failed to read a directory entry: {e}"))?;
        let path = entry.path();
        if !has_package_extension(&path) {
            continue;
        }
        // A directory named `foo.agent` is not a package.
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => continue,
            Ok(_) => {}
            Err(e) => {
                return Err(format!("Failed to stat {}: {e}", path.display()));
            }
        }
        paths.push(path);
    }
    paths.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    Ok(paths)
}

/// Is this a `.agent` package by file name (case-insensitive)?
///
/// Shared with the publish write-back: the wizard copies a file the node just
/// built, so the same one-definition-of-"a package" applies on both paths.
pub(crate) fn has_package_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case(PACKAGE_EXT))
        .unwrap_or(false)
}

/// Build one row from one file. Never fails: an unreadable or malformed
/// package comes back with `error` filled (PRD §14.4 / A8).
fn read_row(path: &Path) -> LocalAgentRow {
    let mut row = LocalAgentRow {
        file_name: path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string(),
        path: path.to_string_lossy().to_string(),
        agent_id: None,
        name: None,
        version: None,
        description: None,
        size_bytes: None,
        role: None,
        labels: Vec::new(),
        capabilities: Vec::new(),
        skills: Vec::new(),
        avatar: None,
        permissions: Vec::new(),
        error: None,
    };

    match std::fs::metadata(path) {
        Ok(meta) => row.size_bytes = Some(meta.len()),
        Err(e) => {
            row.error = Some(format!("Failed to read {}: {e}", path.display()));
            return row;
        }
    }

    match read_package(path) {
        Ok(facts) => {
            let manifest = facts.manifest;
            row.agent_id = Some(manifest.agent_id);
            row.name = Some(manifest.name);
            row.version = Some(manifest.version);
            row.description = Some(manifest.description);
            row.role = manifest.role.filter(|r| !r.trim().is_empty());
            row.labels = manifest
                .labels
                .iter()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            row.capabilities = {
                let mut keys: Vec<String> = manifest.capabilities.keys().cloned().collect();
                keys.sort();
                keys
            };
            row.permissions = manifest
                .permissions
                .iter()
                .map(|p| p.to_permission_string())
                .collect();
            row.skills = facts.skills;
            row.avatar = facts.avatar;
        }
        Err(e) => row.error = Some(e),
    }
    row
}

/// Everything the card needs from one `.agent` file, read in **one** archive
/// pass: the manifest, the packaged avatar, the skill names.
///
/// One pass keeps the three derived fields impossible to disagree with each
/// other — they all come from a single view of the same file — and avoids
/// re-opening the ZIP per field for every package on every dialog open
/// (REQ-032).
pub(crate) struct PackageFacts {
    pub manifest: AgentManifest,
    /// See [`LocalAvatar`].
    pub avatar: Option<LocalAvatar>,
    /// `skills/<name>/SKILL.md` directories, sorted and de-duplicated.
    pub skills: Vec<String>,
}

/// Largest avatar we inline. The shipped avatars are tens of KB; the cap keeps
/// a pathological package from turning the list response into megabytes.
const MAX_AVATAR_BYTES: u64 = 256 * 1024;

type Archive = zip::ZipArchive<std::io::Cursor<Vec<u8>>>;

/// Read one `.agent` archive into the fields a row shows.
pub(crate) fn read_package(path: &Path) -> Result<PackageFacts, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    let reader = std::io::Cursor::new(bytes);
    let mut archive = Archive::new(reader)
        .map_err(|e| format!("Not a valid .agent package (ZIP): {e}"))?;

    let manifest = read_manifest_from(&mut archive)?;
    let avatar = resolve_avatar(&mut archive, &manifest);
    let skills = read_skill_names(&archive);
    Ok(PackageFacts {
        manifest,
        avatar,
        skills,
    })
}

/// Which of the manifest's two avatar declarations the card gets, if any.
///
/// A packaged image wins when it is declared **and** usable — that is the same
/// precedence the Runtime applies (`resolve_effective_avatar`: `manifest.avatar`
/// is what the instance will actually show). A declared-but-unusable packaged
/// image yields `None` rather than silently promoting `builtin_avatar`: the
/// package really will render the deterministic icon at install time, so the row
/// must not promise a different picture than the one it will get.
fn resolve_avatar(archive: &mut Archive, manifest: &AgentManifest) -> Option<LocalAvatar> {
    if let Some(declared) = manifest.avatar.as_deref() {
        return read_avatar(archive, declared).map(|data_url| LocalAvatar::Packaged { data_url });
    }
    manifest
        .builtin_avatar
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(|icon_id| LocalAvatar::Builtin {
            icon_id: icon_id.to_string(),
        })
}

/// `manifest.toml` through the shared domain type, so a row shows exactly what
/// the Runtime will read (`AgentManifest`) and not a second hand-rolled schema.
fn read_manifest_from(archive: &mut Archive) -> Result<AgentManifest, String> {
    let mut manifest_file = archive
        .by_name("manifest.toml")
        .map_err(|e| format!("manifest.toml not found in package: {e}"))?;
    let mut manifest_str = String::new();
    manifest_file
        .read_to_string(&mut manifest_str)
        .map_err(|e| format!("Failed to read manifest.toml: {e}"))?;
    AgentManifest::from_toml(&manifest_str).map_err(|e| format!("Invalid manifest.toml: {e}"))
}

/// Inline a packaged avatar (`avatar = "assets/avatar.png"`) as a `data:` URL.
///
/// Only the packaged form is handled here — the builtin form names an id, not a
/// file (see [`resolve_avatar`]). Every failure is `None`, never a row error: a
/// package with a missing, oversized or unrecognised avatar is still
/// installable. The declared path is treated as untrusted — no absolute path,
/// no `..`, no directory entry — because it comes out of a file the user copied
/// in from anywhere.
fn read_avatar(archive: &mut Archive, declared: &str) -> Option<String> {
    let declared = declared.trim();
    let mime = avatar_mime(declared)?;
    let normalised = declared.replace('\\', "/");
    if normalised.starts_with('/') || normalised.split('/').any(|seg| seg == "..") {
        return None;
    }
    let mut entry = archive.by_name(&normalised).ok()?;
    if entry.is_dir() || entry.size() > MAX_AVATAR_BYTES {
        return None;
    }
    let mut buf = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut buf).ok()?;
    if buf.is_empty() {
        return None;
    }
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &buf);
    Some(format!("data:{mime};base64,{encoded}"))
}

/// Image types the webview renders without a converter. Anything else is
/// skipped rather than handed to an `<img>` that cannot decode it.
fn avatar_mime(path: &str) -> Option<&'static str> {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

/// `skills/<name>/SKILL.md` → `<name>`.
///
/// Taken from the archive's entry names rather than the manifest: `[skills]`
/// only carries the injection mode, so the package's skill directory is the
/// only truthful source (the Hub index lists the same set).
fn read_skill_names(archive: &Archive) -> Vec<String> {
    let mut names: Vec<String> = archive
        .file_names()
        .filter_map(|name| {
            let rest = name.strip_prefix("skills/")?;
            let (skill, tail) = rest.split_once('/')?;
            (tail == "SKILL.md" && !skill.is_empty()).then(|| skill.to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Enumerate a library directory into rows, plus the reason it is empty when
/// it is (REQ-028).
pub(crate) fn list_rows(library: &Path) -> LocalAgentList {
    let entries = match std::fs::read_dir(library) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return LocalAgentList {
                rows: Vec::new(),
                warning: Some(format!("{} does not exist: {e}", library.display())),
            };
        }
        Err(e) => {
            return LocalAgentList {
                rows: Vec::new(),
                warning: Some(format!("Failed to read {}: {e}", library.display())),
            };
        }
    };

    match package_paths(entries) {
        Ok(paths) => LocalAgentList {
            // F15: one file, one row — no grouping by `agent_id`, so two
            // versions of the same package stay visible side by side (A9).
            rows: paths.iter().map(|p| read_row(p)).collect(),
            warning: None,
        },
        Err(e) => LocalAgentList {
            rows: Vec::new(),
            warning: Some(e),
        },
    }
}

// ── Commands ───────────────────────────────────────────────────────────────

/// List every `*.agent` in the library, seeding it from the bundled packages
/// first.
///
/// One command for N files (F13): reading N ZIP manifests in the webview would
/// mean N IPC round-trips and N copies of the parse logic.
///
/// A seed failure does **not** fail the call. The library is the user's
/// directory — it may hold files even when the bundled copy step cannot run
/// (dev build, read-only resource dir) — so the enumeration still happens and
/// the failure is reported as `warning` (raw IO text, REQ-028).
#[tauri::command]
pub async fn list_local_agent_packages(app: AppHandle) -> Result<LocalAgentList, String> {
    let mut warning = None;
    if let Err(e) = seed_if_missing(&app) {
        tracing::warn!("[LOCAL_LIBRARY] seeding skipped: {e}");
        warning = Some(e);
    }

    let library = library_dir(&app)?;
    let mut list = list_rows(&library);
    if list.warning.is_none() {
        list.warning = warning;
    }
    tracing::info!(
        "[LOCAL_LIBRARY] listed {} package(s) from {}{}",
        list.rows.len(),
        library.display(),
        list.warning
            .as_ref()
            .map(|w| format!(" (warning: {w})"))
            .unwrap_or_default()
    );
    Ok(list)
}

/// Install the package at `package_path` on `node_id` **idempotently**.
///
/// `/ensure` (D11 / REQ-026), never `/install`: a second click on the same row
/// must converge on the instance that is already there instead of adding
/// another one. `/install` stays behind the row's "install another instance"
/// action, where an extra instance is the user's explicit intent (REQ-027).
#[tauri::command]
pub async fn ensure_local_agent(
    state: State<'_, AppState>,
    app: AppHandle,
    package_path: String,
    node_id: String,
) -> Result<LocalEnsureResult, String> {
    crate::commands::agent::require_node_id(&node_id)?;
    const LABEL: &str = "LOCAL_ENSURE";
    let started = std::time::Instant::now();

    let path = PathBuf::from(&package_path);
    if !has_package_extension(&path) {
        return Err(format!("Not an .agent package: {package_path}"));
    }
    // The webview only ever passes a path it received from the list. Confining
    // it to the library keeps this IPC endpoint from becoming a second
    // arbitrary-file reader (its whole contract is "install a library file").
    let library = library_dir(&app)?;
    if !is_inside(&path, &library) {
        return Err(format!(
            "{package_path} is not inside the local agent library ({})",
            library.display()
        ));
    }

    let bytes = std::fs::read(&path).map_err(|e| format!("Failed to read {package_path}: {e}"))?;
    tracing::info!(
        "[{LABEL}] read {} bytes from {}",
        bytes.len(),
        path.display()
    );

    let state_for_op = state.clone();
    let ack: EnsureAck = submit_with_retry(&state, LABEL, move || {
        let state = state_for_op.clone();
        let bytes = bytes.clone();
        let node_id = node_id.clone();
        async move {
            let client = state.gateway.read().await;
            client.ensure_agent(&bytes, &node_id).await
        }
    })
    .await?;

    if !ack.already_present {
        // ADR-073: wait on the instance id the ack minted — polling the
        // package id would hang for the whole budget on a multi-instance node.
        wait_until_installed(&state, LABEL, &ack.instance_id, ack.operation_id.as_deref()).await?;
    }

    tracing::info!(
        "[{LABEL}] {package_path} resolved to instance {} in {:.1}s (already_present={})",
        ack.instance_id,
        started.elapsed().as_secs_f64(),
        ack.already_present
    );
    Ok(LocalEnsureResult {
        agent_id: ack.agent_id,
        instance_id: ack.instance_id,
        already_present: ack.already_present,
    })
}

/// True when `path` is a file strictly below `root`.
///
/// No `canonicalize`: the library path and the row path are both produced by
/// this process, and canonicalizing on Windows would rewrite the prefix into a
/// different spelling (`\\?\`) than the one the list handed out.
///
/// `starts_with` alone is not enough — it compares components, so
/// `/lib/../../etc/passwd.agent` *does* start with `/lib`. Any `..` component
/// therefore rejects the path outright (a lexically clean path cannot escape
/// without one), and `root` itself is not a package file.
fn is_inside(path: &Path, root: &Path) -> bool {
    path != root
        && path.starts_with(root)
        && !path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}

/// Wait until the instance lands in the Gateway inventory (ADR-059 §6).
///
/// A timeout is not a failure of the request — the node may still be
/// installing — so the message says so and keeps the operation id, which is
/// what an operator greps the logs for.
async fn wait_until_installed(
    state: &AppState,
    label: &str,
    instance_id: &str,
    operation_id: Option<&str>,
) -> Result<(), String> {
    let op = operation_id.unwrap_or("n/a");
    let client = state.gateway.read().await;
    client
        .wait_for_agent_installed(
            instance_id,
            std::time::Duration::from_secs(INSTALL_WAIT_SECS),
        )
        .await
        .map(|_| ())
        .map_err(|e| {
            format!(
                "[{label}] submitted (operation {op}) but instance {instance_id} did not appear \
                 within {INSTALL_WAIT_SECS}s: {e}"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique scratch directory under the OS temp dir — the desktop crate has
    /// no `tempfile` dependency (see `hub_client`'s tests for the same pattern).
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!("acowork-lib-{name}-{nanos}"));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn child(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Minimal manifest in the **real** wire schema: `permissions` are
    /// `PermissionRepr` tables (ADR-104), not strings — a string list fails to
    /// deserialize, which is how the first version of this fixture was wrong.
    const MANIFEST: &str = r#"
agent_id = "com.example.demo"
version = "1.0.0"
name = "Demo"
description = "A demo agent"
author = "acowork"
runtime_version = "0.1.0"

[[permissions]]
type = "Network"

[[permissions]]
type = "FilesystemRead"
"#;

    /// Write a minimal but real `.agent` (ZIP + manifest.toml).
    fn write_package(path: &Path, manifest: &str) {
        use std::io::Write as _;
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("manifest.toml", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(manifest.as_bytes()).unwrap();
        zip.finish().unwrap();
    }

    #[test]
    fn seeding_copies_each_bundled_package_once() {
        let bundled = Scratch::new("seed-src");
        let scratch = Scratch::new("seed-dst");
        let library = scratch.path().join("agent-library");
        write_package(&bundled.child("a.agent"), MANIFEST);
        write_package(&bundled.child("b.agent"), MANIFEST);

        assert_eq!(seed_from(bundled.path(), &library).unwrap(), 2);
        assert!(library.join("a.agent").exists());
        assert!(library.join("b.agent").exists());
        // Second call: the directory exists, so seeding does not run at all.
        assert_eq!(seed_from(bundled.path(), &library).unwrap(), 0);
    }

    /// REQ-028 / A4: an emptied library stays empty — it is the user's
    /// directory, not a mirror of the shipped packages.
    #[test]
    fn an_emptied_library_is_not_reseeded() {
        let bundled = Scratch::new("empty-src");
        let scratch = Scratch::new("empty-dst");
        let library = scratch.path().join("agent-library");
        write_package(&bundled.child("a.agent"), MANIFEST);

        seed_from(bundled.path(), &library).unwrap();
        std::fs::remove_file(library.join("a.agent")).unwrap();

        assert_eq!(seed_from(bundled.path(), &library).unwrap(), 0);
        let list = list_rows(&library);
        assert!(list.rows.is_empty());
        assert!(list.warning.is_none(), "an empty library is not a warning");
    }

    /// A3: a seed the user deleted must stay deleted.
    #[test]
    fn a_deleted_seed_does_not_resurrect() {
        let bundled = Scratch::new("revive-src");
        let scratch = Scratch::new("revive-dst");
        let library = scratch.path().join("agent-library");
        write_package(&bundled.child("a.agent"), MANIFEST);
        write_package(&bundled.child("b.agent"), MANIFEST);

        assert_eq!(seed_from(bundled.path(), &library).unwrap(), 2);
        std::fs::remove_file(library.join("a.agent")).unwrap();

        assert_eq!(seed_from(bundled.path(), &library).unwrap(), 0);
        assert!(!library.join("a.agent").exists());
        assert_eq!(list_rows(&library).rows.len(), 1);
    }

    /// F14: an existing file is never compared and never overwritten.
    #[test]
    fn seeding_never_overwrites_an_existing_file() {
        let bundled = Scratch::new("keep-src");
        let library = Scratch::new("keep-dst");
        write_package(&bundled.child("a.agent"), MANIFEST);
        std::fs::write(library.child("a.agent"), b"user edited this").unwrap();

        assert_eq!(seed_from(bundled.path(), library.path()).unwrap(), 0);
        assert_eq!(
            std::fs::read(library.child("a.agent")).unwrap(),
            b"user edited this"
        );
    }

    /// A missing bundled directory is not an error (dev build / slim install).
    #[test]
    fn seeding_tolerates_a_missing_resource_dir() {
        let library = Scratch::new("no-src-dst");
        let missing = library.path().join("nope");
        assert_eq!(seed_from(&missing, library.path()).unwrap(), 0);
    }

    #[test]
    fn rows_are_sorted_by_file_name_and_ignore_other_files() {
        let library = Scratch::new("sort");
        write_package(&library.child("z.agent"), MANIFEST);
        write_package(&library.child("a.agent"), MANIFEST);
        std::fs::write(library.child("notes.txt"), b"ignore me").unwrap();
        std::fs::create_dir_all(library.child("dir.agent")).unwrap();

        let list = list_rows(library.path());
        assert!(list.warning.is_none());
        let names: Vec<_> = list.rows.iter().map(|r| r.file_name.as_str()).collect();
        assert_eq!(names, vec!["a.agent", "z.agent"]);
    }

    /// REQ-024: the row is built from `manifest.toml`, not from the filename.
    #[test]
    fn row_identity_comes_from_the_manifest() {
        let library = Scratch::new("identity");
        write_package(&library.child("renamed.agent"), MANIFEST);

        let list = list_rows(library.path());
        let row = &list.rows[0];
        assert_eq!(row.file_name, "renamed.agent");
        assert_eq!(row.agent_id.as_deref(), Some("com.example.demo"));
        assert_eq!(row.name.as_deref(), Some("Demo"));
        assert_eq!(row.version.as_deref(), Some("1.0.0"));
        assert_eq!(row.description.as_deref(), Some("A demo agent"));
        assert_eq!(row.permissions, vec!["network", "filesystem:read"]);
        assert!(row.size_bytes.is_some_and(|s| s > 0));
        assert!(row.error.is_none());
    }

    /// A8: one broken file marks its own row, the rest still list.
    #[test]
    fn a_broken_file_reports_its_own_error_only() {
        let library = Scratch::new("broken");
        write_package(&library.child("good.agent"), MANIFEST);
        std::fs::write(library.child("bad.agent"), b"not a zip").unwrap();

        let list = list_rows(library.path());
        assert!(list.warning.is_none());
        assert_eq!(list.rows.len(), 2);

        let bad = list
            .rows
            .iter()
            .find(|r| r.file_name == "bad.agent")
            .unwrap();
        assert!(bad.agent_id.is_none());
        assert!(bad.error.as_deref().unwrap().contains("ZIP"));
        // Still returned, and still installable-looking to the user: the row
        // exists, the button is what must be disabled.
        assert_eq!(bad.size_bytes, Some(9));

        let good = list
            .rows
            .iter()
            .find(|r| r.file_name == "good.agent")
            .unwrap();
        assert!(good.error.is_none());
    }

    /// A9 / F15: same `agent_id`, different `version` → two rows.
    #[test]
    fn two_versions_of_one_agent_stay_separate() {
        let library = Scratch::new("versions");
        write_package(&library.child("com.example.demo-1.0.0.agent"), MANIFEST);
        write_package(
            &library.child("com.example.demo-1.1.0.agent"),
            &MANIFEST.replace("1.0.0", "1.1.0"),
        );

        let list = list_rows(library.path());
        assert_eq!(list.rows.len(), 2);
        let versions: Vec<_> = list
            .rows
            .iter()
            .filter_map(|r| r.version.clone())
            .collect();
        assert_eq!(versions, vec!["1.0.0", "1.1.0"]);
    }

    /// REQ-028: an absent directory is an empty list *with the reason*.
    #[test]
    fn a_missing_library_dir_reports_a_reason() {
        let scratch = Scratch::new("missing");
        let list = list_rows(&scratch.path().join("absent"));
        assert!(list.rows.is_empty());
        assert!(list.warning.unwrap().contains("does not exist"));
    }

    #[test]
    fn copy_into_the_library_replaces_a_same_named_file() {
        let scratch = Scratch::new("copy");
        let library = scratch.path().join("agent-library");
        let src = scratch.child("incoming.agent");
        std::fs::write(&src, b"first").unwrap();

        let target = copy_into(&library, &src).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"first");

        std::fs::write(&src, b"second").unwrap();
        let target = copy_into(&library, &src).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"second");
        assert_eq!(
            std::fs::read_dir(&library).unwrap().count(),
            1,
            "an overwrite must not leave a second file behind"
        );
    }

    #[test]
    fn the_ensure_command_only_accepts_library_paths() {
        let root = Path::new("/tmp/agent-library");
        assert!(is_inside(Path::new("/tmp/agent-library/a.agent"), root));
        assert!(!is_inside(Path::new("/tmp/elsewhere/a.agent"), root));
        assert!(!is_inside(
            Path::new("/tmp/agent-library/../../etc/passwd.agent"),
            root
        ));
    }

    /// A1 / REQ-023 / REQ-024 against the REAL shipped packages: every bundled
    /// `.agent` must produce a healthy row carrying a manifest identity.
    ///
    /// This is the test that catches manifest-schema drift — the packaged
    /// manifests are written by a different tool, and a field `AgentManifest`
    /// stops accepting would ship as seven broken rows with no other symptom.
    /// The directory is a build input (not present in a packaged app), so the
    /// test reports a skip instead of failing when it is absent.
    #[test]
    fn the_shipped_packages_all_parse_into_healthy_rows() {
        let bundled = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../agents/agent-packages");
        if !bundled.exists() {
            eprintln!("skipped: {} is not present in this checkout", bundled.display());
            return;
        }

        let list = list_rows(&bundled);
        assert!(list.warning.is_none(), "list warning: {:?}", list.warning);
        assert_eq!(list.rows.len(), 7, "the shipped set is seven packages");
        for row in &list.rows {
            assert_eq!(row.error, None, "{} reported an error", row.file_name);
            assert!(row.agent_id.is_some(), "{} has no agent_id", row.file_name);
            assert!(row.name.is_some(), "{} has no name", row.file_name);
            assert!(row.version.is_some(), "{} has no version", row.file_name);
        }
    }

    /// A1 / A3 harness against the **real** library directory.
    ///
    /// The `AppHandle`-based entry points resolve `%LOCALAPPDATA%` through
    /// Tauri, which a unit test cannot construct, so this drives the same
    /// `seed_from` body with the paths spelled out by hand. It writes to the
    /// user's own library, which is why it is env-gated rather than always on:
    ///
    /// ```text
    /// ACW_LOCAL_LIB_PROBE=1 cargo test --lib real_library_dir_probe -- --nocapture
    /// ```
    ///
    /// Run it after deleting a file to watch A3 hold (`copied=0` and the count
    /// stays down).
    #[test]
    fn real_library_dir_probe() {
        let Ok(local_app_data) = std::env::var("LOCALAPPDATA") else {
            eprintln!("skipped: no LOCALAPPDATA");
            return;
        };
        if std::env::var("ACW_LOCAL_LIB_PROBE").is_err() {
            eprintln!("skipped: set ACW_LOCAL_LIB_PROBE=1 to touch the real library dir");
            return;
        }
        let bundled = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../agents/agent-packages");
        let library = Path::new(&local_app_data).join("com.acowork.desktop").join(LIBRARY_DIR);

        let copied = seed_from(&bundled, &library).expect("seed the real library");
        let list = list_rows(&library);
        println!(
            "[probe] copied={copied} rows={} warning={:?} dir={}",
            list.rows.len(),
            list.warning,
            library.display()
        );
        for row in &list.rows {
            // The avatar column prints the *resolved form*, which is the whole
            // point of `LocalAvatar`: `packaged` (inlined bytes), `builtin` (an
            // icon id this row expects the Desktop to own) or none.
            let avatar = match &row.avatar {
                Some(LocalAvatar::Packaged { data_url }) => {
                    format!("packaged({} bytes)", data_url.len())
                }
                Some(LocalAvatar::Builtin { icon_id }) => format!("builtin({icon_id})"),
                None => "none".to_string(),
            };
            println!(
                "[probe]   {} | {:?} | {:?} | {:?} | avatar={} | err={:?}",
                row.file_name, row.agent_id, row.name, row.version, avatar, row.error
            );
        }
    }

    /// A manifest that declares the discovery fields the card renders.
    const RICH_MANIFEST: &str = r#"
agent_id = "com.example.rich"
version = "2.3.4"
name = "Rich"
role = "Senior Reviewer"
description = "Carries every field the card shows"
author = "acowork"
runtime_version = "0.1.0"
avatar = "assets/avatar.png"
labels = ["review", "legacy-code"]

[[permissions]]
type = "Network"

[capabilities.review_code]
description = "Reviews code"

[capabilities.audit_debt]
description = "Audits debt"
"#;

    /// Build a package with extra archive entries. `[u8]` payloads keep the
    /// fixture honest: the avatar is copied byte-for-byte, so a wrong path or a
    /// wrong extension really does change the outcome.
    fn write_package_with(path: &Path, manifest: &str, entries: &[(&str, &[u8])]) {
        use std::io::Write as _;
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("manifest.toml", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(manifest.as_bytes()).unwrap();
        for (name, body) in entries {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(body).unwrap();
        }
        zip.finish().unwrap();
    }

    /// The 1×1 transparent PNG every browser decodes.
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    /// The card reads role / labels / capability keys / skill names / avatar
    /// out of the archive — one fixture, all five fields, so a future refactor
    /// that drops one of them fails here instead of silently thinning the card.
    #[test]
    fn the_row_carries_every_field_the_card_shows() {
        let scratch = Scratch::new("rich");
        let library = scratch.path();
        write_package_with(
            &library.join("rich.agent"),
            RICH_MANIFEST,
            &[
                ("assets/avatar.png", PNG),
                ("skills/reviewing/SKILL.md", b"# Reviewing"),
                ("skills/auditing/SKILL.md", b"# Auditing"),
                ("skills/auditing/notes.md", b"not a skill entry"),
            ],
        );

        let row = &list_rows(library).rows[0];
        assert_eq!(row.error, None);
        assert_eq!(row.role.as_deref(), Some("Senior Reviewer"));
        assert_eq!(row.labels, vec!["review", "legacy-code"]);
        // Sorted keys, so the chip order does not depend on HashMap iteration.
        assert_eq!(row.capabilities, vec!["audit_debt", "review_code"]);
        // Directory names only: the nested notes.md is not a second entry, and
        // the manifest cannot supply these at all.
        assert_eq!(row.skills, vec!["auditing", "reviewing"]);
        let LocalAvatar::Packaged { data_url } = row.avatar.as_ref().expect("avatar inlined")
        else {
            panic!("a declared assets/avatar.png must resolve to Packaged");
        };
        assert!(data_url.starts_with("data:image/png;base64,"), "{data_url}");
    }

    /// Regression: a manifest that declares `builtin_avatar` (and no packaged
    /// image) must report that **as an avatar**. This is the shape the publish
    /// wizard produces when the user keeps the agent's builtin icon, and it used
    /// to arrive at the card as `None`, drawn as a letter block.
    #[test]
    fn a_builtin_avatar_is_reported_as_an_avatar() {
        let scratch = Scratch::new("builtin-avatar");
        let manifest = MANIFEST.replace(
            "version = \"1.0.0\"",
            "version = \"1.0.0\"
builtin_avatar = \"icon-15\"",
        );
        assert!(manifest.contains("builtin_avatar"), "fixture edit missed");
        write_package(&scratch.child("builtin.agent"), &manifest);

        let row = &list_rows(scratch.path()).rows[0];
        assert_eq!(row.error, None);
        assert_eq!(
            row.avatar,
            Some(LocalAvatar::Builtin {
                icon_id: "icon-15".to_string()
            })
        );
    }

    /// The packaged form wins when both are declared — the same precedence the
    /// Runtime applies when it answers `/avatar` for the installed instance.
    #[test]
    fn a_packaged_avatar_wins_over_a_builtin_one() {
        let scratch = Scratch::new("both-avatars");
        let manifest = RICH_MANIFEST.replace(
            "avatar = \"assets/avatar.png\"",
            "avatar = \"assets/avatar.png\"
builtin_avatar = \"icon-15\"",
        );
        write_package_with(
            &scratch.child("both.agent"),
            &manifest,
            &[("assets/avatar.png", PNG)],
        );

        let row = &list_rows(scratch.path()).rows[0];
        assert!(
            matches!(row.avatar, Some(LocalAvatar::Packaged { .. })),
            "{:?}",
            row.avatar
        );
    }

    /// A declared packaged avatar that cannot be inlined is `None`, **not** a
    /// promotion of `builtin_avatar`: the installed instance will render the
    /// deterministic icon, so the row must not promise a different picture.
    #[test]
    fn an_unusable_packaged_avatar_does_not_fall_back_to_the_builtin_one() {
        let scratch = Scratch::new("broken-packaged");
        let manifest = RICH_MANIFEST
            .replace("assets/avatar.png", "assets/avatar.txt")
            .replace(
                "avatar = \"assets/avatar.txt\"",
                "avatar = \"assets/avatar.txt\"
builtin_avatar = \"icon-15\"",
            );
        write_package_with(
            &scratch.child("broken.agent"),
            &manifest,
            &[("assets/avatar.txt", b"i am not a picture")],
        );

        let row = &list_rows(scratch.path()).rows[0];
        assert_eq!(row.avatar, None);
    }

    /// A package without an avatar keeps `avatar: None` — the card's
    /// deterministic-icon fallback, not an error.
    #[test]
    fn a_package_without_an_avatar_has_none() {
        let scratch = Scratch::new("no-avatar");
        write_package(&scratch.child("plain.agent"), MANIFEST);
        let row = &list_rows(scratch.path()).rows[0];
        assert_eq!(row.avatar, None);
        assert_eq!(row.error, None, "a missing avatar is not a broken package");
        assert!(row.skills.is_empty());
        assert!(row.labels.is_empty());
    }

    /// Only image extensions are inlined: an `<img>` pointed at a `.txt` would
    /// fail to decode and show a broken-image icon instead of the initial.
    #[test]
    fn a_non_image_avatar_is_skipped() {
        let scratch = Scratch::new("txt-avatar");
        let manifest = RICH_MANIFEST.replace("assets/avatar.png", "assets/avatar.txt");
        write_package_with(
            &scratch.child("txt.agent"),
            &manifest,
            &[("assets/avatar.txt", b"i am not a picture")],
        );
        let row = &list_rows(scratch.path()).rows[0];
        assert_eq!(row.error, None);
        assert_eq!(row.avatar, None);
    }

    /// The declared path comes out of a file the user copied in from anywhere,
    /// so it is treated as untrusted even though it only ever opens one archive
    /// entry. A traversal-shaped path yields no avatar, not an error.
    #[test]
    fn an_avatar_path_outside_the_archive_is_refused() {
        let scratch = Scratch::new("traversal-avatar");
        let manifest = RICH_MANIFEST.replace("assets/avatar.png", "../../evil.png");
        write_package_with(
            &scratch.child("evil.agent"),
            &manifest,
            &[("../../evil.png", PNG), ("assets/avatar.png", PNG)],
        );
        let row = &list_rows(scratch.path()).rows[0];
        assert_eq!(row.error, None);
        assert_eq!(row.avatar, None);
    }

    /// A manifest that names an avatar the archive does not contain is still a
    /// perfectly installable package: the row stays healthy.
    #[test]
    fn a_declared_avatar_missing_from_the_archive_is_not_a_row_error() {
        let scratch = Scratch::new("ghost-avatar");
        write_package_with(&scratch.child("ghost.agent"), RICH_MANIFEST, &[]);
        let row = &list_rows(scratch.path()).rows[0];
        assert_eq!(row.error, None);
        assert_eq!(row.avatar, None);
        assert_eq!(row.role.as_deref(), Some("Senior Reviewer"));
    }

    /// The shipped packages carry all of it in the archive — this is the
    /// evidence that the Local card shows real fields rather than blanks.
    #[test]
    fn the_shipped_packages_carry_role_labels_capabilities_and_skills() {
        let bundled = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../agents/agent-packages");
        if !bundled.exists() {
            eprintln!("skipped: {} is not present in this checkout", bundled.display());
            return;
        }
        for row in &list_rows(&bundled).rows {
            assert_eq!(row.error, None, "{} reported an error", row.file_name);
            assert!(row.role.is_some(), "{} has no role", row.file_name);
            assert!(!row.labels.is_empty(), "{} has no labels", row.file_name);
            assert!(
                !row.capabilities.is_empty(),
                "{} has no capability keys",
                row.file_name
            );
            assert!(!row.skills.is_empty(), "{} has no skills", row.file_name);
            let Some(LocalAvatar::Packaged { data_url }) = row.avatar.as_ref() else {
                panic!("{} has no packaged avatar", row.file_name)
            };
            assert!(data_url.starts_with("data:image/"), "{} has no avatar", row.file_name);
        }
    }

    #[test]
    fn extension_match_is_case_insensitive_and_requires_agent() {
        assert!(has_package_extension(Path::new("a.agent")));
        assert!(has_package_extension(Path::new("a.AGENT")));
        assert!(!has_package_extension(Path::new("a.agent.zip")));
        assert!(!has_package_extension(Path::new("agent")));
    }
}
