// @vitest-environment jsdom
/**
 * The execution surface's image thumbnail — wiring only.
 *
 * What a click *means* is `lib/openExecAttachment.ts`'s contract and is tested
 * there; this file pins the two things only the row can get wrong:
 *
 *   1. the bytes are downloaded once, by the row, and handed to the viewer —
 *      a second fetch per click is the regression this replaces;
 *   2. the row's own object URL is revoked when virtualization unmounts it,
 *      while the viewer's (a different URL over the same bytes) is not.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

const openExecImageAttachment = vi.fn(async (..._args: unknown[]) => {});
const fetchAgentFileBlob = vi.fn();

vi.mock("../../lib/openExecAttachment", () => ({
  openExecImageAttachment: (...args: unknown[]) => openExecImageAttachment(...(args as [])),
}));
vi.mock("../../lib/agentFile", () => ({
  fetchAgentFileBlob: (...args: unknown[]) => fetchAgentFileBlob(...(args as [])),
}));
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
vi.mock("../../lib/logger", () => ({
  log: { warn: vi.fn(), debug: vi.fn(), info: vi.fn(), error: vi.fn() },
}));

import { AttachmentChipRow } from "./AttachmentChipRow";

const imageItem = {
  type: "image_upload" as const,
  documentId: "image-1",
  filename: "screenshot.png",
  format: "png",
  sizeBytes: 100,
};

const ZOOM_LABEL = "attachment.zoomLabel";

beforeEach(() => {
  openExecImageAttachment.mockClear();
  fetchAgentFileBlob.mockReset();
  fetchAgentFileBlob.mockResolvedValue(new Blob(["full-size bytes"]));
  (globalThis as { URL?: unknown }).URL = Object.assign(globalThis.URL, {
    createObjectURL: vi.fn(() => "blob:thumb"),
    revokeObjectURL: vi.fn(),
  });
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe("image upload thumbnail", () => {
  it("hands the viewer the bytes it already downloaded — no second fetch", async () => {
    render(<AttachmentChipRow item={imageItem} agentId="agent-1" />);
    await screen.findByRole("img", { name: "screenshot.png" });
    expect(fetchAgentFileBlob).toHaveBeenCalledTimes(1);

    fireEvent.click(screen.getByRole("button", { name: ZOOM_LABEL }));

    await waitFor(() => expect(openExecImageAttachment).toHaveBeenCalledTimes(1));
    expect(openExecImageAttachment).toHaveBeenCalledWith(
      { agentId: "agent-1", documentId: "image-1", filename: "screenshot.png" },
      expect.any(Blob),
    );
    expect(fetchAgentFileBlob).toHaveBeenCalledTimes(1);
  });

  it("revokes its own thumbnail URL when the row unmounts", async () => {
    const { unmount } = render(<AttachmentChipRow item={imageItem} agentId="agent-1" />);
    await screen.findByRole("img", { name: "screenshot.png" });

    unmount();
    expect(vi.mocked(URL.revokeObjectURL)).toHaveBeenCalledWith("blob:thumb");
  });

  it("stays inert while an optimistic upload is pending", async () => {
    render(<AttachmentChipRow item={imageItem} agentId="agent-1" pending />);
    const button = await screen.findByRole("button", { name: ZOOM_LABEL });
    expect(button.hasAttribute("disabled")).toBe(true);

    fireEvent.click(button);
    expect(openExecImageAttachment).not.toHaveBeenCalled();
  });

  it("degrades to a placeholder when the blob cannot be read", async () => {
    fetchAgentFileBlob.mockRejectedValue(new Error("agent file → 404"));
    render(<AttachmentChipRow item={imageItem} agentId="agent-1" />);
    await waitFor(() => expect(screen.queryByRole("img")).toBeNull());
    expect(screen.queryByRole("button", { name: ZOOM_LABEL })).toBeNull();
  });

  it("does not fetch at all without an agent to ask", () => {
    render(<AttachmentChipRow item={imageItem} agentId={null} />);
    expect(fetchAgentFileBlob).not.toHaveBeenCalled();
    expect(screen.queryByRole("button", { name: ZOOM_LABEL })).toBeNull();
  });
});
