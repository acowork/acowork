//! In-app browser host (ADR-118).
//!
//! The desktop URL preview used to render external pages in an `<iframe>` inside
//! the main webview. Sites that refuse to be framed (`X-Frame-Options`,
//! CSP `frame-ancestors`) therefore rendered blank, with no way to tell why.
//! The host in this module loads the page as a **top-level document** in its own
//! child webview — the same architecture VS Code's Simple Browser and Cursor
//! use — which no framing header can refuse.
//!
//! | module | responsibility |
//! |---|---|
//! | `registry` | data model: sessions, phases, bounds, eviction (pure, unit-tested) |
//! | `host` | Tauri side: webview lifecycle + `preview://` events (commands) |
//! | `stray_container` | Windows: destroys the window wry leaks when a create fails |
//!
//! The policy that decides what a preview may navigate to lives one level up, in
//! [`crate::navigation_policy`], because the plugin-level navigation guard needs
//! the same rules for every webview in the app.

pub mod host;
pub mod registry;
/// Win32-only compensation for a wry create-failure leak; see the module docs.
#[cfg(windows)]
pub mod stray_container;

pub use host::PreviewHost;