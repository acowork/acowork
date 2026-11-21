/**
 * `UrlPreviewView` — the chrome around the native preview webview (ADR-118).
 *
 * The surface itself is a native child webview that HTML cannot paint over, so
 * everything the user can *do* has to live in this toolbar. These tests pin
 * that contract:
 *
 *   1. back / forward / refresh drive the session the store bound to this tab —
 *      not a DOM iframe (there is none any more);
 *   2. the address bar normalises bare hosts to `https://` and navigates the
 *      existing session;
 *   3. "open in browser" leaves through `openExternal` (the app's single
 *      outbound-navigation path);
 *   4. a typed backend failure replaces the surface with an actionable card and
 *      a retry that rebuilds the host, instead of a blank rectangle.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { UrlPreviewView } from "./UrlPreviewView";
import { usePreviewSessionStore } from "../../stores/previewSessionStore";
import type { PreviewError, PreviewState } from "../../lib/previewHost";

const h = vi.hoisted(() => ({
    previewHistory: vi.fn(),
    reloadPreview: vi.fn(),
    navigatePreview: vi.fn(),
    openExternal: vi.fn(),
    hosts: [] as Array<{ sessionKey: string; url: string; active: boolean; onError?: (e: PreviewError) => void }>,
}));

vi.mock("../../lib/previewHost", async (importOriginal) => {
    const actual = await importOriginal<typeof import("../../lib/previewHost")>();
    return {
        ...actual,
        previewHistory: h.previewHistory,
        reloadPreview: h.reloadPreview,
        navigatePreview: h.navigatePreview,
    };
});

vi.mock("../../lib/openExternal", () => ({ openExternal: h.openExternal }));

vi.mock("./WebPreviewHost", () => ({
    WebPreviewHost: (props: {
        sessionKey: string;
        url: string;
        active: boolean;
        onError?: (e: PreviewError) => void;
    }) => {
        h.hosts.push(props);
        return <div data-testid="preview-host" data-session={props.sessionKey} />;
    },
}));

vi.mock("../../i18n/useTranslation", () => ({
    useTranslation: () => ({ t: (key: string) => key }),
}));

const TAB_ID = "agent-1:url:https://github.com/acowork/ACowork";
const TAB_URL = "https://github.com/acowork/ACowork";

function session(label: string, overrides: Partial<PreviewState> = {}): PreviewState {
    return {
        label,
        url: TAB_URL,
        title: "ACowork",
        phase: "loaded",
        visible: true,
        ...overrides,
    };
}

describe("UrlPreviewView", () => {
    beforeEach(() => {
        usePreviewSessionStore.getState().reset();
        h.previewHistory.mockReset().mockResolvedValue(undefined);
        h.reloadPreview.mockReset().mockResolvedValue(undefined);
        h.navigatePreview.mockReset().mockResolvedValue(undefined);
        h.openExternal.mockReset().mockResolvedValue(undefined);
        h.hosts.length = 0;
    });

    it("hands the tab identity to the host so the session survives tab switches", () => {
        render(<UrlPreviewView url={TAB_URL} fileName="GitHub" sessionKey={TAB_ID} />);

        expect(h.hosts).toHaveLength(1);
        expect(h.hosts[0]).toMatchObject({ sessionKey: TAB_ID, url: TAB_URL, active: true });
    });

    it("drives the bound session's history and reload", () => {
        usePreviewSessionStore.getState().attach(TAB_ID, session("preview-1"));
        render(<UrlPreviewView url={TAB_URL} fileName="GitHub" sessionKey={TAB_ID} />);

        fireEvent.click(screen.getByTitle("fileEditor.navBack"));
        fireEvent.click(screen.getByTitle("fileEditor.navForward"));
        fireEvent.click(screen.getByTitle("fileEditor.navRefresh"));

        expect(h.previewHistory).toHaveBeenCalledWith("preview-1", "back");
        expect(h.previewHistory).toHaveBeenCalledWith("preview-1", "forward");
        expect(h.reloadPreview).toHaveBeenCalledWith("preview-1");
    });

    it("normalises a bare host typed in the address bar and navigates the session", () => {
        usePreviewSessionStore.getState().attach(TAB_ID, session("preview-1"));
        render(<UrlPreviewView url={TAB_URL} fileName="GitHub" sessionKey={TAB_ID} />);

        const input = screen.getByLabelText("fileEditor.previewAddressLabel");
        fireEvent.focus(input);
        fireEvent.change(input, { target: { value: "example.com/docs" } });
        fireEvent.keyDown(input, { key: "Enter" });

        expect(h.navigatePreview).toHaveBeenCalledWith("preview-1", "https://example.com/docs");
    });

    it("leaves the app through openExternal", () => {
        usePreviewSessionStore.getState().attach(TAB_ID, session("preview-1"));
        render(<UrlPreviewView url={TAB_URL} fileName="GitHub" sessionKey={TAB_ID} />);

        fireEvent.click(screen.getByTitle("fileEditor.openInBrowser"));

        expect(h.openExternal).toHaveBeenCalledWith(TAB_URL);
    });

    it("replaces the surface with an actionable card when the surface cannot start", () => {
        render(<UrlPreviewView url={TAB_URL} fileName="GitHub" sessionKey={TAB_ID} />);

        act(() => {
            h.hosts[0].onError?.({ code: "blocked_scheme", detail: "file:///etc/passwd" });
        });

        expect(screen.getByText("fileEditor.previewErrorTitle")).toBeTruthy();
        expect(screen.getByText("fileEditor.previewErrorBlockedScheme")).toBeTruthy();
        expect(screen.queryByTestId("preview-host")).toBeNull();
    });

    it("retry rebuilds the host so a failed surface gets a second chance", () => {
        render(<UrlPreviewView url={TAB_URL} fileName="GitHub" sessionKey={TAB_ID} />);
        act(() => {
            h.hosts[0].onError?.({ code: "create_failed", detail: "boom" });
        });

        fireEvent.click(screen.getByText("fileEditor.previewRetry"));

        expect(screen.getByTestId("preview-host")).toBeTruthy();
        expect(h.hosts).toHaveLength(2);
    });

    it("shows the loaded url from the session, not the stale prop", () => {
        usePreviewSessionStore
            .getState()
            .attach(TAB_ID, session("preview-1", { url: "https://github.com/acowork" }));
        render(<UrlPreviewView url={TAB_URL} fileName="GitHub" sessionKey={TAB_ID} />);

        expect(screen.getByLabelText("fileEditor.previewAddressLabel")).toHaveProperty(
            "value",
            "https://github.com/acowork",
        );
    });
});