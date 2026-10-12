import { describe, expect, it } from "vitest";
import {
  computeContextUsageBreakdown,
  formatDetailedPercent,
  redistributeTokensByBytes,
  type ContextUsageCategory,
  type ContextUsageSection,
} from "./contextUsageBreakdown";

function percentagesByKey(categories: ContextUsageCategory[]): Record<string, number> {
  return Object.fromEntries(categories.map(({ key, percentage }) => [key, percentage]));
}

// Test fixture: keep `size_bytes` proportional to the previous
// `token_estimate` mock (×3) so the percentage expectations stay valid
// while we transition from per-section token estimation to bytes.
const MOCK_RATIO = 3;

function mockSection(key: string, token_estimate: number): ContextUsageSection {
  return { key, size_bytes: token_estimate * MOCK_RATIO };
}

describe("computeContextUsageBreakdown", () => {
  it("maps debug sections into the five usage categories and normalizes to the billed total", () => {
    const result = computeContextUsageBreakdown(
      [
        // Runtime built-in and MCP tools are exposed as separate categories.
        mockSection("system_prompt", 3_270),
        mockSection("identity_context", 250),
        mockSection("workspace_context", 100),
        mockSection("todo_context", 50),
        mockSection("tool_definitions", 9_970),
        mockSection("messages", 130_340),
        mockSection("skill_instructions", 1_050),
      ],
      55.3,
    );

    expect(percentagesByKey(result)).toEqual({
      system: 1.4,
      tools: 3.8,
      messages: 49.7,
      connectors: 0,
      skills: 0.4,
    });
    expect(result.reduce((sum, category) => sum + category.percentage, 0)).toBeCloseTo(55.3, 10);
  });

  it("uses byte sizes as the share denominator (not the per-section token heuristic)", () => {
    // Regression guard: this is the whole reason we switched the helper
    // off `token_estimate`. Section A's per-section `count_text` over-
    // estimates; section B's under-estimates. Both happen in practice
    // when the CJK / code-mix ratio shifts between sections. The byte
    // ratio is the truth — verify the helper trusts the bytes.
    //
    //   section  bytes   heuristic_tokens
    //     A       100        100      ← short ASCII, heuristic close
    //     B      1000         50      ← heavy CJK, heuristic ~5x off
    //
    // Old algorithm (per-section tokens):  A=66.7%  B=33.3%
    // New algorithm (bytes):               A=9.1%   B=90.9%
    const result = computeContextUsageBreakdown(
      [
        { key: "system_prompt", size_bytes: 100 },
        { key: "messages", size_bytes: 1_000 },
      ],
      100,
    );

    expect(percentagesByKey(result)).toEqual({
      system: 9.1,
      tools: 0,
      messages: 90.9,
      connectors: 0,
      skills: 0,
    });
  });

  it("maps MCP tool definitions to the connectors category", () => {
    const result = computeContextUsageBreakdown(
      [
        mockSection("tool_definitions", 40),
        mockSection("connector_tool_definitions", 60),
      ],
      100,
    );

    expect(percentagesByKey(result)).toEqual({
      system: 0,
      tools: 40,
      messages: 0,
      connectors: 60,
      skills: 0,
    });
  });

  it("classifies the user-role todo context with conversation messages", () => {
    const result = computeContextUsageBreakdown(
      [
        mockSection("system_prompt", 1),
        mockSection("todo_context", 3),
      ],
      100,
    );

    expect(percentagesByKey(result)).toEqual({
      system: 25,
      tools: 0,
      messages: 75,
      connectors: 0,
      skills: 0,
    });
  });

  it("applies one-decimal rounding remainder to the largest category", () => {
    const result = computeContextUsageBreakdown(
      [
        mockSection("system_prompt", 1),
        mockSection("tool_definitions", 1),
        mockSection("messages", 3),
      ],
      20.2,
    );

    expect(percentagesByKey(result)).toEqual({
      system: 4,
      tools: 4,
      messages: 12.2,
      connectors: 0,
      skills: 0,
    });
    expect(result.reduce((sum, category) => sum + category.percentage, 0)).toBeCloseTo(20.2, 10);
  });

  it("treats unknown future context blocks as system context", () => {
    const result = computeContextUsageBreakdown(
      [
        mockSection("future_static_block", 40),
        mockSection("messages", 60),
      ],
      20,
    );

    expect(result).toEqual([
      { key: "system", percentage: 8 },
      { key: "tools", percentage: 0 },
      { key: "messages", percentage: 12 },
      { key: "connectors", percentage: 0 },
      { key: "skills", percentage: 0 },
    ]);
  });

  it("returns zero categories when the snapshot has no byte data", () => {
    expect(
      computeContextUsageBreakdown(
        [
          { key: "tool_definitions", size_bytes: Number.NaN },
          { key: "messages", size_bytes: -1 },
        ],
        55.3,
      ),
    ).toEqual([
      { key: "system", percentage: 0 },
      { key: "tools", percentage: 0 },
      { key: "messages", percentage: 0 },
      { key: "connectors", percentage: 0 },
      { key: "skills", percentage: 0 },
    ]);
  });

  it("clamps invalid and out-of-range percentages", () => {
    expect(computeContextUsageBreakdown([], Number.NaN).every(({ percentage }) => percentage === 0)).toBe(true);
    expect(formatDetailedPercent(Number.NaN)).toBe("0.0");
    expect(formatDetailedPercent(120)).toBe("100.0");
    expect(formatDetailedPercent(55.35)).toBe("55.4");
  });
});

describe("redistributeTokensByBytes", () => {
  it("distributes the billed tokens across sections by byte share and sums exactly to the anchor", () => {
    // 100 total tokens, 3 sections with proportional bytes:
    //   A: 100 /  250 = 0.40  → 40
    //   B: 100 /  250 = 0.40  → 40
    //   C:  50 /  250 = 0.20  → 20
    const result = redistributeTokensByBytes(100, [
      { key: "system_prompt", size_bytes: 100 },
      { key: "messages", size_bytes: 100 },
      { key: "skill_instructions", size_bytes: 50 },
    ]);

    expect(result).toEqual({
      system_prompt: 40,
      messages: 40,
      skill_instructions: 20,
    });
    expect(Object.values(result).reduce((s, n) => s + n, 0)).toBe(100);
  });

  it("does not depend on the per-section token_estimate (which is heuristic and drifts)", () => {
    // The whole point: bytes are the source of truth.  Section A has a
    // much smaller token_estimate than its bytes would suggest (heavy
    // CJK → heuristic under-counts); section B is the opposite.  The
    // helper ignores both heuristics and produces proportional results.
    const result = redistributeTokensByBytes(123_000, [
      { key: "system_prompt", size_bytes: 4_600 },
      { key: "tool_definitions", size_bytes: 11_500 },
      { key: "messages", size_bytes: 363_600 },
    ]);

    // 4600 + 11500 + 363600 = 379700.  Per-section shares:
    //   A:  4600 / 379700 ≈ 0.01211 → 1489.9 → 1490
    //   B: 11500 / 379700 ≈ 0.03029 → 3724.9 → 3725
    //   C: 363600/ 379700 ≈ 0.95760 → 117785.2 → 117785
    // Sum must equal exactly 123000.
    expect(result).toEqual({
      system_prompt: 1_490,
      tool_definitions: 3_725,
      messages: 117_785,
    });
    expect(Object.values(result).reduce((s, n) => s + n, 0)).toBe(123_000);
  });

  it("returns an empty record when no byte data is available (caller uses raw token_estimate)", () => {
    expect(redistributeTokensByBytes(100, [])).toEqual({});
    expect(
      redistributeTokensByBytes(100, [
        { key: "system_prompt", size_bytes: 0 },
        { key: "messages", size_bytes: -1 },
        { key: "messages", size_bytes: Number.NaN },
      ]),
    ).toEqual({});
    expect(redistributeTokensByBytes(0, [{ key: "messages", size_bytes: 100 }])).toEqual({});
    expect(redistributeTokensByBytes(Number.NaN, [{ key: "messages", size_bytes: 100 }])).toEqual({});
  });

  it("keeps every per-section count non-negative and integer", () => {
    const result = redistributeTokensByBytes(7, [
      { key: "a", size_bytes: 1 },
      { key: "b", size_bytes: 1 },
      { key: "c", size_bytes: 1 },
    ]);
    for (const value of Object.values(result)) {
      expect(Number.isInteger(value)).toBe(true);
      expect(value).toBeGreaterThanOrEqual(0);
    }
    expect(Object.values(result).reduce((s, n) => s + n, 0)).toBe(7);
  });
});
