/**
 * filePanelAutoCollapse — the rule that trades right-panel width for
 * file-tab width.
 *
 * The reader's ask: when a file tab appears, the right panel folds away so
 * the file pane gets the width; when every file closes, the right panel
 * comes back.
 *
 * Four rules make this safe, and all of them live here rather than in the
 * effect that applies them:
 *
 *   1. **Edge-triggered, never level-triggered.** The fold happens on the
 *      transition "no files → some files", and the unfold on "some files →
 *      no files". A level-triggered rule ("while files are open the panel
 *      must be collapsed") would fight the reader: the moment they clicked
 *      the rail to bring the panel back, the effect would collapse it again.
 *      The transition form lets a manual expand survive.
 *
 *   2. **The reader's own choice wins.** While the file pane is open the
 *      panel is the reader's to move: expanding it by hand means it stays
 *      expanded when the last file closes, and collapsing it by hand means
 *      it stays collapsed. Only the fold this module performed itself is
 *      undone automatically.
 *
 *   3. **A panel that was already collapsed stays collapsed.** The file pane
 *      borrows width only from a panel that was actually on screen; a panel
 *      the reader had put away on purpose is not theirs to give back.
 *
 *   4. **The tab is never touched.** Folding the panel does not change which
 *      tab is selected, so the panel that comes back is the surface the
 *      reader left — no snapshot of the tab is needed, and none is kept.
 *
 * The decision is a pure reducer so the transitions can be tested without a
 * DOM: jsdom computes no layout, and AppLayout drags in Tauri / MQTT /
 * monaco stores.
 */

/** What the layout must do to the right panel. */
export type AutoCollapseAction =
    | { kind: "collapse" }
    | { kind: "expand" }
    | { kind: "none" };

export interface FilePanelAutoState {
    /** Whether a file tab is open (the file pane's own state). */
    hasFiles: boolean;
    /**
     * The panel's pinned state as this module last saw it. It mirrors the
     * layout store, and is advanced by the `panel` event — NOT by the
     * `files` event, so that the store's echo of our own write is
     * recognised as ours (see `pendingApply`).
     */
    expanded: boolean;
    /**
     * What the panel should look like when the last file closes: the state
     * it had before the file pane took its width, or the reader's own later
     * choice.
     */
    restoreExpanded: boolean;
    /** Whether THIS module folded the panel for the file pane. */
    folded: boolean;
    /**
     * The panel state we are about to write, or `null`. It is what tells
     * our own write apart from the reader's: the store echoes every change
     * back, and without this marker the echo of our own fold would read as
     * "the reader collapsed it by hand" and cancel the restore.
     */
    pendingApply: boolean | null;
}

export type FilePanelAutoEvent =
    | { type: "files"; hasFiles: boolean }
    | { type: "panel"; expanded: boolean };

export interface AutoCollapseDecision {
    state: FilePanelAutoState;
    action: AutoCollapseAction;
}

/** The starting point: no file open, the panel as the layout store has it. */
export function initFilePanelAutoState(expanded: boolean): FilePanelAutoState {
    return {
        hasFiles: false,
        expanded,
        restoreExpanded: expanded,
        folded: false,
        pendingApply: null,
    };
}

/**
 * Advance the state machine by one event and report what the layout must do.
 *
 * Pure: the same state and event always produce the same decision, which is
 * what lets the whole rule be tested without a rendered panel.
 */
export function reduceFilePanelAuto(
    state: FilePanelAutoState,
    event: FilePanelAutoEvent,
): AutoCollapseDecision {
    if (event.type === "files") {
        if (event.hasFiles === state.hasFiles) return { state, action: { kind: "none" } };

        if (event.hasFiles) {
            // A file tab appeared. The panel gives up its width — but only
            // if it was actually on screen to give.
            if (state.expanded) {
                return {
                    state: {
                        ...state,
                        hasFiles: true,
                        restoreExpanded: true,
                        folded: true,
                        pendingApply: false,
                    },
                    action: { kind: "collapse" },
                };
            }
            // The panel was already away: the file pane borrows nothing, so
            // nothing may be handed back later either.
            return {
                state: { ...state, hasFiles: true, restoreExpanded: false, folded: false },
                action: { kind: "none" },
            };
        }

        // The last file closed. Only a fold this module performed is undone.
        if (state.folded) {
            const expanded = state.restoreExpanded;
            return {
                state: {
                    ...state,
                    hasFiles: false,
                    restoreExpanded: expanded,
                    folded: false,
                    pendingApply: expanded ? true : null,
                },
                action: expanded ? { kind: "expand" } : { kind: "none" },
            };
        }
        return { state: { ...state, hasFiles: false }, action: { kind: "none" } };
    }

    // The panel changed state.
    if (event.expanded === state.expanded) return { state, action: { kind: "none" } };

    if (state.pendingApply === event.expanded) {
        // Our own write, echoed back by the store. Accept it and keep the
        // decision the `files` event made.
        return {
            state: { ...state, expanded: event.expanded, pendingApply: null },
            action: { kind: "none" },
        };
    }

    // The reader moved the panel themselves. Whatever they chose is what
    // the file pane must give back — and the fold this module performed is
    // no longer theirs to undo.
    return {
        state: {
            ...state,
            expanded: event.expanded,
            restoreExpanded: event.expanded,
            folded: false,
            pendingApply: null,
        },
        action: { kind: "none" },
    };
}
