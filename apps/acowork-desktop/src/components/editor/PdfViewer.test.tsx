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
 *
 * The third half is when the two "add to Chat" entries appear, and what the
 * whole-document one hands the panel. The crop mode itself is not exercised
 * here: it mounts `PdfRegionPicker` (lazy) and its gestures are that
 * component's own test file.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, fireEvent, waitFor, act } from "@testing-library/react";

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

describe("PdfViewer chat entries", () => {
  // The two hand-offs the preview exists to enable. What is pinned is WHEN
  // they appear: a preview that has no PDF behind it cannot promise the agent
  // a look at a print layout, and a panel that wired no callback has nowhere
  // for the gesture to go.
  const onAddRegionToChat = vi.fn();
  const onAddDocumentToChat = vi.fn();
  const convertedTab = { ...file, relPath: "docs/paper.docx", fileName: "paper.docx" };

  beforeEach(() => {
    onAddRegionToChat.mockClear();
    onAddDocumentToChat.mockClear();
  });

  it("offers both entries once the bytes are in hand", () => {
    render(
      <PdfViewer
        {...file}
        addToChatDisabledTitle={null}
        onAddRegionToChat={onAddRegionToChat}
        onAddDocumentToChat={onAddDocumentToChat}
      />,
    );
    expect(screen.getByRole("button", { name: "fileEditor.pdfViewer.selectRegion" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "fileEditor.pdfViewer.addDocument" })).toBeTruthy();
  });

  it("offers nothing while the document has no PDF behind it", () => {
    // The converted-Office tab on a machine without an engine, or one whose
    // conversion failed: `external.state` is not "ready", so there is no
    // layout to send. The frame's error body is what the user sees instead.
    render(
      <PdfViewer
        {...convertedTab}
        external={{ state: "error", error: "soffice exited with code 1", retry: vi.fn() }}
        onAddRegionToChat={onAddRegionToChat}
        onAddDocumentToChat={onAddDocumentToChat}
      />,
    );
    expect(screen.queryByRole("button", { name: "fileEditor.pdfViewer.selectRegion" })).toBeNull();
    expect(screen.queryByRole("button", { name: "fileEditor.pdfViewer.addDocument" })).toBeNull();
  });

  it("offers nothing when the panel wired no hand-off", () => {
    render(<PdfViewer {...file} />);
    expect(screen.queryByRole("button", { name: "fileEditor.pdfViewer.selectRegion" })).toBeNull();
    expect(screen.queryByRole("button", { name: "fileEditor.pdfViewer.addDocument" })).toBeNull();
  });

  it("sends the converted document as a .pdf named after the source", async () => {
    // `paper.docx` is the tab; what the agent must receive is the PDF the
    // converter produced, and it is the NAME that tells the runtime which of
    // its two attachment paths to take.
    const bytes = new Uint8Array([0x25, 0x50, 0x44, 0x46, 1]);
    render(
      <PdfViewer
        {...convertedTab}
        external={{ state: "ready", bytes, retry: vi.fn() }}
        onAddDocumentToChat={onAddDocumentToChat}
      />,
    );
    // Reading the bytes is async (`arrayBuffer()` on a Blob), and the handler
    // settles `sending` back to false when it lands — so the gesture and its
    // continuation are awaited inside one `act`.
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "fileEditor.pdfViewer.addDocument" }));
      await vi.waitFor(() => expect(onAddDocumentToChat).toHaveBeenCalledTimes(1));
    });
    const [sent] = onAddDocumentToChat.mock.calls[0];
    expect(sent).toBeInstanceOf(File);
    expect(sent.name).toBe("paper.pdf");
    expect(sent.type).toBe("application/pdf");
    expect(new Uint8Array(await sent.arrayBuffer())).toEqual(bytes);
  });

  it("sends a native PDF under its own name, read from the blob feed", async () => {
    h.hookState.blob = new Blob(["%PDF-1.7"], { type: "application/pdf" });
    h.hookState.blobUrl = null;
    render(<PdfViewer {...file} onAddDocumentToChat={onAddDocumentToChat} />);
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "fileEditor.pdfViewer.addDocument" }));
      await vi.waitFor(() => expect(onAddDocumentToChat).toHaveBeenCalledTimes(1));
    });
    const [sent] = onAddDocumentToChat.mock.calls[0];
    expect(sent.name).toBe("report.pdf");
    expect(sent.type).toBe("application/pdf");
  });

  it("reports unreadable bytes instead of attaching an empty document", async () => {
    h.hookState.blob = new Blob([""], { type: "application/pdf" });
    h.hookState.blobUrl = null;
    render(<PdfViewer {...file} onAddDocumentToChat={onAddDocumentToChat} />);
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "fileEditor.pdfViewer.addDocument" }));
      await vi.waitFor(() => expect(h.toast).toHaveBeenCalledTimes(1));
    });
    expect(onAddDocumentToChat).not.toHaveBeenCalled();
  });
});
