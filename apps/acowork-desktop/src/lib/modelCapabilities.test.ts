import { describe, it, expect } from "vitest";
import { resolveDefaultReasoningEffort, defaultEffortOf } from "./modelCapabilities";
import type { ModelEntry } from "./types";

// resolveDefaultReasoningEffort is the single source of truth used by both
// `setCurrentModel` (user picks a model from the dropdown) and the MQTT
// `model_confirmed` handler (Runtime confirms a model switch).
//
// The resolution chain mirrors the backend's
// `resolve_effective_reasoning_effort` (llm_effects.rs):
//   1. default_reasoning_effort (provider recommended)
//   2. supports_reasoning -> "auto"
//   3. null (model doesn't support reasoning)

const MODELS: ModelEntry[] = [
  {
    name: "claude-opus-4-7",
    provider: "anthropic",
    reasoning: true,
    default_reasoning_effort: "medium",
  },
  {
    name: "claude-3-5-sonnet",
    provider: "anthropic",
    reasoning: true,
    default_reasoning_effort: "high",
  },
  {
    name: "claude-3-5-sonnet",
    provider: "bedrock",
    reasoning: true,
    default_reasoning_effort: "low",
  },
  {
    name: "deepseek-v4-pro",
    provider: "deepseek",
    reasoning: true,
    // No default_reasoning_effort -- supports_reasoning -> "auto"
  },
  {
    name: "glm-5.2",
    provider: "volcengine-agent-plan",
    reasoning: false,
    // No default_reasoning_effort, no reasoning support
  },
  {
    name: "kimi-k3",
    provider: "volcengine-agent-plan",
    // reasoning is undefined (not set in config) -- treated as falsy
    // No default_reasoning_effort
  },
];

describe("resolveDefaultReasoningEffort", () => {
  // -- Level 1: default_reasoning_effort --
  it("returns the entry's default_reasoning_effort when present", () => {
    expect(
      resolveDefaultReasoningEffort(MODELS, "claude-opus-4-7", "anthropic"),
    ).toBe("medium");
    expect(
      resolveDefaultReasoningEffort(MODELS, "claude-3-5-sonnet", "anthropic"),
    ).toBe("high");
  });

  it("disambiguates by provider when the same model name exists under multiple providers", () => {
    expect(
      resolveDefaultReasoningEffort(MODELS, "claude-3-5-sonnet", "bedrock"),
    ).toBe("low");
    expect(
      resolveDefaultReasoningEffort(MODELS, "claude-3-5-sonnet", "anthropic"),
    ).not.toBe("low");
  });

  // -- Level 2: supports_reasoning -> "auto" --
  it("returns 'auto' when model supports reasoning but has no default_reasoning_effort", () => {
    // Aligns with backend resolve_effective_reasoning_effort Level 3:
    // supports_reasoning == true -> Auto.
    expect(
      resolveDefaultReasoningEffort(MODELS, "deepseek-v4-pro", "deepseek"),
    ).toBe("auto");
  });

  // -- Level 3: null (model doesn't support reasoning) --
  it("returns null when reasoning is explicitly false", () => {
    expect(
      resolveDefaultReasoningEffort(MODELS, "glm-5.2", "volcengine-agent-plan"),
    ).toBeNull();
  });

  it("returns null when reasoning is undefined (not set in config)", () => {
    // supports_reasoning absent in agent_provider.json -> reasoning is
    // undefined in ModelEntry -> treated as falsy -> null.
    // Matches backend: caps.and_then(|c| c.supports_reasoning).unwrap_or(false)
    expect(
      resolveDefaultReasoningEffort(MODELS, "kimi-k3", "volcengine-agent-plan"),
    ).toBeNull();
  });

  // -- Edge cases --
  it("returns null when the model is not in the list", () => {
    expect(
      resolveDefaultReasoningEffort(MODELS, "gpt-99-turbo", "openai"),
    ).toBeNull();
  });

  it("returns null when the model exists but provider is wrong", () => {
    expect(
      resolveDefaultReasoningEffort(MODELS, "claude-opus-4-7", "openai"),
    ).toBeNull();
  });

  it("handles an empty model list gracefully", () => {
    expect(resolveDefaultReasoningEffort([], "claude-opus-4-7", "anthropic"))
      .toBeNull();
  });

  it("treats empty-string provider as a real value (does not match missing entries)", () => {
    expect(
      resolveDefaultReasoningEffort(MODELS, "claude-opus-4-7", ""),
    ).toBeNull();
  });
});

// defaultEffortOf is the entry-level chain shared with the Chat composer
// (AgentChatView), which locates its own model entry and must not
// re-implement the levels. Regression: the Chat surface used inline
// `default_reasoning_effort ?? null`, so reasoning-capable models with no
// configured default (custom providers) hid the effort toggle there while
// the execution surface showed it.
describe("defaultEffortOf", () => {
  it("returns default_reasoning_effort when present", () => {
    expect(
      defaultEffortOf({ reasoning: true, default_reasoning_effort: "medium" }),
    ).toBe("medium");
  });

  it("returns 'auto' when reasoning-capable with no default (the regression case)", () => {
    expect(
      defaultEffortOf({ reasoning: true, default_reasoning_effort: undefined }),
    ).toBe("auto");
  });

  it("returns null when reasoning is falsy or absent", () => {
    expect(defaultEffortOf({ reasoning: false })).toBeNull();
    expect(defaultEffortOf({})).toBeNull();
  });

  it("returns null for a null/undefined entry", () => {
    expect(defaultEffortOf(null)).toBeNull();
    expect(defaultEffortOf(undefined)).toBeNull();
  });

  it("agrees with resolveDefaultReasoningEffort for every list entry", () => {
    for (const m of MODELS) {
      expect(defaultEffortOf(m)).toBe(
        resolveDefaultReasoningEffort(MODELS, m.name, m.provider),
      );
    }
  });
});
