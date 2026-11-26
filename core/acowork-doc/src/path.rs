//! acowork-doc path-safety utilities (design §9 threat table).
//!
//! Single audit point for every path or id crossing an API boundary:
//!
//! - **ID whitelist** — `validate_doc_id` / `validate_dir_id` /
//!   `validate_request_id` enforce the `{prefix}-{12 hex}` shape (and the
//!   `"root"` exception for the root directory).
//! - **Relative path** — `validate_relative_path` rejects `..`, absolute
//!   prefixes, and Windows drive letters; normalises `\` to `/`.
//! - **Document / directory name** — `validate_doc_name` forbids separators,
//!   NUL, leading `.`, and reserved names (`library.json`, `.trash`,
//!   `.requests`, plus Windows reserved `CON`/`PRN`/`NUL`/...).
//! - **Within-library guarantee** — `ensure_within_library` canonicalises
//!   the candidate and verifies it sits under the library root, blocking
//!   both `..` traversal and symlink escape.
//!
//! All checks return [`DocError`] so the HTTP layer can map straight to a
//! 400 / 403 response via the existing variant set.

use std::path::{Component, Path, PathBuf};

use crate::error::DocError;
use crate::types::{DIR_ID_PREFIX, DOC_ID_PREFIX, REQUEST_ID_PREFIX, ROOT_DIR_ID};

// ── Limits & constants ─────────────────────────────────────────────────

/// Matches the id-suffix length produced by `generate_*_id` (12 hex).
const SHORT_ID_HEX_LEN: usize = 12;

/// Cap on a single path segment (file / directory name). Well below typical
/// filesystem limits (ext4 = 255, NTFS = 255).
const MAX_NAME_LEN: usize = 200;

/// Cap on the total relative-path string handed in via API params.
const MAX_RELATIVE_PATH_LEN: usize = 4096;

/// Cap on a library icon reference. The value is an opaque UI key
/// (`icon-07`, later `custom:<relative asset path>`), never inline bytes.
const MAX_ICON_LEN: usize = 256;

/// Cap on a library description. Long enough for a paragraph, short enough
/// that the whole library list stays one cheap read.
const MAX_DESCRIPTION_LEN: usize = 2000;

// ── ID whitelist ───────────────────────────────────────────────────────

/// Validate a document id (`doc-` + 12 lowercase hex).
pub fn validate_doc_id(id: &str) -> Result<(), DocError> {
    validate_id(id, DOC_ID_PREFIX, "doc_id")
}

/// Validate a directory id (`dir-` + 12 hex, or the literal `"root"`).
pub fn validate_dir_id(id: &str) -> Result<(), DocError> {
    if id == ROOT_DIR_ID {
        return Ok(());
    }
    validate_id(id, DIR_ID_PREFIX, "dir_id")
}

/// Validate a request id (`r-` + 12 hex).
pub fn validate_request_id(id: &str) -> Result<(), DocError> {
    validate_id(id, REQUEST_ID_PREFIX, "request_id")
}

fn validate_id(id: &str, prefix: &str, kind: &str) -> Result<(), DocError> {
    let expected_len = prefix.len() + SHORT_ID_HEX_LEN;
    if id.len() != expected_len {
        return Err(DocError::InvalidId(String::from(kind)));
    }
    if !id.starts_with(prefix) {
        return Err(DocError::InvalidId(String::from(kind)));
    }
    // Lowercase hex suffix (matches generate_*_id output).
    let suffix = &id[prefix.len()..];
    if !suffix
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return Err(DocError::InvalidId(String::from(kind)));
    }
    Ok(())
}

// ── Relative path (API input layer) ────────────────────────────────────

/// Validate a relative path string from an API param.
///
/// Rejects:
/// - empty input
/// - absolute paths (`/foo`, `\foo`, `C:\foo`, `C:/foo`)
/// - `..` components (after `\` → `/` normalisation)
///
/// Returns the normalised string (with `\` replaced by `/`). Trailing `/`
/// is preserved so callers can ask for a directory node explicitly.
pub fn validate_relative_path(path: &str) -> Result<String, DocError> {
    if path.is_empty() {
        return Err(DocError::BadRequest(String::from("empty path")));
    }
    if path.len() > MAX_RELATIVE_PATH_LEN {
        return Err(DocError::BadRequest(String::from("path too long")));
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err(DocError::BadRequest(String::from("absolute path")));
    }
    // Windows drive letter, e.g. "C:foo" or "C:\foo".
    if path.len() >= 2 && path.as_bytes()[1] == b':' {
        return Err(DocError::BadRequest(String::from("drive letter")));
    }
    let normalised = path.replace('\\', "/");
    for component in Path::new(&normalised).components() {
        match component {
            Component::ParentDir => {
                return Err(DocError::PathTraversal(String::from(".. in path")));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(DocError::BadRequest(String::from(
                    "absolute component in path",
                )));
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    Ok(normalised)
}

// ── File / directory name (on-disk layer) ───────────────────────────────

/// Validate a document or directory name (single segment, no separators).
///
/// Forbids:
/// - empty / whitespace-only
/// - separators (`/`, `\`) and NUL
/// - leading `.` (avoids hidden files & system dirs like `.trash`,
///   `.requests`)
/// - reserved names (`library.json`, `.trash`, `.requests`, Windows
///   reserved device names)
pub fn validate_doc_name(name: &str) -> Result<(), DocError> {
    if name.is_empty() {
        return Err(DocError::BadRequest(String::from("name is empty")));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(DocError::BadRequest(String::from("name too long")));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(DocError::BadRequest(String::from(
            "name contains separator",
        )));
    }
    if name.contains('\0') {
        return Err(DocError::BadRequest(String::from("name contains NUL")));
    }
    if name.starts_with('.') {
        return Err(DocError::ReservedName(String::from(name)));
    }
    if matches!(name, "library.json" | ".trash" | ".requests") {
        return Err(DocError::ReservedName(String::from(name)));
    }
    if is_windows_reserved(name) {
        return Err(DocError::ReservedName(String::from(name)));
    }
    Ok(())
}

/// Validate a library icon reference (`library.json` / `DirMeta` `icon`).
///
/// The value is a short opaque UI key, not a path and not bytes; we only
/// reject control characters so the tree/JSON round-trip stays clean.
pub fn validate_library_icon(icon: &str) -> Result<(), DocError> {
    if icon.is_empty() {
        return Err(DocError::BadRequest(String::from("icon is empty")));
    }
    if icon.len() > MAX_ICON_LEN {
        return Err(DocError::BadRequest(String::from("icon too long")));
    }
    if icon.chars().any(|c| c.is_control()) {
        return Err(DocError::BadRequest(String::from(
            "icon contains control character",
        )));
    }
    Ok(())
}

/// Validate a library description. Newlines are allowed; control characters
/// other than `\n` / `\t` are not.
pub fn validate_library_description(description: &str) -> Result<(), DocError> {
    if description.len() > MAX_DESCRIPTION_LEN {
        return Err(DocError::BadRequest(String::from("description too long")));
    }
    if description
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(DocError::BadRequest(String::from(
            "description contains control character",
        )));
    }
    Ok(())
}

fn is_windows_reserved(name: &str) -> bool {
    // Compare against the part before the first '.' — Windows device names
    // (CON, PRN, ...) are reserved regardless of extension.
    let base = name
        .split('.')
        .next()
        .unwrap_or(name)
        .to_ascii_uppercase();
    matches!(
        base.as_str(),
        "CON" | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

// ── Within-library guarantee (filesystem layer) ────────────────────────

/// Canonicalise `candidate` and verify it sits under `library_root`.
///
/// Blocks both `..` traversal and symlink escape (design §9). The
/// candidate must already exist on disk — `std::fs::canonicalize`
/// resolves only-existing paths. Callers should `create_dir_all` first
/// when writing new entries, then call this on the freshly-created path.
pub fn ensure_within_library(
    library_root: &Path,
    candidate: &Path,
) -> Result<PathBuf, DocError> {
    let canonical_root = std::fs::canonicalize(library_root).map_err(|e| {
        DocError::CorruptIndex(format!("library root not accessible: {e}"))
    })?;
    let canonical = std::fs::canonicalize(candidate)
        .map_err(|e| DocError::PathTraversal(format!("canonicalize failed: {e}")))?;
    if !canonical.starts_with(&canonical_root) {
        return Err(DocError::PathTraversal(format!(
            "{} escapes library root",
            candidate.display()
        )));
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // ── ID whitelist ─────────────────────────────────────────────────

    #[test]
    fn validate_doc_id_accepts_canonical_shape() {
        assert!(validate_doc_id("doc-abcdef012345").is_ok());
        assert!(validate_doc_id("doc-000000000000").is_ok());
    }

    #[test]
    fn validate_doc_id_rejects_wrong_shape() {
        assert!(validate_doc_id("").is_err());
        assert!(validate_doc_id("doc-").is_err());
        assert!(validate_doc_id("doc-abc").is_err()); // too short
        assert!(validate_doc_id("doc-ABCDEF012345").is_err()); // uppercase
        assert!(validate_doc_id("dir-abcdef012345").is_err()); // wrong prefix
        assert!(validate_doc_id("doc-abcdef0123456").is_err()); // too long
        assert!(validate_doc_id("../etc/passwd").is_err());
    }

    #[test]
    fn validate_dir_id_accepts_root_and_canonical_shape() {
        assert!(validate_dir_id("root").is_ok());
        assert!(validate_dir_id("dir-abcdef012345").is_ok());
    }

    #[test]
    fn validate_dir_id_rejects_garbage() {
        assert!(validate_dir_id("").is_err());
        assert!(validate_dir_id("doc-abcdef012345").is_err()); // doc prefix
        assert!(validate_dir_id("dir-").is_err());
        assert!(validate_dir_id("dir-ABCDEF012345").is_err());
        assert!(validate_dir_id("../root").is_err());
    }

    #[test]
    fn validate_request_id_accepts_canonical_shape() {
        assert!(validate_request_id("r-abcdef012345").is_ok());
        assert!(validate_request_id("r-000000000000").is_ok());
    }

    #[test]
    fn validate_request_id_rejects_garbage() {
        assert!(validate_request_id("").is_err());
        assert!(validate_request_id("r-").is_err());
        assert!(validate_request_id("r-abc").is_err());
        assert!(validate_request_id("doc-abcdef012345").is_err());
    }

    // ── Relative path ────────────────────────────────────────────────

    #[test]
    fn validate_relative_path_accepts_normal_segments() {
        assert_eq!(validate_relative_path("a").unwrap(), "a");
        assert_eq!(validate_relative_path("a/b").unwrap(), "a/b");
        assert_eq!(validate_relative_path("项目A/PRD.md").unwrap(), "项目A/PRD.md");
        // Backslash normalised to forward slash.
        assert_eq!(validate_relative_path("a\\b\\c.md").unwrap(), "a/b/c.md");
        // `./` is tolerated (no-op segment).
        assert_eq!(validate_relative_path("./a/b").unwrap(), "./a/b");
    }

    #[test]
    fn validate_relative_path_rejects_dangerous_inputs() {
        assert!(validate_relative_path("").is_err());
        assert!(validate_relative_path("/etc/passwd").is_err());
        assert!(validate_relative_path("\\Windows\\System32").is_err());
        assert!(validate_relative_path("a/../etc").is_err());
        assert!(validate_relative_path("../etc/passwd").is_err());
        assert!(validate_relative_path("C:\\Windows").is_err());
        assert!(validate_relative_path("C:/Windows").is_err());
        assert!(validate_relative_path("a/b/../../etc").is_err());
        // Over-length input rejected.
        let huge = "a/".repeat(3000);
        assert!(validate_relative_path(&huge).is_err());
    }

    // ── Document name ────────────────────────────────────────────────

    #[test]
    fn validate_doc_name_accepts_normal() {
        assert!(validate_doc_name("产品方案").is_ok());
        assert!(validate_doc_name("My Doc v2").is_ok());
        assert!(validate_doc_name("PRD").is_ok());
    }

    #[test]
    fn validate_doc_name_rejects_separator_nul_empty() {
        assert!(validate_doc_name("").is_err());
        assert!(validate_doc_name("a/b").is_err());
        assert!(validate_doc_name("a\\b").is_err());
        assert!(validate_doc_name("a\0b").is_err());
    }

    #[test]
    fn validate_doc_name_rejects_dot_prefix_and_reserved() {
        assert!(validate_doc_name(".hidden").is_err());
        assert!(validate_doc_name(".trash").is_err());
        assert!(validate_doc_name(".requests").is_err());
        assert!(validate_doc_name("library.json").is_err());
        // Windows reserved (case-insensitive, extension-independent).
        assert!(validate_doc_name("CON").is_err());
        assert!(validate_doc_name("con").is_err());
        assert!(validate_doc_name("CON.txt").is_err());
        assert!(validate_doc_name("COM1").is_err());
        assert!(validate_doc_name("LPT9.log").is_err());
        // But "CONTAINS" is fine (different word).
        assert!(validate_doc_name("CONTAINS").is_ok());
    }

    // ── ensure_within_library ────────────────────────────────────────

    #[test]
    fn ensure_within_library_accepts_inside_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let inside = root.join("项目A");
        fs::create_dir(&inside).unwrap();
        let file = inside.join("PRD.md");
        fs::write(&file, "x").unwrap();
        let resolved = ensure_within_library(root, &file).unwrap();
        assert!(resolved.starts_with(fs::canonicalize(root).unwrap()));
    }

    #[test]
    fn ensure_within_library_rejects_outside_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("library");
        fs::create_dir(&root).unwrap();
        // File placed one level up.
        let outside = tmp.path().join("secret.txt");
        fs::write(&outside, "x").unwrap();
        let err = ensure_within_library(&root, &outside).unwrap_err();
        assert!(matches!(err, DocError::PathTraversal(_)), "got: {err:?}");
    }

    #[test]
    fn ensure_within_library_rejects_traversal_via_dotdot() {
        // Construct `root/inside/sub` and try a path that escapes via `..`.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("library");
        fs::create_dir(&root).unwrap();
        let inside = root.join("inside");
        fs::create_dir(&inside).unwrap();
        let escape_target = inside.join("sub").join("..").join("..").join("escape.txt");
        // Create the file at the resolved target so canonicalize succeeds.
        let resolved_target = std::fs::canonicalize(tmp.path())
            .unwrap()
            .join("escape.txt");
        fs::write(&resolved_target, "x").unwrap();
        // The traversal path must NOT resolve inside root.
        let err = ensure_within_library(&root, &escape_target).unwrap_err();
        assert!(matches!(err, DocError::PathTraversal(_)), "got: {err:?}");
    }

    #[test]
    fn ensure_within_library_rejects_missing_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("library");
        fs::create_dir(&root).unwrap();
        let missing = root.join("nope.md");
        let err = ensure_within_library(&root, &missing).unwrap_err();
        // canonicalize on a missing path surfaces PathTraversal via our wrapper.
        assert!(matches!(err, DocError::PathTraversal(_)), "got: {err:?}");
    }
}
