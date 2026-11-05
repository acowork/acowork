/**
 * fileEditorStore — document-preview tabs skip the base64 JSON read.
 *
 * The preview plan's frontend half stands or falls on this: opening a
 * pdf/docx/xlsx/pptx tab must NOT fetch the `/workspaces/file` JSON
 * endpoint (base64 body through the LLM-visible read path) — the viewer
 * pulls raw bytes from the blob route on its own. What the store still
 * owes the tab: instant `loading:false` and an inferred `mimeType` so
 * FileEditorPanel can branch on it.
 *
 * `refreshFile` closes the other half of the loop: any disk reload must
 * invalidate the blob LRU (otherwise every cached preview goes stale) and
 * bump `previewSeq` so mounted viewers re-fetch.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";

const h = vi.hoisted(() => ({
  invalidate: vi.fn(),
}));

vi.mock("../lib/workspaceFileBlob", () => ({
  invalidateWorkspaceFileBlob: h.invalidate,
  fetchWorkspaceFileBlob: vi.fn(),
  fetchWorkspaceFileBlobUrl: vi.fn(),
  saveWorkspaceFileToDisk: vi.fn(),
  openWorkspaceFileExternally: vi.fn(),
}));
vi.mock("../lib/config", () => ({
  getGatewayUrl: () => "http://gw.test",
  DEFAULT_GATEWAY_URL: "http://gw.test",
}));

import { useFileEditorStore, isDocPreviewFile } from "./fileEditorStore";

const fetchSpy = vi.fn();

beforeEach(() => {
  useFileEditorStore.setState({ openFiles: [], activeFileId: null });
  h.invalidate.mockClear();
  fetchSpy.mockReset();
  vi.stubGlobal("fetch", fetchSpy);
});

function tab(relPath: string) {
  const id = `a1:ws1:${relPath}`;
  return useFileEditorStore.getState().openFiles.find((f) => f.id === id)!;
}

describe("isDocPreviewFile", () => {
  it("covers the no-base64 extensions but not csv", () => {
    for (const n of ["a.PDF", "a.docx", "a.doc", "a.xlsx", "a.xls", "a.pptx", "a.ppt"]) {
      expect(isDocPreviewFile(n)).toBe(true);
    }
    // csv keeps the text read — its content is the editor buffer.
    for (const n of ["a.csv", "a.md", "a.png", "notes.txt"]) {
      expect(isDocPreviewFile(n)).toBe(false);
    }
  });
});

describe("document tabs skip the base64 content load", () => {
  it("openPreview on a pdf produces a ready, contentless tab with an inferred MIME — no fetch", async () => {
    await useFileEditorStore.getState().openPreview("a1", "ws1", "docs/report.pdf");
    expect(fetchSpy).not.toHaveBeenCalled();
    const f = tab("docs/report.pdf");
    expect(f.loading).toBe(false);
    expect(f.content).toBe("");
    expect(f.mimeType).toBe("application/pdf");
  });

  it("openFile on an xlsx (edit intent) takes the same skip — no Monaco path exists", async () => {
    await useFileEditorStore.getState().openFile("a1", "ws1", "docs/budget.xlsx");
    expect(fetchSpy).not.toHaveBeenCalled();
    const f = tab("docs/budget.xlsx");
    expect(f.loading).toBe(false);
    expect(f.mimeType).toBe(
      "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    );
  });

  it("csv still loads text content through the JSON endpoint", async () => {
    fetchSpy.mockResolvedValue({
      ok: true,
      json: async () => ({ content: "a,b\n1,2", size: 8, mimeType: "text/csv" }),
    } as unknown as Response);
    await useFileEditorStore.getState().openFile("a1", "ws1", "docs/data.csv");
    expect(fetchSpy).toHaveBeenCalledTimes(1);
    const f = tab("docs/data.csv");
    expect(f.content).toBe("a,b\n1,2");
  });

  it("unknown document extensions still skip (empty MIME, viewer branches on name)", async () => {
    await useFileEditorStore.getState().openFile("a1", "ws1", "old.doc");
    expect(fetchSpy).not.toHaveBeenCalled();
    expect(tab("old.doc").mimeType).toBe("application/msword");
  });
});

describe("refreshFile invalidates the blob feed", () => {
  it("a doc tab refresh bumps previewSeq without touching the JSON endpoint", async () => {
    await useFileEditorStore.getState().openPreview("a1", "ws1", "docs/report.pdf");
    h.invalidate.mockClear();
    await useFileEditorStore.getState().refreshFile("a1:ws1:docs/report.pdf");
    expect(h.invalidate).toHaveBeenCalledWith("a1", "ws1", "docs/report.pdf");
    expect(fetchSpy).not.toHaveBeenCalled();
    expect(tab("docs/report.pdf").previewSeq).toBe(1);
  });

  it("a text tab refresh ALSO invalidates the blob cache (image previews share it)", async () => {
    await useFileEditorStore.getState().openFile("a1", "ws1", "docs/logo.png");
    // openFile for a non-doc file DID fetch; clear the noise and re-stub.
    fetchSpy.mockClear();
    fetchSpy.mockResolvedValue({
      ok: true,
      json: async () => ({ content: "QUJD", size: 3, mimeType: "image/png" }),
    } as unknown as Response);
    h.invalidate.mockClear();
    await useFileEditorStore.getState().refreshFile("a1:ws1:docs/logo.png");
    expect(h.invalidate).toHaveBeenCalledWith("a1", "ws1", "docs/logo.png");
    expect(fetchSpy).toHaveBeenCalledTimes(1);
    // Text/image tabs bump the same seq so preview consumers re-read.
    expect(tab("docs/logo.png").previewSeq).toBe(1);
  });
});
