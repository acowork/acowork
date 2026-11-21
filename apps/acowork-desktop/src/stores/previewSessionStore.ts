import { create } from "zustand";
import type { PreviewState } from "../lib/previewHost";

/**
 * Frontend projection of the native preview sessions.
 *
 * Why a store instead of component state: the session lives in Rust and its
 * changes arrive as unsolicited `preview://state` events. The *chrome* (toolbar
 * in `UrlPreviewView`) and the *surface owner* (`WebPreviewHost`, which does
 * lifecycle + geometry) are different components, and the session must survive
 * one of them unmounting on a tab switch — otherwise every tab switch would
 * reload the page and lose scroll position. Two keys bridge that:
 *
 *   - `sessionKey` — the owning editor tab (`OpenFile.id`). Stable across
 *     mounts, so a remount finds its session again.
 *   - `label` — the webview label the Rust registry allocated (`preview-N`).
 *
 * `sourceUrls` remembers the `url` prop each session was synced from, so a
 * remount does not yank the user back to the prop URL after they navigated
 * inside the preview: the prop is only re-applied when it actually changed.
 */

interface PreviewSessionStore {
    /** sessionKey → webview label */
    labels: Record<string, string>;
    /** label → latest state pushed by the backend */
    states: Record<string, PreviewState>;
    /** sessionKey → last `url` prop that was pushed to the session */
    sourceUrls: Record<string, string>;

    /** Bind a freshly created session to its owning key. */
    attach: (sessionKey: string, state: PreviewState) => void;
    /** Drop the binding for a closed tab (idempotent). */
    detach: (sessionKey: string) => void;
    /** Apply an event payload; unknown labels are ignored (evicted sessions). */
    applyState: (state: PreviewState) => void;
    /** Remember that `sessionKey` now reflects `url`. */
    setSourceUrl: (sessionKey: string, url: string) => void;
    /** Test/teardown hook. */
    reset: () => void;
}

const EMPTY = {
    labels: {} as Record<string, string>,
    states: {} as Record<string, PreviewState>,
    sourceUrls: {} as Record<string, string>,
};

export const usePreviewSessionStore = create<PreviewSessionStore>((set) => ({
    ...EMPTY,

    attach: (sessionKey, state) =>
        set((prev) => ({
            labels: { ...prev.labels, [sessionKey]: state.label },
            states: { ...prev.states, [state.label]: state },
        })),

    detach: (sessionKey) =>
        set((prev) => {
            const label = prev.labels[sessionKey];
            if (!label) return prev;
            const labels = { ...prev.labels };
            const states = { ...prev.states };
            const sourceUrls = { ...prev.sourceUrls };
            delete labels[sessionKey];
            delete sourceUrls[sessionKey];
            delete states[label];
            return { labels, states, sourceUrls };
        }),

    applyState: (state) =>
        set((prev) => {
            // Only track labels this store knows about: an evicted session keeps
            // emitting until its webview is gone, and resurrecting it would
            // leave a ghost entry the next mount reuses.
            if (!prev.states[state.label]) return prev;
            return { states: { ...prev.states, [state.label]: state } };
        }),

    setSourceUrl: (sessionKey, url) =>
        set((prev) => ({ sourceUrls: { ...prev.sourceUrls, [sessionKey]: url } })),

    reset: () => set({ ...EMPTY }),
}));

/** Label bound to `sessionKey`, or `undefined` when no session is live. */
export function labelFor(sessionKey: string): string | undefined {
    return usePreviewSessionStore.getState().labels[sessionKey];
}

/** Latest state for `sessionKey`, or `undefined` while it is being created. */
export function stateFor(sessionKey: string): PreviewState | undefined {
    const { labels, states } = usePreviewSessionStore.getState();
    const label = labels[sessionKey];
    return label ? states[label] : undefined;
}