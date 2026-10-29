//! ADR-103 D12.1 — the save response's probe verdicts must reach the user.
//!
//! These tests pin the three properties the defect violated:
//!
//!   1. a save that probed something **always** produces a message,
//!   2. a failure is never reported as a success, and a partial success is
//!      never reported as a total one,
//!   3. a save that probed nothing stays silent (no invented verification).

import { describe, it, expect } from "vitest";
import i18n from "../i18n";
import {
  summarizeSaveProbes,
  type SaveProbeReport,
} from "./saveProbeFeedback";

const t = (key: string, options?: Record<string, unknown>) => i18n.t(key, options);

function verified(model: string): SaveProbeReport {
  return {
    model,
    protocol: "responses",
    tier: "deep",
    cached: false,
    outcome: { success: true, tier_passed: ["L1", "L2", "L3"], latency_ms: 210 },
  };
}

function degraded(model: string, fields: string[]): SaveProbeReport {
  return {
    model,
    protocol: "responses",
    tier: "deep",
    cached: false,
    outcome: {
      success: true,
      tier_passed: ["L1", "L2", "L3"],
      latency_ms: 220,
      stripped_fields: fields,
    },
  };
}

function failed(model: string, message: string): SaveProbeReport {
  return {
    model,
    protocol: "responses",
    tier: "standard",
    cached: false,
    outcome: {
      success: false,
      tier_passed: ["L1"],
      latency_ms: 90,
      error: { class: "UnsupportedField", message },
    },
  };
}

describe("summarizeSaveProbes", () => {
  it("reports nothing when the save probed nothing", () => {
    expect(summarizeSaveProbes(undefined, t)).toBeNull();
    expect(summarizeSaveProbes(null, t)).toBeNull();
    expect(summarizeSaveProbes([], t)).toBeNull();
  });

  it("names every verified model on a clean save", () => {
    const feedback = summarizeSaveProbes([verified("m-a"), verified("m-b")], t);
    expect(feedback?.type).toBe("success");
    expect(feedback?.message).toContain("m-a, m-b");
  });

  it("abbreviates a long model list instead of overflowing the toast", () => {
    const feedback = summarizeSaveProbes(
      [verified("m-a"), verified("m-b"), verified("m-c"), verified("m-d"), verified("m-e")],
      t,
    );
    expect(feedback?.message).toContain("m-a, m-b, m-c +2");
  });

  it("surfaces a failed verification with the Gateway's reason", () => {
    // The original bug: this save closed the dialog with no feedback at all.
    const feedback = summarizeSaveProbes(
      [failed("acme-3.1-flash", "endpoint rejected 'reasoning'")],
      t,
    );
    expect(feedback?.type).toBe("error");
    expect(feedback?.message).toContain("acme-3.1-flash");
    expect(feedback?.message).toContain("endpoint rejected 'reasoning'");
  });

  it("never hides a failure behind the models that passed", () => {
    const feedback = summarizeSaveProbes(
      [verified("m-ok"), failed("m-bad", "HTTP 404")],
      t,
    );
    expect(feedback?.type).toBe("error");
    expect(feedback?.message).toContain("m-bad");
    // The passing model is acknowledged, not reported as the headline.
    expect(feedback?.message).toContain("1");
  });

  it("falls back to the error class when the Gateway sent no message", () => {
    const probe = failed("m-bad", "");
    probe.outcome.error = { class: "TransportError" };
    const feedback = summarizeSaveProbes([probe], t);
    expect(feedback?.message).toContain("TransportError");
  });

  it("reports a degraded save as usable, naming the dropped field", () => {
    const feedback = summarizeSaveProbes([degraded("m-a", ["reasoning"])], t);
    expect(feedback?.type).toBe("warning");
    expect(feedback?.message).toContain("reasoning");
    expect(feedback?.message).toContain("m-a");
  });

  it("de-duplicates the dropped fields across models", () => {
    const feedback = summarizeSaveProbes(
      [degraded("m-a", ["reasoning"]), degraded("m-b", ["reasoning"])],
      t,
    );
    expect(feedback?.message).toContain("m-a, m-b");
    expect(feedback?.message?.split("reasoning")).toHaveLength(2);
  });
});
