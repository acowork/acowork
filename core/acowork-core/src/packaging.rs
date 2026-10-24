//! Agent package data isolation types and constants.
//!
//! Defines `PackageOptions` for controlling which data items are included
//! when building an `.agent` package, along with directory/pattern exclusion
//! constants. The default excludes conversation files and user configs to
//! protect user privacy.
//!
//! Memory-subset toggles (`include_episodes`, `include_procedural`, ...) used
//! to live here. They were never effective — packaging already excluded the
//! whole `memory/` directory — and ADR-100 removed them with the rest of the
//! in-package memory: memories now live in the memory service's instance
//! namespace, so a package directory has no memory to select a subset of.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Always-exclude constants
// ---------------------------------------------------------------------------

/// Directories that are always excluded when building an .agent package.
///
/// These contain runtime data that should never be shipped inside a
/// distributable `.agent` file.
pub const PACKAGE_ALWAYS_EXCLUDE_DIRS: &[&str] = &[
    "memory",    // Legacy in-workspace store dir (ADR-082, pre-ADR-100 layout)
    "workspace", // User workspace state
    "runtime",   // Runtime temporary files
];

/// File patterns to always exclude when building an .agent package.
pub const PACKAGE_EXCLUDE_PATTERNS: &[&str] = &["*.log", "*.tmp"];

/// Directories excluded by default but user-can-include via packaging UI.
pub const PACKAGE_DEFAULT_EXCLUDE_DIRS: &[&str] = &[
    "conversations", // JSONL files (user dialog)
    "config",        // User configs
];

// ---------------------------------------------------------------------------
// PackageOptions
// ---------------------------------------------------------------------------

/// Packaging options specified by user via checklist UI.
///
/// Controls which data items are included when building an `.agent` package.
/// Defaults follow the "exclude private data by default" principle from
/// the design document (§5.2 of 15-conversation-persistence.md): `Default`
/// is derived, so every flag starts `false` (nothing private ships).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PackageOptions {
    /// Include conversation JSONL files (default: false).
    ///
    /// When `false`, the `conversations/` directory is skipped entirely.
    /// When `true`, all `.jsonl` files under `conversations/` are included.
    pub include_conversations: bool,

    /// Include user config directory (default: false).
    ///
    /// User configurations may contain preferences or settings specific
    /// to a particular installation.
    pub include_config: bool,
}

/// Check whether a relative path should be excluded from the package.
///
/// Returns `true` if the path should be skipped based on the always-exclude
/// list, file patterns, or the user's `PackageOptions`.
pub fn should_exclude_path(relative_path: &str, options: &PackageOptions) -> bool {
    // Always exclude: memory/, workspace/, runtime/
    for dir in PACKAGE_ALWAYS_EXCLUDE_DIRS {
        if relative_path.starts_with(dir) {
            return true;
        }
    }

    // Always exclude: *.log, *.tmp
    for pattern in PACKAGE_EXCLUDE_PATTERNS {
        if glob_match(pattern, relative_path) {
            return true;
        }
    }

    // Default-exclude directories (user can include via options)
    if relative_path.starts_with("conversations") && !options.include_conversations {
        return true;
    }
    if relative_path.starts_with("config") && !options.include_config {
        return true;
    }

    false
}

/// Simple glob-style match for file patterns like "*.log" and "*.tmp".
///
/// Only supports trailing wildcard patterns (e.g., `*.ext`).
fn glob_match(pattern: &str, path: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix('*') {
        path.ends_with(suffix)
    } else {
        path == pattern
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_package_options_default() {
        let opts = PackageOptions::default();
        assert!(
            !opts.include_conversations,
            "conversations should default to excluded"
        );
        assert!(!opts.include_config, "config should default to excluded");
    }

    #[test]
    fn test_package_options_all_included() {
        let opts = PackageOptions {
            include_conversations: true,
            include_config: true,
        };
        assert!(opts.include_conversations);
        assert!(opts.include_config);
    }

    /// ADR-100 decision 11: memory data is not a packaging choice any more.
    /// Whatever the toggles say, a `memory/` path never travels — and since the
    /// service took the memories over, there is normally no such directory in a
    /// package dir to begin with.
    #[test]
    fn memory_paths_are_excluded_whatever_the_options_say() {
        let opts = PackageOptions {
            include_conversations: true,
            include_config: true,
        };
        assert!(should_exclude_path("memory/private.sqlite", &opts));
        assert!(should_exclude_path("memory/private.sqlite-wal", &opts));
    }

    #[test]
    fn test_should_exclude_always_exclude_dirs() {
        let opts = PackageOptions::default();
        assert!(should_exclude_path("memory/private.sqlite", &opts));
        assert!(should_exclude_path("workspace/state.json", &opts));
        assert!(should_exclude_path("runtime/lock.pid", &opts));
    }

    #[test]
    fn test_should_exclude_log_and_tmp_files() {
        let opts = PackageOptions::default();
        assert!(should_exclude_path("debug.log", &opts));
        assert!(should_exclude_path("temp.tmp", &opts));
        assert!(should_exclude_path("some/dir/debug.log", &opts));
        assert!(!should_exclude_path("manifest.toml", &opts));
    }

    #[test]
    fn test_should_exclude_conversations_default() {
        let opts = PackageOptions::default();
        assert!(should_exclude_path("conversations/session.jsonl", &opts));
    }

    #[test]
    fn test_should_exclude_conversations_included() {
        let opts = PackageOptions {
            include_conversations: true,
            ..Default::default()
        };
        assert!(!should_exclude_path("conversations/session.jsonl", &opts));
    }

    #[test]
    fn test_should_exclude_config_default() {
        let opts = PackageOptions::default();
        assert!(should_exclude_path("config/settings.toml", &opts));
    }

    #[test]
    fn test_should_exclude_config_included() {
        let opts = PackageOptions {
            include_config: true,
            ..Default::default()
        };
        assert!(!should_exclude_path("config/settings.toml", &opts));
    }

    #[test]
    fn test_should_not_exclude_normal_files() {
        let opts = PackageOptions::default();
        assert!(!should_exclude_path("manifest.toml", &opts));
        assert!(!should_exclude_path("prompts/system.md", &opts));
        assert!(!should_exclude_path("skills/search/skill.md", &opts));
    }

    #[test]
    fn test_glob_match() {
        assert!(glob_match("*.log", "debug.log"));
        assert!(glob_match("*.log", "some/path/debug.log"));
        assert!(!glob_match("*.log", "debug.txt"));
        assert!(glob_match("*.tmp", "temp.tmp"));
        assert!(!glob_match("*.tmp", "temp.log"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "other"));
    }
}
