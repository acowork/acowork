//! Contract tests for [`acowork_desktop::browser::registry`] — the preview
//! session bookkeeping behind the in-app browser (ADR-118).
//!
//! Lives in `tests/` for the same mechanical reason as
//! `tests/navigation_policy.rs`: this crate's unit-test binary cannot start on
//! Windows without an application manifest. Run with `cargo test --tests`.

use acowork_desktop::browser::registry::{
    PreviewBounds, PreviewRegistry, Phase, MAX_LIVE_SESSIONS,
};

fn bounds() -> PreviewBounds {
    PreviewBounds {
        x: 10.0,
        y: 20.0,
        width: 300.0,
        height: 400.0,
    }
}

#[test]
fn labels_are_monotonic_and_prefixed() {
    let mut registry = PreviewRegistry::default();
    assert_eq!(registry.allocate_label(), "preview-1");
    assert_eq!(registry.allocate_label(), "preview-2");
    // A closed label is never handed out again: stale frontend references must
    // fail loudly, not address a newer session that occupies the same slot.
    let session = registry.open("https://a.test/".into(), bounds(), 1);
    assert_eq!(session.label, "preview-3");
    registry.remove(&session.label);
    let next = registry.allocate_label();
    assert_eq!(next, "preview-4");
    assert_ne!(next, session.label);
}

#[test]
fn open_starts_loading_and_visible() {
    let mut registry = PreviewRegistry::default();
    let session = registry.open("https://a.test/".into(), bounds(), 7);
    assert_eq!(session.phase, Phase::Loading);
    assert_eq!(session.phase.as_str(), "loading");
    assert!(session.visible);
    assert_eq!(session.last_active_ms, 7);
    assert_eq!(registry.snapshot().len(), 1);
}

#[test]
fn update_reports_unknown_labels_instead_of_creating() {
    let mut registry = PreviewRegistry::default();
    let session = registry.open("https://a.test/".into(), bounds(), 1);
    assert!(registry.update(&session.label, |s| s.phase = Phase::Loaded));
    assert!(!registry.update("preview-999", |s| s.phase = Phase::Loaded));
    assert_eq!(registry.snapshot().len(), 1);
}

#[test]
fn it_is_within_bound_until_max_plus_one() {
    let mut registry = PreviewRegistry::default();
    let mut labels = Vec::new();
    for i in 0..MAX_LIVE_SESSIONS {
        labels.push(
            registry
                .open(format!("https://{i}.test/"), bounds(), i as u64)
                .label,
        );
    }
    assert!(registry.overflow(labels.last().unwrap()).is_empty());
}

#[test]
fn overflow_evicts_the_least_recently_active_and_spares_the_newcomer() {
    let mut registry = PreviewRegistry::default();
    let a = registry.open("https://a.test/".into(), bounds(), 1);
    let b = registry.open("https://b.test/".into(), bounds(), 2);
    let c = registry.open("https://c.test/".into(), bounds(), 3);
    // a is touched last, so b becomes the eviction candidate.
    registry.touch(&a.label, 10);
    let d = registry.open("https://d.test/".into(), bounds(), 11);

    assert_eq!(registry.snapshot().len(), MAX_LIVE_SESSIONS + 1);
    let evict = registry.overflow(&d.label);
    assert_eq!(evict, vec![b.label.clone()]);
    assert!(!evict.contains(&c.label));
    assert!(!evict.contains(&d.label));
}

#[test]
fn overflow_returns_all_excess_when_several_are_over() {
    let mut registry = PreviewRegistry::default();
    for i in 0..(MAX_LIVE_SESSIONS + 2) {
        registry.open(format!("https://{i}.test/"), bounds(), i as u64);
    }
    let newest = registry.snapshot().last().unwrap().label.clone();
    assert_eq!(registry.overflow(&newest).len(), 2);
}

#[test]
fn drain_empties_the_registry() {
    let mut registry = PreviewRegistry::default();
    registry.open("https://a.test/".into(), bounds(), 1);
    registry.open("https://b.test/".into(), bounds(), 2);
    assert_eq!(registry.drain().len(), 2);
    assert!(registry.snapshot().is_empty());
}

#[test]
fn bounds_emptiness_is_the_visibility_switch() {
    let empty = PreviewBounds {
        x: 0.0,
        y: 0.0,
        width: 0.0,
        height: 0.0,
    };
    assert!(empty.is_empty());
    assert!(!bounds().is_empty());
    // A one-pixel sliver is still "empty enough" to be parked: sub-pixel
    // rounding must not leave a visible strip behind a dialog.
    assert!(PreviewBounds { width: 0.4, ..bounds() }.is_empty());
}