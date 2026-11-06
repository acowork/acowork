import { describe, expect, it } from "vitest";

import {
  AGENT_SETUP_CARDS,
  DEFAULT_EXPANDED_CARD_COUNT,
  defaultOpenForCard,
  initialCardOpenState,
} from "./agentSetupCards";

describe("agentSetupCards", () => {
  it("keeps the migrated cards in the panel", () => {
    // The three cards that moved in from the Tools / Debug panels.
    expect(AGENT_SETUP_CARDS).toContain("promptsSkills");
    expect(AGENT_SETUP_CARDS).toContain("builtinTools");
    expect(AGENT_SETUP_CARDS).toContain("mcpServers");
  });

  it("opens exactly the first DEFAULT_EXPANDED_CARD_COUNT cards", () => {
    const open = AGENT_SETUP_CARDS.filter(defaultOpenForCard);
    expect(open).toEqual(AGENT_SETUP_CARDS.slice(0, DEFAULT_EXPANDED_CARD_COUNT));
  });

  it("starts the cards after the third one collapsed", () => {
    const collapsed = AGENT_SETUP_CARDS.slice(DEFAULT_EXPANDED_CARD_COUNT);
    // Guards against the list shrinking to <= 3 entries, which would make the
    // "the rest stay collapsed" half of the rule vacuous.
    expect(collapsed.length).toBeGreaterThan(0);
    for (const id of collapsed) expect(defaultOpenForCard(id)).toBe(false);
  });

  it("seeds the panel open-state map from the same rule", () => {
    const state = initialCardOpenState();
    expect(Object.keys(state).sort()).toEqual([...AGENT_SETUP_CARDS].sort());
    for (const id of AGENT_SETUP_CARDS) {
      expect(state[id]).toBe(defaultOpenForCard(id));
    }
  });
});
