//! Agent clone logic (migrated from gateway `package_manager/clone.rs`,
//! ADR-055 §6.20).
//!
//! Two invariants this module must keep, each the fix of a real bug:
//!
//! 1. The package-level list is **never hard-coded** — see
//!    [`is_package_state_dir`]. An allowlist rots as the package format
//!    evolves; an earlier one classified `skills`/`assets` as full-only and
//!    skeleton clones silently lost them.
//! 2. Instance state is read from the WORKSPACE
//!    (`{install_path}/workspace/`), never from the install directory —
//!    package files and workspace are siblings, and a clone that conflated
//!    them copied nothing.
//!
//! `Full` mode clones every workspace subdir except `files/` and `logs/`,
//! listed in [`acowork_core::workspace::WORKSPACE_LOCAL_DIRS`]. `config/`
//! ships with the clone — the user spent the afternoon setting up MCP
//! servers and custom workspace dirs and a clone should not force them
//! to redo it.

use std::path::Path;

use crate::error::{NodeError, Result};
use crate::state::{InstalledAgent, NodeState};

/// Clone mode: what to copy from the source agent
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloneMode {
    /// Copy every package file (manifest + prompts + skills + assets and
    /// anything else under the package root, minus the runtime/memory/
    /// workspace exclusion list). Skeleton starts a fresh instance with
    /// no per-instance state.
    Skeleton,
    /// Copy every package file PLUS the per-instance state living under
    /// `workspace/` — `config/` (model picks, MCP servers, custom
    /// workspace dirs) and `memory/` (the user's curated memory nodes).
    /// Session history is NOT inherited: `conversations/` is in
    /// `WORKSPACE_LOCAL_DIRS` and the session-meta + conversation-index
    /// rows inside the SQLite store are wiped in lock-step. `files/` and
    /// `logs/` stay behind — see
    /// [`acowork_core::workspace::WORKSPACE_LOCAL_DIRS`].
    Full,
}

/// Clone a source agent to a new agent ID.
///
/// Returns the new InstalledAgent or an error if the clone fails.
///
/// ADR-073: the source is located by instance identity (`source_instance_id`,
/// falling back to the package id for legacy commands); the target is a NEW
/// instance (fresh UUID) installed under
/// `{install_dir}/{new_agent_id}/{new_instance_id}/`.
pub fn clone_agent(
    source_instance_id: &str,
    source_agent_id: &str,
    new_agent_id: &str,
    new_name: &str,
    mode: CloneMode,
    install_dir: &Path,
    state: &mut NodeState,
) -> Result<InstalledAgent> {
    // ADR-073: the install table is keyed by instance identity.
    let source_key = source_instance_id.to_string();

    // 1. Validate source exists
    let source_info = state
        .installed_agents
        .get(&source_key)
        .ok_or_else(|| NodeError::AgentNotFound(source_key.clone()))?;

    // 2. Check conflict — the target is a NEW instance; only a collision
    //    on the same instance_id (impossible for a fresh UUID) would fail.
    let new_instance_id = uuid::Uuid::new_v4().to_string();
    if state.is_installed(&new_instance_id) {
        return Err(NodeError::Package(format!(
            "Instance '{}' is already installed. Uninstall or choose a different ID.",
            new_instance_id
        )));
    }

    // 3. Validate new agent_id (reverse-domain format)
    if !is_valid_agent_id(new_agent_id) {
        return Err(NodeError::Package(format!(
            "Invalid agent ID '{}': must be reverse-domain format (e.g. com.example.myagent)",
            new_agent_id
        )));
    }

    let source_path = Path::new(&source_info.install_path);
    if !source_path.exists() {
        return Err(NodeError::Package(format!(
            "Source agent install path does not exist: {}",
            source_path.display()
        )));
    }

    let target_path = install_dir.join(new_agent_id).join(&new_instance_id);
    std::fs::create_dir_all(&target_path).map_err(|e| {
        NodeError::Package(format!(
            "Failed to create target directory '{}': {}",
            target_path.display(),
            e
        ))
    })?;

    // 4. Copy manifest (with modifications)
    let mut new_manifest = source_info.manifest.clone();
    new_manifest.agent_id = new_agent_id.to_string();
    new_manifest.dev = true;

    let manifest_toml = toml::to_string_pretty(&new_manifest)
        .map_err(|e| NodeError::Package(format!("Failed to serialize manifest: {}", e)))?;
    std::fs::write(target_path.join("manifest.toml"), &manifest_toml)
        .map_err(|e| NodeError::Package(format!("Failed to write manifest: {}", e)))?;

    // 5. Copy package files. Iteration, not allowlist — the previous
    //    `["prompts", "config", "tools", "resources"]` list silently
    //    dropped `skills/` and `assets/` because the maintainer kept
    //    adding files to the package format without editing this list.
    //    Now any package-level dir survives except the per-instance
    //    state dirs in PACKAGE_ALWAYS_EXCLUDE_DIRS (workspace, runtime,
    //    memory) — the same list that keeps those dirs out of a
    //    published `.agent`.
    for entry in read_dir_sorted(source_path)? {
        if !entry.is_dir() {
            // Stray top-level files (no real install has them, but
            // mirror them so we never silently drop a package author's
            // intent).
            let name = entry
                .file_name()
                .ok_or_else(|| {
                    NodeError::Package(format!("Entry with no name: {}", entry.display()))
                })?
                .to_string_lossy()
                .into_owned();
            std::fs::copy(&entry, target_path.join(&name)).map_err(|e| {
                NodeError::Package(format!("Failed to copy '{}': {}", entry.display(), e))
            })?;
            continue;
        }
        let name = entry.file_name().ok_or_else(|| {
            NodeError::Package(format!("Dir entry with no name: {}", entry.display()))
        })?;
        let name = name
            .to_str()
            .ok_or_else(|| {
                NodeError::Package(format!("Non-UTF8 dir name: {:?}", name))
            })?
            .to_owned();
        if is_package_state_dir(&name) {
            continue;
        }
        // skills/ and assets/ (the dirs the previous "full_only" list
        // named) are package content and travel with both modes — they
        // are not "full-only" any more.
        copy_dir_all(&entry, &target_path.join(&name))?;
    }

    // 6. Copy full-mode only state: skeleton already has every package
    //    file. Full mode adds the per-instance state living in the
    //    workspace — the SQLite memory store (memory nodes, but NOT
    //    session history) and the user's `config/` (model picks, MCP
    //    servers, custom workspace dirs in `agent_workspaces.json`).
    //
    //    Iterate every workspace subdir and skip the machine-local /
    //    runtime-artifact ones (`files/`, `logs/`, `conversations/`),
    //    the same shape as the package-files step that uses
    //    PACKAGE_ALWAYS_EXCLUDE_DIRS. The earlier "only conversations +
    //    memory" allowlist silently dropped `config/` — this iteration
    //    is the regression guard.
    //
    //    `conversations/` (the JSONL history) is in WORKSPACE_LOCAL_DIRS
    //    because a clone starts a fresh dialogue, not a continuation.
    //    The session-meta rows + conversation-index rows co-located in
    //    `memory/private.sqlite` (ADR-082) are wiped in lock-step —
    //    otherwise the session list would surface ghost sessions whose
    //    JSONL files the clone deliberately skipped.
    if mode == CloneMode::Full {
        let src_workspace = acowork_core::workspace::workspace_dir(source_path);
        let target_workspace = acowork_core::workspace::workspace_dir(&target_path);

        for entry in read_dir_sorted(&src_workspace)? {
            if !entry.is_dir() {
                continue;
            }
            let name = entry.file_name().ok_or_else(|| {
                NodeError::Package(format!("Dir entry with no name: {}", entry.display()))
            })?;
            let name = name
                .to_str()
                .ok_or_else(|| {
                    NodeError::Package(format!("Non-UTF8 dir name: {:?}", name))
                })?
                .to_owned();
            if acowork_core::workspace::WORKSPACE_LOCAL_DIRS.contains(&name.as_str()) {
                continue;
            }

            if name == acowork_core::workspace::MEMORY_DIR {
                // The agent-private SQLite store (memory nodes,
                // conversation index and session meta in one file,
                // ADR-082). `store_files` returns the db plus whichever
                // sidecars exist; copying only the db would drop
                // un-replayed WAL content, so all of them go.
                let store_files = acowork_core::workspace::store_files(&src_workspace);
                if store_files.first().is_some_and(|db| db.exists()) {
                    let target_memory =
                        target_workspace.join(acowork_core::workspace::MEMORY_DIR);
                    std::fs::create_dir_all(&target_memory).map_err(|e| {
                        NodeError::Package(format!("Failed to create memory dir: {}", e))
                    })?;
                    for src in store_files.iter().filter(|f| f.exists()) {
                        let file_name = src.file_name().ok_or_else(|| {
                            NodeError::Package(format!(
                                "Store file has no name: {}",
                                src.display()
                            ))
                        })?;
                        std::fs::copy(src, target_memory.join(file_name)).map_err(|e| {
                            NodeError::Package(format!(
                                "Failed to copy {}: {}",
                                src.display(),
                                e
                            ))
                        })?;
                    }

                    // Strip session meta + conversation-index rows from
                    // the freshly-copied SQLite. `purge_conversation_state`
                    // keeps the `Episodic` / `Knowledge` / `Procedural`
                    // / `Autobiographical` memory nodes — those are the
                    // user's curated knowledge, not per-instance chat
                    // state. A corrupt or non-SQLite file (unlikely but
                    // possible on a hand-edited install) logs and
                    // continues rather than aborting the clone.
                    let copied_db = target_memory.join(acowork_core::workspace::STORE_FILE);
                    match acowork_sqlite::SqliteStore::open_dim_agnostic(&copied_db) {
                        Ok(store) => {
                            if let Err(e) = store.purge_conversation_state() {
                                tracing::warn!(
                                    error = %e,
                                    db = %copied_db.display(),
                                    "Failed to purge conversation state from cloned memory store; \
                                     clone may surface ghost sessions on first boot"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                db = %copied_db.display(),
                                "Could not open cloned memory file as SQLite; \
                                     skipping conversation-state purge"
                            );
                        }
                    }
                }
            } else {
                copy_dir_all(&entry, &target_workspace.join(&name))?;
            }
        }
    }

    // 7. Register cloned agent. Name handling:
    //
    //    - If the caller supplied a non-empty `new_name`, it wins and
    //      `display_name` is cleared so the UI falls back to `name`
    //      instead of showing the SOURCE's display_name on the clone.
    //    - Otherwise the source name is suffixed "(clone)" so the
    //      clone is recognisable in the list without forcing a rename.
    //
    //    `InstalledAgent.name` mirrors `manifest.name`; deriving the
    //    "(clone)" suffix twice used to produce two different strings
    //    for one agent (the table said "X (clone)", the manifest said
    //    "X").
    let clone_name = if new_name.trim().is_empty() {
        format!("{} (clone)", source_info.manifest.name)
    } else {
        new_name.trim().to_string()
    };
    new_manifest.name = clone_name.clone();
    new_manifest.display_name = None;
    let info = InstalledAgent {
        instance_id: new_instance_id,
        agent_id: new_agent_id.to_string(),
        version: new_manifest.version.clone(),
        name: clone_name,
        install_path: target_path.to_string_lossy().to_string(),
        manifest: new_manifest,
    };

    tracing::info!(
        "Cloned agent instance: {} ({} → {}, mode={:?})",
        info.instance_id,
        source_agent_id,
        new_agent_id,
        mode
    );
    state.add_installed(info.clone());

    Ok(info)
}

/// Recursively copy a directory
fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).map_err(|e| {
        NodeError::Package(format!(
            "Failed to create directory '{}': {}",
            dst.display(),
            e
        ))
    })?;

    let entries = std::fs::read_dir(src).map_err(|e| {
        NodeError::Package(format!(
            "Failed to read directory '{}': {}",
            src.display(),
            e
        ))
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| NodeError::Package(format!("Failed to read entry: {}", e)))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path).map_err(|e| {
                NodeError::Package(format!(
                    "Failed to copy '{}' to '{}': {}",
                    src_path.display(),
                    dst_path.display(),
                    e
                ))
            })?;
        }
    }

    Ok(())
}

/// Validate agent ID format (reverse-domain style, e.g. com.example.myagent)
fn is_valid_agent_id(agent_id: &str) -> bool {
    if agent_id.is_empty() || agent_id.len() > 128 {
        return false;
    }
    // Must have at least one dot
    if !agent_id.contains('.') {
        return false;
    }
    // Each segment must be non-empty and contain only alphanumeric + hyphen
    agent_id
        .split('.')
        .all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_alphanumeric() || c == '-'))
}

/// Is this top-level entry per-instance state rather than package content?
///
/// Reuses [`PACKAGE_ALWAYS_EXCLUDE_DIRS`] — the same list that keeps
/// `workspace/` and `runtime/` out of a published `.agent` — so a new
/// state directory is excluded from clones the day it is added there.
/// One list, no second copy to rot.
///
/// [`PACKAGE_ALWAYS_EXCLUDE_DIRS`]: acowork_core::packaging::PACKAGE_ALWAYS_EXCLUDE_DIRS
fn is_package_state_dir(name: &str) -> bool {
    acowork_core::packaging::PACKAGE_ALWAYS_EXCLUDE_DIRS.contains(&name)
}

/// Top-level entries of `dir`, sorted by name.
///
/// Sorted so a clone is reproducible and so a failure names the same
/// entry on every machine.
fn read_dir_sorted(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        NodeError::Package(format!(
            "Failed to read directory '{}': {}",
            dir.display(),
            e
        ))
    })?;
    let mut paths: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    paths.sort();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_test_agent(dir: &Path, agent_id: &str) {
        // The real package-level layout: prompts, skills and assets beside the
        // manifest; config/conversations/memory live in the workspace.
        let prompts_dir = dir.join("prompts");
        let skills_dir = dir.join("skills");
        let assets_dir = dir.join("assets");
        // Conversations and the private store live under the instance
        // WORKSPACE, as on a real install — not beside the package files.
        let workspace = acowork_core::workspace::workspace_dir(dir);
        let config_dir = workspace.join("config");
        let conversations_dir = workspace.join("conversations");
        let memory_dir = workspace.join(acowork_core::workspace::MEMORY_DIR);

        for d in &[
            &prompts_dir,
            &skills_dir,
            &assets_dir,
            &config_dir,
            &conversations_dir,
            &memory_dir,
        ] {
            std::fs::create_dir_all(d).unwrap();
        }

        let manifest = format!(
            r#"
            agent_id = "{}"
            version = "1.0.0"
            name = "Test Agent"
            display_name = "Source Display"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "openai"
            model = "gpt-4"
            "#,
            agent_id,
        );
        std::fs::write(dir.join("manifest.toml"), manifest).unwrap();
        std::fs::write(prompts_dir.join("system.md"), "You are a test agent.").unwrap();
        std::fs::write(config_dir.join("settings.toml"), "temperature = 0.7").unwrap();
        // The file the original bug report named: the user-curated list
        // of workspace directories. A clone without this forces the user
        // to re-add every directory they wired up.
        std::fs::write(
            config_dir.join("agent_workspaces.json"),
            r#"{"additional_dirs":[]}"#,
        )
        .unwrap();
        std::fs::write(skills_dir.join("search.md"), "# Search skill").unwrap();
        std::fs::write(assets_dir.join("avatar.txt"), "fake-png").unwrap();
        std::fs::write(
            conversations_dir.join("session.jsonl"),
            r#"{"role":"user","content":"hello"}"#,
        )
        .unwrap();
        // Real (empty) SQLite — clone now opens the freshly-copied file
        // to wipe session/conversation rows, and the previous fake-bytes
        // fixture corrupted the `-wal` sidecar through SQLite's
        // "file is not a database" handling. A real, write-less store
        // is the only thing that survives the open-and-purge step.
        let _ = acowork_sqlite::SqliteStore::open(
            memory_dir.join(acowork_core::workspace::STORE_FILE),
            4,
        )
        .expect("seed source SQLite");
        // files/ and logs/ live in WORKSPACE_LOCAL_DIRS — they exist on a
        // real install but must NOT survive a clone (host-specific paths,
        // runtime artefacts).
        std::fs::create_dir_all(workspace.join("files")).unwrap();
        std::fs::create_dir_all(workspace.join("logs")).unwrap();
        std::fs::write(workspace.join("files").join("upload.bin"), b"blob").unwrap();
        std::fs::write(workspace.join("logs").join("runtime.log"), b"log").unwrap();
    }

    fn add_agent_to_state(state: &mut NodeState, agent_id: &str, install_path: &str) {
        let manifest = acowork_core::AgentManifest::from_toml(&format!(
            r#"
            agent_id = "{}"
            version = "1.0.0"
            name = "Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "openai"
            model = "gpt-4"
            "#,
            agent_id
        ))
        .unwrap();
        state.add_installed(InstalledAgent {
            instance_id: format!("inst-{agent_id}"),
            agent_id: agent_id.to_string(),
            version: "1.0.0".to_string(),
            name: "Test Agent".to_string(),
            install_path: install_path.to_string(),
            manifest,
        });
    }

    #[test]
    fn test_clone_skeleton_success() {
        let temp_dir =
            std::env::temp_dir().join(format!("acowork-test-clone-sk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let source_dir = temp_dir.join("source");
        let install_dir = temp_dir.join("installed");
        setup_test_agent(&source_dir, "com.test.weather");

        let mut state = NodeState::new(16);
        add_agent_to_state(&mut state, "com.test.weather", &source_dir.to_string_lossy());

        let result = clone_agent(
            "inst-com.test.weather",
            "com.test.weather",
            "com.test.weather-clone",
            "Weather Copy",
            CloneMode::Skeleton,
            &install_dir,
            &mut state,
        );
        assert!(result.is_ok(), "Clone failed: {:?}", result.err());
        let info = result.unwrap();
        assert_eq!(info.agent_id, "com.test.weather-clone");
        assert!(info.manifest.dev, "Cloned agent should have dev=true");
        // Name comes from the caller, display_name is per-instance cosmetics
        // that must not ride along into a new instance.
        assert_eq!(info.manifest.name, "Weather Copy");
        assert_eq!(info.name, "Weather Copy");
        assert!(
            info.manifest.display_name.as_deref().unwrap_or("").is_empty(),
            "clone must not inherit the source display_name"
        );
        // ADR-073: the target is a fresh instance under {agent_id}/{uuid}/
        assert!(state.is_installed(&info.instance_id));

        // Skeleton = every PACKAGE file, whatever it happens to be called.
        // skills and assets are package content, so they must survive —
        // the allowlist this replaces dropped both, which is how a
        // skeleton clone came out with prompts and nothing else.
        let target = install_dir
            .join("com.test.weather-clone")
            .join(&info.instance_id);
        assert!(target.join("prompts").exists(), "prompts should be copied");
        assert!(
            target.join("skills").join("search.md").exists(),
            "skills are package content and must be copied in skeleton mode"
        );
        assert!(
            target.join("assets").join("avatar.txt").exists(),
            "assets are package content and must be copied in skeleton mode"
        );
        // The workspace is per-instance state — never cloned in skeleton
        // mode, and never rebuilt from package files.
        let target_workspace = acowork_core::workspace::workspace_dir(&target);
        assert!(
            !target_workspace.join("conversations").exists(),
            "skeleton mode must not copy conversations"
        );
        assert!(
            !target_workspace.join("config").exists(),
            "skeleton mode must not copy config — start from a clean instance"
        );
        assert!(
            !target_workspace
                .join(acowork_core::workspace::MEMORY_DIR)
                .exists(),
            "skeleton mode must not copy the private store"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    /// A package directory that is not on any allowlist still gets cloned.
    ///
    /// The regression guard for the class of bug above: the copy is
    /// driven by `PACKAGE_ALWAYS_EXCLUDE_DIRS`, so a directory added to
    /// the package format later is picked up without editing the clone.
    #[test]
    fn skeleton_copies_a_package_dir_the_clone_never_heard_of() {
        let temp_dir =
            std::env::temp_dir().join(format!("acowork-test-clone-new-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let source_dir = temp_dir.join("source");
        let install_dir = temp_dir.join("installed");
        setup_test_agent(&source_dir, "com.test.weather");
        // A directory the clone code has never been told about. If the
        // copy ever goes back to an allowlist, this is the file the test
        // watches fall off.
        std::fs::create_dir_all(source_dir.join("evals")).unwrap();
        std::fs::write(source_dir.join("evals").join("smoke.md"), "ok").unwrap();

        let mut state = NodeState::new(16);
        add_agent_to_state(&mut state, "com.test.weather", &source_dir.to_string_lossy());
        let info = clone_agent(
            "inst-com.test.weather",
            "com.test.weather",
            "com.test.weather-clone",
            "",
            CloneMode::Skeleton,
            &install_dir,
            &mut state,
        )
        .unwrap();

        let target = install_dir
            .join("com.test.weather-clone")
            .join(&info.instance_id);
        assert!(
            target.join("evals").join("smoke.md").exists(),
            "an unknown package dir must be cloned, not dropped"
        );
        // …while per-instance state is still held back.
        assert!(
            !acowork_core::workspace::workspace_dir(&target)
                .join("conversations")
                .exists(),
            "state must stay out of a skeleton clone"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_clone_full_success() {
        let temp_dir =
            std::env::temp_dir().join(format!("acowork-test-clone-full-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let source_dir = temp_dir.join("source");
        let install_dir = temp_dir.join("installed");
        setup_test_agent(&source_dir, "com.test.weather");

        let mut state = NodeState::new(16);
        add_agent_to_state(&mut state, "com.test.weather", &source_dir.to_string_lossy());

        let result = clone_agent(
            "inst-com.test.weather",
            "com.test.weather",
            "com.test.weather-full-clone",
            "",
            CloneMode::Full,
            &install_dir,
            &mut state,
        );
        assert!(result.is_ok(), "Full clone failed: {:?}", result.err());
        let info = result.unwrap();
        assert_eq!(info.agent_id, "com.test.weather-full-clone");

        let target = install_dir
            .join("com.test.weather-full-clone")
            .join(&info.instance_id);
        assert!(
            target.join("skills").exists(),
            "skills should be copied in full mode"
        );
        // The workspace is a SIBLING of the package files, so these two
        // assertions below are the regression guard for the flat-layout
        // bug: they fail if the copy reads the install dir directly.
        let target_workspace = acowork_core::workspace::workspace_dir(&target);
        // A full clone starts a fresh dialogue — the source's past
        // exchanges must not come along. `conversations/` lives in
        // WORKSPACE_LOCAL_DIRS and is skipped; assert both sides of that.
        assert!(
            !target_workspace.join("conversations").exists(),
            "conversations/ must NOT be cloned — a clone is a fresh dialogue, not a continuation"
        );
        assert!(
            !target.join("conversations").exists(),
            "conversations must not land beside the package files either"
        );
        assert!(
            target_workspace
                .join(acowork_core::workspace::MEMORY_DIR)
                .join(acowork_core::workspace::STORE_FILE)
                .exists(),
            "the agent-private store should be copied under workspace/"
        );
        assert!(
            !target.join(acowork_core::workspace::MEMORY_DIR).exists(),
            "the store must not land beside the package files"
        );
        // The full-mode regression that prompted this iteration: a
        // hand-curated config/ (the user picked a model, wired MCP
        // servers, added custom workspace dirs in agent_workspaces.json)
        // must travel with the clone. The earlier allowlist
        // (conversations + memory) dropped the whole directory.
        assert!(
            target_workspace.join("config").join("agent_workspaces.json").exists(),
            "workspace/config must travel with a full clone"
        );
        assert!(
            target_workspace.join("config").join("settings.toml").exists(),
            "every config file must travel — not just the one the bug report named"
        );
        // The denylist guards: files/ and logs/ are machine-local /
        // runtime artefacts even in full mode.
        assert!(
            !target_workspace.join("files").exists(),
            "files/ is host-specific; must not be cloned"
        );
        assert!(
            !target_workspace.join("logs").exists(),
            "logs/ are runtime artefacts; must not be cloned"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_clone_duplicate_agent_id() {
        let temp_dir =
            std::env::temp_dir().join(format!("acowork-test-clone-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let source_dir = temp_dir.join("source");
        let install_dir = temp_dir.join("installed");
        setup_test_agent(&source_dir, "com.test.weather");

        let mut state = NodeState::new(16);
        add_agent_to_state(
            &mut state,
            "com.test.weather-clone",
            &source_dir.to_string_lossy(),
        );
        add_agent_to_state(&mut state, "com.test.weather", &source_dir.to_string_lossy());

        let result = clone_agent(
            "inst-com.test.weather",
            "com.test.weather",
            "com.test.weather-clone", // same package id is now allowed —
            // ADR-073 distinguishes instances from packages
            "",
            CloneMode::Skeleton,
            &install_dir,
            &mut state,
        );
        // ADR-073: cloning to the same agent_id creates a NEW instance
        // (fresh UUID), so it no longer collides.
        assert!(result.is_ok(), "Same-package clone should succeed");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_clone_source_not_found() {
        let temp_dir =
            std::env::temp_dir().join(format!("acowork-test-clone-nf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let install_dir = temp_dir.join("installed");
        let mut state = NodeState::new(16);

        let result = clone_agent(
            "",
            "com.test.nonexistent",
            "com.test.new",
            "",
            CloneMode::Skeleton,
            &install_dir,
            &mut state,
        );
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_is_valid_agent_id() {
        assert!(is_valid_agent_id("com.example.weather"));
        assert!(is_valid_agent_id("com.test.my-agent"));
        assert!(is_valid_agent_id("io.acowork.system"));
        assert!(!is_valid_agent_id(""));
        assert!(!is_valid_agent_id("no-dots"));
        assert!(!is_valid_agent_id("com..empty"));
        assert!(!is_valid_agent_id("com.invalid!char"));
    }

    /// `Full`-mode clone of an agent with real session-meta +
    /// conversation-index rows must wipe those rows in lock-step with
    /// the `conversations/` directory being skipped, AND keep the
    /// user's curated memory nodes intact.
    ///
    /// The unit test of the strip itself lives next to
    /// `purge_conversation_state` in `acowork-sqlite`; this test only
    /// proves the clone pipeline actually does the cleanup at the right
    /// moment (after the file copy, before the cloned agent boots).
    #[test]
    fn full_clone_strips_session_and_conversation_rows_but_keeps_memory_nodes() {
        use acowork_core::session_meta::{SessionMeta, SessionMetaStore};
        use acowork_memory::types::Episode;
        use acowork_sqlite::{SqliteSessionMetaStore, SqliteStore};

        let temp_dir = std::env::temp_dir()
            .join(format!("acowork-test-clone-purge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let source_dir = temp_dir.join("source");
        let install_dir = temp_dir.join("installed");
        setup_test_agent(&source_dir, "com.test.weather");

        // Replace the fake `b"sqlite-data"` written by `setup_test_agent`
        // with a real SQLite populated with the rows a long-lived agent
        // would actually have.
        let workspace = acowork_core::workspace::workspace_dir(&source_dir);
        let db_path = workspace
            .join(acowork_core::workspace::MEMORY_DIR)
            .join(acowork_core::workspace::STORE_FILE);
        std::fs::remove_file(&db_path).unwrap();
        let store = SqliteStore::open(&db_path, 4).unwrap();

        // Session-meta rows — three sessions that would otherwise
        // dangle in the cloned SQLite and surface in the session list
        // despite their JSONL files having been deliberately skipped.
        let store_arc = std::sync::Arc::new(store);
        let meta_store = SqliteSessionMetaStore::new(store_arc.clone());
        for sid in ["s1", "s2", "s3"] {
            meta_store
                .upsert(&SessionMeta {
                    version: 1,
                    session_id: sid.to_string(),
                    agent_id: "com.test.weather".to_string(),
                    created_at: chrono::Utc::now().to_rfc3339(),
                    user_id: None,
                    visibility: None,
                    chat_session_id: None,
                    title: Some("Test session".to_string()),
                    workspace_id: None,
                    model: None,
                    provider: None,
                    account_id: None,
                    reasoning_effort: None,
                    temperature: None,
                    context_window: None,
                    todos: None,
                    message_count: 0,
                    llm_call_counter: None,
                    perf: None,
                    model_ratio: None,
                    last_compaction_offset: None,
                    last_active_at: chrono::Utc::now().to_rfc3339(),
                    tokens: None,
                    corrupted: false,
                    model_usage: None,
                })
                .unwrap();
        }

        // Conversation-index rows — one `ConversationMessage` node per
        // session so the FK cascade and `fts_conversation` clear are
        // both exercised.
        let conv_store = acowork_sqlite::conversation::ConversationStore::from_store(
            store_arc.clone(),
        )
        .unwrap();
        for (sid, idx) in [("s1", 0usize), ("s1", 1), ("s2", 0), ("s3", 0)] {
            conv_store
                .index_message(sid, idx, "user", "hello", &[0.0; 4])
                .unwrap();
        }

        // Memory node the user spent the afternoon curating — must
        // survive the purge.
        store_arc
            .store_episode(&Episode {
                session_id: "s1".to_string(),
                turn_index: 0,
                role: "user".to_string(),
                content: "remembered fact".to_string(),
                embedding: None,
                timestamp: chrono::Utc::now(),
                consolidated: false,
                metadata: Default::default(),
                importance: 0.5,
                knowledge_subtype: None,
                normalized: None,
            })
            .unwrap();
        drop(conv_store);
        drop(meta_store);
        drop(store_arc);

        let mut state = NodeState::new(16);
        add_agent_to_state(&mut state, "com.test.weather", &source_dir.to_string_lossy());

        let result = clone_agent(
            "inst-com.test.weather",
            "com.test.weather",
            "com.test.weather-purge",
            "",
            CloneMode::Full,
            &install_dir,
            &mut state,
        );
        assert!(result.is_ok(), "Full clone failed: {:?}", result.err());
        let info = result.unwrap();

        let target = install_dir
            .join("com.test.weather-purge")
            .join(&info.instance_id);
        let target_workspace = acowork_core::workspace::workspace_dir(&target);
        let cloned_db = target_workspace
            .join(acowork_core::workspace::MEMORY_DIR)
            .join(acowork_core::workspace::STORE_FILE);

        let cloned = SqliteStore::open(&cloned_db, 4).unwrap();

        // The purge: every session_meta row gone, every
        // ConversationMessage node + its fts_conversation row gone.
        let cloned_arc = std::sync::Arc::new(cloned);
        assert_eq!(
            SqliteSessionMetaStore::new(cloned_arc.clone())
                .list_recent(usize::MAX)
                .unwrap()
                .len(),
            0,
            "session_meta rows must not survive a clone"
        );
        assert_eq!(
            cloned_arc
                .node_count_by_label(acowork_sqlite::conversation::LABEL)
                .unwrap(),
            0,
            "conversation-index nodes must not survive a clone"
        );

        // The preservation: the user's curated memory is intact.
        assert!(
            cloned_arc.node_count_by_label("Episodic").unwrap() >= 1,
            "Episodic memory nodes must survive a clone — the purge is conversation-only"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
