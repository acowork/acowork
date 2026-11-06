//! ADR-104 §8 — "the recommendation group becoming noise": a fully handled
//! group is collapsed by default, and the header keeps the count, so nothing
//! becomes unreachable. This suite pins the classification that rule rests on.
//!
//! `hasUnhandledRecommendation` is the only thing the card reads to decide its
//! initial open state, so these cases ARE the default-expand contract — the
//! component test asserts the wiring, not the table.

import { describe, it, expect } from "vitest";
import {
  hasUnhandledRecommendation,
  isHandledRecommendation,
} from "./mcpRecommendations";
import type { McpRecommendationState } from "./types";

describe("isHandledRecommendation", () => {
  it("counts active and dismissed as handled", () => {
    // `active` — the server is on for this agent: nothing left to ask.
    expect(isHandledRecommendation("active")).toBe(true);
    // `dismissed` — the user refused it; refusal is a decision (ADR-104 §5.3).
    expect(isHandledRecommendation("dismissed")).toBe(true);
  });

  it("counts the two actionable states as unhandled", () => {
    expect(isHandledRecommendation("not_installed")).toBe(false);
    expect(isHandledRecommendation("installed_inactive")).toBe(false);
  });
});

describe("hasUnhandledRecommendation", () => {
  const row = (state: McpRecommendationState) => ({ state });

  it("is false for an empty list — no suggestions, no card", () => {
    expect(hasUnhandledRecommendation([])).toBe(false);
  });

  it("is true when any single row still needs a decision", () => {
    expect(hasUnhandledRecommendation([row("active"), row("not_installed")])).toBe(true);
    expect(hasUnhandledRecommendation([row("installed_inactive"), row("dismissed")])).toBe(true);
  });

  it("is false once every row is active or dismissed", () => {
    expect(hasUnhandledRecommendation([row("active"), row("dismissed")])).toBe(false);
    expect(hasUnhandledRecommendation([row("active")])).toBe(false);
  });

  it("reads `state` only — a fully-shaped row is not required", () => {
    // The card asks the question of rows whose lifecycle it already trusts;
    // nothing else in the view influences the answer.
    expect(hasUnhandledRecommendation([{ state: "active" }])).toBe(false);
  });
});
