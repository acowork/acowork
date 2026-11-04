/**
 * PdfViewer — what the viewer owns, which is the BYTES and the gates, not the
 * pages.
 *
 * The column that draws them (`./PdfColumnViewer`) is mocked: its gestures, its
 * lazy painting and its toolbar are that file's own business. What is pinned
 * here is the addressing the frame still owes the Gateway, which bytes reach the
 * column from which source, when the two "add to Chat" entries appear, and that
 * turning framing on is a prop change on the instance the user is already
 * looking at.
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

/** The seam the real column would rather not offer in jsdom (it needs a pdf.js
 *  document): record what the viewer handed it and render a marker instead. */
const column = vi.hoisted(() => ({ props: null as Record<string, unknown> | null }));
vi.mock("./PdfColumnViewer", () => ({
  PdfColumnViewer: (props: Record<string, unknown>) => {
    column.props = props;
    return <div data-testid="pdf-column" />;
  },
}));

import PdfViewer from "./PdfViewer";

/** What the stubbed blob feed serves. */
const PDF_BYTES = new Uint8Array([0x25, 0x50, 0x44, 0x46, 7]);

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
  column.props = null;
  objectUrlSeq = 0;
  createObjectURLMock.mockClear();
  revokeObjectURLMock.mockClear();
  // The url-mode read goes through fetch(); jsdom has no server to ask.
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(PDF_BYTES, { status: 200 }))));
  // jsdom implements neither — same stub as workspaceFileBlob.test.ts.
  Object.defineProperty(URL, "createObjectURL", { value: createObjectURLMock, configurable: true });
  Object.defineProperty(URL, "revokeObjectURL", { value: revokeObjectURLMock, configurable: true });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("PdfViewer", () => {
  it("requests url-mode bytes for its own file and draws them itself", async () => {
    render(<PdfViewer {...file} />);
    expect(h.hookCalls[0]).toEqual({
      ref: { agentId: "a1", workspaceId: "ws1", relPath: "docs/report.pdf" },
      opts: { mode: "url", reloadSeq: 2, enabled: true },
    });
    // The blob URL is not handed to the browser any more. The viewer reads it
    // back and gives the bytes to the column, because drawing the pages is ours
    // now — which is also what makes framing cost nothing.
    await waitFor(() => expect(screen.getByTestId("pdf-column")).toBeTruthy());
    expect(column.props?.bytes).toEqual(PDF_BYTES);
  });

  it("shows the frame spinner while the feed is loading, without a column", () => {
    h.hookState.loading = true;
    h.hookState.blobUrl = null;
    h.hookState.mime = null;
    render(<PdfViewer {...file} />);
    expect(screen.queryByTestId("pdf-column")).toBeNull();
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
    await waitFor(() => expect(screen.getByTestId("pdf-column")).toBeTruthy());
    // The converter's array reaches the column as-is: no second copy, and no
    // fetch of the .docx the tab is actually addressed to.
    expect(column.props?.bytes).toBe(bytes);
    // The frame still needs an object URL, for save-as / open-externally.
    expect(createObjectURLMock).toHaveBeenCalledTimes(1);
    const blob = createObjectURLMock.mock.calls[0][0] as Blob;
    expect(blob.type).toBe("application/pdf");
    // The frame's own feed state is irrelevant in this mode.
    expect(h.hookState.blobUrl).toBe("blob:gw/report.pdf");
    unmount();
    expect(revokeObjectURLMock).toHaveBeenCalledWith("blob:external/1");
  });

  it("shows the loading state without a column while converting", () => {
    render(
      <PdfViewer {...converted} external={{ state: "loading", retry: vi.fn() }} />,
    );
    expect(createObjectURLMock).not.toHaveBeenCalled();
    expect(screen.queryByTestId("pdf-column")).toBeNull();
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

  it("offers both entries once the bytes are in hand", async () => {
    render(
      <PdfViewer
        {...file}
        addToChatDisabledTitle={null}
        onAddRegionToChat={onAddRegionToChat}
        onAddDocumentToChat={onAddDocumentToChat}
      />,
    );
    // The read is async now and the entries are gated on it: no bytes, no
    // promise to the agent.
    await waitFor(() =>
      expect(
        screen.getByRole("button", { name: "fileEditor.pdfViewer.selectRegion" }),
      ).toBeTruthy(),
    );
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
    // The bytes are read up front and asynchronously, so the button only exists
    // once that has landed.
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "fileEditor.pdfViewer.addDocument" })).toBeTruthy(),
    );
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
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "fileEditor.pdfViewer.addDocument" })).toBeTruthy(),
    );
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "fileEditor.pdfViewer.addDocument" }));
      await vi.waitFor(() => expect(onAddDocumentToChat).toHaveBeenCalledTimes(1));
    });
    const [sent] = onAddDocumentToChat.mock.calls[0];
    expect(sent.name).toBe("report.pdf");
    expect(sent.type).toBe("application/pdf");
  });

  it("offers nothing when the bytes cannot be read, and retries on request", async () => {
    // An empty read used to be a button that toasted when clicked. It is now no
    // button at all: the preview needs those same bytes, so a document we cannot
    // read is a document we cannot show — and saying so is the body's job, with
    // a way out that does not need the tab to be reopened.
    h.hookState.blob = new Blob([""], { type: "application/pdf" });
    h.hookState.blobUrl = null;
    render(
      <PdfViewer
        {...file}
        onAddRegionToChat={onAddRegionToChat}
        onAddDocumentToChat={onAddDocumentToChat}
      />,
    );
    await waitFor(() => expect(screen.getByText("fileEditor.docViewer.loadFailed")).toBeTruthy());
    expect(screen.queryByTestId("pdf-column")).toBeNull();
    expect(screen.queryByRole("button", { name: "fileEditor.pdfViewer.addDocument" })).toBeNull();
    expect(screen.queryByRole("button", { name: "fileEditor.pdfViewer.selectRegion" })).toBeNull();
    expect(onAddDocumentToChat).not.toHaveBeenCalled();

    // The retry re-reads the same source. It is not the feed hook's retry,
    // which would refetch a document the frame already holds.
    h.hookState.blob = new Blob([PDF_BYTES], { type: "application/pdf" });
    fireEvent.click(screen.getByText("fileEditor.docViewer.retry"));
    await waitFor(() => expect(screen.getByTestId("pdf-column")).toBeTruthy());
    expect(column.props?.bytes).toEqual(PDF_BYTES);
  });

  it("turns framing on without handing the column a new document", async () => {
    render(<PdfViewer {...file} onAddRegionToChat={onAddRegionToChat} />);
    await waitFor(() => expect(screen.getByTestId("pdf-column")).toBeTruthy());
    const before = column.props?.bytes;
    const toggle = screen.getByRole("button", { name: "fileEditor.pdfViewer.selectRegion" });
    expect(column.props?.framing).toBe(false);
    fireEvent.click(toggle);
    expect(column.props?.framing).toBe(true);
    // A new array would make the column re-open the document and lose the
    // scroll position — which is the whole reason framing is a prop.
    expect(column.props?.bytes).toBe(before);
    fireEvent.click(screen.getByRole("button", { name: "fileEditor.pdfViewer.selectRegion" }));
    expect(column.props?.framing).toBe(false);
    expect(column.props?.bytes).toBe(before);
  });

});
