/**
 * ADR-100 P4: the right-panel memory gate.
 *
 * The memory tab needs TWO facts now — a running agent (it always did) and a
 * ready memory service (it is no longer Runtime-internal). Both `RightNavBar`
 * (tile drawing) and `AppLayout` (tab fall-back) read these helpers, so a
 * hidden tile and an orphaned active tab cannot disagree.
 */

import { describe, it, expect } from "vitest";

import { memoryTabAvailable, memoryTabFallback } from "./rightPanelGate";

const ON = { agentRunning: true, memoryServiceReady: true };

describe("memoryTabAvailable", () => {
    it("needs the agent AND the service", () => {
        expect(memoryTabAvailable(ON)).toBe(true);
    });

    it("is false while the service is not ready, even with a running agent", () => {
        expect(memoryTabAvailable({ ...ON, memoryServiceReady: false })).toBe(false);
    });

    it("is false while the agent is stopped, even with a ready service", () => {
        expect(memoryTabAvailable({ ...ON, agentRunning: false })).toBe(false);
    });

    it("is false when neither holds", () => {
        expect(memoryTabAvailable({ agentRunning: false, memoryServiceReady: false })).toBe(false);
    });
});

describe("memoryTabFallback", () => {
    it("leaves every other tab alone", () => {
        for (const tab of ["status", "workspace", "tools", "setup", "debug"] as const) {
            expect(memoryTabFallback(tab, { agentRunning: false, memoryServiceReady: false })).toBeNull();
        }
    });

    it("keeps the memory tab when it is still available", () => {
        expect(memoryTabFallback("memory", ON)).toBeNull();
    });

    it("bounces an active memory tab to status when the service goes away", () => {
        expect(memoryTabFallback("memory", { ...ON, memoryServiceReady: false })).toBe("status");
    });

    it("bounces it to status when the agent stops (same landing tab as the agent-stop rule)", () => {
        expect(memoryTabFallback("memory", { ...ON, agentRunning: false })).toBe("status");
    });
});
