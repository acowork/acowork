//! ADR-059 §5.4 — Bootstrap orchestrator.
//!
//! Subscribes to the [`SubsystemReadinessRegistry`], recomputes the
//! aggregated [`BootstrapPhase`] on every transition, and exposes the
//! resulting [`BootstrapSnapshot`] to:
//!
//! - the retained MQTT publisher on `acowork/global/bootstrap` (Phase 1.1)
//! - the HTTP `GET /api/bootstrap` projection (Phase 1.3)
//!
//! Aggregation rules (per ADR-059 §5.1):
//!
//! | condition                                        | phase            |
//! |--------------------------------------------------|------------------|
//! | any required subsystem in `Failed`               | `FAILED`         |
//! | any required subsystem still in `Booting`        | `BOOTING`        |
//! | all required ready, no optional failed           | `READY`          |
//! | all required ready, some optional failed/skipped | `DEGRADED`       |
//! | Gateway is shutting down                         | `SHUTTING_DOWN`  |
//!
//! `SHUTTING_DOWN` overrides every other condition and is set
//! imperatively via [`BootstrapOrchestrator::mark_shutting_down`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::bootstrap::registry::{
    SharedSubsystemReadinessRegistry, SubsystemReadinessRegistry, SubsystemState,
};
use crate::bootstrap::BOOTSTRAP_PROTOCOL_VERSION;

/// Aggregated lifecycle phase of the Gateway.
///
/// Mirrors the proto enum
/// `acowork.mqtt.v1.BootstrapPhase`. The two definitions MUST stay in
/// sync; the conversion is centralised in
/// [`BootstrapPhase::from_proto_i32`] / [`BootstrapPhase::as_proto_i32`].
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BootstrapPhase {
    /// Subsystem view has not yet been populated; only used in tests
    /// before any subsystem registers. Wire-level UNSPECIFIED is
    /// never emitted by the orchestrator.
    Unspecified,
    /// At least one required subsystem is still booting.
    Booting,
    /// All required subsystems ready, no optional failed.
    Ready,
    /// All required subsystems ready, but one or more optional ones
    /// failed or were skipped.
    Degraded,
    /// A required subsystem has failed; the Gateway should not be
    /// considered safe.
    Failed,
    /// Gateway is in graceful shutdown. Wire-level UNSPECIFIED will be
    /// observed by transient consumers once the retained snapshot is
    /// cleared on exit.
    ShuttingDown,
}

impl BootstrapPhase {
    /// Convert from the proto enum's i32 representation.
    ///
    /// Unknown values map to [`BootstrapPhase::Unspecified`] — the
    /// orchestrator MUST emit a recognised value, but consumers reading
    /// older or future-defined payloads treat unknowns as
    /// transient/booting per ADR-059 §5.1.
    pub fn from_proto_i32(value: i32) -> Self {
        match value {
            0 => BootstrapPhase::Unspecified,
            1 => BootstrapPhase::Booting,
            2 => BootstrapPhase::Ready,
            3 => BootstrapPhase::Degraded,
            4 => BootstrapPhase::Failed,
            5 => BootstrapPhase::ShuttingDown,
            _ => BootstrapPhase::Unspecified,
        }
    }

    /// Convert to the proto enum's i32 representation.
    pub fn as_proto_i32(self) -> i32 {
        match self {
            BootstrapPhase::Unspecified => 0,
            BootstrapPhase::Booting => 1,
            BootstrapPhase::Ready => 2,
            BootstrapPhase::Degraded => 3,
            BootstrapPhase::Failed => 4,
            BootstrapPhase::ShuttingDown => 5,
        }
    }
}

/// In-memory snapshot of the Gateway's aggregated bootstrap state.
///
/// Contains both the protocol-level fields exposed on the wire
/// (`instance_id` / `version` / `phase` / `phase_detail` /
/// `issued_at_ms`) and a `phase_detail` derived from the current
/// subsystem picture. Per-subsystem detail is intentionally NOT part
/// of the snapshot — see ADR-059 §5.4.4.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapSnapshot {
    pub protocol_version: u32,
    pub instance_id: String,
    pub version: u64,
    pub phase: BootstrapPhase,
    pub phase_detail: String,
    pub issued_at_ms: u64,
}

impl BootstrapSnapshot {
    /// Construct a minimal snapshot — `phase = BOOTING`, version 1,
    /// issued at "now". Used by tests and by the orchestrator's
    /// initial state before any subsystem has registered.
    pub fn booting(instance_id: impl Into<String>) -> Self {
        Self {
            protocol_version: BOOTSTRAP_PROTOCOL_VERSION,
            instance_id: instance_id.into(),
            version: 1,
            phase: BootstrapPhase::Booting,
            phase_detail: "initialising".to_string(),
            issued_at_ms: now_ms(),
        }
    }

    /// Compute the wire-level proto representation.
    ///
    /// The proto type is generated from
    /// `core/acowork-core/proto/mqtt_payload.proto`. Importing it here
    /// keeps the orchestrator independent of MQTT plumbing: callers
    /// (the publisher and the HTTP handler) wrap the result in a
    /// `DataEnvelope` themselves.
    pub fn to_proto(&self) -> acowork_core::mqtt_proto::BootstrapState {
        acowork_core::mqtt_proto::BootstrapState {
            protocol_version: self.protocol_version,
            instance_id: self.instance_id.clone(),
            version: self.version,
            phase: self.phase.as_proto_i32(),
            phase_detail: self.phase_detail.clone(),
            issued_at_ms: self.issued_at_ms,
        }
    }
}

/// Wall-clock milliseconds since the Unix epoch.
///
/// Wrapping `std::time::SystemTime` in a function makes it trivial to
/// swap for a fake clock in tests without exposing it through the
/// public API.
fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Gateway-internal bootstrap orchestrator.
///
/// Holds the live snapshot under an `RwLock`, recomputes on registry
/// events, and exposes a read API for the publisher and HTTP layer.
///
/// Construction:
///
/// ```ignore
/// let registry = SubsystemReadinessRegistry::new_shared();
/// let orchestrator = BootstrapOrchestrator::new(
///     "gateway-instance-uuid".to_string(),
///     registry.clone(),
/// );
///
/// // Subsystems register themselves and receive a handle:
/// let vault_handle = registry.register("vault", ReadinessKind::Required);
/// let publisher_handle = registry.register("publisher", ReadinessKind::Required);
/// // Per-node readiness is registered dynamically as nodes come
/// // online (see mqtt/dispatch.rs):
/// let node_handle = registry.register(
///     format!("node.{node_id}"),
///     ReadinessKind::Required,
/// );
///
/// vault_handle.mark_ready(None);
/// publisher_handle.mark_ready(None);
/// node_handle.mark_ready(None);
/// ```
pub struct BootstrapOrchestrator {
    instance_id: String,
    registry: SharedSubsystemReadinessRegistry,
    snapshot: RwLock<BootstrapSnapshot>,
    version: AtomicU64,
    /// Watch channel carrying the latest snapshot version. Every
    /// recompute / `mark_shutting_down` sends the new version; the
    /// retained MQTT publisher (`acowork/global/bootstrap`) parks on
    /// `changed()` and republishes whenever the value moves — unlike
    /// a `Notify`, a watch receiver NEVER misses a transition that
    /// happened before its `changed()` future was created, so every
    /// state change is eventually published (ADR-059 §7.2 re-delivery
    /// consistency).
    change_watch: watch::Sender<u64>,
}

impl BootstrapOrchestrator {
    /// Build a new orchestrator.
    ///
    /// `instance_id` MUST be unique per Gateway process and stable for
    /// the lifetime of the process — typically a UUID generated at
    /// startup and persisted to the data dir (Phase 0.3).
    pub fn new(
        instance_id: String,
        registry: SharedSubsystemReadinessRegistry,
    ) -> Arc<Self> {
        let snapshot = BootstrapSnapshot::booting(instance_id.clone());
        let subscription = registry.subscribe();
        let (change_tx, _) = watch::channel(1u64);
        let orchestrator = Arc::new(Self {
            instance_id,
            registry,
            snapshot: RwLock::new(snapshot),
            version: AtomicU64::new(1),
            change_watch: change_tx,
        });
        // Do NOT recompute here: at construction time the subsystem
        // registry is empty (subsystems register themselves later,
        // after the orchestrator is wired in). Recomputing now would
        // bump `version` to 2 and publish an unnecessary BOOTING
        // snapshot. The first recompute happens on the first
        // readiness update from the registry.
        //
        // Spawn the background listener that recomputes the snapshot
        // on every registry event, so subsystem `mark_ready` calls
        // drive the aggregated phase automatically. The listener holds
        // only a `Weak` reference: when the last strong reference to
        // the orchestrator is dropped the next `upgrade()` fails and
        // the loop exits, releasing the broadcast receiver (no
        // ref-cycle keeps the registry alive). Synchronous unit tests
        // run without a tokio runtime, so the spawn is guarded by
        // `Handle::try_current()` — those tests drive `recompute()`
        // manually.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let weak = Arc::downgrade(&orchestrator);
            handle.spawn(async move {
                let mut rx = subscription;
                while rx.recv().await.is_ok() {
                    let Some(orch) = weak.upgrade() else {
                        break;
                    };
                    orch.recompute();
                }
            });
        }
        orchestrator
    }

    /// Test-only constructor that injects an arbitrary
    /// [`BootstrapSnapshot`] instead of letting the orchestrator derive
    /// one from the subsystem registry. Used by HTTP handler tests that
    /// need to exercise the `Booting` / `Failed` / `ShuttingDown` /
    /// `Degraded` branches without standing up real subsystems.
    ///
    /// `#[cfg(test)]` keeps the helper out of release builds; the
    /// public constructor ([`Self::new`]) is the only blessed entry
    /// point in production.
    #[cfg(test)]
    pub fn from_snapshot_for_test(
        instance_id: String,
        registry: SharedSubsystemReadinessRegistry,
        snapshot: BootstrapSnapshot,
    ) -> Arc<Self> {
        let (change_tx, _rx) = watch::channel(1u64);
        Arc::new(Self {
            instance_id,
            registry,
            snapshot: RwLock::new(snapshot),
            version: AtomicU64::new(1),
            change_watch: change_tx,
        })
    }

    /// Recompute the snapshot from the current registry state.
    ///
    /// Called by the background listener on every event and once on
    /// construction. Not `async` so it can run inside a synchronous
    /// recompute loop or directly in tests.
    pub fn recompute(self: &Arc<Self>) {
        let mut new_snapshot = self.snapshot.read().clone();
        let registry = &self.registry;

        // SHUTTING_DOWN is sticky: set by `mark_shutting_down`. Do not
        // recompute away from it.
        if new_snapshot.phase == BootstrapPhase::ShuttingDown {
            return;
        }

        let new_phase = if registry.any_required_failed() {
            BootstrapPhase::Failed
        } else if registry.any_required_booting() {
            BootstrapPhase::Booting
        } else if registry.all_required_ready() {
            if registry.any_optional_failed() {
                BootstrapPhase::Degraded
            } else {
                BootstrapPhase::Ready
            }
        } else {
            // No required subsystems registered yet: still booting.
            BootstrapPhase::Booting
        };

        let detail = derive_phase_detail(new_phase, registry);

        // Name the blockers in the Gateway's OWN log.
        //
        // ADR-059 §5.4.4 forbids subsystem identifiers on the wire, and
        // that rule is right: a client must not have to know the
        // Gateway's internal decomposition. But it leaves an operator
        // staring at "2/5 required ready" with no way to tell which
        // subsystem is stuck — the 2026-11-24 incident ended exactly
        // there. The protocol stays aggregate; the process that owns the
        // subsystems is the one that names them.
        if matches!(new_phase, BootstrapPhase::Booting | BootstrapPhase::Failed) {
            let blockers: Vec<String> = registry
                .snapshot()
                .into_iter()
                .filter(|(_, state, kind, _)| {
                    *kind == crate::bootstrap::registry::ReadinessKind::Required
                        && *state != SubsystemState::Ready
                        && *state != SubsystemState::Skipped
                })
                .map(|(id, state, _, detail)| match detail {
                    Some(d) if !d.is_empty() => format!("{id}={state:?}({d})"),
                    _ => format!("{id}={state:?}"),
                })
                .collect();
            if !blockers.is_empty() {
                tracing::warn!(
                    phase = ?new_phase,
                    blockers = %blockers.join(", "),
                    "Gateway not ready — blocking required subsystems"
                );
            }
        }

        let version = self.version.fetch_add(1, Ordering::SeqCst) + 1;

        new_snapshot.phase = new_phase;
        new_snapshot.phase_detail = detail;
        new_snapshot.version = version;
        new_snapshot.issued_at_ms = now_ms();

        *self.snapshot.write() = new_snapshot;

        // Notify subscribers (e.g. the retained MQTT publisher) that
        // a new snapshot is available. `watch::Sender::send_replace`
        // keeps the latest version available to receivers that have
        // not yet polled — a transition that happens while the
        // publisher is between `changed()` calls is never lost.
        let _ = self.change_watch.send_replace(version);
    }

    /// Mark the Gateway as shutting down.
    ///
    /// Idempotent. After this call the snapshot stays in
    /// `SHUTTING_DOWN` and `recompute` is a no-op. The retained MQTT
    /// publisher (Phase 1.1) is responsible for clearing the retained
    /// payload on exit so transient consumers observe
    /// `UNSPECIFIED`.
    pub fn mark_shutting_down(self: &Arc<Self>) {
        {
            let mut snap = self.snapshot.write();
            if snap.phase == BootstrapPhase::ShuttingDown {
                return;
            }
            let version = self.version.fetch_add(1, Ordering::SeqCst) + 1;
            snap.phase = BootstrapPhase::ShuttingDown;
            snap.phase_detail = "gateway shutting down".to_string();
            snap.version = version;
            snap.issued_at_ms = now_ms();
            // Notify subscribers (e.g. the retained MQTT publisher) of
            // the new snapshot version; see `recompute` for the same
            // watch-based signalling.
            let _ = self.change_watch.send_replace(version);
        }
        tracing::info!(
            instance_id = %self.instance_id,
            "Bootstrap snapshot marked SHUTTING_DOWN"
        );
    }

    /// Read the current snapshot.
    ///
    /// Cheap: `RwLock` acquire + clone.
    pub fn snapshot(&self) -> BootstrapSnapshot {
        self.snapshot.read().clone()
    }

    /// Stable instance id of this Gateway.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Subscribe to snapshot-change notifications.
    ///
    /// Returns a watch receiver carrying the latest snapshot version.
    /// [`tokio::sync::watch::Receiver::changed`] resolves whenever a
    /// new version is produced (recompute / `mark_shutting_down`) —
    /// including a change that happened before the `changed()` future
    /// was created, so subscribers never miss a transition. New
    /// subscribers must read [`Self::snapshot`] once on construction
    /// to seed their local state.
    ///
    /// Used by the retained MQTT publisher (`acowork/global/bootstrap`,
    /// Phase 1.1) to republish on every snapshot change without polling.
    pub fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.change_watch.subscribe()
    }
}

/// Build a human-readable `phase_detail` from the current registry
/// view. Aggregated counts only — never include subsystem IDs or
/// error details (ADR-059 §5.4.4).
fn derive_phase_detail(
    phase: BootstrapPhase,
    registry: &SubsystemReadinessRegistry,
) -> String {
    let snap = registry.snapshot();
    if snap.is_empty() {
        return match phase {
            BootstrapPhase::Booting => "initialising".to_string(),
            other => format!("{other:?}").to_lowercase(),
        };
    }
    let total_required = snap
        .iter()
        .filter(|(_, _, kind, _)| *kind == crate::bootstrap::registry::ReadinessKind::Required)
        .count();
    let ready_required = snap
        .iter()
        .filter(|(_, state, kind, _)| {
            *kind == crate::bootstrap::registry::ReadinessKind::Required
                && *state == SubsystemState::Ready
        })
        .count();
    let failed_required = snap
        .iter()
        .filter(|(_, state, kind, _)| {
            *kind == crate::bootstrap::registry::ReadinessKind::Required
                && *state == SubsystemState::Failed
        })
        .count();
    let failed_optional = snap
        .iter()
        .filter(|(_, state, kind, _)| {
            *kind == crate::bootstrap::registry::ReadinessKind::Optional
                && *state == SubsystemState::Failed
        })
        .count();
    match phase {
        BootstrapPhase::Booting => format!("{ready_required}/{total_required} required ready"),
        BootstrapPhase::Ready => format!("{ready_required}/{total_required} required ready"),
        BootstrapPhase::Degraded => format!(
            "{ready_required}/{total_required} required ready, {failed_optional} optional failed"
        ),
        BootstrapPhase::Failed => format!(
            "{failed_required}/{total_required} required failed"
        ),
        BootstrapPhase::ShuttingDown => "gateway shutting down".to_string(),
        BootstrapPhase::Unspecified => "unspecified".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::registry::ReadinessKind;

    fn build() -> (Arc<BootstrapOrchestrator>, SharedSubsystemReadinessRegistry) {
        let registry = SubsystemReadinessRegistry::new_shared();
        let orchestrator = BootstrapOrchestrator::new("instance-A".into(), registry.clone());
        (orchestrator, registry)
    }

    #[test]
    fn empty_registry_is_booting() {
        let (orch, _) = build();
        let snap = orch.snapshot();
        assert_eq!(snap.phase, BootstrapPhase::Booting);
        assert_eq!(snap.instance_id, "instance-A");
        assert_eq!(snap.version, 1);
    }

    #[test]
    fn all_required_ready_transitions_to_ready() {
        let (orch, registry) = build();
        let h1 = registry.register("vault", ReadinessKind::Required);
        let h2 = registry.register("publisher", ReadinessKind::Required);
        orch.recompute();

        h1.mark_ready(None);
        orch.recompute();
        assert_eq!(orch.snapshot().phase, BootstrapPhase::Booting);

        h2.mark_ready(None);
        orch.recompute();
        let snap = orch.snapshot();
        assert_eq!(snap.phase, BootstrapPhase::Ready);
        assert!(snap.phase_detail.contains("required ready"));
        assert!(snap.version >= 3);
    }

    #[test]
    fn optional_failure_yields_degraded() {
        let (orch, registry) = build();
        registry.register("vault", ReadinessKind::Required).mark_ready(None);
        registry.register("publisher", ReadinessKind::Required).mark_ready(None);
        registry.register("embedding", ReadinessKind::Optional).mark_failed(None);
        orch.recompute();
        let snap = orch.snapshot();
        assert_eq!(snap.phase, BootstrapPhase::Degraded);
        assert!(snap.phase_detail.contains("optional failed"));
    }

    #[test]
    fn optional_booting_does_not_block_ready() {
        // Regression for the "killed remote node blocks Desktop
        // startup" bug: a node subsystem demoted to Booting is
        // Optional and must not drag the aggregated phase out of
        // READY (a starting Desktop waits for READY/DEGRADED).
        let (orch, registry) = build();
        registry.register("vault", ReadinessKind::Required).mark_ready(None);
        registry.register("publisher", ReadinessKind::Required).mark_ready(None);
        let node = registry.register("node.remote", ReadinessKind::Optional);
        node.mark_ready(None);
        orch.recompute();
        assert_eq!(orch.snapshot().phase, BootstrapPhase::Ready);

        node.mark_booting(Some("node 'remote' reported offline".into()));
        orch.recompute();
        assert_eq!(
            orch.snapshot().phase,
            BootstrapPhase::Ready,
            "an optional subsystem going Booting must leave READY untouched"
        );
    }

    #[test]
    fn required_failure_yields_failed() {
        let (orch, registry) = build();
        registry.register("vault", ReadinessKind::Required).mark_ready(None);
        registry.register("publisher", ReadinessKind::Required).mark_failed(None);
        registry.register("embedding", ReadinessKind::Optional).mark_ready(None);
        orch.recompute();
        assert_eq!(orch.snapshot().phase, BootstrapPhase::Failed);
    }

    /// Matrix #5 (`mqtt.enabled = false`): the wiring in `gateway/mod.rs`
    /// marks BOTH required MQTT subsystems `Skipped`. A disabled feature
    /// is not a boot blocker — the aggregated phase must reach READY,
    /// not sit in BOOTING for the lifetime of the process (the second
    /// dead-end found in the 2026-11-24 incident review).
    #[test]
    fn required_skipped_satisfies_readiness() {
        let (orch, registry) = build();
        registry.register("vault", ReadinessKind::Required).mark_ready(None);
        let mqtt = registry.register("mqtt", ReadinessKind::Required);
        let publisher = registry.register("publisher", ReadinessKind::Required);
        mqtt.mark_skipped(Some("mqtt disabled by configuration".into()));
        publisher.mark_skipped(Some("mqtt disabled by configuration".into()));
        orch.recompute();
        assert_eq!(
            orch.snapshot().phase,
            BootstrapPhase::Ready,
            "a configuration-disabled required subsystem must not block READY"
        );
    }

    #[test]
    fn version_is_monotonic() {
        let (orch, registry) = build();
        let v0 = orch.snapshot().version;
        let h = registry.register("vault", ReadinessKind::Required);
        orch.recompute();
        h.mark_ready(None);
        orch.recompute();
        h.mark_ready(None); // idempotent
        orch.recompute();
        let v_final = orch.snapshot().version;
        assert!(v_final > v0, "version must advance ({v0} -> {v_final})");
    }

    #[test]
    fn shutting_down_is_sticky() {
        let (orch, registry) = build();
        registry.register("vault", ReadinessKind::Required).mark_ready(None);
        registry.register("publisher", ReadinessKind::Required).mark_ready(None);
        orch.recompute();
        assert_eq!(orch.snapshot().phase, BootstrapPhase::Ready);

        orch.mark_shutting_down();
        assert_eq!(orch.snapshot().phase, BootstrapPhase::ShuttingDown);

        // Adding a ready subsystem does not leave SHUTTING_DOWN.
        registry.register("late", ReadinessKind::Required).mark_ready(None);
        orch.recompute();
        assert_eq!(orch.snapshot().phase, BootstrapPhase::ShuttingDown);
    }

    #[test]
    fn mark_shutting_down_is_idempotent() {
        let (orch, _) = build();
        let v0 = orch.snapshot().version;
        orch.mark_shutting_down();
        let v1 = orch.snapshot().version;
        assert!(v1 > v0);
        orch.mark_shutting_down();
        assert_eq!(orch.snapshot().version, v1);
    }

    #[test]
    fn phase_proto_roundtrip() {
        for phase in [
            BootstrapPhase::Unspecified,
            BootstrapPhase::Booting,
            BootstrapPhase::Ready,
            BootstrapPhase::Degraded,
            BootstrapPhase::Failed,
            BootstrapPhase::ShuttingDown,
        ] {
            let i = phase.as_proto_i32();
            let back = BootstrapPhase::from_proto_i32(i);
            assert_eq!(back, phase);
        }
        // Unknown future value maps to Unspecified.
        assert_eq!(
            BootstrapPhase::from_proto_i32(9999),
            BootstrapPhase::Unspecified
        );
    }

    #[test]
    fn phase_detail_never_exposes_subsystem_ids() {
        let (orch, registry) = build();
        registry.register("vault", ReadinessKind::Required).mark_ready(None);
        registry.register("publisher", ReadinessKind::Required).mark_ready(None);
        registry
            .register("embedding", ReadinessKind::Optional)
            .mark_failed(None);
        orch.recompute();
        let snap = orch.snapshot();
        for forbidden in ["vault", "publisher", "embedding"] {
            assert!(
                !snap.phase_detail.contains(forbidden),
                "phase_detail leaked subsystem id '{forbidden}': {}",
                snap.phase_detail
            );
        }
    }

    #[tokio::test]
    async fn subscribe_changes_wakes_on_recompute() {
        let (orch, registry) = build();
        let mut rx = orch.subscribe_changes();
        // Park on changed() BEFORE we drive the change.
        let waiter = rx.changed();
        let h = registry.register("vault", ReadinessKind::Required);
        h.mark_ready(None);
        orch.recompute();
        // Bound the wait so a regression doesn't hang the test forever.
        let woke = tokio::time::timeout(std::time::Duration::from_millis(500), waiter).await;
        assert!(
            woke.is_ok(),
            "subscribe_changes should wake after recompute"
        );
    }

    #[tokio::test]
    async fn subscribe_changes_wakes_on_shutting_down() {
        let (orch, _) = build();
        let mut rx = orch.subscribe_changes();
        let waiter = rx.changed();
        orch.mark_shutting_down();
        let woke = tokio::time::timeout(std::time::Duration::from_millis(500), waiter).await;
        assert!(
            woke.is_ok(),
            "subscribe_changes should wake after mark_shutting_down"
        );
    }

    #[tokio::test]
    async fn background_listener_recomputes_on_events() {
        // Regression test for the Phase 1.2 listener: a subsystem
        // `mark_ready` must drive the aggregated phase automatically
        // (no manual `recompute` in production code).
        let (orch, registry) = build();
        assert_eq!(orch.snapshot().phase, BootstrapPhase::Booting);

        let vault = registry.register("vault", ReadinessKind::Required);
        let publisher = registry.register("publisher", ReadinessKind::Required);
        vault.mark_ready(None);

        // The listener consumes the event asynchronously; poll briefly.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
        while orch.snapshot().version < 2 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(orch.snapshot().phase, BootstrapPhase::Booting);

        publisher.mark_ready(None);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
        while orch.snapshot().phase != BootstrapPhase::Ready
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(orch.snapshot().phase, BootstrapPhase::Ready);
        assert!(orch.snapshot().version >= 3);
    }

    #[tokio::test]
    async fn background_listener_stops_when_orchestrator_dropped() {
        // The listener holds only a Weak reference; dropping the last
        // strong ref must terminate the loop (no leak / no busy spin).
        let registry = SubsystemReadinessRegistry::new_shared();
        let orch = BootstrapOrchestrator::new("instance-A".into(), registry.clone());
        let handle = registry.register("vault", ReadinessKind::Required);
        handle.mark_ready(None);
        drop(orch);
        // If the listener leaked a strong ref, this would never print.
        registry.register("late", ReadinessKind::Required);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // ── Subscriber-side protocol semantics (ADR-059 §8.3) ────────────

    /// Minimal mock of a snapshot subscriber (the Desktop's MQTT
    /// handler) implementing the ADR-059 §8.3 acceptance rule:
    ///
    /// - same `instance_id`: accept only strictly newer `version`s
    ///   (QoS 1 duplicates and out-of-order redelivery are dropped);
    /// - new `instance_id` with `version == 1`: a NEW Gateway process
    ///   publishes its first retained snapshot at version 1 (BOOTING) —
    ///   accept it and reset the version baseline;
    /// - any OTHER cross-instance payload (a stale retained redelivery
    ///   from the old process) is REJECTED — the old instance's state
    ///   must never overwrite the new instance's.
    struct MockSubscriber {
        instance_id: Option<String>,
        version: u64,
    }

    impl MockSubscriber {
        fn new() -> Self {
            Self {
                instance_id: None,
                version: 0,
            }
        }

        /// Returns true when the candidate snapshot is applied.
        fn apply(&mut self, snap: &BootstrapSnapshot) -> bool {
            match &self.instance_id {
                Some(current) if *current == snap.instance_id => {
                    if snap.version > self.version {
                        self.version = snap.version;
                        true
                    } else {
                        false
                    }
                }
                Some(_) => {
                    // Cross-instance event. Only the new process's
                    // very first snapshot (version 1) wins the switch;
                    // anything else is a stale redelivery.
                    if snap.version == 1 {
                        self.instance_id = Some(snap.instance_id.clone());
                        self.version = snap.version;
                        true
                    } else {
                        false
                    }
                }
                None => {
                    self.instance_id = Some(snap.instance_id.clone());
                    self.version = snap.version;
                    true
                }
            }
        }
    }

    fn snap(instance: &str, version: u64) -> BootstrapSnapshot {
        BootstrapSnapshot {
            protocol_version: 1,
            instance_id: instance.to_string(),
            version,
            phase: BootstrapPhase::Ready,
            phase_detail: String::new(),
            issued_at_ms: 0,
        }
    }

    #[test]
    fn mock_subscriber_rejects_old_instance_snapshots() {
        let mut sub = MockSubscriber::new();

        // Instance-A reaches version 7 (READY).
        assert!(sub.apply(&snap("instance-A", 1)));
        assert!(sub.apply(&snap("instance-A", 7)));
        // QoS 1 duplicate / stale redelivery: same version rejected.
        assert!(!sub.apply(&snap("instance-A", 7)));
        assert!(!sub.apply(&snap("instance-A", 3)));

        // Gateway restarts: instance-B counter restarts at 1.
        assert!(sub.apply(&snap("instance-B", 1)));
        assert_eq!(sub.version, 1);

        // A stale retained payload from instance-A (version 7) must be
        // rejected — the subscriber's baseline is now instance-B.
        assert!(!sub.apply(&snap("instance-A", 7)));
        assert!(!sub.apply(&snap("instance-A", 8)));

        // instance-B advances normally.
        assert!(sub.apply(&snap("instance-B", 2)));
    }
}