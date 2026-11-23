//! Sweeping up after a failed create (Windows).
//!
//! ## Why this exists
//!
//! wry builds a child webview in two steps: `create_container_hwnd` makes the
//! `WRY_WEBVIEW` window, then the WebView2 controller is created inside it. Every
//! step after the first uses `?`, and **none of those early returns destroys the
//! container** — only `impl Drop for InnerWebView` does, and a half-built webview
//! never reaches it. So a failed create leaves a *visible*, empty, brush-less
//! child window sitting exactly where the preview was asked to be.
//!
//! That window is a real HWND above the app's content: it swallows every mouse
//! event over its rectangle. The user sees the panel (stale pixels) and cannot
//! click anything in it — the search box, the tree, half the chat input. It
//! survives closing the tab, because nothing in either process knows it exists.
//!
//! So the failure path has to clean up what the framework left behind. Matched
//! by class + "has no child window" (a live container holds the WebView2
//! controller; a husk holds nothing) and by *appearance since the attempt
//! started*, so a concurrent create that is still in flight can never be
//! destroyed by mistake. Windows-only: the macOS/Linux backends put the webview
//! in a view/widget, not in a window of its own.
//!
//! Known ceiling: this compensates for an upstream bug, not a design of ours. If
//! wry ever destroys the container on its own failure paths
//! (`wry::webview2::InnerWebView::new_in_hwnd`), delete this module and the call
//! site in `host::preview_open`.
//!
//! This is the only module in `browser` that touches Win32 window internals: the
//! session lifecycle in `host` asks it a question ("what appeared?") and tells it
//! what to destroy, and never handles an HWND itself.

use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, TRUE};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, EnumChildWindows, GetClassNameW, GetWindow, GW_CHILD,
};

/// Class name wry registers for the window of every webview it creates.
const CONTAINER_CLASS: &str = "WRY_WEBVIEW";

/// Is this window a webview container that a *failed* create left behind?
///
/// Only two kinds of window live here: a container that holds a live webview
/// (it has a `Chrome_WidgetWin_*` child, the WebView2 controller) and one
/// whose controller was never created (childless). Only the second kind is
/// garbage — and only under this class, so the app's own windows and the
/// main webview can never be swept.
fn is_empty_container(class: &str, has_child: bool) -> bool {
    class == CONTAINER_CLASS && !has_child
}

fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    // The class name is a fixed literal for every container; 48 chars is
    // ample (the returned length is the truncation-safe copy count).
    let len = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..len.max(0) as usize])
}

unsafe extern "system" fn collect(hwnd: HWND, acc: LPARAM) -> BOOL {
    let acc = unsafe { &mut *(acc as *mut Vec<isize>) };
    let has_child = !unsafe { GetWindow(hwnd, GW_CHILD) }.is_null();
    if is_empty_container(&class_of(hwnd), has_child) {
        acc.push(hwnd as isize);
    }
    TRUE
}

/// HWNDs of webview containers under `parent` that hold no webview.
///
/// `parent` is the app's main window HWND, passed as an `isize` because a
/// raw pointer is not `Send` — and every caller of this module ends up
/// crossing a thread boundary.
pub fn empty_containers(parent: isize) -> Vec<isize> {
    let mut acc: Vec<isize> = Vec::new();
    unsafe {
        EnumChildWindows(
            parent as HWND,
            Some(collect),
            &mut acc as *mut _ as LPARAM,
        )
    };
    acc
}

/// Destroy the containers that appeared since `before` was sampled.
///
/// Must run on the window's owning thread: `DestroyWindow` fails across
/// threads. The caller hops to the main thread for exactly this reason.
pub fn destroy_new(parent: isize, before: &[isize]) {
    for hwnd in empty_containers(parent) {
        // Only what this attempt added: a container that was already empty
        // belongs to an earlier failure, and a concurrent create that is
        // still in flight must not be destroyed under it.
        if !before.contains(&hwnd) {
            unsafe { DestroyWindow(hwnd as HWND) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_empty_container;

    #[test]
    fn only_childless_webview_containers_are_stale() {
        // A container whose controller never got created: the husk that
        // swallowed clicks over the whole preview panel.
        assert!(is_empty_container("WRY_WEBVIEW", false));
        // A live webview (its controller is a child window): never touch.
        assert!(!is_empty_container("WRY_WEBVIEW", true));
        // Anything else in the window hierarchy is not ours to destroy.
        assert!(!is_empty_container("Chrome_WidgetWin_1", false));
        assert!(!is_empty_container("TAURI_DRAG_RESIZE_BORDERS", false));
    }
}