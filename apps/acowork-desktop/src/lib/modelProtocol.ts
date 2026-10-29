//! ADR-103 D11 — the model-level protocol display rules.
//!
//! Three surfaces in the UI show the protocol state of a (provider, model)
//! pair, and they must agree:
//!
//!   1. the `Ⓡ Responses` chip in the provider model picker,
//!   2. the `R` letter badge in the chat model dropdown,
//!   3. the yellow "selected but not enabled" badge + the protocol dropdown
//!      inside the per-model editor.
//!
//! The Gateway owns every decision (D1 gate, D10 verdict); this module only
//! *projects* the four fields `GET /api/models` returns into a badge kind. It
//! MUST NOT re-derive a protocol from offline data or infer the gate itself —
//! `protocol_active` already is the gate's answer.
//!
//! Visual tiers (D11):
//!
//! | badge | meaning | form |
//! |---|---|---|
//! | `verified` | selected, probed, enabled | solid `R` |
//! | `degraded` | enabled, but only after a field trim | solid `R` + yellow dot |
//! | `declared` | offline data claims support, never probed, not enabled | gray `R` |
//! | `blocked` | selected but NOT enabled — carries the reason | yellow badge, no `R` |
//! | `none` | nothing to say (default protocol, no declaration) | nothing |

/** Probe verdict states (mirrors `ProbeState` in `acowork-core/protocol.rs`). */
export type ProtocolProbeState =
  | "verified"
  | "degraded"
  | "failed"
  | "stale"
  | "unverified"
  | "unknown";

/** Persisted probe verdict (mirrors `ProtocolProbe`). */
export interface ProtocolProbe {
  state: ProtocolProbeState;
  /** Tiers that passed, e.g. `["L1","L2","L3"]`. */
  tier_passed?: string[];
  /** Failure reason shown by the yellow badge (D11). */
  reason?: string;
  /** Base URL the probe actually hit, when it differs from the default. */
  observed_base_url?: string;
  /** RFC 3339 probe timestamp (cache TTL / staleness display). */
  probed_at?: string;
}

/** The four ADR-103 protocol fields carried by `ModelInfo`. */
export interface ModelProtocolFields {
  /** Offline declaration: `"responses"` / `"completions"` / absent. */
  shape?: string;
  /** The raw hand-picked value (`null` = follow the data). */
  protocol_selected?: string;
  /** The protocol actually in effect (D1 verdict). */
  protocol_active?: string;
  /** The D10 verdict, when the model was probed. */
  protocol_probe?: ProtocolProbe;
}

/** Why a selection is not enabled — an i18n key suffix, not prose. */
export type ProtocolBlockedReason =
  | "unverified"
  | "failed"
  | "stale"
  | "gate_off"
  | "unknown";

export type ProtocolBadge =
  | { kind: "verified" }
  | { kind: "degraded"; tiers?: string[] }
  | { kind: "declared" }
  | { kind: "blocked"; reason: ProtocolBlockedReason; detail?: string }
  | { kind: "none" };

/** The protocol value the dropdown / chip uses for "Responses". */
export const RESPONSES_PROTOCOL = "responses";

/**
 * The protocol dropdown options (D11 red line: **always** offered).
 *
 * The option list is deliberately data-independent. Hiding `Responses` for
 * models the offline data did not tag would hide the only path to it for
 * DeepSeek / MiniMax / custom gateways, which no dataset covers (§1.1).
 */
export const PROTOCOL_OPTIONS = ["follow", "openai", "responses"] as const;
export type ProtocolOption = (typeof PROTOCOL_OPTIONS)[number];

/** Map a stored selection to the dropdown option value. */
export function protocolOptionOf(selected?: string): ProtocolOption {
  if (!selected) return "follow";
  if (selected === RESPONSES_PROTOCOL) return "responses";
  if (selected === "openai") return "openai";
  // Any other explicit protocol (anthropic / google / ollama / an unknown wire
  // value) is not offered by this dropdown; show it as "follow" rather than
  // silently pretending the selection is one of our three options.
  return "follow";
}

/** Map a dropdown option back to the wire value (`null` = clear). */
export function protocolOfOption(option: ProtocolOption): string | null {
  return option === "follow" ? null : option;
}

/** Whether a badge kind renders the `R` glyph (the yellow one does not, D11). */
export function showsResponsesMark(badge: ProtocolBadge): boolean {
  return (
    badge.kind === "verified" ||
    badge.kind === "degraded" ||
    badge.kind === "declared"
  );
}

/**
 * Project the Gateway's four fields into one badge.
 *
 * `protocol_active` is the D1 verdict and therefore the only trustworthy
 * statement about what the model is doing right now: when it says `responses`
 * the selection passed the gate, and when it says anything else while the
 * selection is `responses`, the selection is blocked — the probe verdict (or
 * the global switch) is why.
 */
export function deriveProtocolBadge(m: ModelProtocolFields): ProtocolBadge {
  const selected = m.protocol_selected === RESPONSES_PROTOCOL;
  const active = m.protocol_active === RESPONSES_PROTOCOL;
  const declared = m.shape === RESPONSES_PROTOCOL;
  const probe = m.protocol_probe;

  if (selected) {
    if (active) {
      // Gate passed. `degraded` means it passed only after a D8 field trim —
      // usable, but the user deserves to know (S7 renders the stripped list).
      return probe?.state === "degraded"
        ? { kind: "degraded", tiers: probe.tier_passed }
        : { kind: "verified" };
    }
    // Selected but not enabled. The reason comes from the verdict; a passing
    // verdict with a non-`responses` active protocol means the platform switch
    // is off (§4.4 — the fallback must stay visible).
    switch (probe?.state) {
      case "failed":
        return {
          kind: "blocked",
          reason: "failed",
          detail: probe.reason,
        };
      case "stale":
        return { kind: "blocked", reason: "stale" };
      case "verified":
      case "degraded":
        return { kind: "blocked", reason: "gate_off" };
      case "unknown":
        return { kind: "blocked", reason: "unknown" };
      default:
        return { kind: "blocked", reason: "unverified" };
    }
  }

  // No selection: the only thing left to say is what the offline data claims.
  return declared ? { kind: "declared" } : { kind: "none" };
}

/** One row's worth of inputs for the pre-save request estimate. */
export interface ProbeEstimateInput {
  /** Protocol currently stored for this model (`undefined` = none). */
  previousSelected?: string;
  /** Protocol the user is about to save. */
  nextSelected?: string;
  /** The model was just added to the provider. */
  isNew?: boolean;
  /** A change that invalidates earlier verdicts for the whole provider. */
  configChanged?: boolean;
}

export interface ProbeEstimate {
  /** Models that will be probed (each costs one L1 request). */
  models: number;
  /** Total real requests, L1 per model + the deep tier's extra round trips. */
  requests: number;
}

/**
 * How many real requests a save will send (D12 / D11 save-button hint).
 *
 * D12 probes only the `responses` rows this write changed, and runs L3 once per
 * (provider, protocol) — the first such row pays for it. Measured against the
 * probe runner, the deep tier = L1 + L3 + one `function_call_output` replay
 * (3 requests), every other row = L1 only.
 */
export function estimateProbeRequests(rows: ProbeEstimateInput[]): ProbeEstimate {
  let models = 0;
  for (const row of rows) {
    if (row.nextSelected !== RESPONSES_PROTOCOL) continue;
    const changed =
      row.isNew ||
      row.configChanged === true ||
      row.previousSelected !== row.nextSelected;
    if (changed) models += 1;
  }
  return { models, requests: models + (models > 0 ? 2 : 0) };
}