//! Restricted path resolution for the agent package workspace.
//!
//! The package workspace exposes only `prompts/` and `skills/`; all paths
//! are validated here so query and mutation services share one boundary.

use std::path::{Component, Path, PathBuf};

use crate::usecases::workspace_query::WorkspaceError;

const ALLOWED_ROOTS: [&str; 2] = ["prompts", "skills"];

/// Resolve a package-relative path and ensure it stays within one allowed subtree.
/// An empty path denotes the virtual workspace root and is only valid for listing.
pub(crate) fn resolve_within(
    package_dir: &Path,
    requested_path: &str,
) -> Result<(PathBuf, PathBuf, String), WorkspaceError> {
    let normalized = requested_path.replace('\\', "/");
    let relative = Path::new(&normalized);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(invalid_path("absolute paths and traversal are not allowed"));
    }

    let components: Vec<_> = relative.components().collect();
    if components.is_empty() {
        let canonical_package = std::fs::canonicalize(package_dir).map_err(|e| {
            WorkspaceError::Io(std::io::Error::other(format!(
                "package root not accessible: {e}"
            )))
        })?;
        return Ok((canonical_package.clone(), canonical_package, String::new()));
    }
    let top = components[0].as_os_str().to_str().unwrap_or_default();
    if !ALLOWED_ROOTS.contains(&top) {
        return Err(invalid_path("only prompts/ and skills/ are accessible"));
    }

    let canonical_package = std::fs::canonicalize(package_dir).map_err(|e| {
        WorkspaceError::Io(std::io::Error::other(format!(
            "package root not accessible: {e}"
        )))
    })?;
    let subtree = canonical_package.join(top);
    let subtree_metadata = std::fs::symlink_metadata(&subtree)
        .map_err(|_| WorkspaceError::NotFound(format!("package directory not found: {top}")))?;
    if subtree_metadata.file_type().is_symlink() {
        return Err(invalid_path(
            "prompts/ and skills/ roots cannot be symlinks",
        ));
    }
    let canonical_subtree = std::fs::canonicalize(&subtree).map_err(|e| {
        WorkspaceError::NotFound(format!("package directory not found: {top}: {e}"))
    })?;
    if !canonical_subtree.starts_with(&canonical_package) || !canonical_subtree.is_dir() {
        return Err(invalid_path("package directory escapes package root"));
    }

    let abs_path = canonical_package.join(relative);
    let mut ancestor = abs_path.as_path();
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| invalid_path("invalid path"))?;
    }
    let canonical_ancestor = std::fs::canonicalize(ancestor).map_err(WorkspaceError::Io)?;
    if !canonical_ancestor.starts_with(&canonical_subtree) {
        return Err(invalid_path("path escapes prompts/ or skills/"));
    }

    Ok((canonical_package, abs_path, normalized))
}

pub(crate) fn is_package_workspace(workspace_id: Option<&str>) -> bool {
    workspace_id == Some("__agent_package__")
}

pub(crate) fn is_protected_root(relative_path: &str) -> bool {
    matches!(
        relative_path.replace('\\', "/").as_str(),
        "prompts" | "skills"
    )
}

fn invalid_path(message: &str) -> WorkspaceError {
    WorkspaceError::InvalidPath(message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn package_fixture() -> (tempfile::TempDir, PathBuf) {
        let temp = tempdir().expect("tempdir");
        let package = temp.path().join("package");
        std::fs::create_dir_all(package.join("prompts")).unwrap();
        std::fs::create_dir_all(package.join("skills").join("hello")).unwrap();
        std::fs::write(package.join("prompts/system.md"), "prompt").unwrap();
        std::fs::write(package.join("skills/hello/SKILL.md"), "skill").unwrap();
        std::fs::write(package.join("manifest.toml"), "secret").unwrap();
        (temp, package)
    }

    #[test]
    fn resolves_only_allowed_package_subtrees() {
        let (_temp, package) = package_fixture();
        for path in ["prompts/system.md", "skills/hello/SKILL.md"] {
            let (_, abs, rel) = resolve_within(&package, path).expect("allowed path");
            assert!(abs.is_file());
            assert_eq!(rel, path);
        }
        for path in [
            "manifest.toml",
            "config/agent.json",
            "../manifest.toml",
            "prompts/../../manifest.toml",
            r"C:\secret.txt",
        ] {
            assert!(
                resolve_within(&package, path).is_err(),
                "must reject {path}"
            );
        }
    }

    #[test]
    fn empty_path_resolves_virtual_package_root() {
        let (_temp, package) = package_fixture();
        let (root, path, relative) = resolve_within(&package, "").expect("root");
        assert_eq!(root, path);
        assert!(relative.is_empty());
        assert_eq!(root, std::fs::canonicalize(package).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape_and_cross_subtree() {
        let (temp, package) = package_fixture();
        std::os::unix::fs::symlink(temp.path(), package.join("prompts/outside")).unwrap();
        assert!(resolve_within(&package, "prompts/outside/secret").is_err());
        let other = temp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::os::unix::fs::symlink(other, package.join("skills/outside")).unwrap();
        assert!(resolve_within(&package, "skills/outside/secret").is_err());
    }
}
