import { describe, expect, it, vi } from "vitest";
import { flushPendingWrites } from "./agentSetupPendingWrites";

describe("flushPendingWrites", () => {
  it("cancels timers and flushes only the selected agent's latest values", () => {
    vi.useFakeTimers();
    try {
      const timerA = setTimeout(() => {}, 1000);
      const timerB = setTimeout(() => {}, 1000);
      const timers = new Map([
        ["agent-a:maxTokens", timerA],
        ["agent-b:maxTokens", timerB],
      ]);
      const pendingValues = new Map([
        ["agent-a:maxTokens", { agentId: "agent-a", value: 16000 }],
        ["agent-b:maxTokens", { agentId: "agent-b", value: 32000 }],
      ]);
      const flushed: unknown[] = [];

      const keys = flushPendingWrites("agent-a", timers, pendingValues, (item) => {
        flushed.push(item);
      });

      expect(keys).toEqual(["agent-a:maxTokens"]);
      expect(flushed).toEqual([{ agentId: "agent-a", value: 16000 }]);
      expect(timers.has("agent-a:maxTokens")).toBe(false);
      expect(pendingValues.has("agent-a:maxTokens")).toBe(false);
      expect(timers.has("agent-b:maxTokens")).toBe(true);
      expect(pendingValues.has("agent-b:maxTokens")).toBe(true);
      expect(vi.getTimerCount()).toBe(1);
    } finally {
      vi.useRealTimers();
    }
  });
});
