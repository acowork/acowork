//! Workspace directory resolver — manages the list of allowed directories.
//!
//! ## Responsibility split (Session-level workspace refactor)
//!
//! - **WorkspaceResolver** (this module): manages the Agent-level workspace
//!   list — allowed directories, path validation, search directories.
//!   *No longer owns "current directory" selection* — that belongs to
//!   `SessionManager` per session.
//!
//! - **SessionManager**: owns per-session `current_dir` mapping
//!   (`session_id → workspace_id`). Tool execution reads
//!   `SessionHandle::current_work_dir` directly (shared Arc with
//!   `SessionCore`), bypassing `WorkspaceResolver` re-resolution.
//!
//! ## Key APIs
//!
//! 1. **`agent_home()`**: the agent's install directory.
//!    Used for runtime data: conversations, memory, logs, identity.
//!
//! 2. **`search_dirs()`**: directories to search (content_search / glob_search).
//!    All workspace directories. Falls back to `[agent_home]`.
//!
//! 3. **`allowed_dirs()`**: all allowed dirs (for PathGuardedTool validation).
//!
//! 4. **`find_by_id()`**: look up a workspace by its ID.

use serde::Deserialize;
use std::path::Path;
use std::sync::{Arc, RwLock};

/// Shared, hot-reloadable workspace resolver.
///
/// Wrapped in Arc<RwLock<>> so tools share one reference and see
/// workspace changes immediately when Gateway pushes WorkspaceConfigUpdate.
pub type SharedResolver = Arc<RwLock<WorkspaceResolver>>;

/// Access level for a workspace directory
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceAccess {
    ReadOnly,
    ReadWrite,
}

impl WorkspaceAccess {
    /// Convert to the string representation used in JSON serialization.
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkspaceAccess::ReadOnly => "read-only",
            WorkspaceAccess::ReadWrite => "read-write",
        }
    }
}

impl std::fmt::Display for WorkspaceAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl serde::Serialize for WorkspaceAccess {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for WorkspaceAccess {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "read-write" => Ok(WorkspaceAccess::ReadWrite),
            _ => Ok(WorkspaceAccess::ReadOnly),
        }
    }
}

/// A single workspace directory entry
#[derive(Clone, Debug)]
pub struct WorkspaceDir {
    pub id: String,
    pub path: String,
    pub access: WorkspaceAccess,
    /// Whether this was the last active workspace when the user last selected it.
    /// Used as the default workspace for new sessions.
    pub last_active: bool,
    /// Prompt file to inject into system prompt (e.g. "CLAUDE.md", "AGENTS.md").
    pub prompt_file: Option<String>,
}

/// Central resolver for workspace directories.
///
/// Constructed once at startup from the agent's `work_dir`.
/// Reads `agent_workspaces.json` to discover user-configured directories.
///
/// Does NOT manage "current directory" — that is per-session state
/// owned by `SessionManager`.
#[derive(Clone, Debug)]
pub struct WorkspaceResolver {
    /// Agent install dir (for logs, conversations, memory, identity)
    agent_home: String,
    /// All allowed dirs from agent_workspaces.json + fallbacks
    allowed_dirs: Vec<WorkspaceDir>,
    /// `id → alias`, the human's own label for a workspace.
    ///
    /// Kept beside [`Self::allowed_dirs`] rather than on [`WorkspaceDir`]
    /// because it is **display data, not access data**: nothing that enforces
    /// a boundary (path validation, search roots, prompt files) has any
    /// business reading it, while everything that renders a workspace at a
    /// human does (`display_name`). Separate field, separate audience.
    aliases: std::collections::HashMap<String, String>,
}

impl WorkspaceResolver {
    /// Build a resolver from the agent's work_dir.
    ///
    /// Reads `agent_workspaces.json` from `work_dir` to discover
    /// user-configured workspace directories.
    pub fn new(work_dir: &str) -> Self {
        let allowed_dirs = load_workspace_dirs(work_dir);
        let aliases = load_workspace_aliases(work_dir);
        Self {
            agent_home: work_dir.to_string(),
            allowed_dirs,
            aliases,
        }
    }

    /// Reload the resolver from disk (re-reads agent_workspaces.json).
    ///
    /// Used after receiving a `WorkspaceConfigUpdate` from Gateway, which
    /// writes the updated config to disk before calling this.
    pub fn reload(work_dir: &str) -> Self {
        Self::new(work_dir)
    }

    /// Agent home dir (for conversations, memory, logs, identity, etc.)
    pub fn agent_home(&self) -> &str {
        &self.agent_home
    }

    /// Look up a workspace directory by its ID.
    /// Returns None if not found.
    pub fn find_by_id(&self, id: &str) -> Option<&WorkspaceDir> {
        self.allowed_dirs.iter().find(|d| d.id == id)
    }

    /// Look up a workspace directory by its ID with a trace-level
    /// breadcrumb. Returns the same `Option<&WorkspaceDir>` as
    /// [`Self::find_by_id`] — the only added side-effect is a single
    /// `tracing::trace!` line recording the lookup, its outcome, and
    /// the loaded id list. Useful when correlating a "workspace
    /// disappeared on restart" report with the actual id set the
    /// resolver had at startup. See
    /// `desktop-onboarding-bugfix_154b7ff7.md` §Fix 3.
    pub fn find_by_id_with_trace(&self, id: &str) -> Option<&WorkspaceDir> {
        let result = self.find_by_id(id);
        let loaded_ids: Vec<&str> = self.allowed_dirs.iter().map(|d| d.id.as_str()).collect();
        tracing::trace!(
            queried_id = %id,
            found = result.is_some(),
            loaded_count = loaded_ids.len(),
            loaded_ids = ?loaded_ids,
            "WorkspaceResolver::find_by_id"
        );
        result
    }

    /// The workspace as a human should read it: the alias the user gave it,
    /// else the directory's own last segment, else `None`.
    ///
    /// This is the single home of the ID→name rule for every surface that
    /// puts a workspace in front of a person or in front of a model that
    /// writes for a person. `ws-49616826fcfc` is an address, not a name: a
    /// reader who is shown it learns nothing, and a chat background that said
    /// "工作区 ws-49616826fcfc" is exactly the reported defect. Desktop and
    /// mobile already apply the same rule (`workspaceDisplayName` in
    /// `WorkspaceSelector.tsx`, the `alias || tail(path)` label in
    /// `AgentDrawer.tsx`) — this is its runtime counterpart.
    ///
    /// `None` means "nothing better than the id exists" and the caller keeps
    /// showing the id. Never returns an empty string: an alias of blanks is
    /// the same as no alias.
    pub fn display_name(&self, id: &str) -> Option<String> {
        if id == "__agent_home__" {
            return Some("agent home".to_string());
        }
        if let Some(alias) = self.aliases.get(id) {
            let alias = alias.trim();
            if !alias.is_empty() {
                return Some(alias.to_string());
            }
        }
        let path = &self.find_by_id(id)?.path;
        let name = Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .filter(|n| !n.trim().is_empty())?;
        Some(name)
    }

    /// All searchable directories (for content_search / glob_search).
    ///
    /// Returns all workspace directories. If no workspaces are configured,
    /// returns `[agent_home]`.
    pub fn search_dirs(&self) -> Vec<&str> {
        if self.allowed_dirs.is_empty() {
            vec![&self.agent_home]
        } else {
            self.allowed_dirs.iter().map(|d| d.path.as_str()).collect()
        }
    }

    /// All allowed dirs (for PathGuardedTool path validation).
    pub fn allowed_dirs(&self) -> &[WorkspaceDir] {
        &self.allowed_dirs
    }

    /// Return the workspace ID of the last active workspace, if any.
    /// Checks the `last_active` flag on each user-configured workspace.
    /// Returns `None` if no workspace has `last_active` set.
    pub fn last_active_workspace_id(&self) -> Option<&str> {
        self.allowed_dirs
            .iter()
            .find(|d| d.last_active)
            .map(|d| d.id.as_str())
    }

    /// Read and return the prompt file content for a workspace.
    /// Returns None if no prompt_file is configured or the file doesn't exist.
    pub fn read_prompt_file(&self, workspace_id: &str) -> Option<String> {
        let ws = self.find_by_id(workspace_id)?;
        let prompt_file = ws.prompt_file.as_ref()?;
        let file_path = Path::new(&ws.path).join(prompt_file);
        std::fs::read_to_string(&file_path).ok()
    }

    /// Create a resolver directly from a list of workspace directories.
    /// For tests only — bypasses disk I/O so tests can inject arbitrary
    /// workspace configurations without creating `agent_workspaces.json`.
    pub fn new_for_test(allowed_dirs: Vec<WorkspaceDir>) -> Self {
        Self {
            agent_home: "/tmp/agent-home".to_string(),
            allowed_dirs,
            aliases: std::collections::HashMap::new(),
        }
    }
}

/// Load `id → alias` from `agent_workspaces.json`.
///
/// A second pass over the same file `load_workspace_dirs` reads, on purpose:
/// the two consumers want different things from the entry (enforcement vs
/// display), and threading an alias through `WorkspaceDir` would make every
/// synthetic `WorkspaceDir` literal — the package root, the agent home, and
/// every test fixture — carry a field it can never fill. A missing or
/// unreadable file simply yields no aliases; `display_name` then falls back to
/// the directory name.
fn load_workspace_aliases(work_dir: &str) -> std::collections::HashMap<String, String> {
    #[derive(Deserialize)]
    struct WorkspaceConfig {
        #[serde(default)]
        additional_dirs: Vec<AliasEntry>,
    }

    #[derive(Deserialize)]
    struct AliasEntry {
        id: String,
        #[serde(default)]
        alias: Option<String>,
    }

    let path = Path::new(work_dir)
        .join("config")
        .join("agent_workspaces.json");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return std::collections::HashMap::new();
    };
    let Ok(config) = serde_json::from_str::<WorkspaceConfig>(&content) else {
        return std::collections::HashMap::new();
    };
    config
        .additional_dirs
        .into_iter()
        .filter_map(|e| {
            let alias = e.alias?;
            let alias = alias.trim();
            (!alias.is_empty()).then(|| (e.id, alias.to_string()))
        })
        .collect()
}

/// Load workspace directories from `agent_workspaces.json`.
fn load_workspace_dirs(work_dir: &str) -> Vec<WorkspaceDir> {
    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct WorkspaceConfig {
        #[serde(default)]
        additional_dirs: Vec<WorkspaceDirEntry>,
    }

    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct WorkspaceDirEntry {
        id: String,
        path: String,
        alias: Option<String>,
        access: String,
        added_at: String,
        #[serde(default)]
        last_active: bool,
        #[serde(default)]
        prompt_file: Option<String>,
    }

    let config_path = Path::new(work_dir)
        .join("config")
        .join("agent_workspaces.json");

    if !config_path.exists() {
        tracing::warn!(
            work_dir,
            config_path = %config_path.display(),
            "No agent_workspaces.json found, using work_dir as default"
        );
        return fallback_dirs(work_dir);
    }

    match std::fs::read_to_string(&config_path) {
        Ok(content) => match serde_json::from_str::<WorkspaceConfig>(&content) {
            Ok(config) => {
                let mut dirs: Vec<WorkspaceDir> = Vec::new();

                for entry in config.additional_dirs.into_iter() {
                    dirs.push(WorkspaceDir {
                        id: entry.id,
                        path: entry.path,
                        access: if entry.access == "read-write" {
                            WorkspaceAccess::ReadWrite
                        } else {
                            WorkspaceAccess::ReadOnly
                        },
                        last_active: entry.last_active,
                        prompt_file: entry.prompt_file,
                    });
                }

                // Include package root (parent of work_dir) as read-only
                if let Some(package_root) = Path::new(work_dir).parent() {
                    let package_root_str = package_root.to_string_lossy().to_string();
                    if package_root_str != work_dir {
                        dirs.push(WorkspaceDir {
                            id: "__package_root__".to_string(),
                            path: package_root_str,
                            access: WorkspaceAccess::ReadOnly,
                            last_active: false,
                            prompt_file: None,
                        });
                    }
                }

                // Always include agent_home as read-write
                dirs.push(WorkspaceDir {
                    id: "__agent_home__".to_string(),
                    path: work_dir.to_string(),
                    access: WorkspaceAccess::ReadWrite,
                    last_active: false,
                    prompt_file: None,
                });

                tracing::info!(
                    work_dir,
                    count = dirs.len(),
                    dirs = ?dirs.iter().map(|d| d.path.as_str()).collect::<Vec<_>>(),
                    "Loaded workspace directories from agent_workspaces.json"
                );

                dirs
            }
            Err(e) => {
                tracing::error!(
                    work_dir,
                    error = %e,
                    "Failed to parse agent_workspaces.json, using work_dir as default"
                );
                fallback_dirs(work_dir)
            }
        },
        Err(e) => {
            tracing::error!(
                work_dir,
                error = %e,
                "Failed to read agent_workspaces.json, using work_dir as default"
            );
            fallback_dirs(work_dir)
        }
    }
}

/// Fallback: use work_dir as the only allowed directory
fn fallback_dirs(work_dir: &str) -> Vec<WorkspaceDir> {
    let mut dirs = vec![];

    // Include package root as read-only
    if let Some(package_root) = Path::new(work_dir).parent() {
        let package_root_str = package_root.to_string_lossy().to_string();
        if package_root_str != work_dir {
            dirs.push(WorkspaceDir {
                id: "__package_root__".to_string(),
                path: package_root_str,
                access: WorkspaceAccess::ReadOnly,
                last_active: false,
                prompt_file: None,
            });
        }
    }

    dirs.push(WorkspaceDir {
        id: "__agent_home__".to_string(),
        path: work_dir.to_string(),
        access: WorkspaceAccess::ReadWrite,
        last_active: false,
        prompt_file: None,
    });

    dirs
}

// ── Persistence & Formatting ───────────────────────────────────────────────

/// Write workspace config JSON to `agent_workspaces.json` atomically (tmp + rename).
///
/// On Windows, `std::fs::rename` fails if the target file exists and is open
/// (e.g. another thread calling `reload`). We work around this by removing
/// the old file first. The tmp path uses the full base name so it's clear
/// which file the temp belongs to.
pub fn write_workspace_config(work_dir: &str, config_json: &str) -> Result<(), String> {
    let config_dir = Path::new(work_dir).join("config");
    std::fs::create_dir_all(&config_dir)
        .map_err(|e| format!("Failed to create config dir: {}", e))?;

    let config_path = config_dir.join("agent_workspaces.json");
    let tmp_path = config_dir.join("agent_workspaces.json.tmp");
    std::fs::write(&tmp_path, config_json)
        .map_err(|e| format!("Failed to write temp config: {}", e))?;
    // Windows: rename fails if target exists and is open, so remove first.
    // On Unix this is a no-op if the rename succeeds, but the remove is
    // harmless and ensures Windows compatibility.
    let _ = std::fs::remove_file(&config_path);
    std::fs::rename(&tmp_path, &config_path)
        .map_err(|e| format!("Failed to rename config: {}", e))?;

    tracing::info!(
        work_dir,
        "Wrote agent_workspaces.json from Gateway WorkspaceConfigUpdate"
    );
    Ok(())
}

/// Format workspace context for a specific session.
///
/// Takes the WorkspaceResolver (for the full allowed list + agent_home),
/// and the session's current workspace selection.
/// `current_ws_id` of `"__agent_home__"` means agent home is active.
pub fn format_workspace_context_for_session(
    resolver: &WorkspaceResolver,
    current_ws_id: &str,
) -> String {
    // Determine current directory path
    let (current_path, current_alias, current_access, is_agent_home) =
        if current_ws_id == "__agent_home__" {
            (
                resolver.agent_home().to_string(),
                None::<String>,
                None,
                true,
            )
        } else {
            match resolver.find_by_id(current_ws_id) {
                Some(dir) => (
                    dir.path.clone(),
                    resolver.display_name(current_ws_id),
                    Some(dir.access.clone()),
                    false,
                ),
                None => (resolver.agent_home().to_string(), None, None, true),
            }
        };

    let mut buf = String::new();
    buf.push_str("## Workspace Environment\n\n");

    // 1. Current Working Directory
    if is_agent_home {
        buf.push_str(&format!(
            "Current Working Directory: {} (agent home)\n",
            escape_md(&current_path)
        ));
    } else {
        let alias = current_alias.as_deref().unwrap_or("-");
        let access_str = current_access
            .as_ref()
            .map(|a| a.as_str())
            .unwrap_or("read-write");
        buf.push_str(&format!(
            "Current Working Directory: {} ({}, {})\n",
            escape_md(&current_path),
            alias,
            access_str,
        ));
        buf.push_str("This is your currently active workspace.\n\n");
    }

    // 2. Agent Home Directory
    buf.push_str(&format!(
        "Agent Home Directory: {} (installation directory)\n\n",
        escape_md(resolver.agent_home())
    ));

    // 3. Available Workspaces table (user workspaces only, not synthetic entries)
    let user_workspaces: Vec<&WorkspaceDir> = resolver
        .allowed_dirs()
        .iter()
        .filter(|d| d.id != "__agent_home__" && d.id != "__package_root__")
        .collect();

    if !user_workspaces.is_empty() {
        buf.push_str("### Available Workspaces\n");
        buf.push_str("| # | Path | Access |\n");
        buf.push_str("|---|------|--------|\n");

        for (i, ws) in user_workspaces.iter().enumerate() {
            let active_marker = if ws.id == current_ws_id { " *" } else { "" };
            buf.push_str(&format!(
                "| {} | {} | {} |\n",
                i + 1,
                escape_md(&ws.path),
                ws.access,
            ));
            let _ = active_marker;
        }
    }

    buf.push_str(
        "\nIMPORTANT: When performing file operations or running shell commands, ALWAYS use the\n",
    );
    buf.push_str("Current Working Directory path shown above as your starting directory.\n");
    buf.push_str(
        "Do NOT use the Agent Home Directory for project work — it contains the agent's own\n",
    );
    buf.push_str("configuration files, not your project code.\n");
    buf.push_str(
        "All listed directories are authorized for access at the indicated permission level.\n",
    );

    buf
}

/// Escape special characters for Markdown table cells.
fn escape_md(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ").replace('\r', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolver_no_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let resolver = WorkspaceResolver::new(dir.path().to_str().unwrap());
        assert_eq!(resolver.agent_home(), dir.path().to_str().unwrap());
        // find_by_id should work for synthetic entries
        assert!(resolver.find_by_id("__agent_home__").is_some());
        assert_eq!(
            resolver.find_by_id("__agent_home__").unwrap().path,
            dir.path().to_str().unwrap()
        );
    }

    #[test]
    fn test_resolver_with_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let config = r#"{
            "version": "1.0.0",
            "additional_dirs": [
                {
                    "id": "ws-1",
                    "path": "D:\\projects\\my-project",
                    "alias": "my-project",
                    "access": "read-write",
                    "added_at": "2026-05-01T00:00:00Z",
                    "last_active": true
                },
                {
                    "id": "ws-2",
                    "path": "D:\\projects\\other",
                    "alias": "other",
                    "access": "read-only",
                    "added_at": "2026-05-01T00:00:00Z"
                }
            ]
        }"#;
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(
            dir.path().join("config").join("agent_workspaces.json"),
            config,
        )
        .unwrap();

        let resolver = WorkspaceResolver::new(dir.path().to_str().unwrap());
        assert_eq!(resolver.agent_home(), dir.path().to_str().unwrap());

        // find_by_id should locate user workspaces
        let ws1 = resolver.find_by_id("ws-1").unwrap();
        assert_eq!(ws1.path, "D:\\projects\\my-project");
        let ws2 = resolver.find_by_id("ws-2").unwrap();
        assert_eq!(ws2.path, "D:\\projects\\other");

        // search_dirs should include all workspace dirs
        let search = resolver.search_dirs();
        assert!(search.iter().any(|d| d.contains("my-project")));
        assert!(search.iter().any(|d| d.contains("other")));
    }

    /// Contract: a workspace shown to a human is shown by its **name** — alias
    /// first, directory name second, nothing invented third.
    ///
    /// Reported defect (2026-11-06): a chat background read "工作区
    /// ws-49616826fcfc". The rule already existed on both clients
    /// (`workspaceDisplayName` / `alias || tail(path)`); this pins the runtime
    /// half of it so the router cannot be the one surface that has no name.
    #[test]
    fn display_name_prefers_alias_then_directory_name() {
        let dir = tempfile::tempdir().unwrap();
        let config = r#"{
            "version": "1.0.0",
            "additional_dirs": [
                { "id": "ws-1", "path": "D:/projects/my-project", "alias": "ACoworkDev",
                  "access": "read-write", "added_at": "2026-05-01T00:00:00Z" },
                { "id": "ws-2", "path": "D:/projects/no-alias",
                  "access": "read-write", "added_at": "2026-05-01T00:00:00Z" },
                { "id": "ws-3", "path": "D:/projects/blank-alias", "alias": "   ",
                  "access": "read-write", "added_at": "2026-05-01T00:00:00Z" }
            ]
        }"#;
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(
            dir.path().join("config").join("agent_workspaces.json"),
            config,
        )
        .unwrap();
        let resolver = WorkspaceResolver::new(dir.path().to_str().unwrap());

        assert_eq!(resolver.display_name("ws-1").as_deref(), Some("ACoworkDev"));
        // No alias → the directory's own last segment, which is what a human
        // recognises when the path row says nothing.
        assert_eq!(resolver.display_name("ws-2").as_deref(), Some("no-alias"));
        // Blank alias is the same as no alias — never an empty name.
        assert_eq!(
            resolver.display_name("ws-3").as_deref(),
            Some("blank-alias")
        );
        // A workspace the registry no longer has gets no name at all: the
        // caller falls back to the id, which is honest.
        assert_eq!(resolver.display_name("ws-gone"), None);
        // The synthetic id the composer uses is not a name either.
        assert_eq!(
            resolver.display_name("__agent_home__").as_deref(),
            Some("agent home")
        );
    }

    #[test]
    fn test_format_workspace_context_for_session() {
        let dir = tempfile::tempdir().unwrap();
        let config = r#"{
            "version": "1.0.0",
            "additional_dirs": [
                {
                    "id": "ws-1",
                    "path": "D:\\projects\\my-project",
                    "alias": "my-project",
                    "access": "read-write",
                    "added_at": "2026-05-01T00:00:00Z"
                }
            ]
        }"#;
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(
            dir.path().join("config").join("agent_workspaces.json"),
            config,
        )
        .unwrap();

        let resolver = WorkspaceResolver::new(dir.path().to_str().unwrap());

        // Agent home as current
        let ctx = format_workspace_context_for_session(&resolver, "__agent_home__");
        assert!(ctx.contains("(agent home)"));
        assert!(ctx.contains("my-project"));

        // User workspace as current
        let ctx = format_workspace_context_for_session(&resolver, "ws-1");
        assert!(ctx.contains("my-project"));
        assert!(ctx.contains("read-write"));
    }

    /// Contract: `read_prompt_file` is the function that surfaces AGENTS.md
    /// content into the system prompt. It is called from
    /// `SessionManager::update_session_workspace_context`, which itself is
    /// invoked during session creation. The bug that motivated this test was
    /// that session creation paths other than the initial startup never
    /// invoked that flow, so the prompt file was never read for new sessions.
    /// This test pins down the resolver-side behavior so any regression in
    /// `SessionManager`'s wiring is caught at the resolver level too.
    #[test]
    fn test_read_prompt_file_returns_content_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        let workspace_path = dir.path().join("my-project");
        std::fs::create_dir_all(&workspace_path).unwrap();

        // Write the prompt file that the workspace will reference.
        let prompt_content = "# Project Rules\n\nFollow these conventions.\n";
        std::fs::write(workspace_path.join("AGENTS.md"), prompt_content).unwrap();

        // Build a config that points at the workspace AND configures the
        // prompt file path. Resolver reads both pieces of metadata.
        let config = format!(
            r#"{{
                "version": "1.0.0",
                "additional_dirs": [
                    {{
                        "id": "ws-1",
                        "path": "{}",
                        "access": "read-write",
                        "added_at": "2026-05-01T00:00:00Z",
                        "prompt_file": "AGENTS.md"
                    }}
                ]
            }}"#,
            workspace_path.display().to_string().replace('\\', "\\\\")
        );
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(
            dir.path().join("config").join("agent_workspaces.json"),
            config,
        )
        .unwrap();

        let resolver = WorkspaceResolver::new(dir.path().to_str().unwrap());
        let content = resolver.read_prompt_file("ws-1");
        assert_eq!(content.as_deref(), Some(prompt_content));
    }

    #[test]
    fn test_read_prompt_file_returns_none_when_not_configured() {
        let dir = tempfile::tempdir().unwrap();
        let config = r#"{
            "version": "1.0.0",
            "additional_dirs": [
                {
                    "id": "ws-1",
                    "path": "D:\\projects\\my-project",
                    "access": "read-write",
                    "added_at": "2026-05-01T00:00:00Z"
                }
            ]
        }"#;
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(
            dir.path().join("config").join("agent_workspaces.json"),
            config,
        )
        .unwrap();

        let resolver = WorkspaceResolver::new(dir.path().to_str().unwrap());
        assert_eq!(resolver.read_prompt_file("ws-1"), None);
    }

    #[test]
    fn test_read_prompt_file_returns_none_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let config = r#"{
            "version": "1.0.0",
            "additional_dirs": [
                {
                    "id": "ws-1",
                    "path": "D:\\nonexistent\\path",
                    "access": "read-write",
                    "added_at": "2026-05-01T00:00:00Z",
                    "prompt_file": "AGENTS.md"
                }
            ]
        }"#;
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(
            dir.path().join("config").join("agent_workspaces.json"),
            config,
        )
        .unwrap();

        let resolver = WorkspaceResolver::new(dir.path().to_str().unwrap());
        // Path doesn't exist on disk, so read fails and returns None —
        // never panics on a missing file.
        assert_eq!(resolver.read_prompt_file("ws-1"), None);
    }
}
