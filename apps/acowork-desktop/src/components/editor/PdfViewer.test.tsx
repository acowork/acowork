/**
 * PdfViewer — minimal mount test for the native-iframe PDF path.
 *
 * The WebView's built-in viewer does the actual rendering; what this
 * component owns (and what this file pins) is the chrome contract from the
 * preview plan:
 *   - the blob URL from `useWorkspaceFileBlob(mode:"url")` is handed to an
 *     `<iframe>` as-is (no re-fetch, no data: re-encode);
 *   - loading / error states are the frame's, with a working retry;
 *   - the two escape hatches call the Gateway-route transfer helpers with
 *     the file's own addressing (agentId / workspaceId / relPath).
 *
 * `external` mode (the converted-Office path) is the second half of the
 * file: the blob feed must stay switched off — the tab's own bytes are the
 * SOURCE document, not a PDF — and the iframe has to come from an object
 * URL minted from the supplied bytes and revoked on unmount.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";

const h = vi.hoisted(() => ({
  hookState: {
    blob: null as Blob | null,
    blobUrl: null as string | null,
    mime: null as string | null,
    loading: false,
    error: null as string | null,
    retry: vi.fn(),
  },
  hookCalls: [] as unknown[],
  saveToDisk: vi.fn(() => Promise.resolve(true)),
  openExternal: vi.fn(() => Promise.resolve()),
  toast: vi.fn(),
}));

vi.mock("../../hooks/useWorkspaceFileBlob", () => ({
  useWorkspaceFileBlob: (ref: unknown, opts: unknown) => {
    h.hookCalls.push({ ref, opts });
    return h.hookState;
  },
}));
vi.mock("../../lib/workspaceFileBlob", () => ({
  saveWorkspaceFileToDisk: h.saveToDisk,
  openWorkspaceFileExternally: h.openExternal,
}));
vi.mock("../common/ToastProvider", () => ({ showToast: h.toast }));
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string, params?: Record<string, unknown>) =>
      params ? `${key} ${JSON.stringify(params)}` : key,
  }),
}));

import PdfViewer from "./PdfViewer";

const file = {
  agentId: "a1",
  workspaceId: "ws1",
  relPath: "docs/report.pdf",
  fileName: "report.pdf",
  previewSeq: 2,
};

let objectUrlSeq = 0;
const createObjectURLMock = vi.fn(() => `blob:external/${++objectUrlSeq}`);
const revokeObjectURLMock = vi.fn();

beforeEach(() => {
  h.hookCalls.length = 0;
  h.hookState = {
    blob: null,
    blobUrl: "blob:gw/report.pdf",
    mime: "application/pdf",
    loading: false,
    error: null,
    retry: vi.fn(),
  };
  h.saveToDisk.mockClear();
  h.openExternal.mockClear();
  h.toast.mockClear();
  objectUrlSeq = 0;
  createObjectURLMock.mockClear();
  revokeObjectURLMock.mockClear();
  // jsdom implements neither — same stub as workspaceFileBlob.test.ts.
  Object.defineProperty(URL, "createObjectURL", { value: createObjectURLMock, configurable: true });
  Object.defineProperty(URL, "revokeObjectURL", { value: revokeObjectURLMock, configurable: true });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("PdfViewer", () => {
  it("requests url-mode bytes for its own file and embeds the blob URL", () => {
    render(<PdfViewer {...file} />);
    expect(h.hookCalls[0]).toEqual({
      ref: { agentId: "a1", workspaceId: "ws1", relPath: "docs/report.pdf" },
      opts: { mode: "url", reloadSeq: 2, enabled: true },
    });
    const frame = document.querySelector("iframe");
    expect(frame).toBeTruthy();
    expect(frame?.getAttribute("src")).toBe("blob:gw/report.pdf");
  });

  it("shows the frame spinner while the feed is loading, without an iframe", () => {
    h.hookState.loading = true;
    h.hookState.blobUrl = null;
    h.hookState.mime = null;
    render(<PdfViewer {...file} />);
    expect(document.querySelector("iframe")).toBeNull();
    expect(screen.getByText("fileEditor.previewLoading")).toBeTruthy();
  });

  it("offers a working retry when the fetch failed", () => {
    h.hookState.blobUrl = null;
    h.hookState.error = "Error: workspace file → 500";
    render(<PdfViewer {...file} />);
    expect(screen.getByText("fileEditor.docViewer.loadFailed")).toBeTruthy();
    fireEvent.click(screen.getByText("fileEditor.docViewer.retry"));
    expect(h.hookState.retry).toHaveBeenCalled();
  });

  it("hands save-as and open-externally the Gateway-addressed file", async () => {
    render(<PdfViewer {...file} />);
    fireEvent.click(screen.getByLabelText("fileEditor.docViewer.saveAs"));
    await vi.waitFor(() =>
      expect(h.saveToDisk).toHaveBeenCalledWith("a1", "ws1", "docs/report.pdf", "report.pdf"),
    );
    fireEvent.click(screen.getByLabelText("fileEditor.docViewer.openExternally"));
    await vi.waitFor(() =>
      expect(h.openExternal).toHaveBeenCalledWith("a1", "ws1", "docs/report.pdf", "report.pdf"),
    );
  });
});

describe("PdfViewer external mode (converted Office document)", () => {
  const converted = { ...file, relPath: "docs/paper.docx", fileName: "paper.docx" };

  it("switches the blob feed off and embeds an object URL of the given bytes", async () => {
    const bytes = new Uint8Array([0x25, 0x50, 0x44, 0x46, 1]);
    const { unmount } = render(
      <PdfViewer {...converted} external={{ state: "ready", bytes, retry: vi.fn() }} />,
    );
    expect(h.hookCalls[0]).toEqual({
      ref: { agentId: "a1", workspaceId: "ws1", relPath: "docs/paper.docx" },
      // The tab's raw bytes are a .docx — fetching them for a PDF frame
      // would download the source document all over again.
      opts: { mode: "url", reloadSeq: 2, enabled: false },
    });
    await waitFor(() =>
      expect(document.querySelector("iframe")?.getAttribute("src")).toBe("blob:external/1"),
    );
    expect(createObjectURLMock).toHaveBeenCalledTimes(1);
    const blob = createObjectURLMock.mock.calls[0][0] as Blob;
    expect(blob.type).toBe("application/pdf");
    // The frame's own feed state is irrelevant in this mode.
    expect(h.hookState.blobUrl).toBe("blob:gw/report.pdf");
    unmount();
    expect(revokeObjectURLMock).toHaveBeenCalledWith("blob:external/1");
  });

  it("shows the loading state without an iframe while converting", () => {
    render(
      <PdfViewer {...converted} external={{ state: "loading", retry: vi.fn() }} />,
    );
    expect(createObjectURLMock).not.toHaveBeenCalled();
    expect(document.querySelector("iframe")).toBeNull();
    expect(screen.getByText("fileEditor.previewLoading")).toBeTruthy();
  });

  it("shows the frame error and routes retry to the converter", () => {
    const retry = vi.fn();
    render(
      <PdfViewer
        {...converted}
        external={{ state: "error", error: "soffice exited with code 1", retry }}
      />,
    );
    expect(screen.getByText("fileEditor.docViewer.loadFailed")).toBeTruthy();
    fireEvent.click(screen.getByText("fileEditor.docViewer.retry"));
    expect(retry).toHaveBeenCalled();
    // The feed hook's own retry must not be used — it has no feed.
    expect(h.hookState.retry).not.toHaveBeenCalled();
  });

  it("renders the forwarded banner above the frame body", async () => {
    render(
      <PdfViewer
        {...converted}
        banner={<div>print-layout note</div>}
        external={{ state: "ready", bytes: new Uint8Array([1]), retry: vi.fn() }}
      />,
    );
    expect(screen.getByText("print-layout note")).toBeTruthy();
  });
});
