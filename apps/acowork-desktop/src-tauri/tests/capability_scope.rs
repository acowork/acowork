//! Security contract for the desktop capability scoping (ADR-118).
//!
//! The in-app browser creates child webviews that render **untrusted third-party
//! documents**. Tauri's `windows` matcher grants a capability to *every* webview
//! of a matching window, so a capability written as `"windows": ["main"]` would
//! hand a rendered site the app's IPC surface (`shell:allow-open`, `fs`, …).
//! `capabilities/default.json` therefore scopes `main-capability` by
//! `webviews: ["main"]` and by nothing else.
//!
//! This test reads the shipped capability files and fails if that ever regresses
//! — a JSON edit is exactly the kind of change no compiler catches.

use std::fs;
use std::path::{Path, PathBuf};

fn capabilities_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("capabilities")
}

fn capability_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(capabilities_dir())
        .expect("capabilities/ directory must exist")
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no capability file found");
    files
}

/// Every capability file must scope access by **webview label**.
///
/// A non-empty `windows` list is rejected outright rather than inspected for
/// globs: the next window label someone adds would silently widen the grant
/// again, and there is no use case here that needs window scoping.
#[test]
fn capabilities_never_scope_by_window_label() {
    for file in capability_files() {
        let raw = fs::read_to_string(&file).expect("capability file must be readable");
        let json: serde_json::Value =
            serde_json::from_str(&raw).expect("capability file must be valid JSON");
        let windows = json.get("windows");
        assert!(
            windows.is_none() || windows == Some(&serde_json::Value::Array(vec![])),
            "{}: `windows` scoping would grant the capability to every webview of that \
             window — including preview webviews that render untrusted sites. Use `webviews`.",
            file.display()
        );
    }
}

#[test]
fn main_capability_is_scoped_to_the_main_webview_only() {
    let raw = fs::read_to_string(capabilities_dir().join("default.json"))
        .expect("capabilities/default.json must exist");
    let json: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");

    let webviews = json
        .get("webviews")
        .and_then(|v| v.as_array())
        .expect("default.json must scope by `webviews`");
    let labels: Vec<&str> = webviews.iter().filter_map(|v| v.as_str()).collect();
    assert_eq!(labels, vec!["main"], "main-capability must cover `main` only");

    // The prefix is the contract `navigation_policy` derives roles from, and the
    // one the preview host generates labels with: no capability may match it.
    for pattern in &labels {
        assert!(
            !pattern.contains("preview"),
            "main-capability pattern `{pattern}` would cover preview webviews"
        );
    }
}