//! Provider compatibility cache — learns (provider, model) quirks from
//! *confirmed, repeated* evidence and forgets single-luck accidents.
//!
//! ## Background
//!
//! Many OpenAI-compatible providers (volcano-engine, deepseek, …) reject
//! requests with `400 Bad Request` when the client sends fields the provider's
//! schema does not understand (e.g. `thinking.type`, `stream_options`,
//! `temperature`, …). Without a memory layer the runtime ran a multi-step
//! fallback chain on **every** turn.
//!
//! ## v1 defect (2026-09-05 production incident) and why v2 exists
//!
//! v1 persisted a `StripProfile` after a **single** lucky fallback success and
//! never re-probed. A transient / content-dependent 400 (DeepSeek rejecting a
//! history whose assistant tool round lacks `reasoning_content`) was only
//! "fixed" by the last-resort fallback that stripped `tools` — the 400 then
//! disappeared because the schema validator no longer checked the assistant
//! message. That one accident became a **durable** rule
//! (`deepseek::deepseek-v4-flash → strip_tools: true`), silently disabling
//! function calling for every later request until the model degraded to
//! writing tool calls as text. Restarting did not help because the file was on
//! disk.
//!
//! ## v2 rules
//!
//! 1. **Confirmation gate** — a single fallback success only creates an
//!    in-memory *candidate*. The entry becomes durable only after
//!    [`COMPAT_CONFIRM_REQUIRED`] *distinct* requests succeed inside
//!    [`COMPAT_CONFIRM_WINDOW_SECS`] with the *same* error class and the same
//!    strip action. A plain (unmodified) success inside the window resets the
//!    candidates — accidents must not harden.
//! 2. **TTL lease** — a durable entry is a lease, not a fact. [`CompatCache::get`]
//!    treats entries older than [`COMPAT_PROFILE_TTL_SECS`] as a miss so the
//!    provider is re-probed.
//! 3. **Invalidation cooldown** — after [`CompatCache::invalidate`] the key is
//!    re-recorded only after [`COMPAT_INVALIDATE_COOLDOWN_SECS`], so the very
//!    request batch that broke the cache cannot rewrite it.
//! 4. **Error classification, not blanket stripping** — 400/422 bodies are
//!    classified ([`ErrorClass`]). Only `request_schema` / `tools_schema`
//!    errors may feed learning. `content_integrity` errors (history /
//!    reasoning payload) are **not** learnable: the caller must surface them
//!    instead of masking them via `strip_tools`.
//! 5. **Legacy v1 files are ignored** — a bare-map v1 file is treated as empty
//!    and re-confirmed from scratch, so a poisoned `strip_tools` entry
//!    evaporates on the next persist.
//!
//! ## Persistence
//!
//! Path: `{work_dir}/config/provider_compat.json`
//!
//! v3 file shape: `{ "version": 3, "entries": { key: PersistedEntry } }`.
//! Only *durable* (confirmed) entries are persisted. Atomic write via
//! `*.tmp` → rename; throttled to at most one write per
//! [`COMPAT_PERSIST_MIN_INTERVAL_SECS`], except promotions and invalidations
//! which persist immediately.
//!
//! ## Key format
//!
//! `"{namespace}::{shape}::{model_id}"` (see [`cache_key`]) — keeps different
//! vendors with the same underlying model (e.g. `glm-5.2` on volcano-engine
//! vs. a self-hosted OpenAI-compatible wrapper) isolated, **and** keeps two
//! wire shapes of the same vendor+model apart (ADR-103 D7.3: `/chat/completions`
//! and `/responses` only look alike, their parameter surfaces are not).

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Version of the on-disk file format. v1 (bare map) is intentionally not
/// readable — see module docs rule 5. v2 keys carried no wire shape, so v3
/// (shape-qualified keys, ADR-103 D7.3) discards them instead of keeping rows
/// that can never be read again.
const CACHE_FILE_VERSION: u8 = 3;

/// Wire shape of the Chat-Completions family (`/chat/completions`).
pub const SHAPE_COMPLETIONS: &str = "completions";

/// Wire shape of the Responses family (`/responses`, ADR-103).
pub const SHAPE_RESPONSES: &str = "responses";

/// Build a compat-cache key (ADR-103 D7.3).
///
/// The shape is part of the namespace on purpose: a vendor that serves both
/// `/chat/completions` and `/responses` may resolve to the same host, and a
/// learned profile from one shape MUST NOT leak into the other.
///
/// `namespace` is the provider id when known, otherwise the base-url host.
pub fn cache_key(namespace: &str, shape: &str, model: &str) -> String {
    format!("{namespace}::{shape}::{model}")
}

/// Distinct successful requests required before a candidate becomes durable.
pub const COMPAT_CONFIRM_REQUIRED: u32 = 3;
/// Durable profiles older than this (since promotion) are re-probed (TTL lease).
pub const COMPAT_PROFILE_TTL_SECS: u64 = 24 * 60 * 60;
/// After `invalidate`, suppress re-recording for this key for this long.
pub const COMPAT_INVALIDATE_COOLDOWN_SECS: u64 = 5 * 60;
/// Window in which the required confirmations must land (else candidate resets).
pub const COMPAT_CONFIRM_WINDOW_SECS: u64 = 24 * 60 * 60;
/// Throttle non-critical persists to one per interval.
pub const COMPAT_PERSIST_MIN_INTERVAL_SECS: u64 = 60;
/// Maximum in-memory candidates kept per key (oldest evicted).
const MAX_CANDIDATES_PER_KEY: usize = 8;

/// The learning outcome of one "trim something and retry" action, generic
/// over the wire shape.
///
/// ADR-103 D8 requires the Responses trim to follow the *same evidence
/// discipline* as the completions fallback ladder while explicitly NOT sharing
/// [`StripProfile`] (a completions-shaped struct). So the policy below —
/// confirmation gate, TTL lease, plain-success reset, invalidation cooldown —
/// is generic over the action, and each wire shape supplies its own profile:
///
/// | shape | profile | file |
/// |---|---|---|
/// | `/chat/completions` | [`StripProfile`] | `provider_compat.json` |
/// | `/responses` | `ResponsesTrim` (`responses_compat.rs`) | `provider_compat_responses.json` |
///
/// The files are separate on purpose: keys are already shape-qualified
/// ([`cache_key`]), and keeping the payloads apart means an ADR-103 rollback
/// cannot make the completions cache unreadable.
pub trait TrimAction:
    Clone + PartialEq + Send + Sync + 'static + Serialize + for<'de> Deserialize<'de>
{
    /// Whether two profiles prescribe the *same* trim (diagnostic bookkeeping
    /// such as timestamps or fallback generations must be ignored).
    fn same_action(&self, other: &Self) -> bool;

    /// One-line description for the promotion / invalidation logs. Free-form:
    /// the policy never interprets it.
    fn describe(&self) -> String;

    /// Refresh "last used" bookkeeping after a fast-path hit. Diagnostics only —
    /// it must not change the trim action. Default: nothing to record.
    fn touch_last_success(&mut self, _now_unix_ts: u64) {}
}

/// Describes which fields to strip from a request before sending it.
///
/// One entry per `(provider_id, model_id)` pair. A `true` flag means
/// "do not send this field at all".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StripProfile {
    /// Drop the `stream_options` object (some providers reject `include_usage`).
    pub strip_stream_options: bool,
    /// Drop the `reasoning_effort` field (o-series / MiniMax only — incompatible with non-OpenAI).
    pub strip_reasoning_effort: bool,
    /// Drop the `thinking.type` field (GLM 5.x custom field).
    pub strip_thinking: bool,
    /// Drop the `temperature` field (some providers require it absent rather than `null`).
    pub strip_temperature: bool,
    /// Drop the entire `tools` array (last resort — disables function calling).
    pub strip_tools: bool,
    /// Cap `max_tokens` to this value if originally larger (`None` = no cap).
    pub max_tokens_cap: Option<u32>,
    /// Which fallback generation succeeded (1=FB1, 2=FB2, …). Diagnostic only.
    pub fallback_generation: u32,
    /// Unix timestamp (seconds) of the last successful use of this profile.
    /// Refreshed on every fast-path hit; persisted for diagnostics.
    pub last_success_unix_ts: u64,
}

impl StripProfile {
    /// Empty profile — nothing stripped. Useful as a default.
    pub fn empty() -> Self {
        Self {
            strip_stream_options: false,
            strip_reasoning_effort: false,
            strip_thinking: false,
            strip_temperature: false,
            strip_tools: false,
            max_tokens_cap: None,
            fallback_generation: 0,
            last_success_unix_ts: now_unix_ts(),
        }
    }

    /// Compare actionable strip fields, ignoring diagnostic bookkeeping
    /// (`fallback_generation`, `last_success_unix_ts`).
    pub fn same_strip_action(&self, other: &StripProfile) -> bool {
        self.strip_stream_options == other.strip_stream_options
            && self.strip_reasoning_effort == other.strip_reasoning_effort
            && self.strip_thinking == other.strip_thinking
            && self.strip_temperature == other.strip_temperature
            && self.strip_tools == other.strip_tools
            && self.max_tokens_cap == other.max_tokens_cap
    }
}

impl TrimAction for StripProfile {
    fn same_action(&self, other: &Self) -> bool {
        self.same_strip_action(other)
    }

    fn touch_last_success(&mut self, now_unix_ts: u64) {
        self.last_success_unix_ts = now_unix_ts;
    }

    fn describe(&self) -> String {
        let mut parts = Vec::new();
        for (flag, name) in [
            (self.strip_stream_options, "stream_options"),
            (self.strip_reasoning_effort, "reasoning_effort"),
            (self.strip_thinking, "thinking"),
            (self.strip_temperature, "temperature"),
            (self.strip_tools, "tools"),
        ] {
            if flag {
                parts.push(name);
            }
        }
        if self.max_tokens_cap.is_some() {
            parts.push("max_tokens_cap");
        }
        format!(
            "strip[{}], max_tokens<={:?}, fb{}",
            parts.join(","),
            self.max_tokens_cap,
            self.fallback_generation
        )
    }
}

/// Coarse classification of a provider rejection body.
///
/// The cache must *only* learn from schema-shaped rejections. Content-shaped
/// rejections (bad history / reasoning payload) must surface to the caller so
/// the real defect is visible — the 2026-09-05 incident was caused by
/// learning from a content error through the strip-tools loophole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// The request body carries fields this provider schema does not accept
    /// (e.g. `stream_options`, `thinking`, `temperature`). Learnable.
    RequestSchema,
    /// The `tools` payload itself is rejected (function schema / tool choice).
    /// Learnable — may lead to `strip_tools`.
    ToolsSchema,
    /// The *conversation content* is rejected (e.g. DeepSeek: "reasoning_content
    /// must be passed back"). NOT learnable — never mask with `strip_tools`.
    ContentIntegrity,
    /// Could not classify. NOT learnable — surface the error.
    Unknown,
}

impl ErrorClass {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorClass::RequestSchema => "request_schema",
            ErrorClass::ToolsSchema => "tools_schema",
            ErrorClass::ContentIntegrity => "content_integrity",
            ErrorClass::Unknown => "unknown",
        }
    }

    /// Whether repeated evidence of this class may become a durable profile.
    pub fn is_learnable(self) -> bool {
        matches!(self, ErrorClass::RequestSchema | ErrorClass::ToolsSchema)
    }

    /// Whether a `strip_tools` fallback may be attempted for this class.
    pub fn may_strip_tools(self) -> bool {
        matches!(self, ErrorClass::ToolsSchema)
    }

    /// Heuristic keyword classification. Conservative on purpose: anything we
    /// cannot confidently attribute to the request schema is `Unknown`, and the
    /// caller must surface it rather than degrade silently.
    pub fn classify(body: &str) -> Self {
        let lower = body.to_ascii_lowercase();
        // DeepSeek thinking-mode integrity: "The reasoning_content in the
        // thinking mode must be passed back to the API". Also seen:
        // "the message history ... must be passed back". High-precision only.
        const CONTENT_MARKERS: &[&str] = &[
            "reasoning_content",
            "reasoning content",
            "must be passed back",
            "passed back",
        ];
        if CONTENT_MARKERS.iter().any(|m| lower.contains(m)) {
            return ErrorClass::ContentIntegrity;
        }
        const TOOLS_MARKERS: &[&str] = &[
            "parallel_tool_calls",
            "tool_choice",
            "tool choice",
            "tool_calls",
            "tool calls",
            "function",
            "tools",
        ];
        if TOOLS_MARKERS.iter().any(|m| lower.contains(m)) {
            return ErrorClass::ToolsSchema;
        }
        const SCHEMA_MARKERS: &[&str] = &[
            "unknown parameter",
            "unknown argument",
            "unknown field",
            "unexpected field",
            "extra field",
            "unrecognized",
            "not supported",
            "unsupported",
            "invalid parameter",
            // Volcengine Ark puts the code straight into `error.code` with no
            // space ("InvalidParameter"), so the marker above never matched and
            // its 400s were surfaced without ever trying the fallback ladder.
            "invalidparameter",
            "invalid request",
            "invalid argument",
        ];
        if SCHEMA_MARKERS.iter().any(|m| lower.contains(m)) {
            return ErrorClass::RequestSchema;
        }
        ErrorClass::Unknown
    }
}

/// A durable, on-disk entry. `promoted_at_unix_ts` anchors the TTL lease.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedEntry<P> {
    pub profile: P,
    /// The error class that the profile learned from.
    pub class: String,
    /// Unix ts when the entry was promoted to durable (TTL anchor).
    pub promoted_at_unix_ts: u64,
}

impl<P> PersistedEntry<P> {
    fn error_class(&self) -> ErrorClass {
        match self.class.as_str() {
            "request_schema" => ErrorClass::RequestSchema,
            "tools_schema" => ErrorClass::ToolsSchema,
            "content_integrity" => ErrorClass::ContentIntegrity,
            _ => ErrorClass::Unknown,
        }
    }

    fn expired_at(&self, now: u64) -> bool {
        now.saturating_sub(self.promoted_at_unix_ts) >= COMPAT_PROFILE_TTL_SECS
    }
}

/// In-memory evidence that a trim action helped. Never persisted.
#[derive(Debug, Clone)]
struct Candidate<P> {
    profile: P,
    class: ErrorClass,
    first_success_unix_ts: u64,
    last_success_unix_ts: u64,
    confirmations: u32,
}

#[derive(Debug, Clone)]
struct EntryState<P> {
    durable: Option<PersistedEntry<P>>,
    candidates: Vec<Candidate<P>>,
    /// Unix ts of the last `invalidate`; suppresses re-recording during cooldown.
    invalidated_at_unix_ts: Option<u64>,
}

impl<P> Default for EntryState<P> {
    fn default() -> Self {
        Self {
            durable: None,
            candidates: Vec::new(),
            invalidated_at_unix_ts: None,
        }
    }
}

/// On-disk v2 format.
#[derive(Debug, Serialize, Deserialize)]
struct DiskFileV2<P> {
    version: u8,
    entries: HashMap<String, PersistedEntry<P>>,
}

/// In-memory + on-disk cache of durable trim profiles keyed by
/// `"namespace::shape::model_id"`, plus non-persisted confirmation candidates.
///
/// Generic over the profile ([`TrimAction`]) so one implementation serves every
/// wire shape; a bare `CompatCache` is the completions shape.
/// Thread-safe via `parking_lot::RwLock`. Hot-path reads (`get`) never block
/// long — one `HashMap` lookup plus a TTL check.
pub struct CompatCache<P = StripProfile> {
    inner: RwLock<HashMap<String, EntryState<P>>>,
    persist_path: PathBuf,
    /// Serializes concurrent atomic writes so spawned persist tasks never race
    /// on the same `*.tmp` file.
    persist_lock: Arc<tokio::sync::Mutex<()>>,
    /// Throttle: last persist wall-clock, seconds since unix epoch.
    last_persist_unix_ts: AtomicU64,
}

impl CompatCache<StripProfile> {
    /// Load the completions-shaped cache.
    ///
    /// Kept as an inherent method on the specialised type so the historical
    /// call sites (`CompatCache::load(path)`) stay inference-free, while other
    /// shapes go through [`CompatCache::open`] with an explicit profile.
    pub fn load(persist_path: PathBuf) -> Arc<Self> {
        Self::open(persist_path)
    }
}

impl<P: TrimAction> CompatCache<P> {
    /// Load a cache of `P` from disk.
    ///
    /// * Missing file → empty cache.
    /// * v2 file → durable entries loaded (TTL enforced lazily in `get`).
    /// * v1 bare-map file / malformed file → logged, treated as empty, and the
    ///   legacy file is rewritten as an empty v2 file so a poisoned
    ///   `strip_tools` entry cannot survive a restart.
    pub fn open(persist_path: PathBuf) -> Arc<Self> {
        let cache = Arc::new(Self {
            inner: RwLock::new(HashMap::new()),
            persist_path,
            persist_lock: Arc::new(tokio::sync::Mutex::new(())),
            last_persist_unix_ts: AtomicU64::new(0),
        });

        let raw = match fs::read_to_string(&cache.persist_path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                debug!(
                    path = %cache.persist_path.display(),
                    "provider_compat.json not found — starting with empty cache",
                );
                return cache;
            }
            Err(e) => {
                warn!(
                    path = %cache.persist_path.display(),
                    error = %e,
                    "failed to read provider_compat.json — starting with empty cache",
                );
                return cache;
            }
        };

        match serde_json::from_str::<DiskFileV2<P>>(&raw) {
            Ok(file) if file.version == CACHE_FILE_VERSION => {
                let mut entries: HashMap<String, EntryState<P>> = HashMap::new();
                for (key, persisted) in file.entries {
                    entries.insert(
                        key,
                        EntryState {
                            durable: Some(persisted),
                            ..Default::default()
                        },
                    );
                }
                debug!(
                    path = %cache.persist_path.display(),
                    entries = entries.len(),
                    "CompatCache loaded (v2)",
                );
                *cache.inner.write() = entries;
            }
            Ok(file) => {
                warn!(
                    path = %cache.persist_path.display(),
                    version = file.version,
                    expected = CACHE_FILE_VERSION,
                    "provider_compat.json has an unsupported version — discarding and re-learning from scratch",
                );
                cache.rewrite_empty_legacy();
            }
            Err(e) => {
                warn!(
                    path = %cache.persist_path.display(),
                    error = %e,
                    "provider_compat.json is malformed or a legacy v1 bare map — discarding (v1 single-sample entries are not trusted) and re-learning from scratch",
                );
                cache.rewrite_empty_legacy();
            }
        }
        cache
    }

    /// Best-effort replacement of a legacy/malformed file with an empty v2 file
    /// so the next boot does not re-read the poisoned content. Synchronous
    /// because `load` runs at startup.
    fn rewrite_empty_legacy(&self) {
        let path = self.persist_path.clone();
        let result = (|| -> io::Result<()> {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let tmp = path.with_extension("json.tmp");
            let serialized = serde_json::to_string_pretty(&DiskFileV2::<P> {
                version: CACHE_FILE_VERSION,
                entries: HashMap::new(),
            })
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            fs::write(&tmp, serialized.as_bytes())?;
            fs::rename(&tmp, &path)?;
            Ok(())
        })();
        if let Err(e) = result {
            warn!(
                path = %path.display(),
                error = %e,
                "failed to replace legacy provider_compat.json with an empty v2 file",
            );
        }
    }

    /// Hot-path read of a durable profile.
    ///
    /// Returns `None` when there is no durable profile or when the durable
    /// entry has outlived its TTL lease (re-probe required). Cheap: one
    /// `RwLock::read` + `HashMap::get`, no await.
    pub fn get(&self, key: &str) -> Option<P> {
        let now = now_unix_ts();
        let guard = self.inner.read();
        let state = guard.get(key)?;
        let durable = state.durable.as_ref()?;
        if durable.expired_at(now) {
            return None;
        }
        Some(durable.profile.clone())
    }

    /// Record that a request **without any profile applied** succeeded.
    ///
    /// Contradicting evidence: it resets in-memory candidates (an accident
    /// window is over) and, when the durable entry is already past its TTL
    /// (i.e. we were re-probing), drops that expired durable profile.
    pub fn record_plain_success(&self, key: &str) {
        let now = now_unix_ts();
        let mut changed = false;
        {
            let mut guard = self.inner.write();
            let state = guard.entry(key.to_string()).or_default();
            if let Some(durable) = state.durable.as_ref()
                && durable.expired_at(now)
            {
                info!(
                    key,
                    "CompatCache: re-probe plain success replaced expired profile (TTL lease renewed from scratch)",
                );
                state.durable = None;
                changed = true;
            }
            if !state.candidates.is_empty() {
                state.candidates.clear();
                changed = true;
            }
            if state.invalidated_at_unix_ts.is_some() {
                state.invalidated_at_unix_ts = None;
                changed = true;
            }
        }
        if changed {
            debug!(key, "CompatCache: plain success reset pending candidates");
            self.persist_throttled();
        }
    }

    /// Refresh the TTL-adjacent diagnostics (`last_success_unix_ts`) of a
    /// durable profile after a fast-path hit. Never promotes or demotes.
    pub fn touch(&self, key: &str) {
        let now = now_unix_ts();
        {
            let mut guard = self.inner.write();
            let state = guard.entry(key.to_string()).or_default();
            if let Some(durable) = state.durable.as_mut() {
                durable.profile.touch_last_success(now);
            }
        }
        self.persist_throttled();
    }

    /// Record a successful *fallback* (a stripped variant) under the error
    /// class that started the chain.
    ///
    /// * Non-learnable classes are ignored — the caller should not even have
    ///   reached a fallback for them.
    /// * While the key is in invalidation cooldown, evidence is dropped (the
    ///   batch that broke the cache must not rewrite it).
    /// * A durable profile matching this (class, action) is refreshed.
    /// * Otherwise this is one more confirmation for a matching candidate;
    ///   `COMPAT_CONFIRM_REQUIRED` confirmations within the window promote the
    ///   candidate to durable (immediate persist).
    pub fn record_fallback_success(&self, key: &str, profile: P, class: ErrorClass) {
        if !class.is_learnable() {
            debug!(
                key,
                class = class.as_str(),
                "CompatCache: refusing to record non-learnable fallback success",
            );
            return;
        }

        let now = now_unix_ts();
        let mut promote: Option<PersistedEntry<P>> = None;
        {
            let mut guard = self.inner.write();
            let state = guard.entry(key.to_string()).or_default();

            // Invalidation cooldown — do not re-record.
            if let Some(inv_at) = state.invalidated_at_unix_ts
                && now.saturating_sub(inv_at) < COMPAT_INVALIDATE_COOLDOWN_SECS
            {
                debug!(
                    key,
                    class = class.as_str(),
                    "CompatCache: fallback success ignored — key in invalidation cooldown",
                );
                return;
            }
            state.invalidated_at_unix_ts = None;

            // Fast path already covered by the same (class, action): refresh.
            if let Some(durable) = state.durable.as_mut()
                && durable.error_class() == class
                && durable.profile.same_action(&profile)
            {
                let lease_expired = durable.expired_at(now);
                durable.profile = profile.clone();
                if lease_expired {
                    // We are in a TTL re-probe: the plain request just failed
                    // with the same class and the same strip action succeeded
                    // again — direct evidence the lease is still needed.
                    durable.promoted_at_unix_ts = now;
                    info!(
                        key,
                        class = class.as_str(),
                        "CompatCache: TTL re-probe re-confirmed durable profile (lease renewed)",
                    );
                }
            } else {
                // Candidate path: find same (class, action) evidence.
                let idx = state
                    .candidates
                    .iter()
                    .position(|c| c.class == class && c.profile.same_action(&profile));
                if let Some(i) = idx {
                    let cand = &mut state.candidates[i];
                    cand.confirmations += 1;
                    cand.last_success_unix_ts = now;
                    if cand.confirmations >= COMPAT_CONFIRM_REQUIRED
                        && now.saturating_sub(cand.first_success_unix_ts)
                            <= COMPAT_CONFIRM_WINDOW_SECS
                    {
                        let confirmed = cand.clone();
                        promote = Some(PersistedEntry {
                            profile: confirmed.profile.clone(),
                            class: class.as_str().to_string(),
                            promoted_at_unix_ts: now,
                        });
                        // Promote replaces any previous durable (provider
                        // behavior changed) and clears all candidate evidence.
                        state.durable = promote.clone();
                        state.candidates.clear();
                    }
                } else {
                    if state.candidates.len() >= MAX_CANDIDATES_PER_KEY {
                        state.candidates.remove(0);
                    }
                    state.candidates.push(Candidate {
                        profile,
                        class,
                        first_success_unix_ts: now,
                        last_success_unix_ts: now,
                        confirmations: 1,
                    });
                }
            }
        }

        match promote.as_ref() {
            Some(p) => {
                info!(
                    key,
                    class = class.as_str(),
                    action = p.profile.describe(),
                    confirmations = COMPAT_CONFIRM_REQUIRED,
                    "CompatCache: candidate promoted to durable profile (confirmed across distinct requests)",
                );
                // Immediate persist: promotion is the durable moment.
                self.persist_immediate();
            }
            None => {
                debug!(
                    key,
                    class = class.as_str(),
                    "CompatCache: fallback success recorded as candidate evidence or durable refresh",
                );
                self.persist_throttled();
            }
        }
    }

    /// Drop a durable entry and reset all evidence. Next requests re-probe;
    /// re-recording is suppressed for [`COMPAT_INVALIDATE_COOLDOWN_SECS`].
    pub fn invalidate(&self, key: &str, reason: &str) {
        let now = now_unix_ts();
        {
            let mut guard = self.inner.write();
            let state = guard.entry(key.to_string()).or_default();
            let had_durable = state.durable.is_some();
            if had_durable {
                warn!(
                    key,
                    reason,
                    "CompatCache: invalidated durable profile — provider behavior changed or content error surfaced",
                );
            }
            state.durable = None;
            state.candidates.clear();
            state.invalidated_at_unix_ts = Some(now);
        }
        self.persist_immediate();
    }

    /// Snapshot durable entries for persistence.
    fn durable_snapshot(&self) -> HashMap<String, PersistedEntry<P>> {
        let guard = self.inner.read();
        guard
            .iter()
            .filter_map(|(k, state)| state.durable.as_ref().map(|d| (k.clone(), d.clone())))
            .collect()
    }

    /// Async persist if the throttle interval has elapsed.
    fn persist_throttled(&self) {
        let now = now_unix_ts();
        let last = self.last_persist_unix_ts.load(Ordering::Relaxed);
        if now.saturating_sub(last) < COMPAT_PERSIST_MIN_INTERVAL_SECS {
            return;
        }
        self.last_persist_unix_ts.store(now, Ordering::Relaxed);
        self.spawn_persist();
    }

    /// Async persist immediately (promotions / invalidations).
    fn persist_immediate(&self) {
        self.last_persist_unix_ts
            .store(now_unix_ts(), Ordering::Relaxed);
        self.spawn_persist();
    }

    fn spawn_persist(&self) {
        let path = self.persist_path.clone();
        let entries = self.durable_snapshot();
        let lock = self.persist_lock.clone();
        tokio::spawn(async move {
            let _permit = lock.lock().await;
            if let Err(e) = write_atomic(&path, &entries).await {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to persist provider_compat.json",
                );
            }
        });
    }
}

/// Atomic write helper — write to `*.tmp`, then `rename` over target.
async fn write_atomic<P: TrimAction>(
    path: &Path,
    durable: &HashMap<String, PersistedEntry<P>>,
) -> io::Result<()> {
    let file = DiskFileV2::<P> {
        version: CACHE_FILE_VERSION,
        entries: durable.clone(),
    };
    let serialized = serde_json::to_string_pretty(&file)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let tmp_path = path.with_extension("json.tmp");

    // Off-thread blocking I/O to avoid stalling the runtime.
    let path_owned = path.to_path_buf();
    let tmp_owned = tmp_path.clone();
    tokio::task::spawn_blocking(move || -> io::Result<()> {
        if let Some(parent) = path_owned.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&tmp_owned, serialized.as_bytes())?;
        fs::rename(&tmp_owned, &path_owned)?;
        Ok(())
    })
    .await
    .map_err(|e| io::Error::other(format!("join error: {e}")))?
}

fn now_unix_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::time::Duration;

    #[test]
    fn cache_keys_are_shape_qualified() {
        // ADR-103 D7.3: same namespace + model, different wire shape, MUST
        // yield different keys — otherwise a completed profile learned on
        // `/chat/completions` would be trusted by the `/responses` chain.
        assert_eq!(
            cache_key("openai", SHAPE_COMPLETIONS, "gpt-5"),
            "openai::completions::gpt-5"
        );
        assert_eq!(
            cache_key("openai", SHAPE_RESPONSES, "gpt-5"),
            "openai::responses::gpt-5"
        );
        assert_ne!(
            cache_key("openai", SHAPE_COMPLETIONS, "gpt-5"),
            cache_key("openai", SHAPE_RESPONSES, "gpt-5")
        );
    }

    fn tmp_path(name: &str) -> PathBuf {
        let mut p = env::temp_dir();
        p.push(format!(
            "acowork_compat_v2_{}_{}_{}.json",
            name,
            std::process::id(),
            now_unix_ts()
        ));
        let _ = fs::remove_file(&p);
        p
    }

    /// Actionable profile — same_strip_action ignores `fallback_generation`.
    fn schema_profile(fb: u32) -> StripProfile {
        StripProfile {
            strip_stream_options: true,
            strip_reasoning_effort: false,
            strip_thinking: true,
            strip_temperature: false,
            strip_tools: false,
            max_tokens_cap: Some(8192),
            fallback_generation: fb,
            last_success_unix_ts: 0,
        }
    }

    fn tools_profile(fb: u32) -> StripProfile {
        let mut p = schema_profile(fb);
        p.strip_tools = true;
        p
    }

    fn seed_durable(
        cache: &CompatCache,
        key: &str,
        profile: StripProfile,
        class: &str,
        promoted_at: u64,
    ) {
        cache.inner.write().insert(
            key.to_string(),
            EntryState {
                durable: Some(PersistedEntry {
                    profile,
                    class: class.to_string(),
                    promoted_at_unix_ts: promoted_at,
                }),
                ..Default::default()
            },
        );
    }

    fn candidate_count(cache: &CompatCache, key: &str) -> usize {
        cache
            .inner
            .read()
            .get(key)
            .map(|s| s.candidates.len())
            .unwrap_or(0)
    }

    #[test]
    fn classify_error_bodies() {
        assert_eq!(
            ErrorClass::classify(
                "The reasoning_content in the thinking mode must be passed back to the API"
            ),
            ErrorClass::ContentIntegrity,
        );
        assert_eq!(
            ErrorClass::classify("request error: the message history must be passed back in order"),
            ErrorClass::ContentIntegrity,
        );
        assert_eq!(
            ErrorClass::classify("unknown parameter 'thinking' for model glm-5.2"),
            ErrorClass::RequestSchema,
        );
        assert_eq!(
            ErrorClass::classify("invalid request: extra field `temperature` not permitted"),
            ErrorClass::RequestSchema,
        );
        // Volcengine Ark error body (verbatim, trimmed): the code has no space,
        // and `param` is empty so the message alone can't say which field.
        assert_eq!(
            ErrorClass::classify(
                r#"{"error":{"code":"InvalidParameter","message":"A parameter specified in the request is not valid Request id: 021789901337591a6cdd8247c48d753298cd4bcf5eec90e452d6d","param":"","type":"BadRequest"}}"#
            ),
            ErrorClass::RequestSchema,
        );
        assert_eq!(
            ErrorClass::classify("tools[0].function.parameters: unknown field at 2:7"),
            ErrorClass::ToolsSchema,
        );
        assert_eq!(
            ErrorClass::classify("parallel_tool_calls is not supported by this deployment"),
            ErrorClass::ToolsSchema,
        );
        assert_eq!(
            ErrorClass::classify("upstream timeout after 60s"),
            ErrorClass::Unknown
        );
        // Non-learnable classes are never learned from, and only ToolsSchema
        // may ever strip tools.
        assert!(!ErrorClass::ContentIntegrity.is_learnable());
        assert!(!ErrorClass::Unknown.is_learnable());
        assert!(ErrorClass::RequestSchema.is_learnable());
        assert!(!ErrorClass::RequestSchema.may_strip_tools());
        assert!(ErrorClass::ToolsSchema.is_learnable());
        assert!(ErrorClass::ToolsSchema.may_strip_tools());
    }

    #[tokio::test]
    async fn single_success_is_candidate_not_durable() {
        let path = tmp_path("single");
        let cache = CompatCache::load(path.clone());
        let key = "deepseek::deepseek-v4-flash";
        // One lucky strip_tools success (the 2026-09-05 shape) must NOT persist.
        cache.record_fallback_success(key, tools_profile(4), ErrorClass::ToolsSchema);
        assert!(
            cache.get(key).is_none(),
            "single success must not be durable"
        );
        assert_eq!(candidate_count(&cache, key), 1);
        let _ = fs::remove_file(&path);
    }

    #[tokio::test]
    async fn confirmations_promote_to_durable() {
        let path = tmp_path("promote");
        let cache = CompatCache::load(path.clone());
        let key = "volcano-engine-coding::glm-5.2";
        for _ in 0..COMPAT_CONFIRM_REQUIRED - 1 {
            cache.record_fallback_success(key, schema_profile(2), ErrorClass::RequestSchema);
            assert!(cache.get(key).is_none(), "not promoted before threshold");
        }
        cache.record_fallback_success(key, schema_profile(2), ErrorClass::RequestSchema);
        let got = cache
            .get(key)
            .expect("promoted after COMPAT_CONFIRM_REQUIRED successes");
        assert!(got.strip_stream_options);
        assert!(got.strip_thinking);
        assert!(!got.strip_tools);
        assert_eq!(candidate_count(&cache, key), 0);
        let _ = fs::remove_file(&path);
    }

    #[tokio::test]
    async fn promotion_persists_v2_file_and_reloads() {
        let path = tmp_path("persist");
        let cache = CompatCache::load(path.clone());
        let key = "volcano-engine-coding::glm-5.2";
        for _ in 0..COMPAT_CONFIRM_REQUIRED {
            cache.record_fallback_success(key, schema_profile(3), ErrorClass::RequestSchema);
        }
        // Give the spawned persist task a moment to flush.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let reloaded = CompatCache::load(path.clone());
        let got: StripProfile = reloaded.get(key).expect("durable entry reloaded from disk");
        assert!(got.strip_thinking);
        assert_eq!(got.fallback_generation, 3);
        // File must be v2, holding only the durable entry.
        let raw = fs::read_to_string(&path).unwrap();
        let file: DiskFileV2<StripProfile> = serde_json::from_str(&raw).expect("file is v2 shape");
        assert_eq!(file.version, 3);
        assert_eq!(file.entries.len(), 1);
        assert_eq!(file.entries[key].class, "request_schema");
        let _ = fs::remove_file(&path);
    }

    #[tokio::test]
    async fn ttl_expiry_makes_get_a_miss_then_rerecord_renews_lease() {
        let path = tmp_path("ttl");
        let cache = CompatCache::load(path.clone());
        let key = "p::m";
        let now = now_unix_ts();
        // Durable profile promoted long ago -> expired lease.
        seed_durable(
            &cache,
            key,
            schema_profile(2),
            "request_schema",
            now - COMPAT_PROFILE_TTL_SECS - 1,
        );
        assert!(cache.get(key).is_none(), "expired lease must re-probe");
        // Re-probe outcome: same class + same strip action succeeds again.
        cache.record_fallback_success(key, schema_profile(2), ErrorClass::RequestSchema);
        assert!(
            cache.get(key).is_some(),
            "re-probe re-confirmed and renewed the lease"
        );
        let _ = fs::remove_file(&path);
    }

    #[tokio::test]
    async fn plain_success_resets_candidates_and_retires_expired_durable() {
        let path = tmp_path("plain");
        let cache = CompatCache::load(path.clone());
        let key = "p::m";
        cache.record_fallback_success(key, schema_profile(2), ErrorClass::RequestSchema);
        assert_eq!(candidate_count(&cache, key), 1);
        // A plain (unmodified) success means the "problem" was transient.
        cache.record_plain_success(key);
        assert_eq!(candidate_count(&cache, key), 0);

        let now = now_unix_ts();
        seed_durable(
            &cache,
            key,
            schema_profile(2),
            "request_schema",
            now - COMPAT_PROFILE_TTL_SECS - 1,
        );
        cache.record_plain_success(key);
        assert!(
            cache
                .inner
                .read()
                .get(key)
                .and_then(|s| s.durable.as_ref())
                .is_none(),
            "plain success after TTL expiry retires the old durable profile",
        );
        let _ = fs::remove_file(&path);
    }

    #[tokio::test]
    async fn invalidate_enters_cooldown_and_suppresses_rerecord() {
        let path = tmp_path("cooldown");
        let cache = CompatCache::load(path.clone());
        let key = "p::m";
        cache.record_fallback_success(key, tools_profile(4), ErrorClass::ToolsSchema);
        cache.invalidate(key, "test-invalidate");
        assert!(cache.get(key).is_none());
        // The very batch that broke the cache must not rewrite it.
        cache.record_fallback_success(key, tools_profile(4), ErrorClass::ToolsSchema);
        assert!(cache.get(key).is_none());
        assert_eq!(candidate_count(&cache, key), 0);
        {
            let state = cache.inner.read().get(key).cloned().unwrap();
            assert!(state.invalidated_at_unix_ts.is_some());
        }
        // Once the cooldown elapses, learning may resume.
        {
            let mut guard = cache.inner.write();
            let state = guard.get_mut(key).unwrap();
            state.invalidated_at_unix_ts =
                Some(now_unix_ts() - COMPAT_INVALIDATE_COOLDOWN_SECS - 1);
        }
        cache.record_fallback_success(key, tools_profile(4), ErrorClass::ToolsSchema);
        assert_eq!(candidate_count(&cache, key), 1);
        assert!(cache.get(key).is_none(), "still needs confirmations");
        let _ = fs::remove_file(&path);
    }

    #[tokio::test]
    async fn non_learnable_class_never_records() {
        let path = tmp_path("content");
        let cache = CompatCache::load(path.clone());
        let key = "deepseek::deepseek-v4-flash";
        cache.record_fallback_success(key, tools_profile(4), ErrorClass::ContentIntegrity);
        cache.record_fallback_success(key, schema_profile(2), ErrorClass::Unknown);
        assert!(cache.inner.read().get(key).is_none());
        let _ = fs::remove_file(&path);
    }

    #[tokio::test]
    async fn different_action_needs_its_own_confirmations() {
        let path = tmp_path("diff");
        let cache = CompatCache::load(path.clone());
        let key = "p::m";
        // request_schema success (strip stream_options+thinking)
        for _ in 0..COMPAT_CONFIRM_REQUIRED {
            cache.record_fallback_success(key, schema_profile(2), ErrorClass::RequestSchema);
        }
        assert!(cache.get(key).is_some());
        // A single strip_tools success for tools_schema must not be durable.
        cache.record_fallback_success(key, tools_profile(4), ErrorClass::ToolsSchema);
        assert_eq!(candidate_count(&cache, key), 1);
        let got = cache.get(key).unwrap();
        assert!(
            !got.strip_tools,
            "new evidence must not override durable instantly"
        );
        let _ = fs::remove_file(&path);
    }

    #[tokio::test]
    async fn legacy_v1_file_is_discarded_and_rewritten_empty() {
        let path = tmp_path("legacy");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        // v1 shape = bare map keyed by "provider::model". The poisoned entry
        // from the 2026-09-05 incident must not survive.
        let v1 = r#"{
            "deepseek::deepseek-v4-flash": {
                "strip_stream_options": false,
                "strip_reasoning_effort": false,
                "strip_thinking": false,
                "strip_temperature": false,
                "strip_tools": true,
                "max_tokens_cap": null,
                "fallback_generation": 4,
                "last_success_unix_ts": 1788592521
            }
        }"#;
        fs::write(&path, v1).unwrap();
        let cache = CompatCache::load(path.clone());
        assert!(cache.get("deepseek::deepseek-v4-flash").is_none());
        // Legacy file was proactively replaced by an empty v2 file.
        let raw = fs::read_to_string(&path).unwrap();
        let file: DiskFileV2<StripProfile> = serde_json::from_str(&raw).expect("rewritten file is v2");
        assert_eq!(file.version, 3);
        assert!(file.entries.is_empty());
        let _ = fs::remove_file(&path);
    }

    #[tokio::test]
    async fn malformed_file_yields_empty_cache() {
        let path = tmp_path("malformed");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, "{ not valid json").unwrap();
        let cache = CompatCache::load(path.clone());
        assert!(cache.get("any::key").is_none());
        let raw = fs::read_to_string(&path).unwrap();
        let file: DiskFileV2<StripProfile> = serde_json::from_str(&raw).expect("rewritten file is v2");
        assert!(file.entries.is_empty());
        let _ = fs::remove_file(&path);
    }
}
