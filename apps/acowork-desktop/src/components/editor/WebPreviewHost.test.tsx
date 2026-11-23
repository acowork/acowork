/**
 * `WebPreviewHost` — lifecycle and geometry of one native preview webview.
 *
 * This component owns the two facts the rest of the UI must not re-derive
 * (ADR-118): a session is created **once per tab** and then parked, never
 * destroyed on unmount, and the native surface is visible only when its tab is
 * active with a real rectangle on screen.
 *
 * What is asserted, and why each line protects a user-visible behaviour:
 *
 *   - bounds come from the placeholder's DOM rect, snapped to integers;
 *   - a tab switch (unmount → remount with the same session key) re-attaches to
 *     the live page instead of reloading it (`openPreview` is called once);
 *   - an inactive or removed tab must not leave a native surface floating above
 *     the UI, so visibility is pushed `false` in both cases;
 *   - an evicted session (`unknown_preview`) is recreated rather than leaving a
 *     permanently blank tab;
 *   - creation failures surface through `onError` with a typed code.
 */

import { describe, it, expect, vi, beforeEach, afterEach, type MockInstance } from "vitest";
import { render, waitFor } from "@testing-library/react";
import { WebPreviewHost } from "./WebPreviewHost";
import { usePreviewSessionStore } from "../../stores/previewSessionStore";

const h = vi.hoisted(() => ({
    openPreview: vi.fn(),
    setPreviewBounds: vi.fn(),
    setPreviewVisible: vi.fn(),
    navigatePreview: vi.fn(),
    closePreview: vi.fn(),
}));

vi.mock("../../lib/previewHost", async (importOriginal) => {
    const actual = await importOriginal<typeof import("../../lib/previewHost")>();
    return {
        ...actual,
        openPreview: h.openPreview,
        setPreviewBounds: h.setPreviewBounds,
        setPreviewVisible: h.setPreviewVisible,
        navigatePreview: h.navigatePreview,
        closePreview: h.closePreview,
    };
});

const URL_UNDER_TEST = "https://github.com/acowork/ACowork";

/** Only the rect spy is restored between tests: clearing the hoisted `vi.fn`
 *  implementations would break the passive-effect cleanups React flushes after
 *  the test body returns. */
let rectSpy: MockInstance;

function sessionState(label: string) {
    return {
        label,
        url: URL_UNDER_TEST,
        title: null,
        phase: "loading" as const,
        visible: true,
    };
}

describe("WebPreviewHost", () => {
    beforeEach(() => {
        usePreviewSessionStore.getState().reset();
        h.openPreview.mockReset().mockResolvedValue(sessionState("preview-1"));
        h.setPreviewBounds.mockReset().mockResolvedValue(undefined);
        h.setPreviewVisible.mockReset().mockResolvedValue(undefined);
        h.navigatePreview.mockReset().mockResolvedValue(undefined);
        h.closePreview.mockReset().mockResolvedValue(undefined);
        rectSpy = vi.spyOn(Element.prototype, "getBoundingClientRect").mockReturnValue({
            x: 10,
            y: 20,
            left: 10,
            top: 20,
            width: 300,
            height: 200,
            right: 310,
            bottom: 220,
            toJSON: () => ({}),
        } as DOMRect);
    });

    afterEach(() => {
        rectSpy.mockRestore();
    });

    it("creates the session at the measured rect and shows it", async () => {
        render(<WebPreviewHost sessionKey="tab-1" url={URL_UNDER_TEST} active />);

        await waitFor(() => expect(h.openPreview).toHaveBeenCalledTimes(1));
        expect(h.openPreview).toHaveBeenCalledWith(URL_UNDER_TEST, {
            x: 10,
            y: 20,
            width: 300,
            height: 200,
        });

        await waitFor(() =>
            expect(h.setPreviewVisible).toHaveBeenCalledWith("preview-1", true),
        );
        expect(usePreviewSessionStore.getState().labels["tab-1"]).toBe("preview-1");
    });

    it("re-attaches on remount instead of reloading the page", async () => {
        const first = render(<WebPreviewHost sessionKey="tab-1" url={URL_UNDER_TEST} active />);
        await waitFor(() => expect(h.openPreview).toHaveBeenCalledTimes(1));
        first.unmount();

        render(<WebPreviewHost sessionKey="tab-1" url={URL_UNDER_TEST} active />);

        await waitFor(() =>
            expect(h.setPreviewVisible).toHaveBeenCalledWith("preview-1", true),
        );
        expect(h.openPreview).toHaveBeenCalledTimes(1);
        expect(h.navigatePreview).not.toHaveBeenCalled();
    });

    it("parks the webview on unmount instead of destroying the session", async () => {
        const view = render(<WebPreviewHost sessionKey="tab-1" url={URL_UNDER_TEST} active />);
        await waitFor(() => expect(h.setPreviewVisible).toHaveBeenCalledWith("preview-1", true));

        view.unmount();

        expect(h.setPreviewVisible).toHaveBeenLastCalledWith("preview-1", false);
        expect(h.closePreview).not.toHaveBeenCalled();
        expect(usePreviewSessionStore.getState().labels["tab-1"]).toBe("preview-1");
    });

    it("hides the surface while its tab is inactive", async () => {
        render(<WebPreviewHost sessionKey="tab-1" url={URL_UNDER_TEST} active={false} />);

        await waitFor(() =>
            expect(h.setPreviewVisible).toHaveBeenCalledWith("preview-1", false),
        );
        expect(h.setPreviewVisible).not.toHaveBeenCalledWith("preview-1", true);
    });

    it("navigates an existing session when the url prop changes", async () => {
        usePreviewSessionStore.getState().attach("tab-1", sessionState("preview-1"));
        usePreviewSessionStore.getState().setSourceUrl("tab-1", "https://old.example/");

        render(<WebPreviewHost sessionKey="tab-1" url="https://new.example/" active />);

        await waitFor(() =>
            expect(h.navigatePreview).toHaveBeenCalledWith("preview-1", "https://new.example/"),
        );
        expect(h.openPreview).not.toHaveBeenCalled();
    });

    it("opens one webview when two mounts of the same tab race to create it", async () => {
        // The real-world shape of this: `StrictMode` mounts every effect twice,
        // and both mounts see an empty store because the first create has not
        // resolved yet. Two native creates is what WebView2 refuses
        // (`ERROR_INVALID_STATE`), and each refusal used to leave a stray window
        // over the panel.
        let resolveOpen!: (state: ReturnType<typeof sessionState>) => void;
        h.openPreview.mockImplementation(
            () =>
                new Promise((resolve) => {
                    resolveOpen = resolve;
                }),
        );

        render(<WebPreviewHost sessionKey="tab-1" url={URL_UNDER_TEST} active />);
        render(<WebPreviewHost sessionKey="tab-1" url={URL_UNDER_TEST} active />);

        await waitFor(() => expect(h.openPreview).toHaveBeenCalledTimes(1));
        resolveOpen(sessionState("preview-1"));

        await waitFor(() =>
            expect(h.setPreviewVisible).toHaveBeenCalledWith("preview-1", true),
        );
        expect(h.openPreview).toHaveBeenCalledTimes(1);
        expect(usePreviewSessionStore.getState().labels["tab-1"]).toBe("preview-1");
    });

    it("destroys the session when this mount dies mid-create and nobody claims it", async () => {
        // A session whose owner vanished mid-create has no address any more:
        // leaving it alive strands a native surface (hidden, but forever) and
        // eats a MAX_LIVE_SESSIONS slot.
        let resolveOpen!: (state: ReturnType<typeof sessionState>) => void;
        h.openPreview.mockImplementation(
            () =>
                new Promise((resolve) => {
                    resolveOpen = resolve;
                }),
        );

        const view = render(<WebPreviewHost sessionKey="tab-1" url={URL_UNDER_TEST} active />);
        await waitFor(() => expect(h.openPreview).toHaveBeenCalledTimes(1));

        view.unmount();
        resolveOpen(sessionState("preview-1"));

        await waitFor(() =>
            expect(h.closePreview).toHaveBeenCalledWith("preview-1"),
        );
        expect(usePreviewSessionStore.getState().labels["tab-1"]).toBeUndefined();
    });

    it("leaves the session alone when another host for the key is still mounted", async () => {
        // StrictMode shape: mount A starts the create, unmounts, mount B is
        // already awaiting the same in-flight promise. A must not close what B
        // is about to adopt.
        let resolveOpen!: (state: ReturnType<typeof sessionState>) => void;
        h.openPreview.mockImplementation(
            () =>
                new Promise((resolve) => {
                    resolveOpen = resolve;
                }),
        );

        const first = render(<WebPreviewHost sessionKey="tab-1" url={URL_UNDER_TEST} active />);
        await waitFor(() => expect(h.openPreview).toHaveBeenCalledTimes(1));
        const second = render(<WebPreviewHost sessionKey="tab-1" url={URL_UNDER_TEST} active />);

        first.unmount();
        resolveOpen(sessionState("preview-1"));

        await waitFor(() =>
            expect(usePreviewSessionStore.getState().labels["tab-1"]).toBe("preview-1"),
        );
        expect(h.closePreview).not.toHaveBeenCalled();
        second.unmount();
    });

    it("reports a typed error when the session cannot be created", async () => {
        h.openPreview.mockRejectedValue("blocked_scheme: ftp://example.com/");
        const onError = vi.fn();

        render(
            <WebPreviewHost sessionKey="tab-1" url={URL_UNDER_TEST} active onError={onError} />,
        );

        await waitFor(() =>
            expect(onError).toHaveBeenCalledWith({
                code: "blocked_scheme",
                detail: "ftp://example.com/",
            }),
        );
        expect(h.setPreviewVisible).not.toHaveBeenCalled();
    });

    it("recreates a session the backend evicted (unknown_preview)", async () => {
        usePreviewSessionStore.getState().attach("tab-1", sessionState("preview-1"));
        usePreviewSessionStore.getState().setSourceUrl("tab-1", URL_UNDER_TEST);
        // Only the stale label is unknown: its replacement must work normally.
        h.setPreviewBounds.mockImplementation((label: string) =>
            label === "preview-1"
                ? Promise.reject("unknown_preview: preview-1")
                : Promise.resolve(),
        );
        h.openPreview.mockResolvedValue(sessionState("preview-2"));

        render(<WebPreviewHost sessionKey="tab-1" url={URL_UNDER_TEST} active />);

        await waitFor(() => expect(h.openPreview).toHaveBeenCalledTimes(1));
        await waitFor(() =>
            expect(usePreviewSessionStore.getState().labels["tab-1"]).toBe("preview-2"),
        );
        expect(h.setPreviewVisible).toHaveBeenCalledWith("preview-2", true);
    });
});