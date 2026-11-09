//! Avatar value rules — the vocabulary every hop shares.
//!
//! An avatar is a *reference*, not an image: either a packaged image path
//! relative to the instance dir (`"assets/avatar-02.png"`) or a builtin
//! icon id (`"icon-05"`). The bytes are served by the Node (it owns the
//! package tree and is up whether or not a Runtime is); everything that
//! *validates* a user's pick shares the rules below.
//!
//! This module exists because the rules used to live inside the
//! per-instance overrides type. The overrides file is gone (instance
//! settings — including the avatar and the name — now live in
//! `{workspace}/config/agent_config.json`, see `acowork-runtime`'s
//! `agent_config`), but the rules are still shared by the Runtime's
//! `PUT /avatar-config`, the Node's upload + serve routes and the package
//! avatar-asset listing.

/// Extensions accepted for a user-supplied avatar file (ADR-017).
///
/// One list for every hop that validates the value. A whitelist that
/// exists in three places drifts: one hop ends up admitting what another
/// serves.
pub const AVATAR_EXTENSIONS: [&str; 6] = ["png", "jpg", "jpeg", "gif", "webp", "svg"];

/// True when `path` ends in one of [`AVATAR_EXTENSIONS`], ASCII
/// case-insensitively. Extension only — path-traversal checks belong to
/// the caller that resolves the file.
pub fn has_avatar_extension(path: &str) -> bool {
    match std::path::Path::new(path).extension().and_then(|e| e.to_str()) {
        Some(ext) => {
            let ext = ext.to_ascii_lowercase();
            AVATAR_EXTENSIONS.contains(&ext.as_str())
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_check_is_case_insensitive_and_ignores_directories() {
        assert!(has_avatar_extension("assets/avatar-02.PNG"));
        assert!(has_avatar_extension("a/b/icon.webp"));
        assert!(!has_avatar_extension("assets/notes.txt"));
        assert!(!has_avatar_extension("assets/avatar"));
        assert!(!has_avatar_extension(""));
    }
}
