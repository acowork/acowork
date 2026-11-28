//! Instance identity write path — the **one** place a rename lands.
//!
//! # Two things, one fact
//!
//! The fact is `{work_dir}/config/agent_config.json` → `name`. The
//! sidebar / agent list is a **derived projection** of it, published by the
//! Node as a retained MQTT inventory row. The Runtime writes the fact; the
//! Node owns the projection. Renaming in-process (the `self_evolve` tool,
//! ADR-124 §4.1) used to write the file and stop, so the file said one
//! thing and the list view another — forever, because nothing re-read it.
//!
//! # Failure semantics: fail-visible, NOT fail-closed
//!
//! A rename is a fact first and a projection second, so a projection that
//! cannot complete keeps the rename and reports it:
//!
//! - [`NameApplyOutcome::NotApplicable`] — there is no projection hop at all
//!   (standalone / dev / tests: no `--http-advertise-endpoint`, no Node).
//!   The rename is a *complete* success and emits **no** warning and no
//!   audit line. Inventing a failure for a consumer that does not exist is
//!   noise, and it would make rename unusable in dev.
//! - [`NameApplyOutcome::Projected`] — the Node republished and returned 2xx.
//! - [`NameApplyOutcome::Pending`] — the fact is written and kept (never
//!   rolled back: the fact source is *correct*, reverting it would create the
//!   inconsistency we are fixing), the projection did not happen and is
//!   self-healing.
//!
//! Both mis-implementations are banned: "projection failed → pretend it worked"
//! (the defect this module exists to fix) and "roll the write back" (throws
//! away a true fact).
//!
//! # Why `Pending` converges
//!
//! `build_installed_info_fresh` (the Node) **re-reads** `agent_config.json`
//! on every republish (`read_identity`), so any later republish trigger
//! carries the rename over with no retry bookkeeping of ours:
//!
//! - the Node republishes its whole inventory on **every MQTT (re)connect**
//!   (ADR-055 §6.5, `control::` — which is also when a `Pending` caused by a
//!   dropped connection ends up carried);
//! - the `PUT avatar-config` reverse proxy hook;
//! - the next `identity/refresh` from any later rename.
//!
//! `Pending` is therefore a state that drains itself, not a queue that needs
//! draining. This is the premise the whole fail-visible trade rests on: keep
//! the rename (the fact is right) and let the projection catch up, because
//! catching up is free.

use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::agent_config::update_agent_config;
use crate::http::SharedAgentCore;

/// Reject absurd names at the trust boundary. Generous enough for any real
/// name; the point is to stop an unbounded string from reaching the file
/// and every list response.
pub const MAX_NAME_CHARS: usize = 100;

/// Projection attempts (first try + 2 retries), then [`NameApplyOutcome::Pending`].
const PROJECTION_ATTEMPTS: u32 = 3;

/// Backoff between projection attempts. The call is loopback-to-node on the
/// same trust domain, so a couple of hundred ms is already generous for a
/// transient blip; a real outage is `Pending`, not a stall.
const PROJECTION_BACKOFF: [Duration; 2] = [Duration::from_millis(100), Duration::from_millis(300)];

/// Per-attempt HTTP timeout, so a black-holed request cannot hang a rename.
///
/// Deliberately short: the target is the Node on loopback that spawned this
/// process, so an attempt either answers in milliseconds or is not going to.
/// Worst case a caller waits `3 * 2s + 0.4s` before getting `Pending` — a
/// rename must not hold a UI request hostage to a dead hop.
const PROJECTION_TIMEOUT: Duration = Duration::from_secs(2);

// ── Outcome ───────────────────────────────────────────────────────────

/// What happened to the identity **projection** after the name was written.
///
/// Three states, not two: "no consumer" and "consumer succeeded" are both
/// successes but they are not the same fact, and collapsing them is what
/// makes "standalone must stay silent" untestable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameApplyOutcome {
    /// No projection hop exists (standalone / dev / no Node). Complete success.
    NotApplicable,
    /// The Node republished the identity inventory; the list view is current.
    Projected,
    /// The name is in effect; the projection did not complete and will be
    /// carried over by the next republish. `reason` is diagnostic and never
    /// contains the node token.
    Pending { reason: String, attempts: u32 },
}

impl NameApplyOutcome {
    /// Wire name for this outcome: `"not_applicable"` | `"projected"` |
    /// `"pending"`. One spelling, so the tool result (S4) and the audit line
    /// (S5) cannot drift into two vocabularies for the same state.
    pub fn projection(&self) -> &'static str {
        match self {
            Self::NotApplicable => "not_applicable",
            Self::Projected => "projected",
            Self::Pending { .. } => "pending",
        }
    }
}

/// Why an identity write did not land. No partial state exists when this is
/// returned: validation runs before the write, and a failed read aborts it, so
/// the config on disk is untouched.
///
/// Shared by both doors, because the *avatar* fields live in the same
/// document: `PUT /avatar-config` writes `avatar`, `builtin_avatar` and `name`
/// through the same read-modify-write, so it classifies its failures the same
/// way (`Invalid` → `400`, `Persist` → `500`) instead of inventing a second
/// scheme for the same file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityError {
    /// The value is not usable — the caller's fault (HTTP `400`).
    Invalid(String),
    /// `agent_config.json` could not be read or written; nothing changed
    /// (HTTP `500`).
    Persist(String),
}

impl From<String> for IdentityError {
    /// The read/serialize errors of `update_agent_config` arrive as strings.
    fn from(message: String) -> Self {
        Self::Persist(message)
    }
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) | Self::Persist(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for IdentityError {}

// ── Validation ────────────────────────────────────────────────────────

/// Trim, reject empty / over-long / control-bearing names.
///
/// Control characters are rejected because a name is not free text: it is
/// interpolated into `{{agent_name}}`, a sidebar row and log lines.
pub fn validate_name(raw: &str) -> Result<String, IdentityError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(IdentityError::Invalid(
            "name cannot be empty: an agent always has a name".to_string(),
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(IdentityError::Invalid(
            "name must not contain control characters".to_string(),
        ));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(IdentityError::Invalid(format!(
            "name too long ({} > {MAX_NAME_CHARS} chars)",
            name.chars().count()
        )));
    }
    Ok(name.to_string())
}

// ── Projection ────────────────────────────────────────────────────────

/// The Node this Runtime's identity projection lives on.
///
/// Injected like `McpNotifyRef`: built once from the spawn-time config
/// (`--http-advertise-endpoint` + `--node-token`), so the rename path needs
/// no extra state and takes no extra argument.
#[derive(Clone, Debug)]
pub struct IdentityNotifier {
    /// Node reverse-proxy base URL (`http://{host}:19900`), no trailing slash.
    base: String,
    /// Gateway-issued node token. `None` = the Node spawned this Runtime
    /// without one; kept as `None` rather than dropped so a rename reports
    /// the *real* reason instead of claiming there is no projection hop.
    /// **Never** logged, echoed, or put in a `reason`.
    token: Option<String>,
    /// This Runtime's instance id (`{advertise}/agents/{instance_id}/...`).
    instance_id: String,
}

impl IdentityNotifier {
    pub fn new(
        base: impl Into<String>,
        token: Option<String>,
        instance_id: impl Into<String>,
    ) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            token,
            instance_id: instance_id.into(),
        }
    }

    /// One projection attempt. `Err(reason)` is diagnostic and token-free.
    async fn attempt(&self) -> Result<(), String> {
        let Some(token) = self.token.as_deref() else {
            return Err(
                "node token unavailable (Runtime was spawned without --node-token)".to_string(),
            );
        };
        let url = format!("{}/agents/{}/identity/refresh", self.base, self.instance_id);
        let response = reqwest::Client::new()
            .post(&url)
            .header("X-ACowork-Node-Token", token)
            .timeout(PROJECTION_TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    format!(
                        "node did not answer within {}s",
                        PROJECTION_TIMEOUT.as_secs()
                    )
                } else if e.is_connect() {
                    "node unreachable (connection failed)".to_string()
                } else {
                    "node request failed".to_string()
                }
            })?;
        let status = response.status().as_u16();
        if response.status().is_success() {
            Ok(())
        } else if status == 401 || status == 403 {
            Err(format!("node rejected the node token (HTTP {status})"))
        } else if status == 502 {
            // The Node got this far and could not publish the inventory (see
            // `package::republish_installed_info`) — a different thing for a
            // human to fix than a wrong token or a dead node.
            Err("node could not publish the identity inventory (HTTP 502)".to_string())
        } else {
            Err(format!("node answered HTTP {status}"))
        }
    }

    /// Project with retries; never returns an error — a failed projection is a
    /// [`NameApplyOutcome::Pending`], not a failed rename.
    async fn project(&self) -> NameApplyOutcome {
        let mut reason = String::new();
        for attempt in 0..PROJECTION_ATTEMPTS {
            match self.attempt().await {
                Ok(()) => return NameApplyOutcome::Projected,
                Err(why) => {
                    reason = why;
                    if let Some(delay) = PROJECTION_BACKOFF.get(attempt as usize) {
                        tokio::time::sleep(*delay).await;
                    }
                }
            }
        }
        NameApplyOutcome::Pending {
            reason,
            attempts: PROJECTION_ATTEMPTS,
        }
    }
}

/// The process's projection target, installed once at boot by
/// `startup::agent_init`.
///
/// A process constant, not a mutable global: it is written before the HTTP
/// server binds — so no request can observe a "not installed yet" window,
/// which would make "no Node" and "not yet" indistinguishable — and is
/// read-only afterwards. It is not a field on `HttpState` because these values
/// arrive as spawn-time flags that state has no other carrier for, and
/// threading one more argument through `RuntimeHttpServer::start` plus its ~27
/// test call sites to carry a constant buys nothing.
///
/// Reads go through [`node_projection`] and are passed to [`apply_name`] as an
/// **argument**, so the use case never reaches for process state itself: the
/// dependency is in the signature, and a test hands over its own fake or
/// `None` without touching this.
static NODE: OnceLock<IdentityNotifier> = OnceLock::new();

/// Install the projection target — once, at boot, before the HTTP server binds.
pub fn install_node_projection(node: IdentityNotifier) {
    debug_assert!(
        NODE.get().is_none(),
        "the node projection is installed once, at boot"
    );
    let _ = NODE.set(node);
}

/// This process's projection target; `None` = no Node spawned it (standalone /
/// dev / tests), which is [`NameApplyOutcome::NotApplicable`].
pub fn node_projection() -> Option<&'static IdentityNotifier> {
    NODE.get()
}

// ── The write path ────────────────────────────────────────────────────

/// Rename this instance: write the fact, hot-update the live core, then
/// project it to the Node (see the module docs for the three outcomes).
///
/// The two callers are `PUT /agents/{id}/avatar-config` (user rename) and
/// the `self_evolve` `rename` action (agent rename). One field, one writer
/// (ADR-111) — both routes land here, and nothing else writes `name`.
///
/// `node` is the projection target ([`node_projection`] in production, a fake
/// or `None` in tests) — an argument rather than a read of process state, so
/// there is exactly one entry point and no hidden boot-order contract.
pub async fn apply_name(
    work_dir: &Path,
    core_slot: &SharedAgentCore,
    node: Option<&IdentityNotifier>,
    name: &str,
) -> Result<NameApplyOutcome, IdentityError> {
    let name = validate_name(name)?;

    // The fact source, through the one read-modify-write primitive: a corrupt
    // or unreadable file is *reported*, never read as "no config" and written
    // back as defaults, which would blank every other field.
    update_agent_config(work_dir, |config| -> Result<(), IdentityError> {
        config.name = Some(name.clone());
        Ok(())
    })?;

    // Push the new name into the live core: the file is the durable record,
    // the core is what the next turn's `{{agent_name}}` reads. Without this a
    // rename takes effect in the sidebar immediately and in the agent's own
    // prompt only after a restart — the two-name drift again, spread over time.
    if let Some(core) = core_slot
        .read()
        .ok()
        .and_then(|slot| slot.as_ref().map(Arc::clone))
    {
        core.set_instance_name(name);
    }

    // From here on the rename has happened; every path returns `Ok`.
    let Some(notifier) = node else {
        // No consumer exists — say nothing. (No `warn!` on this branch, and no
        // audit line: `NotApplicable` is a complete success.)
        return Ok(NameApplyOutcome::NotApplicable);
    };
    let outcome = notifier.project().await;
    match &outcome {
        NameApplyOutcome::NotApplicable => {}
        NameApplyOutcome::Projected => {
            tracing::debug!(
                projection = outcome.projection(),
                "instance identity projected"
            );
        }
        NameApplyOutcome::Pending { reason, attempts } => {
            // Not a failure of the rename: the name is in effect, the sidebar
            // follows on the next republish (see the module docs).
            tracing::debug!(
                projection = outcome.projection(),
                attempts,
                reason = %reason,
                "instance identity projection deferred"
            );
        }
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_work_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "acowork-identity-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn stored_name(work_dir: &Path) -> Option<String> {
        crate::agent_config::load_agent_config(work_dir)
            .unwrap()
            .and_then(|c| c.name)
    }

    fn no_core() -> SharedAgentCore {
        Arc::new(std::sync::RwLock::new(None))
    }

    /// A stand-in Node: answers every `identity/refresh` with `status` and
    /// counts the calls.
    async fn fake_node(status: u16) -> (String, Arc<AtomicUsize>) {
        use axum::routing::post;
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let app = axum::Router::new().route(
            "/agents/{id}/identity/refresh",
            post(move || {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    axum::http::StatusCode::from_u16(status).unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (base, hits)
    }

    #[test]
    fn name_validation_trims_and_rejects_bad_values() {
        assert_eq!(validate_name("  Nova  ").unwrap(), "Nova");
        assert!(matches!(
            validate_name("   "),
            Err(IdentityError::Invalid(_))
        ));
        assert!(matches!(
            validate_name("bad\nname"),
            Err(IdentityError::Invalid(_))
        ));
        assert!(matches!(
            validate_name(&"x".repeat(MAX_NAME_CHARS + 1)),
            Err(IdentityError::Invalid(_))
        ));
        assert!(validate_name(&"x".repeat(MAX_NAME_CHARS)).is_ok());
    }

    /// `NotApplicable` = the rename is a **complete** success: no warning, no
    /// audit line, nothing to converge later. Inventing a failure here would
    /// break rename in standalone / dev mode.
    #[tokio::test]
    async fn rename_standalone_is_fail_visible_not_applicable() {
        let work_dir = temp_work_dir("standalone");
        let outcome = apply_name(&work_dir, &no_core(), None, "  Nova  ")
            .await
            .unwrap();

        assert_eq!(outcome, NameApplyOutcome::NotApplicable);
        assert_eq!(outcome.projection(), "not_applicable");
        // The fact was written, trimmed.
        assert_eq!(stored_name(&work_dir).as_deref(), Some("Nova"));
        // And nothing was invented about a projection that does not exist:
        // no audit line (the NotApplicable branch never reaches the audit
        // writer S5 adds, and warns nowhere — there is no `warn!` on it).
        assert!(!work_dir.join("config/self_change_log.jsonl").exists());
    }

    /// A config that cannot be parsed is a `Persist` error — never "no config",
    /// written back as defaults. That second behaviour destroyed every other
    /// field of the file, and it is the reason both doors share
    /// `agent_config::update_agent_config`.
    #[tokio::test]
    async fn rename_on_corrupt_config_reports_and_leaves_it_alone() {
        let work_dir = temp_work_dir("corrupt");
        std::fs::create_dir_all(work_dir.join("config")).unwrap();
        std::fs::write(work_dir.join("config/agent_config.json"), b"{ not json").unwrap();

        let err = apply_name(&work_dir, &no_core(), None, "Nova")
            .await
            .expect_err("a corrupt config must not be silently replaced");

        assert!(matches!(err, IdentityError::Persist(_)), "got {err:?}");
        let on_disk = std::fs::read_to_string(work_dir.join("config/agent_config.json")).unwrap();
        assert_eq!(on_disk, "{ not json", "the file must be left untouched");
    }

    /// Node present + token → `Projected`, and the Node really was called.
    #[tokio::test]
    async fn rename_with_node_and_token_is_projected() {
        let (base, hits) = fake_node(200).await;
        let work_dir = temp_work_dir("projected");
        let notifier = IdentityNotifier::new(base, Some("test-token".into()), "inst-1");

        let outcome = apply_name(&work_dir, &no_core(), Some(&notifier), "Nova")
            .await
            .unwrap();

        assert_eq!(outcome, NameApplyOutcome::Projected);
        assert_eq!(outcome.projection(), "projected");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(stored_name(&work_dir).as_deref(), Some("Nova"));
    }

    /// Node present, token missing → rename kept, projection `Pending` with a
    /// diagnosable reason, never an error, and the token never echoed.
    #[tokio::test]
    async fn rename_with_node_but_no_token_is_pending_not_failed() {
        let work_dir = temp_work_dir("pending-token");
        let notifier = IdentityNotifier::new("http://127.0.0.1:1", None, "inst-1");

        let outcome = apply_name(&work_dir, &no_core(), Some(&notifier), "Nova")
            .await
            .expect("a deferred projection must not fail the rename");

        match outcome {
            NameApplyOutcome::Pending { reason, attempts } => {
                assert_eq!(attempts, PROJECTION_ATTEMPTS);
                assert!(reason.contains("token"), "reason was: {reason}");
            }
            other => panic!("expected Pending, got {other:?}"),
        }
        // The fact source keeps the new name — no rollback.
        assert_eq!(stored_name(&work_dir).as_deref(), Some("Nova"));
    }

    /// Node rejects the token → still `Pending`, retried, reason names the
    /// status and leaks nothing.
    #[tokio::test]
    async fn rename_rejected_by_node_is_pending_after_retries() {
        let (base, hits) = fake_node(403).await;
        let work_dir = temp_work_dir("pending-403");
        let secret = "super-secret-node-token";
        let notifier = IdentityNotifier::new(base, Some(secret.into()), "inst-1");

        let outcome = apply_name(&work_dir, &no_core(), Some(&notifier), "Nova")
            .await
            .unwrap();

        match outcome {
            NameApplyOutcome::Pending { reason, attempts } => {
                assert_eq!(attempts, PROJECTION_ATTEMPTS);
                assert!(reason.contains("403"), "reason was: {reason}");
                assert!(!reason.contains(secret), "token leaked into reason");
            }
            other => panic!("expected Pending, got {other:?}"),
        }
        assert_eq!(hits.load(Ordering::SeqCst), PROJECTION_ATTEMPTS as usize);
        assert_eq!(stored_name(&work_dir).as_deref(), Some("Nova"));
    }
}
