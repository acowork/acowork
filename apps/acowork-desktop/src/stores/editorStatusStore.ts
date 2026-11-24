import { create } from "zustand";

/**
 * Transient cursor / selection signals for the currently active file tab.
 *
 * Why a store instead of local `useState` in `FileEditorPanel`:
 *
 *   - Cursor position changes on every selection move (mouse click,
 *     arrow key, drag-select). As local state that re-renders the WHOLE
 *     editor panel — Monaco, the tab bar, the diff banners — on every
 *     arrow key. The store lets the two-line status footer subscribe on
 *     its own, so only the footer re-renders.
 *
 *   - The panel WRITES and the footer READS. That split is the whole
 *     reason this store exists; the LSP signals it used to carry are
 *     gone (the footer gets them as props from the panel, which already
 *     holds them).
 *
 * Reset semantics: every field is keyed on "the file panel currently has
 * an active file". When the panel unmounts or `activeFile` becomes `null`,
 * `FileEditorPanel` resets the slices to their idle sentinel (see the
 * reset effects in `FileEditorPanel`).
 */

export interface EditorCursorPosition {
    /** 1-based line number (Monaco's coordinate space). */
    line: number;
    /** 1-based column number. */
    column: number;
}

interface EditorStatusState {
    /** Latest cursor position reported by Monaco's `onDidChangeCursorSelection`. */
    cursor: EditorCursorPosition;
    /** Number of characters currently selected; `0` when no active selection. */
    selectedCount: number;

    /**
     * Push a new cursor position. Called on every Monaco
     * `onDidChangeCursorSelection`.
     */
    setCursor: (cursor: EditorCursorPosition) => void;
    /**
     * Push the count of selected characters. `0` for empty selections.
     */
    setSelectedCount: (count: number) => void;
    /**
     * Reset everything to the idle sentinel — called when the panel
     * unmounts or becomes empty so the cluster cannot show stale data
     * from a previously-closed file.
     */
    resetToIdle: () => void;
}

const INITIAL_STATE: Pick<EditorStatusState, "cursor" | "selectedCount"> = {
    cursor: { line: 1, column: 1 },
    selectedCount: 0,
};

export const useEditorStatusStore = create<EditorStatusState>((set) => ({
    ...INITIAL_STATE,

    setCursor: (cursor) => set({ cursor }),
    setSelectedCount: (selectedCount) => set({ selectedCount }),
    resetToIdle: () => set({ ...INITIAL_STATE }),
}));
