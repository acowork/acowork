//! Centralized timeout configuration for cross-crate timeouts (ADR-023).
//!
//! Three layers, organized by mutability:
//!   1. [`Timeouts`] — user-configurable, TOML-serializable. Both
//!      `RuntimeConfig` and `GatewayConfig` flatten this so that existing
//!      TOML field names (`provider_request_timeout_ms`, etc.) are
//!      preserved byte-for-byte.
//!   2. [`constants`] — cross-crate hardcoded `Duration` constants that
//!      operators do not tune, but which were previously duplicated as
//!      loose `const FOO_SECS: u64` across five crates.
//!   3. [`validate`] — startup safety-bound checks (fail-fast).
//!
//! Sub-process-internal timeouts (embed supervisor heartbeat, LSP reaper,
//! tick intervals, test asserts) intentionally do NOT live here — they
//! belong to the sub-process behavior contract and are documented in
//! ADR-023 appendix A.4.
//!
//! Field types are `u64` milliseconds / seconds rather than `Duration`
//! to keep the serialized TOML representation identical to the pre-ADR-023
//! layout. `Duration` accessors are provided for ergonomic consumption.

use serde::{Deserialize, Serialize};
use std::time::Duration;

// ── Layer 1: user-configurable subset ───────────────────────────────────

/// Aggregated, user-configurable timeout configuration.
///
/// Consumed by `RuntimeConfig` and `GatewayConfig` via `#[serde(flatten)]`,
/// so every field name below must match the historical TOML key exactly.
///
/// Use the `*_duration()` accessors instead of reading the raw `u64`
/// fields where a [`Duration`] is needed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Timeouts {
    // ── LLM provider HTTP layer ──
    /// Whole LLM HTTP request timeout in milliseconds (thinking + generation).
    #[serde(default = "default_provider_request_timeout_ms")]
    pub provider_request_timeout_ms: u64,
    /// LLM provider TCP connect timeout in milliseconds.
    #[serde(default = "default_provider_connect_timeout_ms")]
    pub provider_connect_timeout_ms: u64,
    /// LLM provider per-chunk stream silence detection in milliseconds.
    #[serde(default = "default_provider_stream_read_timeout_ms")]
    pub provider_stream_read_timeout_ms: u64,

    // ── Built-in tool layer ──
    /// Default HTTP timeout for built-in tools in milliseconds
    /// (web_fetch, embedding clients, etc.).
    #[serde(default = "default_tool_http_timeout_ms")]
    pub tool_http_timeout_ms: u64,

    // ── Agent loop layer ──
    /// Overall timeout for one iteration in milliseconds (an iteration may
    /// contain multiple LLM calls plus tool executions).
    ///
    /// ADR-023 bug fix: the Gateway-side default was previously `30_000`
    /// while the Runtime-side default was `900_000`. Both now default to
    /// `900_000` (15 min) through this single source.
    #[serde(default = "default_iteration_timeout_ms")]
    pub iteration_timeout_ms: u64,
    /// Single tool execution timeout in milliseconds.
    #[serde(default = "default_tool_timeout_ms")]
    pub tool_timeout_ms: u64,

    /// ADR-083: end-to-end deadline for ONE context compaction, in
    /// milliseconds. Covers the whole distill target fallback chain —
    /// the three tiers share this single budget rather than each getting
    /// its own (`provider_request_timeout_ms` is per-HTTP-call and is a
    /// different layer). On expiry the compaction fails fast instead of
    /// letting the UI spin for up to `timeout × retries × tiers`.
    #[serde(default = "default_compaction_deadline_ms")]
    pub compaction_deadline_ms: u64,

    // ── Session lifecycle layer ──
    /// Session in-memory eviction threshold in seconds (Runtime side).
    #[serde(default = "default_session_idle_timeout_secs")]
    pub session_idle_timeout_secs: u64,
    /// Gateway-side idle agent kill threshold in seconds.
    ///
    /// Kept distinct from `session_idle_timeout_secs` because operator
    /// intent (kill a whole agent process) may differ from internal
    /// session eviction (evict a session from memory but keep JSONL).
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,

    // ── Retry layer ──
    /// Bounded retry policy shared across `reliable.rs` and future callers.
    #[serde(default)]
    pub retry: RetryConfig,
}

impl Timeouts {
    /// Whole LLM HTTP request timeout as a [`Duration`].
    pub fn provider_request(&self) -> Duration {
        Duration::from_millis(self.provider_request_timeout_ms)
    }
    /// LLM provider TCP connect timeout as a [`Duration`].
    pub fn provider_connect(&self) -> Duration {
        Duration::from_millis(self.provider_connect_timeout_ms)
    }
    /// LLM provider per-chunk stream read timeout as a [`Duration`].
    pub fn provider_stream_read(&self) -> Duration {
        Duration::from_millis(self.provider_stream_read_timeout_ms)
    }
    /// Built-in tool HTTP timeout as a [`Duration`].
    pub fn tool_http(&self) -> Duration {
        Duration::from_millis(self.tool_http_timeout_ms)
    }
    /// Iteration timeout as a [`Duration`].
    pub fn iteration(&self) -> Duration {
        Duration::from_millis(self.iteration_timeout_ms)
    }
    /// Single tool execution timeout as a [`Duration`].
    pub fn tool_exec(&self) -> Duration {
        Duration::from_millis(self.tool_timeout_ms)
    }
    /// ADR-083: end-to-end context compaction deadline as a [`Duration`].
    pub fn compaction(&self) -> Duration {
        Duration::from_millis(self.compaction_deadline_ms)
    }
    /// Session in-memory eviction threshold as a [`Duration`].
    pub fn session_idle(&self) -> Duration {
        Duration::from_secs(self.session_idle_timeout_secs)
    }
    /// Gateway idle agent kill threshold as a [`Duration`].
    pub fn idle_agent(&self) -> Duration {
        Duration::from_secs(self.idle_timeout_secs)
    }
}

/// Bounded retry policy (serializable source of truth).
///
/// The runtime's `ReliableProvider` builds its internal retry state from
/// this config. Kept separate from `reliable.rs::RetryConfig` (which is a
/// non-serializable runtime construct) to avoid dragging serde into the
/// provider layer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetryConfig {
    /// Maximum number of attempts (including the first). Must be >= 1.
    #[serde(default = "default_retry_max_attempts")]
    pub max_attempts: u32,
    /// Exponential backoff base delay in milliseconds.
    #[serde(default = "default_retry_backoff_base_ms")]
    pub backoff_base_ms: u64,
    /// Backoff cap (max wait per attempt) in milliseconds.
    #[serde(default = "default_retry_backoff_cap_ms")]
    pub backoff_cap_ms: u64,
    /// Whether a server-suggested `Retry-After` header takes precedence
    /// over the computed backoff.
    #[serde(default = "default_retry_honor_retry_after")]
    pub honor_retry_after: bool,
}

impl RetryConfig {
    /// Backoff base delay as a [`Duration`].
    pub fn backoff_base(&self) -> Duration {
        Duration::from_millis(self.backoff_base_ms)
    }
    /// Backoff cap as a [`Duration`].
    pub fn backoff_cap(&self) -> Duration {
        Duration::from_millis(self.backoff_cap_ms)
    }
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: default_retry_max_attempts(),
            backoff_base_ms: default_retry_backoff_base_ms(),
            backoff_cap_ms: default_retry_backoff_cap_ms(),
            honor_retry_after: default_retry_honor_retry_after(),
        }
    }
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            provider_request_timeout_ms: default_provider_request_timeout_ms(),
            provider_connect_timeout_ms: default_provider_connect_timeout_ms(),
            provider_stream_read_timeout_ms: default_provider_stream_read_timeout_ms(),
            tool_http_timeout_ms: default_tool_http_timeout_ms(),
            iteration_timeout_ms: default_iteration_timeout_ms(),
            tool_timeout_ms: default_tool_timeout_ms(),
            compaction_deadline_ms: default_compaction_deadline_ms(),
            session_idle_timeout_secs: default_session_idle_timeout_secs(),
            idle_timeout_secs: default_idle_timeout_secs(),
            retry: RetryConfig::default(),
        }
    }
}

// Default factories. Centralized so a single edit here updates both the
// runtime and gateway defaults.
fn default_provider_request_timeout_ms() -> u64 {
    600_000 // 10 min: LLM streaming can be long (thinking + generation)
}
fn default_provider_connect_timeout_ms() -> u64 {
    10_000 // 10 sec
}
fn default_provider_stream_read_timeout_ms() -> u64 {
    45_000 // 45 sec per-chunk interval
}
fn default_tool_http_timeout_ms() -> u64 {
    30_000 // 30 sec
}
fn default_iteration_timeout_ms() -> u64 {
    900_000 // 15 min (ADR-023: unified across runtime + gateway)
}
fn default_tool_timeout_ms() -> u64 {
    600_000 // 10 min
}
fn default_compaction_deadline_ms() -> u64 {
    300_000 // ADR-083: 5 min — bounded end-to-end compaction (was unbounded)
}
fn default_session_idle_timeout_secs() -> u64 {
    300 // 5 min
}
fn default_idle_timeout_secs() -> u64 {
    1800 // 30 min
}
fn default_retry_max_attempts() -> u32 {
    3
}
fn default_retry_backoff_base_ms() -> u64 {
    1_000
}
fn default_retry_backoff_cap_ms() -> u64 {
    10_000
}
fn default_retry_honor_retry_after() -> bool {
    true
}

// ── Layer 2: cross-crate hardcoded constants ────────────────────────────

/// Cross-crate hardcoded timeout constants.
///
/// These were previously duplicated as loose `const FOO_SECS: u64` in
/// individual crates (see ADR-023 appendix A.2). Centralized here as
/// strongly-typed [`Duration`] to remove unit ambiguity. Operators do
/// not tune these; if a value needs to become configurable, promote it
/// to [`Timeouts`] instead.
pub mod constants {
    use std::time::Duration;

    /// Tool approval / user question wait (was `APPROVAL_TIMEOUT_SECS = 300`).
    pub const APPROVAL: Duration = Duration::from_secs(300);

    /// HTTP → Runtime gRPC response wait (was `SESSION_IPC_TIMEOUT_SECS = 10`).
    pub const SESSION_IPC: Duration = Duration::from_secs(10);

    /// Default synchronous Intent routing wait
    /// (was `DEFAULT_INTENT_TIMEOUT_SECS = 30`).
    pub const INTENT_DEFAULT: Duration = Duration::from_secs(30);

    /// How long a **task dispatch** waits for the target Runtime's
    /// acceptance receipt (`POST /api/intents/{id}/accept`) after the
    /// Gateway has published the Intent.
    ///
    /// Publishing only proves the broker took the bytes. Without the
    /// receipt a Runtime that could not route the message — the incident
    /// was `no such column: intent_key` in its session-meta store — looks
    /// exactly like a healthy one at the Gateway, so the dispatch is
    /// stamped delivered and the task is stranded: the sender has no
    /// error to retry on and no fact to reconcile against later.
    ///
    /// Accepting is a local decision (one session-meta read, maybe one
    /// session create), so the success path returns in milliseconds. The
    /// bound is eaten whole by a wedged Runtime — **and by a peer built
    /// before this protocol**, which never answers at all: a mixed-version
    /// fleet pays it on every dispatch (design 27 §5.7). Reaching it
    /// degrades to the pre-receipt behaviour (trust the publish), which is
    /// why it is generous enough not to trip on a loaded machine.
    /// Callers must budget the whole dispatch path, not this leg alone:
    /// a task dispatch may spend up to `INTENT_DEFAULT` waking the target
    /// plus this wait before it answers.
    pub const INTENT_ACCEPT: Duration = Duration::from_secs(5);

    /// MCP receive timeout for init / list (was `RECV_TIMEOUT_SECS = 30`).
    pub const MCP_RECV: Duration = Duration::from_secs(30);

    /// How long an MCP probe may stay **silent** before it is considered
    /// stalled (stdout/stderr of the spawned server reset the clock).
    ///
    /// The bound on a first launch is not "the whole download takes too long"
    /// — a package manager can legitimately spend minutes — it is "the server
    /// produced no output for a long time", which is what a hang looks like.
    /// Same semantic as `INSTALL_IDLE_TIMEOUT` for install commands.
    pub const MCP_PROBE_IDLE: Duration = Duration::from_secs(30);

    /// Silence budget for a probe that runs a lazy package resolver
    /// (`npx` / `uvx` / `bunx` / `pnpx`).
    ///
    /// A *backstop*, not a stall judgement: for these kinds silence proves
    /// nothing. Measured on this platform (2026-11): `uv` prints one line when
    /// a package starts and one when it finishes, 39s apart for a 35MiB
    /// download — a 900MB wheel is minutes of silence; `npx` under a piped
    /// stderr printed nothing at all for a whole 11s first-run install. A real
    /// failure (process died) closes the pipe and fails the handshake on its
    /// own, so the only thing left to catch is a wedged-but-alive process, and
    /// it is worth waiting an hour before declaring that.
    pub const MCP_PROBE_IDLE_LAZY: Duration = Duration::from_secs(60 * 60);

    /// While a probe waits, remind the log this often that silence is not a
    /// verdict — otherwise a quiet large download looks identical to a hang.
    pub const MCP_PROBE_SILENCE_NOTICE: Duration = Duration::from_secs(60);

    /// Handshake bound for a **bounded** connect to a lazily resolved package
    /// (`npx` / `uvx` / `bunx` / `pnpx`) — i.e. the steady path, not the probe.
    ///
    /// The probe must not use this (it passes `None` and watches output
    /// instead): this is the answer to "how long may a launch take before we
    /// call it dead". Measured on this platform (2026-11): `uvx docling-mcp`
    /// answers the handshake in 11.4s warm with 21 tools — a heavy preset.
    /// Cold, the same launch runs the first import of the ML stack and can
    /// exceed 30s silently, which is why the protocol default is too tight
    /// here. 300s is thus ~26x the warm measurement, while still bounded: a
    /// Runtime restart connects without a probe in front of it, and something
    /// has to eventually report an error instead of waiting forever.
    pub const MCP_LAZY_START: Duration = Duration::from_secs(300);

    /// MCP default per-tool call timeout (was `DEFAULT_TOOL_TIMEOUT_SECS = 180`).
    pub const MCP_DEFAULT_TOOL: Duration = Duration::from_secs(180);

    /// MCP tool call ceiling / safety cap (was `MAX_TOOL_TIMEOUT_SECS = 600`).
    pub const MCP_MAX_TOOL: Duration = Duration::from_secs(600);

    /// LSP JSON-RPC request timeout (was `REQUEST_TIMEOUT = 30`).
    pub const LSP_REQUEST: Duration = Duration::from_secs(30);

    /// LSP `initialize` handshake timeout (was `INIT_TIMEOUT = 60`).
    pub const LSP_INIT: Duration = Duration::from_secs(60);

    /// Default HTTP timeout for built-in tools (web_fetch, search backends).
    /// Was `DEFAULT_TOOL_HTTP_TIMEOUT` / `DEFAULT_TOOL_HTTP_TIMEOUT_MS` in
    /// `acowork-runtime/src/config.rs`.  The user-configurable equivalent
    /// is `Timeouts::tool_http_timeout_ms`.
    pub const TOOL_HTTP: Duration = Duration::from_secs(30);

    /// ADR-045: Tool execution progress heartbeat interval. The runtime
    /// emits `ChunkEvent::ToolProgress` every `TOOL_HEARTBEAT` while a
    /// tool is in flight, **and skips the first tick so the first event
    /// lands at 5s, not at 0s** — short commands (5s) complete without
    /// ever sending a heartbeat, preserving the pre-ADR-045 UX.
    ///
    /// Single source of truth for "5s" referenced by:
    /// - runtime heartbeat task (acowork-runtime/src/agent/loop_tools.rs)
    /// - any future operator-facing documentation
    ///
    /// Not user-tunable: this is a UX threshold, not an operational
    /// timeout. Promote to `Timeouts` if it ever becomes tunable.
    pub const TOOL_HEARTBEAT: Duration = Duration::from_secs(5);
}

// ── Layer 3: safety-bound validation ────────────────────────────────────

/// Fail-fast validation of a [`Timeouts`] instance at startup.
///
/// Returns `Err` with a descriptive message identifying which field
/// violates which constraint.
///
/// Intentionally strict on lower bounds — a `0` value means "instant
/// timeout", which is almost never intended and is forbidden. Loose on
/// upper bounds — operators may legitimately raise timeouts for slow
/// networks or large models.
pub fn validate(t: &Timeouts) -> Result<(), String> {
    // All duration-bearing millisecond fields must be at least 1 second.
    let one_sec_ms = 1_000u64;
    let ms_fields: &[(&str, u64)] = &[
        ("provider_request_timeout_ms", t.provider_request_timeout_ms),
        ("provider_connect_timeout_ms", t.provider_connect_timeout_ms),
        (
            "provider_stream_read_timeout_ms",
            t.provider_stream_read_timeout_ms,
        ),
        ("tool_http_timeout_ms", t.tool_http_timeout_ms),
        ("iteration_timeout_ms", t.iteration_timeout_ms),
        ("tool_timeout_ms", t.tool_timeout_ms),
        ("compaction_deadline_ms", t.compaction_deadline_ms),
    ];
    for (name, ms) in ms_fields {
        if *ms < one_sec_ms {
            return Err(format!("Timeouts.{name} must be >= 1000 ms, got {ms} ms"));
        }
    }

    if t.session_idle_timeout_secs == 0 || t.idle_timeout_secs == 0 {
        return Err(
            "Timeouts.session_idle_timeout_secs / idle_timeout_secs must be > 0".to_string(),
        );
    }

    if t.retry.max_attempts == 0 {
        return Err("Timeouts.retry.max_attempts must be >= 1".to_string());
    }
    if t.retry.backoff_base_ms == 0 {
        return Err("Timeouts.retry.backoff_base_ms must be > 0".to_string());
    }
    if t.retry.backoff_cap_ms < t.retry.backoff_base_ms {
        return Err(format!(
            "Timeouts.retry.backoff_cap_ms ({}) must be >= backoff_base_ms ({})",
            t.retry.backoff_cap_ms, t.retry.backoff_base_ms
        ));
    }

    // Cross-field advisory: a single tool outliving its parent iteration
    // is legal but usually a misconfiguration. Warn rather than reject.
    if t.iteration_timeout_ms < t.tool_timeout_ms {
        tracing::warn!(
            iteration_timeout_ms = t.iteration_timeout_ms,
            tool_timeout_ms = t.tool_timeout_ms,
            "Timeouts.iteration_timeout_ms < tool_timeout_ms: a single tool \
             can outlive its parent iteration. Check operator intent."
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        let t = Timeouts::default();
        validate(&t).expect("defaults must validate");
    }

    #[test]
    fn iteration_default_is_15_min() {
        // ADR-023 bug fix: unified default across runtime + gateway.
        assert_eq!(Timeouts::default().iteration_timeout_ms, 900_000);
        assert_eq!(Timeouts::default().iteration(), Duration::from_secs(900));
    }

    #[test]
    fn zero_iteration_is_rejected() {
        let t = Timeouts {
            iteration_timeout_ms: 0,
            ..Default::default()
        };
        assert!(validate(&t).is_err());
    }

    #[test]
    fn sub_second_ms_field_is_rejected() {
        let t = Timeouts {
            provider_request_timeout_ms: 500,
            ..Default::default()
        };
        assert!(validate(&t).is_err());
    }

    #[test]
    fn zero_idle_is_rejected() {
        let t = Timeouts {
            idle_timeout_secs: 0,
            ..Default::default()
        };
        assert!(validate(&t).is_err());
    }

    #[test]
    fn zero_retry_attempts_is_rejected() {
        let t = Timeouts {
            retry: RetryConfig {
                max_attempts: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(validate(&t).is_err());
    }

    #[test]
    fn backoff_cap_below_base_is_rejected() {
        let t = Timeouts {
            retry: RetryConfig {
                backoff_base_ms: 5_000,
                backoff_cap_ms: 1_000,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(validate(&t).is_err());
    }

    #[test]
    fn duration_accessors_match_raw_fields() {
        let t = Timeouts::default();
        assert_eq!(t.provider_request(), Duration::from_millis(600_000));
        assert_eq!(t.provider_connect(), Duration::from_millis(10_000));
        assert_eq!(t.provider_stream_read(), Duration::from_millis(45_000));
        assert_eq!(t.tool_http(), Duration::from_millis(30_000));
        assert_eq!(t.tool_exec(), Duration::from_millis(600_000));
        assert_eq!(t.compaction(), Duration::from_millis(300_000));
        assert_eq!(t.session_idle(), Duration::from_secs(300));
        assert_eq!(t.idle_agent(), Duration::from_secs(1800));
        assert_eq!(t.retry.backoff_base(), Duration::from_millis(1_000));
        assert_eq!(t.retry.backoff_cap(), Duration::from_millis(10_000));
    }

    #[test]
    fn default_compaction_deadline_is_five_minutes() {
        // ADR-083: the whole compaction (all distill tiers) is bounded to 5 min.
        assert_eq!(Timeouts::default().compaction_deadline_ms, 300_000);
        assert_eq!(Timeouts::default().compaction(), Duration::from_secs(300));
    }

    #[test]
    fn sub_second_compaction_deadline_is_rejected() {
        let t = Timeouts {
            compaction_deadline_ms: 500,
            ..Default::default()
        };
        assert!(validate(&t).is_err());
    }

    #[test]
    fn serialize_preserves_legacy_toml_field_names() {
        // Guard against accidental field renames that would break existing
        // user TOML files when Timeouts is flattened into RuntimeConfig /
        // GatewayConfig.
        let toml = toml::to_string(&Timeouts::default()).unwrap();
        for key in [
            "provider_request_timeout_ms",
            "provider_connect_timeout_ms",
            "provider_stream_read_timeout_ms",
            "tool_http_timeout_ms",
            "iteration_timeout_ms",
            "tool_timeout_ms",
            "compaction_deadline_ms",
            "session_idle_timeout_secs",
            "idle_timeout_secs",
        ] {
            assert!(toml.contains(key), "missing TOML key: {key}");
        }
    }

    #[test]
    fn roundtrip_via_toml_is_lossless() {
        let original = Timeouts::default();
        let toml = toml::to_string(&original).unwrap();
        let decoded: Timeouts = toml::from_str(&toml).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn deserialize_from_partial_toml_uses_defaults() {
        // A user TOML that only sets one field must fill the rest from
        // #[serde(default)] factories.
        let toml = "iteration_timeout_ms = 120000\n";
        let t: Timeouts = toml::from_str(toml).unwrap();
        assert_eq!(t.iteration_timeout_ms, 120_000);
        // Untouched fields fall back to defaults.
        assert_eq!(t.provider_request_timeout_ms, 600_000);
        assert_eq!(t.retry.max_attempts, 3);
    }

    #[test]
    fn deserialize_empty_toml_is_all_defaults() {
        let t: Timeouts = toml::from_str("").unwrap();
        assert_eq!(t, Timeouts::default());
    }

    #[test]
    fn constants_have_expected_values() {
        use constants::*;
        assert_eq!(APPROVAL, Duration::from_secs(300));
        assert_eq!(SESSION_IPC, Duration::from_secs(10));
        assert_eq!(INTENT_DEFAULT, Duration::from_secs(30));
        assert_eq!(INTENT_ACCEPT, Duration::from_secs(5));
        assert_eq!(MCP_RECV, Duration::from_secs(30));
        assert_eq!(MCP_PROBE_IDLE, Duration::from_secs(30));
        assert_eq!(MCP_PROBE_IDLE_LAZY, Duration::from_secs(60 * 60));
        assert_eq!(MCP_PROBE_SILENCE_NOTICE, Duration::from_secs(60));
        assert_eq!(MCP_LAZY_START, Duration::from_secs(300));
        assert_eq!(MCP_DEFAULT_TOOL, Duration::from_secs(180));
        assert_eq!(MCP_MAX_TOOL, Duration::from_secs(600));
        assert_eq!(LSP_REQUEST, Duration::from_secs(30));
        assert_eq!(LSP_INIT, Duration::from_secs(60));
        assert_eq!(TOOL_HTTP, Duration::from_secs(30));
    }
}
