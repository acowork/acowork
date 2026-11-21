/**
 * `previewSessionStore` — the frontend projection of native preview sessions.
 *
 * The behaviour that matters is the one a user feels: a tab switch must not
 * reload the page. That only works if the session binding outlives the
 * component, so the tests below are about identity and lifetime rather than
 * about rendering:
 *
 *   1. `attach` binds a backend label to an owning tab key;
 *   2. `applyState` updates a known session and ignores an unknown one (an
 *      evicted webview keeps emitting until it is gone — resurrecting it would
 *      leave a ghost entry the next mount would try to reuse);
 *   3. `detach` removes both directions of the mapping;
 *   4. `sourceUrls` remembers the prop each session was synced from.
 */

import { describe, it, expect, beforeEach } from "vitest";
import { usePreviewSessionStore, labelFor, stateFor } from "./previewSessionStore";
import type { PreviewState } from "../lib/previewHost";

function state(label: string, overrides: Partial<PreviewState> = {}): PreviewState {
    return {
        label,
        url: "https://example.com/",
        title: null,
        phase: "loaded",
        visible: true,
        ...overrides,
    };
}

describe("previewSessionStore", () => {
    beforeEach(() => {
        usePreviewSessionStore.getState().reset();
    });

    it("binds a label to the owning tab and exposes its state", () => {
        usePreviewSessionStore.getState().attach("tab-1", state("preview-1"));

        expect(labelFor("tab-1")).toBe("preview-1");
        expect(stateFor("tab-1")?.phase).toBe("loaded");
        expect(stateFor("tab-2")).toBeUndefined();
    });

    it("applies state events for a known session", () => {
        const store = usePreviewSessionStore.getState();
        store.attach("tab-1", state("preview-1", { phase: "loading" }));

        usePreviewSessionStore
            .getState()
            .applyState(state("preview-1", { phase: "loaded", title: "Example" }));

        expect(stateFor("tab-1")).toMatchObject({ phase: "loaded", title: "Example" });
    });

    it("ignores events for a session it does not know (evicted webview)", () => {
        const store = usePreviewSessionStore.getState();
        store.attach("tab-1", state("preview-1"));
        const before = usePreviewSessionStore.getState().states;

        usePreviewSessionStore.getState().applyState(state("preview-9"));

        expect(usePreviewSessionStore.getState().states).toBe(before);
        expect(usePreviewSessionStore.getState().states["preview-9"]).toBeUndefined();
    });

    it("detach removes both the binding and the payload", () => {
        const store = usePreviewSessionStore.getState();
        store.attach("tab-1", state("preview-1"));
        store.setSourceUrl("tab-1", "https://example.com/");

        usePreviewSessionStore.getState().detach("tab-1");

        const after = usePreviewSessionStore.getState();
        expect(after.labels["tab-1"]).toBeUndefined();
        expect(after.states["preview-1"]).toBeUndefined();
        expect(after.sourceUrls["tab-1"]).toBeUndefined();
    });

    it("detaching an unknown tab is a no-op (idempotent)", () => {
        const before = usePreviewSessionStore.getState().labels;
        usePreviewSessionStore.getState().detach("never-opened");
        expect(usePreviewSessionStore.getState().labels).toBe(before);
    });

    it("remembers which url prop a session was synced from", () => {
        const store = usePreviewSessionStore.getState();
        store.attach("tab-1", state("preview-1"));
        store.setSourceUrl("tab-1", "https://a.example/");

        expect(usePreviewSessionStore.getState().sourceUrls["tab-1"]).toBe("https://a.example/");
    });

    it("keeps two tabs on separate labels", () => {
        const store = usePreviewSessionStore.getState();
        store.attach("tab-1", state("preview-1"));
        store.attach("tab-2", state("preview-2"));

        expect(labelFor("tab-1")).toBe("preview-1");
        expect(labelFor("tab-2")).toBe("preview-2");
    });
});