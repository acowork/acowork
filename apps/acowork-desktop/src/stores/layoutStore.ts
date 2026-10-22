import { create } from "zustand";

import type { NavView } from "../lib/types";

/** Right panel tabs — keep in sync with AppLayout.PanelTab. */
export type PanelTab = "debug" | "status" | "setup" | "tools" | "memory" | "workspace";

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
}));