//! Preview session registry — the **data model** of the in-app browser host.
//!
//! This module is deliberately free of Tauri types: it is the bookkeeping a
//! browser surface needs (which sessions exist, what each one is showing, where
//! it sits, which one to evict) and nothing else. `host.rs` owns the Tauri side
//! (creating/destroying webviews, emitting events) and translates to and from
//! these structs. Keeping the split means the eviction and label rules are
//! covered by plain unit tests, with no window or event loop required.
//!
//! ## Why labels are never reused
//!
//! Labels are the only identity a webview has on the wire (IPC commands,
//! events, `get_webview`), and they are also what
//! [`crate::navigation_policy::NavigationRole`] keys off. A monotonic counter
//! means a stale reference from the frontend (a closed tab, a raced
//! `set_bounds`) fails loudly as "unknown preview" instead of silently
//! addressing a newer session that happens to occupy the same slot.

use serde::{Deserialize, Serialize};

/// How many preview webviews may stay alive at once.
///
/// Each webview is a full WebView2 renderer, so this is a memory bound, not a
/// UI bound: closing the oldest *inactive* session and reloading it on return
/// is much cheaper than keeping an unbounded pile of renderers resident. Three
/// covers the realistic working set (a page, its docs, its PR) without the
/// tab-switch reload becoming noticeable.
pub const MAX_LIVE_SESSIONS: usize = 3;

/// Rectangle of a preview webview, in **logical** pixels relative to the
/// top-left corner of the window's client area — the same coordinate space
/// `WebviewBuilder`/`add_child` take, and the space `getBoundingClientRect()`
/// is measured in.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PreviewBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl PreviewBounds {
    /// Whether this rectangle can display anything at all.
    pub fn is_empty(&self) -> bool {
        self.width < 1.0 || self.height < 1.0
    }
}

/// Load phase of one preview session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Created, no page-load callback has fired yet.
    Loading,
    /// A `PageLoadEvent::Finished` has been observed.
    Loaded,
}

impl Phase {
    /// Stable wire name (`loading` / `loaded`).
    ///
    /// There is no `failed` phase: a session that cannot be created is reported
    /// as a rejected command, and the renderer exposes no "crashed" signal we
    /// could turn into one. Inventing the state in the data model would make it
    /// look observable when it is not.
    pub fn as_str(&self) -> &'static str {
        match self {
            Phase::Loading => "loading",
            Phase::Loaded => "loaded",
        }
    }
}

/// One live preview session.
#[derive(Debug, Clone)]
pub struct PreviewSession {
    /// Webview label (`preview-<n>`), unique for the process lifetime.
    pub label: String,
    /// Current document URL, as reported by the page-load callback.
    pub url: String,
    /// Document title as reported by the engine; `None` until it reports one.
    pub title: Option<String>,
    /// Which page is loaded.
    pub phase: Phase,
    /// Where the webview sits.
    pub bounds: PreviewBounds,
    /// Whether the webview is currently shown.
    pub visible: bool,
    /// Monotonic timestamp of the last time this session was the active one —
    /// the eviction order.
    pub last_active_ms: u64,
}

/// Registry of live preview sessions.
#[derive(Debug, Default)]
pub struct PreviewRegistry {
    next_id: u64,
    sessions: Vec<PreviewSession>,
}

impl PreviewRegistry {
    /// Allocate the next label. Never reused (see the module docs).
    pub fn allocate_label(&mut self) -> String {
        self.next_id += 1;
        format!("{}{}", crate::navigation_policy::PREVIEW_LABEL_PREFIX, self.next_id)
    }

    /// Register a new session and return it. The caller creates the webview.
    pub fn open(&mut self, url: String, bounds: PreviewBounds, now_ms: u64) -> PreviewSession {
        let label = self.allocate_label();
        let session = PreviewSession {
            label,
            url,
            title: None,
            phase: Phase::Loading,
            bounds,
            visible: true,
            last_active_ms: now_ms,
        };
        self.sessions.push(session.clone());
        session
    }

    pub fn get(&self, label: &str) -> Option<&PreviewSession> {
        self.sessions.iter().find(|s| s.label == label)
    }

    /// Mutate one session in place. Returns `false` when the label is unknown —
    /// callers use that to answer `unknown_preview` instead of pretending.
    pub fn update(&mut self, label: &str, f: impl FnOnce(&mut PreviewSession)) -> bool {
        match self.sessions.iter_mut().find(|s| s.label == label) {
            Some(session) => {
                f(session);
                true
            }
            None => false,
        }
    }

    /// Mark a session as the active one (eviction order, not visibility).
    pub fn touch(&mut self, label: &str, now_ms: u64) -> bool {
        self.update(label, |s| s.last_active_ms = now_ms)
    }

    /// Forget one session; the caller tears the webview down.
    pub fn remove(&mut self, label: &str) -> Option<PreviewSession> {
        let index = self.sessions.iter().position(|s| s.label == label)?;
        Some(self.sessions.remove(index))
    }

    /// Forget every session (panel closed, app shutting down).
    pub fn drain(&mut self) -> Vec<PreviewSession> {
        std::mem::take(&mut self.sessions)
    }

    /// Sessions that must be closed to stay within [`MAX_LIVE_SESSIONS`],
    /// oldest first, never including `keep`.
    ///
    /// Returns as many labels as the caller has to close; an empty vec means
    /// we are inside the bound.
    pub fn overflow(&self, keep: &str) -> Vec<String> {
        let mut candidates: Vec<&PreviewSession> = self
            .sessions
            .iter()
            .filter(|s| s.label != keep)
            .collect();
        if self.sessions.len() <= MAX_LIVE_SESSIONS {
            return Vec::new();
        }
        candidates.sort_by_key(|s| s.last_active_ms);
        let excess = self.sessions.len() - MAX_LIVE_SESSIONS;
        candidates
            .into_iter()
            .take(excess)
            .map(|s| s.label.clone())
            .collect()
    }

    /// Snapshot for the frontend (ordered by label, so a list render is stable).
    pub fn snapshot(&self) -> Vec<PreviewSession> {
        let mut all = self.sessions.clone();
        all.sort_by(|a, b| a.label.cmp(&b.label));
        all
    }
}

/// Milliseconds since the Unix epoch. The one place that reads the clock, so
/// registry tests can pass explicit timestamps instead of sleeping.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
