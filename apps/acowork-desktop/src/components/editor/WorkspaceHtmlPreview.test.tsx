/**
 * `WorkspaceHtmlPreview` — the wiring that makes a workspace `.html` file render
 * like a real web page (ADR-112).
 *
 * The bug being guarded: the previous renderer served the page from a Blob URL
 * with a `<base href>` pointed at the bearer-gated `/workspace-files/…` route.
 * The document painted, but every `<link>` / `<script>` / `@font-face` was
 * loaded by the browser without an `Authorization` header and 401'd, leaving an
 * unstyled skeleton. What matters here is therefore not "does it render an
 * iframe" but:
 *
 *   1. the surface is the shared address-bar browser (`UrlPreviewView`) and it
 *      receives the **ticketed HTTP url** — not a blob, not the raw file path;
 *   2. a failed mint shows an actionable error with a retry, and never silently
 *      falls back to the broken blob path;
 *   3. a failure never falls back to the blob path and leaves a retry that
 *      mints a fresh ticket (there is no cache to invalidate: a ticket is minted
 *      per open, so a Gateway restart cannot leave a stale link behind).
 */

import { describe, it, expect, vi, beforeEach } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

const h = vi.hoisted(() => ({
    previewUrls: [] as Array<{ url: string; fileName: string }>,
    getWorkspacePreviewUrl: vi.fn(),
}));

vi.mock("./UrlPreviewView", () => ({
    UrlPreviewView: (props: { url: string; fileName: string }) => {
        h.previewUrls.push({ url: props.url, fileName: props.fileName });
        return <div data-testid="url-preview">{props.url}</div>;
    },
}));

vi.mock("../../lib/workspacePreviewUrl", () => ({
    getWorkspacePreviewUrl: (...args: unknown[]) => h.getWorkspacePreviewUrl(...(args as [])),
}));

vi.mock("../../lib/logger", () => ({
    log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
}));

vi.mock("../../i18n/useTranslation", () => ({
    useTranslation: () => ({
        t: (key: string) => key,
    }),
}));

import { WorkspaceHtmlPreview } from "./WorkspaceHtmlPreview";

const TICKETED_URL =
    "http://gateway:19876/workspace-preview/v1.1000.deadbeef/agent-1/ws-1/pages/index.html";

function renderPreview() {
    return render(
        <WorkspaceHtmlPreview
            agentId="agent-1"
            workspaceId="ws-1"
            relPath="pages/index.html"
            fileName="index.html"
        />,
    );
}

beforeEach(() => {
    h.previewUrls.length = 0;
    h.getWorkspacePreviewUrl.mockReset();
});

describe("WorkspaceHtmlPreview", () => {
    it("hands the ticketed http url to the shared address-bar browser", async () => {
        h.getWorkspacePreviewUrl.mockResolvedValue(TICKETED_URL);
        renderPreview();

        await screen.findByTestId("url-preview");

        expect(h.getWorkspacePreviewUrl).toHaveBeenCalledWith("agent-1", "ws-1", "pages/index.html");
        expect(h.previewUrls).toEqual([{ url: TICKETED_URL, fileName: "index.html" }]);
        // The whole point: a real http(s) document, so the browser resolves the
        // page's CSS / JS / font sub-resources itself.
        expect(h.previewUrls[0].url.startsWith("http://")).toBe(true);
        expect(h.previewUrls[0].url).not.toContain("blob:");
        expect(h.previewUrls[0].url).not.toContain("workspace-files");
    });

    it("shows a loading state before the ticket arrives", () => {
        h.getWorkspacePreviewUrl.mockReturnValue(new Promise(() => {}));
        renderPreview();
        expect(screen.getByText("fileEditor.loadingUrl")).toBeTruthy();
        expect(h.previewUrls).toHaveLength(0);
    });

    it("surfaces a failed mint with an error card instead of falling back", async () => {
        h.getWorkspacePreviewUrl.mockRejectedValue(new Error("preview ticket request failed: HTTP 403"));
        renderPreview();

        await screen.findByText("fileEditor.htmlPreviewFailed");
        expect(screen.getByText(/HTTP 403/)).toBeTruthy();
        // No iframe, no silent fallback to the blob path that caused the bug.
        expect(h.previewUrls).toHaveLength(0);
    });

    it("retry mints again and then renders the browser", async () => {
        h.getWorkspacePreviewUrl
            .mockRejectedValueOnce(new Error("preview ticket request failed: HTTP 403"))
            .mockResolvedValueOnce(TICKETED_URL);
        renderPreview();

        const retry = await screen.findByText("fileEditor.htmlPreviewRetry");
        fireEvent.click(retry);

        await screen.findByTestId("url-preview");
        expect(h.getWorkspacePreviewUrl).toHaveBeenCalledTimes(2);
        expect(h.previewUrls).toEqual([{ url: TICKETED_URL, fileName: "index.html" }]);
    });

    it("does not render a stale url when the file changes mid-mint", async () => {
        // Opening a second HTML tab while the first is still minting must not
        // let the first response overwrite the second file's preview.
        let resolveFirst: ((url: string) => void) | undefined;
        h.getWorkspacePreviewUrl
            .mockReturnValueOnce(new Promise<string>((resolve) => { resolveFirst = resolve; }))
            .mockResolvedValueOnce(TICKETED_URL);

        const { rerender } = renderPreview();
        rerender(
            <WorkspaceHtmlPreview
                agentId="agent-1"
                workspaceId="ws-1"
                relPath="other.html"
                fileName="other.html"
            />,
        );
        await waitFor(() => expect(h.previewUrls).toHaveLength(1));
        expect(h.previewUrls[0].url).toBe(TICKETED_URL);

        // The abandoned first mint resolves late; it must be ignored.
        resolveFirst?.("http://gateway:19876/workspace-preview/stale/a/w/pages/index.html");
        await Promise.resolve();
        expect(h.previewUrls).toHaveLength(1);
        expect(h.previewUrls[0].url).toBe(TICKETED_URL);
    });
});
