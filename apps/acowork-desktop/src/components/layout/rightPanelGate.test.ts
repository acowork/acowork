/**
 * ADR-101 P4: the right-panel memory gate.
 *
 * The memory tab needs TWO facts now — a running agent (it always did) and a
 * ready memory service (it is no longer Runtime-internal). Both `RightNavBar`
 * (tile drawing) and `AppLayout` (tab fall-back) read these helpers, so a
 * hidden tile and an orphaned active tab cannot disagree.
 */

import { describe, it, expect } from "vitest";

import {
    memoryTabAvailable,
    memoryTabFallback,
    visiblePanelTab,
    workspacePanelVisible,
} from "./rightPanelGate";

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
        for (const tab of ["status", "workspace", "setup", "debug"] as const) {
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


describe("visiblePanelTab", () => {
    const pinned = { activePanelTab: "memory", rightPanelCollapsed: false, hoverPanelTab: null };

    it("is the active tab when the panel is pinned open", () => {
        expect(visiblePanelTab(pinned)).toBe("memory");
    });

    it("is nothing while the layout is collapsed", () => {
        expect(visiblePanelTab({ ...pinned, rightPanelCollapsed: true })).toBeNull();
    });

    it("is the previewed tab while the drawer is up — collapsed no longer means invisible", () => {
        expect(visiblePanelTab({ activePanelTab: "memory", rightPanelCollapsed: true, hoverPanelTab: "workspace" }))
            .toBe("workspace");
    });

    it("prefers the peek over the pinned tab, because a peek means the pinned panel is not mounted at all", () => {
        // `AppLayout` mounts `RightPanel` only when `!rightPanelCollapsed`, so
        // while the drawer is up the pinned tab is off screen. Reporting both
        // would let the fs-watch subscribe to a tree nobody can see.
        expect(visiblePanelTab({ ...pinned, hoverPanelTab: "workspace" })).toBe("workspace");
    });

    it("treats a missing hover field as no peek (older callers and fixtures)", () => {
        expect(visiblePanelTab({ activePanelTab: "status", rightPanelCollapsed: false })).toBe("status");
    });
});

describe("workspacePanelVisible", () => {
    it("is true for a pinned workspace tab", () => {
        expect(workspacePanelVisible({ activePanelTab: "workspace", rightPanelCollapsed: false })).toBe(true);
    });

    it("is true for a workspace tab the drawer is only previewing", () => {
        // The whole reason this helper exists: a tree the user is looking at
        // has to keep refreshing, or the peek shows a snapshot and the file
        // they hovered for is missing from it.
        expect(workspacePanelVisible({ activePanelTab: "status", rightPanelCollapsed: true, hoverPanelTab: "workspace" }))
            .toBe(true);
    });

    it("is false while a different tab is peeked", () => {
        expect(workspacePanelVisible({ activePanelTab: "workspace", rightPanelCollapsed: true, hoverPanelTab: "memory" }))
            .toBe(false);
    });

    it("is false when nothing is on screen", () => {
        expect(workspacePanelVisible({ activePanelTab: "workspace", rightPanelCollapsed: true, hoverPanelTab: null }))
            .toBe(false);
    });
});
