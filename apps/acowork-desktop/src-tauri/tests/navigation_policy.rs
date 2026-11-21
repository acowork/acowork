//! Contract tests for [`acowork_desktop::navigation_policy`].
//!
//! These live in `tests/` rather than inline `#[cfg(test)] mod tests` for a
//! mechanical reason: this crate's *unit*-test binary cannot start on Windows
//! (no application manifest → `comctl32.dll` v5.82 → missing
//! `TaskDialogIndirect`, imported statically by `rfd`). See
//! `src-tauri/comctl32-v6.manifest` and `build.rs` step 8. Integration-test
//! binaries do get the manifest, so the contract is verified here:
//!
//! ```text
//! cd apps/acowork-desktop/src-tauri && cargo test --tests
//! ```

use acowork_desktop::navigation_policy::{is_allowed, NavigationRole};

fn url(s: &str) -> tauri::Url {
    s.parse().expect("test URL must parse")
}

#[test]
fn roles_are_derived_from_the_label_prefix() {
    assert_eq!(NavigationRole::for_label("main"), NavigationRole::App);
    assert_eq!(NavigationRole::for_label(""), NavigationRole::App);
    assert_eq!(
        NavigationRole::for_label("preview-1"),
        NavigationRole::Preview
    );
    assert_eq!(
        NavigationRole::for_label("preview-"),
        NavigationRole::Preview
    );
    // Not a prefix match: the separator is part of the contract.
    assert_eq!(NavigationRole::for_label("previews"), NavigationRole::App);
}

#[test]
fn app_role_admits_its_own_origins() {
    assert!(is_allowed(NavigationRole::App, &url("tauri://localhost")));
    assert!(is_allowed(NavigationRole::App, &url("tauri://localhost/")));
    assert!(is_allowed(NavigationRole::App, &url("http://tauri.localhost/")));
    assert!(is_allowed(
        NavigationRole::App,
        &url("asset://localhost/icon.png")
    ));
    assert!(is_allowed(
        NavigationRole::App,
        &url("http://asset.localhost/icon.png")
    ));
}

#[test]
fn app_role_rejects_deep_paths_on_its_own_origin() {
    // Reload / root only; a leaked relative link must not navigate the SPA.
    assert!(!is_allowed(
        NavigationRole::App,
        &url("tauri://localhost/docs/x.md")
    ));
    assert!(!is_allowed(
        NavigationRole::App,
        &url("http://tauri.localhost/docs/x.md")
    ));
}

#[test]
fn app_role_refuses_the_filesystem() {
    for candidate in [
        "file:///C:/Windows/win.ini",
        "c:/Windows/win.ini",
        "d:/projects/secret.md",
    ] {
        assert!(
            !is_allowed(NavigationRole::App, &url(candidate)),
            "app role must refuse {candidate}"
        );
    }
}

#[test]
fn app_role_treats_external_hosts_by_platform() {
    // Windows: fail closed (WebView2 reports main-frame navigations only).
    // Elsewhere: permissive, because iframe embeds share the callback.
    let external = url("https://example.com/x");
    let expected = !cfg!(target_os = "windows");
    assert_eq!(is_allowed(NavigationRole::App, &external), expected);
}

#[test]
fn preview_role_admits_external_documents_without_exception() {
    // The regression this whole migration exists for: GitHub sends
    // `X-Frame-Options: deny`, which only a top-level document escapes.
    assert!(is_allowed(
        NavigationRole::Preview,
        &url("https://github.com/acowork/ACowork/pull/3")
    ));
    assert!(is_allowed(
        NavigationRole::Preview,
        &url("http://example.com")
    ));
    assert!(is_allowed(
        NavigationRole::Preview,
        &url("https://accounts.google.com/signin")
    ));
    assert!(is_allowed(NavigationRole::Preview, &url("about:blank")));
    assert!(is_allowed(
        NavigationRole::Preview,
        &url("blob:https://example.com/1234")
    ));
}

#[test]
fn preview_role_refuses_our_own_surfaces() {
    // A preview must not become a second window onto the app or the disk.
    for candidate in [
        "tauri://localhost/",
        "http://tauri.localhost/",
        "asset://localhost/C:/Users/x/secret.txt",
        "http://asset.localhost/C:/Users/x/secret.txt",
        "file:///C:/Windows/win.ini",
        "c:/Windows/win.ini",
    ] {
        assert!(
            !is_allowed(NavigationRole::Preview, &url(candidate)),
            "preview role must refuse {candidate}"
        );
    }
    assert!(!is_allowed(
        NavigationRole::Preview,
        &url("data:text/html,<h1>hi</h1>")
    ));
}

#[test]
fn preview_role_refuses_the_apps_own_dev_server_in_debug_builds() {
    // `devUrl` in `tauri.conf.json`: in a debug build this loopback origin is
    // the app itself, so it is our surface like `tauri.localhost` is.
    #[cfg(debug_assertions)]
    {
        assert!(!is_allowed(
            NavigationRole::Preview,
            &url("http://localhost:5173/")
        ));
        assert!(!is_allowed(
            NavigationRole::Preview,
            &url("http://127.0.0.1:5173/")
        ));
    }
}

#[test]
fn preview_role_admits_a_local_dev_server_that_is_not_ours() {
    // The reason the loopback refusal is port-scoped rather than blanket: an
    // agent building a frontend on this machine is the case the preview host
    // exists for, and its server is on some other port.
    for candidate in [
        "http://localhost:3000/",
        "http://localhost:8080/app",
        "http://127.0.0.1:19876/health",
        "http://localhost/",
    ] {
        assert!(
            is_allowed(NavigationRole::Preview, &url(candidate)),
            "preview role must admit {candidate}"
        );
    }
}