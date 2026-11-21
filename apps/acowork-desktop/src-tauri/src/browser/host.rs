//! In-app browser host — the Tauri side of the URL preview (ADR-118).
//!
//! ## Why this exists
//!
//! The URL preview used to be an `<iframe>` inside the main webview, so any site
//! that refuses to be framed (`X-Frame-Options: deny`, CSP
//! `frame-ancestors 'none'` — GitHub, Google, most OAuth pages) rendered as an
//! unexplained blank pane. Those headers only constrain *sub-framed* documents,
//! so the fix is to load the page as a **top-level document** in its own child
//! webview (`Window::add_child`) — the same shape VS Code's Simple Browser and
//! Cursor use. Nothing about the page changes; only its host does.
//!
//! ## Division of responsibility
//!
//! - `registry` holds the data model (sessions, phases, bounds, eviction) and is
//!   covered by pure unit tests.
//! - This module owns the Tauri objects: create/position/show/hide/destroy
//!   webviews, and translate engine callbacks into `preview://state` events.
//!   The frontend is a *controller* here, never the source of truth.
//!
//! ## Windows: creation must stay async
//!
//! `Window::add_child` deadlocks when called from a synchronous command or an
//! event handler (`tauri::window::Window::add_child` docs, wry#583). Every
//! command that creates or destroys a webview is therefore `async fn`, which
//! Tauri runs off the main thread — do not "simplify" them into sync commands.
//!
//! ## Sandbox
//!
//! The preview webview gets **no Tauri capability**: `capabilities/default.json`
//! deliberately scopes `main-capability` with `webviews: ["main"]`, so a
//! rendered site cannot reach the app's IPC surface (`shell`, `fs`, …). Tauri's
//! `windows` matcher would have granted it to every webview of the window — see
//! that file's comment. Do not replace that scoping with a `windows` glob.

use std::sync::Mutex;

use serde::Serialize;
use tauri::webview::{NewWindowResponse, PageLoadEvent, WebviewBuilder};
use tauri::{
    AppHandle, Emitter, EventTarget, LogicalPosition, LogicalSize, Manager, Url, Webview,
    WebviewUrl,
};

use super::registry::{now_ms, Phase, PreviewBounds, PreviewRegistry, PreviewSession};
use crate::navigation_policy::{self, NavigationRole};

/// Webview the frontend listens on (the window's primary webview).
const MAIN_WEBVIEW: &str = "main";

/// Emitted whenever a session's observable state changes. Payload:
/// [`PreviewStateDto`].
pub const STATE_EVENT: &str = "preview://state";

/// Emitted when a page requests `window.open`. Payload:
/// `{ "url": "…", "from": "preview-2" }`. The frontend opens a new preview tab.
pub const NEW_WINDOW_EVENT: &str = "preview://new-window";

/// Managed state: the session registry behind a lock.
///
/// The lock is `std::sync::Mutex` on purpose — no await point ever holds it, so
/// an async mutex would only add overhead.
#[derive(Default)]
pub struct PreviewHost {
    registry: Mutex<PreviewRegistry>,
}

impl PreviewHost {
    /// Run `f` with the registry locked. Panics only if another thread panicked
    /// while holding the lock, which is a bug rather than a runtime condition.
    fn with<R>(&self, f: impl FnOnce(&mut PreviewRegistry) -> R) -> R {
        let mut guard = self.registry.lock().expect("preview registry poisoned");
        f(&mut guard)
    }
}

/// Wire shape of one session. Field names are camelCase because this crosses
/// into TypeScript.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewStateDto {
    pub label: String,
    pub url: String,
    pub title: Option<String>,
    /// `loading` | `loaded` — see [`Phase::as_str`]
    pub phase: String,
    pub visible: bool,
}

impl From<&PreviewSession> for PreviewStateDto {
    fn from(session: &PreviewSession) -> Self {
        Self {
            label: session.label.clone(),
            url: session.url.clone(),
            title: session.title.clone(),
            phase: session.phase.as_str().to_string(),
            visible: session.visible,
        }
    }
}

/// Push one session's state to the frontend. Best-effort: a failed emit (the
/// window is going away) must not abort a command.
fn emit_state(app: &AppHandle, session: &PreviewSession) {
    let payload = PreviewStateDto::from(session);
    let _ = app.emit_to(
        EventTarget::AnyLabel {
            label: MAIN_WEBVIEW.to_string(),
        },
        STATE_EVENT,
        payload,
    );
}

/// Parse and policy-check a URL the frontend wants to display.
///
/// The same [`NavigationRole::Preview`] rule set the navigation guard applies,
/// reused here so a refused scheme fails at the command boundary with a reason
/// instead of producing a blank webview.
fn parse_preview_url(raw: &str) -> Result<Url, String> {
    let url: Url = raw
        .trim()
        .parse()
        .map_err(|_| format!("invalid_url: {raw}"))?;
    if !navigation_policy::is_allowed(NavigationRole::Preview, &url) {
        return Err(format!("blocked_scheme: {url}"));
    }
    Ok(url)
}

/// Resolve a live preview webview, or answer the typed error the frontend
/// branches on.
fn webview_for(app: &AppHandle, label: &str) -> Result<Webview, String> {
    app.get_webview(label)
        .ok_or_else(|| format!("unknown_preview: {label}"))
}

/// Open a preview session: register it, then create its webview at `bounds`.
///
/// Async on purpose — see the module docs (Windows deadlock).
#[tauri::command]
pub async fn preview_open(
    app: AppHandle,
    url: String,
    bounds: PreviewBounds,
) -> Result<PreviewStateDto, String> {
    let parsed = parse_preview_url(&url)?;
    let window = app
        .get_window(MAIN_WEBVIEW)
        .ok_or_else(|| "main_window_missing".to_string())?;

    let session = app
        .state::<PreviewHost>()
        .with(|registry| registry.open(parsed.to_string(), bounds, now_ms()));
    let label = session.label.clone();

    // Engine callbacks need their own handles; everything they touch is a
    // short-lived lock plus an emit, so none of them can deadlock the creator.
    let builder = {
        let app_title = app.clone();
        let app_load = app.clone();
        let app_new_window = app.clone();
        let from_label = label.clone();
        let title_label = label.clone();
        let load_label = label.clone();
        WebviewBuilder::new(&label, WebviewUrl::External(parsed))
            // Second line of defence: the plugin-level guard already runs for
            // this webview, but keeping the builder's own handler means the
            // role rules hold even if the plugin is ever detached.
            .on_navigation(|url| navigation_policy::is_allowed(NavigationRole::Preview, url))
            .on_document_title_changed(move |_webview, title| {
                let updated = app_title.state::<PreviewHost>().with(|registry| {
                    registry.update(&title_label, |s| {
                        s.title = Some(title.clone());
                    });
                    registry.get(&title_label).cloned()
                });
                if let Some(session) = updated {
                    emit_state(&app_title, &session);
                }
            })
            .on_page_load(move |_webview, payload| {
                let url = payload.url().to_string();
                let loaded = payload.event() == PageLoadEvent::Finished;
                let updated = app_load.state::<PreviewHost>().with(|registry| {
                    registry.update(&load_label, |s| {
                        s.url = url.clone();
                        s.phase = if loaded { Phase::Loaded } else { Phase::Loading };
                    });
                    registry.get(&load_label).cloned()
                });
                if let Some(session) = updated {
                    emit_state(&app_load, &session);
                }
            })
            .on_new_window(move |url, _features| {
                // Browser semantics inside the app: a `window.open` becomes a
                // new preview tab (the frontend owns tab creation). Denying the
                // native window keeps the "one window" promise of the desktop
                // app, and is what VS Code does with Simple Browser.
                let _ = app_new_window.emit_to(
                    EventTarget::AnyLabel {
                        label: MAIN_WEBVIEW.to_string(),
                    },
                    NEW_WINDOW_EVENT,
                    serde_json::json!({ "url": url.to_string(), "from": from_label }),
                );
                NewWindowResponse::Deny
            })
            // Downloads need a destination UI we do not have yet: refuse rather
            // than let WebView2 drop files somewhere the user cannot find.
            .on_download(|_webview, _event| false)
            .zoom_hotkeys_enabled(true)
            .enable_clipboard_access()
            .devtools(cfg!(debug_assertions))
    };

    let created = window.add_child(
        builder,
        LogicalPosition::new(bounds.x, bounds.y),
        LogicalSize::new(bounds.width.max(1.0), bounds.height.max(1.0)),
    );

    match created {
        Ok(_) => {
            // Evict beyond the memory bound; the newcomer is never a candidate.
            let evicted = app.state::<PreviewHost>().with(|registry| {
                let labels = registry.overflow(&label);
                for stale in &labels {
                    registry.remove(stale);
                }
                labels
            });
            for stale in evicted {
                if let Some(webview) = app.get_webview(&stale) {
                    let _ = webview.close();
                }
            }
            app.state::<PreviewHost>()
                .with(|registry| registry.get(&label).map(PreviewStateDto::from))
                .ok_or_else(|| format!("unknown_preview: {label}"))
        }
        Err(error) => {
            app.state::<PreviewHost>()
                .with(|registry| registry.remove(&label));
            Err(format!("create_failed: {error}"))
        }
    }
}

/// Move/resize a preview webview. Called on every layout commit from the
/// frontend (panel resize, tab switch, DPI change).
#[tauri::command]
pub fn preview_set_bounds(
    app: AppHandle,
    label: String,
    bounds: PreviewBounds,
) -> Result<(), String> {
    let webview = webview_for(&app, &label)?;
    if bounds.is_empty() {
        // A zero-sized webview still paints on some platforms; hiding is the
        // honest way to say "not on screen".
        webview.hide().map_err(|e| e.to_string())?;
    } else {
        webview
            .set_position(LogicalPosition::new(bounds.x, bounds.y))
            .map_err(|e| e.to_string())?;
        webview
            .set_size(LogicalSize::new(bounds.width, bounds.height))
            .map_err(|e| e.to_string())?;
        webview.show().map_err(|e| e.to_string())?;
    }
    let updated = app.state::<PreviewHost>().with(|registry| {
        registry.update(&label, |s| s.bounds = bounds);
        registry.get(&label).cloned()
    });
    if let Some(session) = updated {
        emit_state(&app, &session);
    }
    Ok(())
}

/// Show or hide without changing geometry (inactive tab, native overlay open).
#[tauri::command]
pub fn preview_set_visible(app: AppHandle, label: String, visible: bool) -> Result<(), String> {
    let webview = webview_for(&app, &label)?;
    if visible {
        webview.show().map_err(|e| e.to_string())?;
    } else {
        webview.hide().map_err(|e| e.to_string())?;
    }
    let updated = app.state::<PreviewHost>().with(|registry| {
        registry.update(&label, |s| s.visible = visible);
        registry.touch(&label, now_ms());
        registry.get(&label).cloned()
    });
    if let Some(session) = updated {
        emit_state(&app, &session);
    }
    Ok(())
}

/// Navigate an existing preview (address bar, or a link the UI resolved).
#[tauri::command]
pub fn preview_navigate(app: AppHandle, label: String, url: String) -> Result<(), String> {
    let parsed = parse_preview_url(&url)?;
    let webview = webview_for(&app, &label)?;
    let updated = app.state::<PreviewHost>().with(|registry| {
        registry.update(&label, |s| {
            s.url = parsed.to_string();
            s.phase = Phase::Loading;
        });
        registry.touch(&label, now_ms());
        registry.get(&label).cloned()
    });
    webview.navigate(parsed).map_err(|e| e.to_string())?;
    if let Some(session) = updated {
        emit_state(&app, &session);
    }
    Ok(())
}

/// Reload the current document.
#[tauri::command]
pub fn preview_reload(app: AppHandle, label: String) -> Result<(), String> {
    let webview = webview_for(&app, &label)?;
    let updated = app.state::<PreviewHost>().with(|registry| {
        registry.update(&label, |s| s.phase = Phase::Loading);
        registry.get(&label).cloned()
    });
    webview.reload().map_err(|e| e.to_string())?;
    if let Some(session) = updated {
        emit_state(&app, &session);
    }
    Ok(())
}

/// Walk the page's own session history.
///
/// Tauri 2.12 exposes no `go_back`/`go_forward` on `Webview`, so this drives the
/// document's History API. At the ends of the history the calls are no-ops,
/// which is why the toolbar buttons stay enabled instead of guessing bounds.
#[tauri::command]
pub fn preview_history(app: AppHandle, label: String, direction: String) -> Result<(), String> {
    let webview = webview_for(&app, &label)?;
    let script = match direction.as_str() {
        "back" => "history.back()",
        "forward" => "history.forward()",
        other => return Err(format!("invalid_direction: {other}")),
    };
    webview
        .eval(script.to_string())
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Destroy one session. Idempotent from the UI's point of view: an already
/// closed session is not an error.
#[tauri::command]
pub fn preview_close(app: AppHandle, label: String) -> Result<(), String> {
    app.state::<PreviewHost>()
        .with(|registry| registry.remove(&label));
    if let Some(webview) = app.get_webview(&label) {
        let _ = webview.close();
    }
    Ok(())
}

/// Destroy every session (preview panel unmounted, window going away).
#[tauri::command]
pub fn preview_close_all(app: AppHandle) -> Result<(), String> {
    let sessions = app.state::<PreviewHost>().with(|registry| registry.drain());
    for session in sessions {
        if let Some(webview) = app.get_webview(&session.label) {
            let _ = webview.close();
        }
    }
    Ok(())
}

/// All live sessions, for the frontend to rebuild its tabs after a reload.
#[tauri::command]
pub fn preview_list(app: AppHandle) -> Vec<PreviewStateDto> {
    app.state::<PreviewHost>()
        .with(|registry| registry.snapshot())
        .iter()
        .map(PreviewStateDto::from)
        .collect()
}