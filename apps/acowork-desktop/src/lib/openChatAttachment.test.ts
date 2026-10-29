// @vitest-environment jsdom
/**
 * `openChatAttachment` routing contract.
 *
 * The dispatcher is the only place that decides what a click on a chat
 * attachment means, and it decides from the row's own metadata — so the
 * routing table is the thing worth pinning: get it wrong and a 200 MB binary
 * goes into a Monaco model, or an image opens as a save dialog.
 *
 * Failures are equally load-bearing: a chat attachment can legitimately be
 * gone (migrated rows, an offline Gateway) and a click that does nothing is
 * the defect this whole plan exists to remove.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const objectUrlMock = vi.fn(async () => "blob:full");
const textMock = vi.fn(async () => "file body");
vi.mock("../stores/chatSessionStore", () => ({
  // The lightbox branch takes an OWNED url (uncached, released on close) —
  // not the shared thumbnail cache the chips use.
  createChatAttachmentObjectUrl: (...a: unknown[]) => objectUrlMock(...(a as [])),
  fetchChatAttachmentText: (...a: unknown[]) => textMock(...(a as [])),
}));

const lightboxMock = vi.fn();
vi.mock("../components/common/ImageLightbox", () => ({
  openImageLightbox: (...a: unknown[]) => lightboxMock(...(a as [])),
}));

const toastMock = vi.fn();
vi.mock("../components/common/ToastProvider", () => ({
  showToast: (...a: unknown[]) => toastMock(...(a as [])),
}));

const downloadMock = vi.fn(async () => true);
vi.mock("./user-chat-api", async () => {
  const actual = await vi.importActual<typeof import("./user-chat-api")>("./user-chat-api");
  return {
    canPreviewAttachment: actual.canPreviewAttachment,
    downloadChatAttachmentBySession: (...a: unknown[]) => downloadMock(...(a as [])),
  };
});

const openPreviewMock = vi.fn();
vi.mock("../stores/fileEditorStore", () => ({
  useFileEditorStore: {
    getState: () => ({ openAttachmentPreview: openPreviewMock }),
  },
}));

import { openChatAttachment } from "./openChatAttachment";

const image = { id: "a-1", filename: "shot.png", mime: "image/png", size: 134906 };
const source = { id: "a-2", filename: "main.ts", mime: "text/plain", size: 2048 };
const binary = { id: "a-3", filename: "deck.pdf", mime: "application/pdf", size: 4096 };
const hugeText = { id: "a-4", filename: "dump.json", mime: "application/json", size: 9 * 1024 * 1024 };

describe("openChatAttachment", () => {
  beforeEach(() => {
    objectUrlMock.mockClear();
    textMock.mockClear();
    lightboxMock.mockClear();
    toastMock.mockClear();
    downloadMock.mockClear();
    openPreviewMock.mockClear();
  });

  it("opens images in the lightbox, with a save action attached", async () => {
    await openChatAttachment("cs-1", image);
    expect(objectUrlMock).toHaveBeenCalledWith("cs-1", "a-1");
    expect(lightboxMock).toHaveBeenCalledTimes(1);
    const request = lightboxMock.mock.calls[0][0] as {
      src: string;
      filename: string;
      onDownload?: () => void;
      onClose?: () => void;
    };
    expect(request.src).toBe("blob:full");
    expect(request.filename).toBe("shot.png");
    expect(typeof request.onDownload).toBe("function");
    // No Monaco tab, no save dialog for an image.
    expect(openPreviewMock).not.toHaveBeenCalled();
    expect(downloadMock).not.toHaveBeenCalled();
  });

  it("hands the lightbox a URL it gives back, so a viewed image is not retained", async () => {
    const revoke = vi.fn();
    const original = URL.revokeObjectURL;
    Object.defineProperty(URL, "revokeObjectURL", { configurable: true, value: revoke });
    try {
      await openChatAttachment("cs-1", image);
      const request = lightboxMock.mock.calls[0][0] as { onClose?: () => void };
      expect(typeof request.onClose).toBe("function");
      // Closing the overlay is what frees the bytes; opening it is not.
      expect(revoke).not.toHaveBeenCalled();
      request.onClose!();
      expect(revoke).toHaveBeenCalledWith("blob:full");
    } finally {
      Object.defineProperty(URL, "revokeObjectURL", { configurable: true, value: original });
    }
  });

  it("opens previewable text in a read-only tab keyed by chat + attachment", async () => {
    await openChatAttachment("cs-1", source);
    expect(textMock).toHaveBeenCalledWith("cs-1", "a-2");
    expect(openPreviewMock).toHaveBeenCalledWith({
      chatId: "cs-1",
      attachmentId: "a-2",
      fileName: "main.ts",
      content: "file body",
      language: expect.any(String),
    });
    expect(downloadMock).not.toHaveBeenCalled();
  });

  it("saves anything else through the dialog path", async () => {
    await openChatAttachment("cs-1", binary);
    expect(downloadMock).toHaveBeenCalledWith("cs-1", expect.objectContaining({ id: "a-3" }));
    expect(openPreviewMock).not.toHaveBeenCalled();
    expect(lightboxMock).not.toHaveBeenCalled();
  });

  it("refuses to load an oversized text file into Monaco", async () => {
    await openChatAttachment("cs-1", hugeText);
    expect(textMock).not.toHaveBeenCalled();
    expect(downloadMock).toHaveBeenCalledTimes(1);
  });

  it("toasts instead of failing silently when the blob is gone", async () => {
    objectUrlMock.mockRejectedValueOnce(new Error("attachment → 404"));
    await openChatAttachment("cs-1", image);
    expect(lightboxMock).not.toHaveBeenCalled();
    expect(toastMock).toHaveBeenCalledTimes(1);
    expect((toastMock.mock.calls[0][0] as { type: string }).type).toBe("error");
  });

  it("ignores a click with no addressable attachment", async () => {
    await openChatAttachment("", image);
    await openChatAttachment("cs-1", { ...image, id: "" });
    expect(lightboxMock).not.toHaveBeenCalled();
    expect(downloadMock).not.toHaveBeenCalled();
    expect(objectUrlMock).not.toHaveBeenCalled();
  });
});