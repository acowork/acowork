//! Navigation policy — **one** allow-list, parameterised by the role of the
//! webview that is asking.
//!
//! Why this module exists
//! ----------------------
//! `navigation_guard` is a *plugin-level* hook, so Tauri wires it into **every**
//! webview the app creates — not just the main window's (evidence:
//! `tauri-2.12.3/src/manager/webview.rs:600-630`, `prepare_webview` chains
//! `plugins.on_navigation(&webview, url)` into each webview's navigation
//! callback). Once the URL preview stops being an `<iframe>` inside the main
//! webview and becomes its own child webview (ADR-118), a single global rule
//! becomes wrong in both directions at once:
//!
//! - the **App** webview must never navigate to an arbitrary external site
//!   (that is the leak the guard was written for), while
//! - the **Preview** webview exists precisely to load arbitrary external sites,
//!   and must never navigate to the app's own origins (no reason to, and an
//!   app-origin document inside a preview is confusing at best).
//!
//! Two hard-coded rule sets would drift, so the *rule* stays one function and
//! the *role* is the parameter. Both roles are covered by the table tests at the
//! bottom of this file.
//!
//! Roles are derived from the webview label, because that is the only identity
//! the navigation callback receives. The label prefix is therefore part of the
//! contract with `browser::host` — see [`PREVIEW_LABEL_PREFIX`].

use tauri::Url;

/// Label prefix owned by the preview webview host (`browser::host`). A webview
/// labelled `preview-3` gets [`NavigationRole::Preview`]; everything else —
/// including the main webview, labelled `main` — gets [`NavigationRole::App`].
pub const PREVIEW_LABEL_PREFIX: &str = "preview-";

/// Port the app's own frontend is served from in a debug build (`devUrl` in
/// `tauri.conf.json`). Consulted only by [`is_allowed_for_preview`], and only
/// in debug builds: a release build has no dev server to collide with.
#[cfg(debug_assertions)]
const DEV_APP_DEV_PORT: u16 = 5173;

/// Which navigation rule set applies to a webview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationRole {
    /// The application's own webviews: the main window (`main`) and anything we
    /// ship ourselves. Allowed to reach app origins and local assets; external
    /// `http(s)` is refused on Windows.
    App,
    /// The in-app browser host: renders untrusted third-party documents at the
    /// top level. External `http(s)` is the point; app origins are refused.
    Preview,
}

impl NavigationRole {
    /// Role of the webview with this label. Defaults to [`NavigationRole::App`]
    /// — a webview we did not name is one of ours, and the App rule set is the
    /// restrictive one for external hosts.
    pub fn for_label(label: &str) -> Self {
        if label.starts_with(PREVIEW_LABEL_PREFIX) {
            NavigationRole::Preview
        } else {
            NavigationRole::App
        }
    }
}

/// Is `url` a legitimate top-level navigation for a webview in `role`?
///
/// Callers must fail closed: a `false` here cancels the navigation.
pub fn is_allowed(role: NavigationRole, url: &Url) -> bool {
    match role {
        NavigationRole::App => is_allowed_for_app(url),
        NavigationRole::Preview => is_allowed_for_preview(url),
    }
}

/// The original single-role rule set, unchanged in behaviour.
///
/// External `http(s)` is refused **on Windows only**, where WebView2 reports
/// `NavigationStarting` for the main frame alone; the macOS / Linux wry
/// delegate also reports *iframe* navigations, where an embed is
/// indistinguishable from a top-level navigation, so the guard stays permissive
/// there. That permissiveness is still required after ADR-118: hosted
/// applications (ADR-116, `AppFrame`) keep using an `<iframe>`, and the app's
/// plain-HTML surfaces keep using iframes for local assets.
fn is_allowed_for_app(url: &Url) -> bool {
    match url.scheme() {
        // macOS / Linux production origin of the bundled frontend.
        "tauri" => url.host_str() == Some("localhost") && is_app_root_navigation(url),
        // Local preview assets (`convertFileSrc`): `asset://localhost/…`.
        "asset" => true,
        "http" | "https" => {
            let host = url.host_str().unwrap_or_default();
            // Windows form of the asset protocol.
            if host == "asset.localhost" {
                return true;
            }
            // Windows production origin. Only root-path reloads pass.
            if host == "tauri.localhost" {
                return is_app_root_navigation(url);
            }
            // The app origin of the Tauri bundle on macOS/Linux is `tauri://`
            // (handled above); the packaged SPA never serves from http(s).
            // Vite dev-server origin (`devUrl`). Debug builds only — a
            // release binary never legitimately navigates to a dev server.
            #[cfg(debug_assertions)]
            if host == "localhost" || host == "127.0.0.1" {
                return true;
            }
            // External link: fail closed on Windows (main-frame-only
            // callback), stay permissive elsewhere (iframe embeds share
            // the callback — see the module docs).
            !cfg!(target_os = "windows")
        }
        // iframe placeholders (`about:blank`) and generated blobs. Not a
        // top-level app navigation, but harmless to admit.
        "about" | "blob" | "data" => true,
        // `file:`, drive letters (`d:`), and any custom scheme: no
        // legitimate webview navigation in this app. Fail closed.
        _ => false,
    }
}

/// The in-app browser's rule set: the complement of the App rules where it
/// matters.
///
/// - `http(s)` is always allowed: that is what the host is for, and the whole
///   point of the migration is that no origin can refuse to be displayed.
/// - App origins (`tauri://`, `tauri.localhost`, the debug dev server) and
///   local assets (`asset://`, `asset.localhost`) are refused: a preview must
///   not become a second window onto our own UI or the user's disk.
/// - `about:blank` is admitted — it is the placeholder wry and the page itself
///   navigate to; `blob:` is admitted because sites use it for generated
///   documents. `data:` is not: modern Chromium refuses top-level `data:`
///   navigations anyway, so admitting it only widens the surface.
/// - `file:` and drive-letter schemes are refused: the user's filesystem is not
///   reachable from a web page.
fn is_allowed_for_preview(url: &Url) -> bool {
    match url.scheme() {
        "http" | "https" => {
            let host = url.host_str().unwrap_or_default();
            // Refuse our own surfaces: no preview of the app itself.
            if host == "tauri.localhost" || host == "asset.localhost" {
                return false;
            }
            // A debug build serves the app *itself* from Vite (`devUrl`), so
            // that one loopback origin is our own UI wearing an `http` scheme —
            // the same surface the two checks above refuse.
            //
            // Loopback has three canonical spellings, and `Url` normalises the
            // IPv6 one on parse: `[0:0:…:1]` arrives as `[::1]`. All three are
            // listed because an origin check that answers differently for two
            // spellings of one origin is not an origin check — the first cut of
            // this rule compared only the first two and let `[::1]:5173`
            // through (measured, not assumed).
            //
            // Only that port, deliberately. A blanket loopback refusal (the
            // shape this started as) also refused the local dev server an agent
            // is building, and watching that page is the most useful thing this
            // host does: the desktop is a projection of the agent's work, not
            // its browser (ADR-118 §Consequences). Release builds have no dev
            // server, so the check is compiled out rather than evaluated.
            #[cfg(debug_assertions)]
            if url.port() == Some(DEV_APP_DEV_PORT)
                && matches!(host, "localhost" | "127.0.0.1" | "[::1]")
            {
                return false;
            }
            true
        }
        "about" | "blob" => true,
        // `tauri:`, `asset:`, `file:`, drive letters, custom schemes: refused.
        _ => false,
    }
}

/// True for the `""` / `/` paths — the URL a reload of the app origin
/// resolves to. Deeper paths on the app origin are leaked relative-path
/// navigations and are rejected.
fn is_app_root_navigation(url: &Url) -> bool {
    matches!(url.path(), "" | "/")
}
