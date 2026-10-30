import { describe, it, expect } from "vitest";
import {
  clampReasoningLevel,
  normalizeReasoningLevel,
  reasoningLevelRank,
  reasoningLevelsFor,
  GENERIC_REASONING_LEVELS,
} from "./reasoningEfforts";

describe("reasoningEfforts", () => {
  describe("normalizeReasoningLevel", () => {
    it("maps the catalog spellings onto the canonical ones", () => {
      expect(normalizeReasoningLevel("none")).toBe("off");
      expect(normalizeReasoningLevel("default")).toBe("auto");
      expect(normalizeReasoningLevel("LOW")).toBe("low");
      expect(normalizeReasoningLevel("xhigh")).toBe("xhigh");
      expect(normalizeReasoningLevel("minimal")).toBe("minimal");
    });

    it("rejects unknown spellings", () => {
      expect(normalizeReasoningLevel("bogus")).toBeUndefined();
      expect(normalizeReasoningLevel("")).toBeUndefined();
    });
  });

  describe("reasoningLevelRank", () => {
    it("orders the depth levels", () => {
      expect(reasoningLevelRank("minimal")).toBeLessThan(
        reasoningLevelRank("low")!,
      );
      expect(reasoningLevelRank("low")).toBeLessThan(
        reasoningLevelRank("medium")!,
      );
      expect(reasoningLevelRank("medium")).toBeLessThan(
        reasoningLevelRank("high")!,
      );
      expect(reasoningLevelRank("high")).toBeLessThan(
        reasoningLevelRank("xhigh")!,
      );
      expect(reasoningLevelRank("xhigh")).toBeLessThan(
        reasoningLevelRank("max")!,
      );
    });

    it("gives auto and off no rank", () => {
      expect(reasoningLevelRank("auto")).toBeUndefined();
      expect(reasoningLevelRank("off")).toBeUndefined();
    });
  });

  describe("reasoningLevelsFor", () => {
    it("returns the model's declared levels, sorted by depth", () => {
      expect(
        reasoningLevelsFor(["max", "low", "high"]),
      ).toEqual(["low", "high", "max"]);
    });

    it("drops the non-depth spellings", () => {
      expect(reasoningLevelsFor(["none", "low", "high"])).toEqual([
        "low",
        "high",
      ]);
    });

    it("falls back to the generic set for a custom model", () => {
      expect(reasoningLevelsFor(undefined)).toEqual([
        ...GENERIC_REASONING_LEVELS,
      ]);
      expect(reasoningLevelsFor([])).toEqual([...GENERIC_REASONING_LEVELS]);
      expect(reasoningLevelsFor(["bogus"])).toEqual([
        ...GENERIC_REASONING_LEVELS,
      ]);
    });

    it("keeps a single-level model", () => {
      expect(reasoningLevelsFor(["high"])).toEqual(["high"]);
    });
  });

  describe("clampReasoningLevel", () => {
    it("keeps a supported level", () => {
      expect(clampReasoningLevel("medium", ["low", "medium", "high"])).toBe(
        "medium",
      );
    });

    it("folds an unsupported level down", () => {
      expect(clampReasoningLevel("medium", ["low", "high", "max"])).toBe(
        "low",
      );
      expect(clampReasoningLevel("xhigh", ["low", "high", "max"])).toBe("high");
    });

    it("folds up to the shallowest supported level", () => {
      expect(clampReasoningLevel("low", ["high", "max"])).toBe("high");
    });

    it("never touches auto or off", () => {
      expect(clampReasoningLevel("auto", ["high"])).toBe("auto");
      expect(clampReasoningLevel("off", ["high"])).toBe("off");
    });

    it("keeps the level when the support set is unknown", () => {
      expect(clampReasoningLevel("medium", undefined)).toBe("medium");
      expect(clampReasoningLevel("medium", [])).toBe("medium");
    });
  });
});
