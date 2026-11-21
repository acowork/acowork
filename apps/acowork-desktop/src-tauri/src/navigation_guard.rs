// SPDX-License-Identifier: Apache-2.0
// Copyright the ACowork project contributors.
//
//! Navigation guard — the plugin that applies [`crate::navigation_policy`] to
//! every webview of this app.
//!
//! Background: markdown links in chat surfaces used to fall through to the
//! webview's default navigation when their `href` survived neither URL
//! sanitisation nor the resolver. react-markdown rewrites a Windows drive
//! path (`D:/…`) to `""` — and an empty href navigates to the *current*
//! URL, a full reload that on WebView2 could crash the renderer. The
//! frontend fix (`markdownUrlTransform` + the `<a>` interceptor in
//! `MessageBubble` / `CompactionCard`) removes the known vectors; this
//! guard makes any future leak fail closed instead of reloading or
//! white-screening the app.
//!
//! The rules themselves live in `navigation_policy`, parameterised by the
//! *role* of the webview that is asking (App vs the in-app browser host of
//! ADR-118). This module only decides the role and logs refusals — keeping the
//! decision in one place is what stops the two rule sets from drifting.
//!
//! Registered in `lib.rs` before the windows are created so the handler lands
//! in every webview — including the ones `browser::host` creates later at
//! runtime, because Tauri chains plugin `on_navigation` handlers into each
//! webview's navigation callback (`tauri/src/manager/webview.rs`,
//! `prepare_webview`).

use crate::navigation_policy::{self, NavigationRole};

/// Build the navigation-guard plugin.
pub fn init<R: tauri::Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri::plugin::Builder::new("navigation-guard")
        .on_navigation(|webview, url| {
            let role = NavigationRole::for_label(webview.label());
            let allowed = navigation_policy::is_allowed(role, url);
            if !allowed {
                tracing::warn!(
                    webview = webview.label(),
                    ?role,
                    url = %url,
                    "navigation-guard: blocked unexpected webview navigation"
                );
            }
            allowed
        })
        .build()
}