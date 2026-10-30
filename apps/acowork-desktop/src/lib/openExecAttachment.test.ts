// @vitest-environment jsdom
/**
 * `openExecImageAttachment` — the execution surface's click contract.
 *
 * The dispatcher is the only place that decides what clicking an exec-surface
 * image means, so these tests pin the three properties that make it correct
 * and cheap:
 *
 *   1. bytes the caller already holds are NOT downloaded a second time,
 *   2. the object URL the viewer shows is owned and given back on close
 *      (otherwise one full-size blob leaks per image the user ever opened),
 *   3. a failure is visible, not a click that does nothing.
 *
 * The last case pins cross-surface parity: an image in a chat row and an image
 * in an execution row open the SAME viewer with the SAME affordances. That is
 * the acceptance property of the feature, and without it the two surfaces can
 * drift back apart one branch at a time.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const lightboxMock = vi.fn();
const toastMock = vi.fn();
const saveMock = vi.fn(async (..._args: unknown[]) => true);
const fetchBlobMock = vi.fn();

vi.mock("../components/common/ImageLightbox", () => ({
  openImageLightbox: (...args: unknown[]) => lightboxMock(...(args as [])),
}));
vi.mock("../components/common/ToastProvider", () => ({
  showToast: (...args: unknown[]) => toastMock(...(args as [])),
}));
vi.mock("./agentFile", () => ({
  fetchAgentFileBlob: (...args: unknown[]) => fetchBlobMock(...(args as [])),
  saveAgentFile: (...args: unknown[]) => saveMock(...(args as [])),
}));
vi.mock("../i18n", () => ({
  default: { t: (key: string, opts?: Record<string, unknown>) => `${key}:${opts?.filename ?? ""}` },
}));
vi.mock("./logger", () => ({ log: { warn: vi.fn(), debug: vi.fn(), info: vi.fn(), error: vi.fn() } }));

// The chat half of the parity case runs the other dispatcher; these are the
// seams it reads through. Mocked rather than real so this file never mounts a
// store or crosses the Tauri bridge.
vi.mock("../stores/chatSessionStore", () => ({
  createChatAttachmentObjectUrl: vi.fn(async () => "blob:chat"),
  fetchChatAttachmentText: vi.fn(async () => ""),
}));
vi.mock("./user-chat-api", () => ({
  canPreviewAttachment: vi.fn(() => false),
  downloadChatAttachmentBySession: vi.fn(async () => true),
}));
vi.mock("../stores/fileEditorStore", () => ({
  useFileEditorStore: { getState: () => ({ openAttachmentPreview: vi.fn() }) },
}));

(globalThis as { URL?: unknown }).URL = Object.assign(globalThis.URL, {
  createObjectURL: vi.fn(() => "blob:exec"),
  revokeObjectURL: vi.fn(),
});

import { openExecImageAttachment } from "./openExecAttachment";
import { openChatAttachment } from "./openChatAttachment";

const ref = { agentId: "agent-1", documentId: "doc-1", filename: "shot.png" };

beforeEach(() => {
  lightboxMock.mockReset();
  toastMock.mockReset();
  saveMock.mockClear();
  fetchBlobMock.mockReset();
  fetchBlobMock.mockResolvedValue(new Blob(["bytes"]));
  vi.mocked(URL.createObjectURL).mockClear();
  vi.mocked(URL.revokeObjectURL).mockClear();
});

describe("openExecImageAttachment", () => {
  it("reuses the bytes the row already holds — no second download", async () => {
    const blob = new Blob(["already fetched by the thumbnail"]);
    await openExecImageAttachment(ref, blob);

    expect(fetchBlobMock).not.toHaveBeenCalled();
    expect(lightboxMock).toHaveBeenCalledTimes(1);
    expect(vi.mocked(URL.createObjectURL)).toHaveBeenCalledWith(blob);
  });

  it("fetches once when the caller has no bytes", async () => {
    await openExecImageAttachment(ref);
    expect(fetchBlobMock).toHaveBeenCalledWith("agent-1", "doc-1");
    expect(lightboxMock).toHaveBeenCalledTimes(1);
  });

  it("hands the viewer a save action and takes its URL back on close", async () => {
    await openExecImageAttachment(ref);
    const request = lightboxMock.mock.calls[0][0] as {
      src: string;
      filename: string;
      onDownload?: () => void;
      onClose?: () => void;
    };
    expect(request.filename).toBe("shot.png");

    request.onDownload?.();
    expect(saveMock).toHaveBeenCalledWith({ agentId: "agent-1", documentId: "doc-1" }, "shot.png");

    request.onClose?.();
    expect(vi.mocked(URL.revokeObjectURL)).toHaveBeenCalledWith(request.src);
  });

  it("toasts instead of failing silently when the blob is gone", async () => {
    fetchBlobMock.mockRejectedValue(new Error("agent file → 404"));
    await openExecImageAttachment(ref);

    expect(lightboxMock).not.toHaveBeenCalled();
    expect(toastMock).toHaveBeenCalledTimes(1);
    expect((toastMock.mock.calls[0][0] as { type: string }).type).toBe("error");
  });

  it("does nothing without an address to open", async () => {
    await openExecImageAttachment({ ...ref, agentId: "" });
    await openExecImageAttachment({ ...ref, documentId: "" });
    expect(lightboxMock).not.toHaveBeenCalled();
    expect(toastMock).not.toHaveBeenCalled();
  });
});

describe("both message surfaces zoom the same way", () => {
  it("chat and execution image rows open one viewer with one affordance set", async () => {
    await openChatAttachment("cs-1", { id: "a-1", filename: "chat.png", mime: "image/png", size: 10 });
    await openExecImageAttachment(ref);

    const [chat, exec] = lightboxMock.mock.calls.map((c) => c[0] as Record<string, unknown>);
    for (const request of [chat, exec]) {
      expect(typeof request.src).toBe("string");
      expect(typeof request.onDownload).toBe("function");
      expect(typeof request.onClose).toBe("function");
    }
  });
});
