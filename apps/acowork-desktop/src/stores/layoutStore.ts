import { create } from "zustand";

import type { NavView } from "../lib/types";

/**
 * Right panel tabs — keep in sync with AppLayout.PanelTab.
 *
 * Five entries. The `tools` tab is gone: its Builtin Tools / MCP cards moved
 * into the `setup` panel (see `components/right-panel/agentSetupCards.ts`),
 * so there is no longer a rail entry that could become the active tab.
 */
export type PanelTab = "debug" | "status" | "setup" | "memory" | "workspace";

/** Harness page tabs — keep in sync with HarnessPage's tab list. */
export type HarnessTabId = "providers" | "mcp" | "embedding" | "lsp";

/**
 * Live bounding rect of the file editor panel, reported by
 * `useReportFilePanelBounds` from inside `FileEditorPanel`.
 *
 * Coordinates are measured against `window` (i.e. `getBoundingClientRect()`)
 * so a consumer rendered in the global status bar can use them directly with
 * `position: absolute; left: ${bounds.left}px; right: ${windowW - bounds.right}px`.
 *
 * `mounted = false` means no file is open (the panel is unmounted in
 * `AppLayout` when `hasOpenFiles` is false); consumers should hide
 * themselves rather than render at stale (0,0) coordinates.
 */
export interface FilePanelBounds {
    left: number;
    right: number;
    mounted: boolean;
}

interface LayoutState {
    /** Currently active tab in the right panel. */
    activePanelTab: PanelTab;
    setActivePanelTab: (tab: PanelTab) => void;

    /** Whether the right panel is collapsed. */
    rightPanelCollapsed: boolean;
    /**
     * Update the collapsed state. Accepts either a boolean or an updater
     * function (mirrors React's `setState(prev => !prev)` API) so call sites
     * can avoid stale-closure bugs.
     */
    setRightPanelCollapsed: (collapsed: boolean | ((prev: boolean) => boolean)) => void;

    /**
     * The tab the hover drawer is currently previewing, or `null`.
     *
     * The drawer is a *preview*: it floats over the row while
     * `rightPanelCollapsed` stays true, so the layout keeps the width the user
     * gave the file editor. That also means `collapsed` alone no longer answers
     * "is this panel on screen?" — and the answer matters to consumers that
     * hold no DOM of their own, above all the demand-driven fs-watch set in
     * `lib/workspaceFsWatch.ts`, which must keep watching the tree the drawer is
     * showing or the previewed file list goes stale under the user's cursor.
     *
     * Written by `AppLayout` (the drawer's owner), read through
     * `layout/rightPanelGate.ts`'s `visiblePanelTab`. Never persisted.
     */
    hoverPanelTab: PanelTab | null;
    setHoverPanelTab: (tab: PanelTab | null) => void;

    /**
     * Whether the pm view's right settings panel is open. Separate from
     * `rightPanelCollapsed` (which belongs to the chat view's agent panel);
     * the two share only the rail geometry and the panel width.
     */
    pmPanelOpen: boolean;
    setPmPanelOpen: (open: boolean | ((prev: boolean) => boolean)) => void;

    /** Whether the docs view's right settings panel is open (see pmPanelOpen). */
    docPanelOpen: boolean;
    setDocPanelOpen: (open: boolean | ((prev: boolean) => boolean)) => void;

    /**
     * Monotonically-increasing counter for "show me the workspace panel" requests.
     * AppLayout consumes each new value once (via a local ref) to expand the
     * right panel and switch the active tab to "workspace", even when the
     * user clicks the trigger repeatedly for the same file.
     */
    workspacePanelRequestSeq: number;
    requestShowWorkspacePanel: () => void;

    /**
     * Monotonically-increasing counter for "focus the workspace search input"
     * requests. WorkspaceExplorer consumes each new value once (via a local
     * ref) so the same Ctrl+F press can both open the panel (via the AppLayout
     * handler) and place the caret in the input (via this counter).
     */
    workspaceSearchFocusSeq: number;
    requestFocusWorkspaceSearch: () => void;

    /**
     * Live bounding rect of the file editor panel. Updated by
     * `useReportFilePanelBounds` running inside `FileEditorPanel`.
     */
    filePanelBounds: FilePanelBounds;
    /**
     * Replace the file panel bounds wholesale. The reporting hook coalesces
     * updates via `requestAnimationFrame` so a drag of the resize handle does
     * not flood the store at refresh-rate cadence.
     */
    setFilePanelBounds: (bounds: FilePanelBounds) => void;

    /**
     * Monotonically-increasing counter for "switch the main area" requests
     * raised from outside `AppLayout` (e.g. the sidebar `UserList` opening a
     * user chat, ADR-076 §决策 8). `AppLayout` owns `currentView`, so a peer
     * component cannot set it directly; same consume-once ref contract as
     * `workspacePanelRequestSeq`. Read `navViewRequest` for the payload.
     */
    navViewRequestSeq: number;
    navViewRequest: NavView | null;
    requestNavView: (view: NavView) => void;

    /**
     * Monotonically-increasing counter for "open the harness page on this
     * tab" requests (ADR-104: the MCP recommendation group sends the
     * user to the MCP tab to enter an API key). `HarnessPage` owns its active
     * tab, so the request travels through the same consume-once seq contract
     * as `navViewRequestSeq`; callers request the view *and* the tab.
     */
    harnessTabRequestSeq: number;
    harnessTabRequest: HarnessTabId | null;
    requestHarnessTab: (tab: HarnessTabId) => void;
}

export const useLayoutStore = create<LayoutState>((set) => ({
    activePanelTab: "workspace",
    setActivePanelTab: (tab) => set({ activePanelTab: tab }),

    rightPanelCollapsed: false,
    setRightPanelCollapsed: (collapsed) =>
        set((state) => ({
            rightPanelCollapsed:
                typeof collapsed === "function"
                    ? (collapsed as (prev: boolean) => boolean)(state.rightPanelCollapsed)
                    : collapsed,
        })),

    hoverPanelTab: null,
    setHoverPanelTab: (tab) => set({ hoverPanelTab: tab }),

    pmPanelOpen: false,
    setPmPanelOpen: (open) =>
        set((state) => ({
            pmPanelOpen:
                typeof open === "function"
                    ? (open as (prev: boolean) => boolean)(state.pmPanelOpen)
                    : open,
        })),

    docPanelOpen: false,
    setDocPanelOpen: (open) =>
        set((state) => ({
            docPanelOpen:
                typeof open === "function"
                    ? (open as (prev: boolean) => boolean)(state.docPanelOpen)
                    : open,
        })),

    workspacePanelRequestSeq: 0,
    requestShowWorkspacePanel: () =>
        set((state) => ({ workspacePanelRequestSeq: state.workspacePanelRequestSeq + 1 })),

    workspaceSearchFocusSeq: 0,
    requestFocusWorkspaceSearch: () =>
        set((state) => ({ workspaceSearchFocusSeq: state.workspaceSearchFocusSeq + 1 })),

    filePanelBounds: { left: 0, right: 0, mounted: false },
    setFilePanelBounds: (bounds) => set({ filePanelBounds: bounds }),

    navViewRequestSeq: 0,
    navViewRequest: null,
    requestNavView: (view) =>
        set((state) => ({
            navViewRequest: view,
            navViewRequestSeq: state.navViewRequestSeq + 1,
        })),

    harnessTabRequestSeq: 0,
    harnessTabRequest: null,
    requestHarnessTab: (tab) =>
        set((state) => ({
            harnessTabRequest: tab,
            harnessTabRequestSeq: state.harnessTabRequestSeq + 1,
        })),
}));
