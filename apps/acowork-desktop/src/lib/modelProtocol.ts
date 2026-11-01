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
  /**
   * The protocol this verdict is about (ADR-103 D14). Absent on verdicts
   * written before D14, which could only ever be about `Responses` — the
   * read side mirrors `ProtocolProbe::protocol()` and treats `None` as
   * `Responses` rather than guessing.
   */
  protocol?: string;
  /** Failure reason shown by the yellow badge (D11). */
  reason?: string;
  /** Base URL the probe actually hit, when it differs from the default. */
  observed_base_url?: string;
  /**
   * Request fields the probe had to drop (D8 trim). Non-empty is what makes a
   * `degraded` verdict concrete: the user is told *which* parameter is being
   * dropped instead of a vague "degraded".
   */
  stripped_fields?: string[];
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
  | "unknown"
  /** D14: the stored verdict is about a *different* protocol, so it says
   *  nothing about this selection — the override stays gated off. */
  | "otherProtocol";

export type ProtocolBadge =
  | { kind: "verified" }
  /** Works, but only without `stripped` (ADR-103 D8/D10.3). */
  | { kind: "degraded"; tiers?: string[]; stripped: string[] }
  | { kind: "declared" }
  | { kind: "blocked"; reason: ProtocolBlockedReason; detail?: string }
  | { kind: "none" };

/** The protocol value the dropdown / chip uses for "Responses". */
export const RESPONSES_PROTOCOL = "responses";

/**
 * The protocols the Gateway probe can speak (ADR-103 D14.3).
 *
 * These are exactly the `ProbeProtocol` variants on the Rust side: a wire
 * shape exists to be asked about. `google` / `ollama` overrides take no part
 * in the gate (the platform serves them through the compatible adapters), so
 * they are never probed and never billed.
 */
export const PROBEABLE_PROTOCOLS = ["responses", "openai", "anthropic"] as const;

export function isProbeableProtocol(protocol?: string): boolean {
  return (
    protocol !== undefined &&
    (PROBEABLE_PROTOCOLS as readonly string[]).includes(protocol)
  );
}

/**
 * The provider-level protocols that speak exactly one wire shape, so a
 * per-model protocol choice is meaningless for them (ADR-103 D14).
 *
 * `anthropic` is the case that matters: the Anthropic Messages API is the
 * only shape an Anthropic provider serves, so offering a dropdown whose only
 * other options are `Chat Completions` and `Responses` would be offering a
 * choice the endpoint cannot honour — the selection would then be rejected
 * by the D14 probe (or, worse, drive the Runtime to a route that does not
 * exist).
 *
 * `google` and `ollama` are deliberately **not** listed: the platform
 * serves them through other adapters, and the per-model override is still a
 * meaningful thing to express for them.
 */
export const SINGLE_PROTOCOL_PROVIDERS = ["anthropic"] as const;

/**
 * The protocol dropdown options (D11 red line: **always** offered).
 *
 * The option list is deliberately data-independent. Hiding `Responses` for
 * models the offline data did not tag would hide the only path to it for
 * DeepSeek / MiniMax / custom gateways, which no dataset covers (§1.1).
 *
 * ADR-103 D14: `anthropic` is offered too — the probe now speaks the Messages
 * shape, so selecting it on a non-Anthropic provider is a verifiable claim,
 * not an untestable wish.
 */
export const PROTOCOL_OPTIONS = [
  "follow",
  "openai",
  "responses",
  "anthropic",
] as const;
export type ProtocolOption = (typeof PROTOCOL_OPTIONS)[number];

/**
 * Should the per-model protocol dropdown be shown at all?
 *
 * ADR-103 D14: a provider that speaks exactly one wire protocol has nothing
 * to choose between per model, so the dropdown is hidden for it and the
 * protocol is simply the provider's.
 *
 * `providerProtocol` is the Gateway-projected provider-level protocol
 * (`undefined` = the provider is not configured yet, e.g. while browsing the
 * catalog) — in that case the dropdown stays visible, because there is no
 * fact to base a decision on.
 *
 * `hasStoredOverride` is the escape hatch (§4.2 item 6): a row that already
 * carries a model-level selection differing from the provider-level protocol
 * must stay editable — hiding the dropdown there would leave the incident
 * configuration visible nowhere and clearable by nothing.
 */
export function showsProtocolDropdown(
  providerProtocol?: string,
  hasStoredOverride?: boolean,
): boolean {
  if (hasStoredOverride) return true;
  if (!providerProtocol) return true;
  return !(SINGLE_PROTOCOL_PROVIDERS as readonly string[]).includes(
    providerProtocol,
  );
}

/** Map a stored selection to the dropdown option value. */
export function protocolOptionOf(selected?: string): ProtocolOption {
  if (!selected) return "follow";
  if (selected === RESPONSES_PROTOCOL) return "responses";
  if (selected === "openai") return "openai";
  if (selected === "anthropic") return "anthropic";
  // Any other explicit protocol (google / ollama / an unknown wire value) is
  // not offered by this dropdown; show it as "follow" rather than silently
  // pretending the selection is one of our four options.
  return "follow";
}

/** Map a dropdown option back to the wire value (`null` = clear). */
export function protocolOfOption(option: ProtocolOption): string | null {
  return option === "follow" ? null : option;
}

/**
 * The trimmed field names of a degraded badge as one display string.
 *
 * Empty for every other badge kind, so a caller can branch on the string
 * instead of re-deriving the badge shape (`degraded` without fields — e.g. an
 * older stored verdict — renders the generic wording).
 */
export function strippedFieldsText(badge: ProtocolBadge): string {
  return badge.kind === "degraded" ? badge.stripped.join(" / ") : "";
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
 * `protocol_active` is the gate's verdict and therefore the only trustworthy
 * statement about what the model is doing right now. ADR-103 D14 widened the
 * gate from `Responses`-only to *any* override of the provider-level
 * protocol — so "selected but not enabled" is now a statement about whichever
 * protocol the row carries, and the yellow badge MUST appear for every
 * selection the gate rejected, not only for `Responses` (a fallback the user
 * cannot see is the silent failure v0.3 removed).
 */
export function deriveProtocolBadge(m: ModelProtocolFields): ProtocolBadge {
  const selection = m.protocol_selected;
  const active = m.protocol_active;
  const declared = m.shape === RESPONSES_PROTOCOL;
  const probe = m.protocol_probe;

  if (selection && selection !== active) {
    // Selected but NOT enabled — the fallback is visible for every protocol.
    // A verdict is about one route: if it speaks another protocol it says
    // nothing about this selection, and "re-verify this one" is the fix.
    // A verdict without a `protocol` field predates D14 and can only have
    // been about `Responses` — mirror the Rust read side.
    if (probe && (probe.protocol ?? RESPONSES_PROTOCOL) !== selection) {
      return { kind: "blocked", reason: "otherProtocol", detail: probe.reason };
    }
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
      case "unknown":
        // A passing verdict that still did not become active means the
        // projection and the verdict disagree, which is not something the UI
        // can explain — fall back to "no usable verdict" rather than
        // inventing a cause.
        return { kind: "blocked", reason: "unknown" };
      default:
        return { kind: "blocked", reason: "unverified" };
    }
  }

  if (selection === RESPONSES_PROTOCOL) {
    // Gate passed. `degraded` means it passed only after a D8 field trim —
    // usable, but the user deserves to know (S7 renders the stripped list).
    return probe?.state === "degraded"
      ? {
          kind: "degraded",
          tiers: probe.tier_passed,
          stripped: probe.stripped_fields ?? [],
        }
      : { kind: "verified" };
  }

  // No selection, or a non-Responses selection that took effect: the only
  // thing left to say is what the offline data claims (the `R` chip speaks
  // `Responses`, so a live Chat-Completions/Anthropic row shows nothing).
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
  /**
   * The provider-level protocol (`undefined` = not configured yet).
   *
   * ADR-103 D14: a row whose effective selection equals the provider-level
   * protocol is the status quo and is never probed, so it must not be
   * counted here either — otherwise the UI would announce real requests the
   * save will not send.
   */
  providerProtocol?: string;
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
 * D12 probes the rows this write changed whose effective selection the gate
 * can reject, and D12.3 (as revised by D14) runs L3 once per
 * **(provider, protocol)** — one deep representative *per protocol*. Measured
 * against the probe runner, the deep tier = L1 + L3 + one
 * `function_call_output` replay (3 requests), every other row = L1 only.
 * So the total is `models + 2 × distinct probeable protocols`.
 *
 * ADR-103 D14 mirrors the Gateway's planning rule exactly:
 * - a row whose selection **equals** the provider-level protocol is the
 *   status quo → not probed;
 * - a `responses` row is probed even when it matches (the D11 badge and the
 *   D10 verdicts depend on it);
 * - any other row that **overrides** the provider-level protocol is probed,
 *   because the D14 gate has to verify the override before it takes effect;
 * - a row whose effective selection has **no probe shape** (`google` /
 *   `ollama`) is never planned, so it is never counted (D14.3).
 */
export function estimateProbeRequests(rows: ProbeEstimateInput[]): ProbeEstimate {
  let models = 0;
  const protocols = new Set<string>();
  for (const row of rows) {
    const selected = row.nextSelected;
    if (!selected || !isProbeableProtocol(selected)) continue;
    const overridesProviderLevel =
      row.providerProtocol !== undefined && selected !== row.providerProtocol;
    const isResponses = selected === RESPONSES_PROTOCOL;
    // The status quo needs no verdict; `responses` always does; an override
    // does too (that is the D14 gate).
    if (!overridesProviderLevel && !isResponses) continue;
    const changed =
      row.isNew ||
      row.configChanged === true ||
      row.previousSelected !== row.nextSelected;
    if (changed) {
      models += 1;
      // One deep representative per protocol (D12.3/D14) — the extra two
      // requests are charged once per distinct protocol, not once per model.
      protocols.add(selected);
    }
  }
  return { models, requests: models + protocols.size * 2 };
}