/**
 * modelProtocol tests (ADR-103 D11).
 *
 * These lock the S6 regression contract from §7:
 *
 *   1. "shape 缺失仍能选 Responses" — `PROTOCOL_OPTIONS` is data-independent.
 *   2. "failed 时保存可用且不启用" — a failed verdict produces a yellow
 *      `blocked` badge (not `verified`, not `none`) and never suppresses the
 *      option, so the save still goes through.
 *   3. `protocol_active` is the only input that decides "enabled": a verified
 *      verdict with the platform switch off must still render blocked.
 */
import { describe, expect, it } from "vitest";
import {
  PROTOCOL_OPTIONS,
  RESPONSES_PROTOCOL,
  deriveProtocolBadge,
  estimateProbeRequests,
  protocolOfOption,
  protocolOptionOf,
  showsResponsesMark,
} from "./modelProtocol";

describe("protocol options", () => {
  it("always offers Responses regardless of offline data", () => {
    // D11 red line: the dropdown must not be filtered by `shape`, otherwise
    // untagged providers (DeepSeek / MiniMax / custom gateways) can never be
    // switched to Responses.
    expect(PROTOCOL_OPTIONS).toContain(RESPONSES_PROTOCOL);
    expect([...PROTOCOL_OPTIONS]).toEqual(["follow", "openai", "responses"]);
  });

  it("maps the stored selection to and from the dropdown option", () => {
    expect(protocolOptionOf(undefined)).toBe("follow");
    expect(protocolOptionOf("responses")).toBe("responses");
    expect(protocolOptionOf("openai")).toBe("openai");
    // An explicit protocol this dropdown does not offer is shown as "follow"
    // rather than mislabelled as one of our three options.
    expect(protocolOptionOf("anthropic")).toBe("follow");

    expect(protocolOfOption("follow")).toBeNull();
    expect(protocolOfOption("responses")).toBe("responses");
    expect(protocolOfOption("openai")).toBe("openai");
  });
});

describe("deriveProtocolBadge", () => {
  it("renders nothing for the default protocol without a declaration", () => {
    expect(deriveProtocolBadge({})).toEqual({ kind: "none" });
    expect(
      deriveProtocolBadge({ protocol_active: "openai", shape: undefined }),
    ).toEqual({ kind: "none" });
    // `completions` never shows a `C` badge (YAGNI, D11).
    expect(deriveProtocolBadge({ shape: "completions" })).toEqual({ kind: "none" });
  });

  it("renders the gray R for an untested offline declaration", () => {
    const badge = deriveProtocolBadge({ shape: "responses" });
    expect(badge).toEqual({ kind: "declared" });
    expect(showsResponsesMark(badge)).toBe(true);
  });

  it("renders the solid R when the selection passed the gate", () => {
    const badge = deriveProtocolBadge({
      shape: "responses",
      protocol_selected: "responses",
      protocol_active: "responses",
      protocol_probe: { state: "verified", tier_passed: ["L1", "L2", "L3"] },
    });
    expect(badge).toEqual({ kind: "verified" });
    expect(showsResponsesMark(badge)).toBe(true);
  });

  it("marks a degraded pass so the trim is visible", () => {
    const badge = deriveProtocolBadge({
      protocol_selected: "responses",
      protocol_active: "responses",
      protocol_probe: { state: "degraded", tier_passed: ["L1", "L2"] },
    });
    expect(badge).toEqual({ kind: "degraded", tiers: ["L1", "L2"], stripped: [] });
    expect(showsResponsesMark(badge)).toBe(true);
  });

  it("carries the trimmed field names into the degraded badge", () => {
    // S7: "works, but `reasoning` was dropped" must reach the tooltip — a
    // degraded badge without the field list is a fact the user cannot act on.
    const badge = deriveProtocolBadge({
      protocol_selected: "responses",
      protocol_active: "responses",
      protocol_probe: {
        state: "degraded",
        tier_passed: ["L1", "L2", "L3"],
        stripped_fields: ["reasoning"],
      },
    });
    expect(badge).toEqual({
      kind: "degraded",
      tiers: ["L1", "L2", "L3"],
      stripped: ["reasoning"],
    });
  });

  it("a verified verdict has no stripped fields to report", () => {
    const badge = deriveProtocolBadge({
      protocol_selected: "responses",
      protocol_active: "responses",
      protocol_probe: {
        state: "verified",
        tier_passed: ["L1", "L2", "L3"],
        stripped_fields: [],
      },
    });
    expect(badge.kind).toBe("verified");
  });

  it("shows the yellow badge with the reason when the probe failed", () => {
    // Regression case from §7: "failed 时保存可用且不启用" — the model is saved,
    // `protocol_active` fell back to the default, and the user is told why.
    const badge = deriveProtocolBadge({
      protocol_selected: "responses",
      protocol_active: "openai",
      protocol_probe: {
        state: "failed",
        reason: "endpoint unavailable: /responses 404",
      },
    });
    expect(badge).toEqual({
      kind: "blocked",
      reason: "failed",
      detail: "endpoint unavailable: /responses 404",
    });
    // The yellow badge carries no `R` (D11): the `R` means "usable".
    expect(showsResponsesMark(badge)).toBe(false);
  });

  it("distinguishes stale, unverified and unknown verdicts", () => {
    const base = { protocol_selected: "responses", protocol_active: "openai" };
    expect(deriveProtocolBadge({ ...base, protocol_probe: { state: "stale" } })).toEqual({
      kind: "blocked",
      reason: "stale",
    });
    expect(deriveProtocolBadge({ ...base, protocol_probe: { state: "unverified" } })).toEqual({
      kind: "blocked",
      reason: "unverified",
    });
    expect(
      deriveProtocolBadge({ ...base, protocol_probe: { state: "unverified" } }),
    ).toEqual({ kind: "blocked", reason: "unverified" });
    // No verdict at all still needs the yellow badge: the user picked
    // Responses and nothing enabled it yet.
    expect(deriveProtocolBadge(base)).toEqual({ kind: "blocked", reason: "unverified" });
  });

  it("treats a passing verdict with a fallback as the platform switch being off", () => {
    // §4.4: `llm.responses_api_enabled=false` forces the fallback, and the
    // fallback MUST be visible — never a silent protocol change.
    const badge = deriveProtocolBadge({
      protocol_selected: "responses",
      protocol_active: "openai",
      protocol_probe: { state: "verified", tier_passed: ["L1", "L2", "L3"] },
    });
    expect(badge).toEqual({ kind: "blocked", reason: "gate_off" });
  });

  it("keeps the declaration visible when the user chose the default protocol", () => {
    const badge = deriveProtocolBadge({
      shape: "responses",
      protocol_selected: "openai",
      protocol_active: "openai",
    });
    expect(badge).toEqual({ kind: "declared" });
  });
});

describe("estimateProbeRequests", () => {
  it("counts nothing when no responses row changed", () => {
    expect(estimateProbeRequests([])).toEqual({ models: 0, requests: 0 });
    expect(
      estimateProbeRequests([
        { previousSelected: "responses", nextSelected: "responses" },
        { previousSelected: undefined, nextSelected: "openai" },
      ]),
    ).toEqual({ models: 0, requests: 0 });
  });

  it("charges one L1 per changed responses row plus one deep tier", () => {
    // Measured against the probe runner: deep = L1 + L3 + function_call replay
    // (3 requests), every other row = L1 only. Two rows ⇒ 4 requests, which is
    // exactly what the S5 smoke saw against the fake provider.
    expect(
      estimateProbeRequests([
        { isNew: true, nextSelected: "responses" },
        { isNew: true, nextSelected: "responses" },
      ]),
    ).toEqual({ models: 2, requests: 4 });

    expect(
      estimateProbeRequests([{ isNew: true, nextSelected: "responses" }]),
    ).toEqual({ models: 1, requests: 3 });
  });

  it("reprobes rows whose config changed or whose selection was cleared", () => {
    expect(
      estimateProbeRequests([
        { previousSelected: "responses", nextSelected: "responses", configChanged: true },
      ]),
    ).toEqual({ models: 1, requests: 3 });

    // Clearing a selection probes nothing: there is no endpoint to verify.
    expect(
      estimateProbeRequests([{ previousSelected: "responses", nextSelected: undefined }]),
    ).toEqual({ models: 0, requests: 0 });
  });
});