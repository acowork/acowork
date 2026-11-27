# ADR-083: Context Compaction Cancellation + Distillation Deadline Guard

## Status

Proposed (pending decision)

## Date

2026-09-23

## Decision Makers

大鱼 (Dayu)

## Supersedes / Related

- [ADR-010](../zh/ADR-010-context-compression-simplification.md) — context compression simplification
- [ADR-011](../zh/ADR-011-compaction-as-distillation.md) — compaction as distillation
- [ADR-023](../zh/ADR-023-centralized-timeout-config.md) — centralized timeout configuration
- [ADR-044](../zh/ADR-044-cancellation-token.md) — Stop signal chain + unified `CancelHandle`
- [ADR-045](../zh/ADR-045-tool-progress-and-cancel.md) — tool progress heartbeat + single-tool cancel (this ADR reuses its immediate-dispatch pattern)
- [ADR-052](../zh/ADR-052-tool-compression-llm-autonomous.md) — LLM-autonomous tool compression
- [ADR-056](../zh/ADR-056-global-default-compact-model.md) — global default compact model + cross-provider 3-tier fallback
- [ADR-061](../zh/ADR-061-context-compression-byte-budget.md) — 5-level context compression strategy

## Context

On 2026-09-23 a production incident occurred. The `N.Ponytail` agent triggered automatic context
compaction (`Triggering LLM compaction ... force=true`, `usage_percent=89.97`) targeting the
cross-provider distillation model `custom-agnes/agnes-3.0-flash`. The Desktop UI then showed the
"compacting…" spinner for **over 14 minutes with no state change**, and the compress button was
`disabled` (greyed out) — the user could neither cancel nor switch models to retry.

Code-verified root causes:

1. **The distillation LLM call has no overall deadline.** `episode_distill::compact_with_llm` does a
   bare `await provider.chat(request)`, and the enclosing `compact_history_if_needed` tier-fallback
   loop has no deadline either.
2. **The only timeout is the reqwest client's `provider_request_timeout_ms` (default 600 s)**, which
   `ReliableProvider` amplifies with `max_attempts = 3` serial retries — worst case ≳ 30 min per
   compaction, and the tier fallbacks share no budget, so the total has no upper bound.
3. **The compaction path is not on the cancellation tree.** `loop_context.rs` never reads
   `CancelHandle`; even if the user stops the whole session, the in-flight distillation call cannot
   be interrupted until the await returns naturally.
4. **The Desktop compress button is disabled while `isCompacting = true`** (only the label changes to
   `compressing`), leaving the user no way to intervene.
5. **The backend only emits the boolean events `CompactingStarted` / `CompactingEnded`** — there is no
   cancelled / timeout / failed distinction, so the frontend cannot give accurate feedback.

## Decision

Add **cancellation** and a **total deadline** as first-class capabilities for context compaction, and
upgrade the frontend from passive waiting to active control.

1. **Deadline guard (mandatory).** All tiers in `compact_history_if_needed` share one remaining
   budget, default **5 minutes**, and each tier's `provider.chat` runs under `tokio::time::timeout`
   so it fails fast with a distinguishable `SummaryError::Timeout` — a single manual or automatic
   compaction takes **at most 5 minutes end to end**. The guard lives in the tier loop that owns the
   shared budget, not inside `compact_with_llm` (a per-call timeout cannot express "this budget is
   shared by N tiers").
2. **Cancellable (mandatory).** Wire compaction into the existing `CancelHandle` (ADR-044). The
   distillation `provider.chat()` await runs under a `tokio::select!` that also awaits
   `cancel_handle.cancelled()`. A user cancel interrupts within ≤ 500 ms, **leaves history intact**,
   returns the session to Idle, and lets the user re-compact with a different model.
3. **Dual-state frontend button (mimic send → stop).** While compacting, the compress button becomes
   a **clickable "Cancel compact"** button (accent color + `Square` icon + "Cancel compact" label).
   Otherwise it reverts to the normal "Compact summary" button. Structurally identical to the
   existing `sending ? handleStop : handleSend` in `ChatPanel.tsx`.
4. **Richer completion semantics.** Add `ChunkEvent::CompactionCancelled { reason }` with
   reason ∈ `{UserCancelled, Timeout, Failed}`; success still uses `CompactingEnded`. The frontend
   picks distinct toast text and resets the button correctly.
5. **Centralized config.** Add `Timeouts::compaction_deadline_ms` (default `300_000`) to the existing
   ADR-023 timeout source of truth, overridable via TOML.

**Non-goals:** no change to the 5-level compression strategy (ADR-061) or the 3-tier distillation
model fallback chain (ADR-056); no second cancellation mechanism (reuse ADR-044 `CancelHandle` +
ADR-045 `mqtt_publish_control` immediate dispatch); no change to `provider_request_timeout_ms`
(10 min) — that is per-HTTP-request semantics, a different layer from this end-to-end deadline.

## Current Pipeline (facts, with file paths)

Manual compaction trigger:

```
[1] Desktop ContextUsageIcon.tsx:513  "Compress summary" button → handleCompressSummary
        ↓ sendCompressAction(agentId, sessionId, 1)
[2] chatStore.ts:1575  invoke("mqtt_publish_control", { command: "compress_action",
                          payload: { session_id, compress_type: 1 } })
[3] Runtime mqtt client → gateway_loop.rs:1055  CompressType(1) → CompressionAction::CompressSummary
[4] session_task.rs:1341  CompressSummary → agent_loop.compact_history_if_needed(&model, true).await
[5] loop_context.rs:676  compact_history_if_needed
        ↓ :689  try_send_chunk(ChunkEvent::CompactingStarted)
        ↓ :729  for target in &targets { ... episode_distill.rs:604 provider.chat(request).await }  ← no timeout, no cancel
[6] loop_context.rs:1147  try_send_chunk(ChunkEvent::CompactingEnded)
```

Event push (backend → frontend):

```
ChunkEvent::CompactingStarted/Ended
    → startup/subsystems.rs:358  publisher.publish_compacting(sid, true/false)
    → MQTT retained topic
    → chatStore.ts:3039  subscribes "compacting_started" / "compacting_ended"
    → sessionState.isCompacting = true/false
    → ContextUsageIcon.tsx:87  isCompacting → button disabled
```

## Design

### Cancellation chain (new parts marked ★)

```mermaid
graph TD
    A[Desktop ContextUsageIcon isCompacting=true] -->|★ handleCancelCompact| B[chatStore.cancelCompressAction]
    B -->|★ compress_action compress_type=CANCEL 3| C[Gateway MQTT broker]
    C -->|★ gateway_loop.rs 3 → CancelCompaction| D[SessionMessage::CompressAction]
    D --> E[session_task inbox]
    E -->|★ immediate dispatch (ADR-045 pattern)| F[compaction_cancel_handle.cancel]
    F --> G[loop_context.rs compact_history_if_needed]
    G -->|★ tokio::select!| H{provider.chat vs cancel.cancelled}
    H -->|LLM returns| I[CompactingEnded success]
    H -->|cancel| J[★ CompactionCancelled UserCancelled]
    H -->|5min timeout| K[★ CompactionCancelled Timeout]
```

### Deadline scope

Deadline `= now + compaction_deadline_ms` (default 300 s) is established once, then the 3 tiers
(global default → provider compact_model → current chat model) **share the remaining budget**. This
is the essential difference from today (each tier gets a fresh 600 s × 3).

**Patch (2026-11-27).** The original implementation gave each tier *all* the remaining budget, so a
single hung tier ate the whole deadline and the tiers behind it never got a call (observed: tier 3's
provider hung for 4 minutes, the chat-model tier was never invoked); on top of that `LowQuality` used
to `break` out of the whole chain. Both contradict [ADR-056 §7](./ADR-056-global-default-compact-model.md)
("never abort distillation with an error; every downgrade is a minimal-loss attempt"). The semantics are
therefore:

- Per-tier `timeout = remaining budget / remaining tiers` (equal split). A hang costs at most its own
  slice, and the end-to-end total stays ≤ `compaction_deadline_ms`.
- A tier timeout is a retryable tier error (`SummaryError::Timeout`) and **steps down to the next
  tier**; only a genuinely exhausted budget emits `CompactionCancelled(Timeout)`.
- Every tier error (including `LowQuality`) discards that tier's *output* only and keeps descending;
  `CompactionFailed` requires **all** tiers to fail. **The end state is decided by *why* the chain
  stopped, not by *which check noticed it***: no summary left and the shared deadline has passed →
  `CompactionCancelled(Timeout)` (whether the exhaustion was caught by the loop-head remaining-budget
  guard or by the last tier's slice expiring); a fast error while budget remains but no tier is left →
  `CompactionFailed(last tier error)`. Otherwise the same budget state would surface as two different
  outcomes in the UI depending on which check happened to fire. Known ceiling: the equal split is strict for a
  tier that is legitimately slower than its share (measured normal calls are 26–28 s against 75 s per
  tier at 4 tiers, so there is ample headroom); a per-provider `timeout_ms` is the upgrade path.

## Data Model

proto (`core/acowork-core/proto/mqtt_payload.proto`):

```proto
enum CompressType {
  COMPRESS_TYPE_UNSPECIFIED  = 0;
  COMPRESS_TYPE_SUMMARY      = 1;
  COMPRESS_TYPE_TOOL_RESULTS = 2;  // reserved (ADR-052 removed the impl)
  COMPRESS_TYPE_CANCEL       = 3;  // ★ new
}

enum CompactionCancelReason {
  COMPACTION_CANCEL_REASON_UNSPECIFIED = 0;
  COMPACTION_CANCEL_REASON_USER        = 1;
  COMPACTION_CANCEL_REASON_TIMEOUT     = 2;
  COMPACTION_CANCEL_REASON_FAILED      = 3;
}
```

> `COMPRESS_TYPE_CANCEL = 3`, not `2`, because `2` is already declared as TOOL_RESULTS.

Timeouts (`core/acowork-core/src/timeout_config.rs`):

```rust
pub struct Timeouts {
    // ... existing fields ...
    pub compaction_deadline_ms: u64,   // ★ default 300_000 (5 min)
}
```

## Module Change List

| Crate | File | Change |
|---|---|---|
| runtime | `src/agent/loop_context.rs` | Establish `deadline`; per-tier remaining budget; `tokio::select!` (LLM vs `cancel_handle.cancelled()`); emit `CompactionCancelled` / `CompactingEnded` by reason |
| runtime | `src/episode_distill.rs` | Add `SummaryError::Timeout` (**retryable** — raised by the tier loop when a tier's share of the deadline runs out, the next tier may be faster); the module only says *why one tier produced no usable summary*, the retry policy stays with the caller |
| runtime | `src/agent/session_core.rs` | Expose a dedicated `compaction_cancel_handle` (separate `CancelHandle` instance from session Stop) |
| runtime | `src/agent/session/session_task.rs` | `CompressionAction::CompactionCancel` → immediate `cancel()` (skip deferred queue, matching `loop_inbound.rs:123`) |
| runtime | `src/startup/gateway_loop.rs` | `compress_type=3` → `CompactionCancel` |
| runtime | `src/startup/subsystems.rs` | `CompactionCancelled { reason }` → `publish_compaction_cancelled` |
| runtime | `src/agent/loop_.rs` | Enum extensions (`CompressionAction::CompactionCancel`, `ChunkEvent::CompactionCancelled`, `CompactionCancelReason`) |
| core | `proto/mqtt_payload.proto` | `COMPRESS_TYPE_CANCEL=3` + `CompactionCancelReason` |
| core | `src/timeout_config.rs` | `compaction_deadline_ms` + default + `validate()` |
| desktop | `components/chat/ContextUsageIcon.tsx` | `canStart` / `canCancel` split; `onClick` ternary; `Square` icon; accent styling |
| desktop | `stores/chatStore.ts` | `cancelCompressAction` (`compress_type: 3`); subscribe `compaction_cancelled` |
| desktop | `i18n/locales/*.json` | `contextUsage.cancelCompact` / `compactionCancelled` / `compactionTimeout` |

## UI

| State | Icon | Label | Color | Clickable |
|---|---|---|---|---|
| Idle | compress icon | Compact summary | neutral | ✅ (`canStart`) |
| Compacting | `Square` (filled) | Cancel compact | `--color-accent` | ✅ (`canCancel`, never disabled) |

```tsx
const canStart  = isIdle && !isCompacting && contextUsage != null;
const canCancel = isCompacting;

<button
  onClick={isCompacting ? handleCancelCompact : handleCompressSummary}
  disabled={!isCompacting && !canStart}
  className={cn(base, isCompacting ? "bg-[var(--color-accent)]/10 text-[var(--color-accent)] ..." : neutral)}
>
  {isCompacting ? <Square size={14} fill="currentColor" /> : <CompressIcon size={14} />}
  {isCompacting ? t("contextUsage.cancelCompact") : t("contextUsage.compressSummary")}
</button>
```

Toasts on completion: success → info; user-cancelled → info; timeout → warn (with "try another
compact model" hint); failed → error.

## Edge Cases

| Scenario | Behavior |
|---|---|
| Cancel during tier 1 | `select!` fires; tier loop breaks; history unchanged |
| Cancel after tier 1, before tier 2 | `is_cancelled()` check at loop top → return immediately |
| Deadline hits mid-tier | `Timeout`; loop terminates (does not advance tier) |
| Cancel vs LLM success race | `select!` semantics — whoever is ready first wins; a completed LLM result counts as success |
| Cancel arrives before compaction starts | Pre-check → skip compaction, do not emit Started |
| Cancel during automatic (90%) compaction | Same path; on cancel the session goes Idle, no FIFO fallback (ADR-061 §11.3 preserved) |
| History integrity after cancel | Guaranteed: history is only mutated via `replace_middle_with_summary` on full success; cancel/timeout takes the `None` branch |

**Core principle:** cancel/timeout must be equivalent to "nothing happened" — history untouched,
session Idle, immediate retry possible.

## Compatibility

- **proto**: adding enum value `3` is backward-compatible. An old Runtime receiving `3` hits the
  `other =>` arm in `gateway_loop.rs` and returns `invalid compress_type` (an existing error path,
  not silent). An old Gateway never sends `3`.
- **Cancellation**: reuses ADR-044's project-local `CancelHandle` (not
  `tokio_util::sync::CancellationToken`). Compaction uses a **dedicated** handle instance so it does
  not collide with session Stop semantics.
- **Config**: `compaction_deadline_ms` is introduced with `#[serde(default)]`; old `runtime.toml`
  files get 300 s.
- **Behavior change**: "compaction can wait 30 min" becomes "at most 5 min". Very long contexts on
  slow models may now time out earlier — a deliberate trade (bounded > unbounded), tunable via config.

## Test Plan

Unit:
- `compact_with_llm`: network error → passthrough; normal → unchanged; each quality-gate rejection → its
  own `SummaryError` variant (the never-returning mock is covered by the tier-chain case below).
- `compact_history_if_needed`: each tier gets an equal share of the remaining budget
  (`compaction_deadline_gives_every_tier_a_shot_and_stays_bounded`: with a never-returning provider
  every tier is still called once, the total stays ≤ deadline + ε, and the result is
  `CompactionFailed(Timeout)`); pre-cancelled → no Started; total ≤ deadline + ε; success →
  `CompactingEnded`; cancel → `CompactionCancelled(UserCancelled)`; budget genuinely exhausted →
  `CompactionCancelled(Timeout)`; `LowQuality` steps down instead of aborting
  (`call_phase_fallback_steps_down_on_low_quality`).
- `session_task`: `CompactionCancel` immediate dispatch (≤ 500 ms interrupt).
- `timeout_config`: default / deserialize / `validate()` bounds.
- `chatStore`: `cancelCompressAction` sends `compress_type: 3`; `compaction_cancelled` resets `isCompacting`.

Integration: manual compact → cancel → re-compact; manual compact → slow provider → 5 min timeout;
auto compact → cancel; cancel vs success race determinism.

E2E (Desktop + Runtime): simulate unreachable distillation provider → timeout toast at 5 min;
suspend provider → cancel → ≤ 500 ms interrupt.

## Rollout Phases

| Phase | Content | Depends | Independently deliverable |
|---|---|---|---|
| M1 | Frontend dual-state button + `cancelCompressAction` + i18n | — | ✅ visual |
| M2 | Backend deadline + shared budget + `SummaryError::Timeout` + `compaction_deadline_ms` | — | ✅ fixes "stuck 30 min" |
| M3 | Cancel chain (proto `CANCEL=3` + `CompactionCancel` + `select!` + immediate dispatch) | M1, M2 | ✅ real interrupt |
| M4 | Richer completion semantics (`CompactionCancelled{reason}` + publisher + toast) | M2, M3 | ✅ polish |
| M5 | Tests + `docs/protocols/zh/mqtt.md` update | M1–M4 | ✅ |

**Minimum to resolve this incident:** M1 + M2. **Full experience:** M3 + M4.

## Open Questions

- Per-agent override of `compaction_deadline_ms` (via `manifest.toml`) — deferred to a future ADR.
- Progress feedback during compaction (stream the distillation summary) — out of scope.
- Interaction between `ReliableProvider` internal retry and the outer deadline — outer deadline wins;
  confirm inner retry converges within remaining budget during M2.
- The 5-minute default is a value judgment (not statistically derived) — revisit with real data.
