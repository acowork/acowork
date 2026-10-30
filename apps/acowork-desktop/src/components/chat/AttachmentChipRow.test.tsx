// @vitest-environment jsdom
/**
 * The execution surface's image thumbnail releases what it took.
 *
 * A transcript row is unmounted the moment it scrolls out of the virtualized
 * window, and the row owns an object URL over the downloaded image. The bug
 * this pins: the release was written as a `setState` updater inside the effect
 * cleanup, and React drops updates to an unmounted component — so the updater
 * never ran and every image the user scrolled past leaked a URL for the life
 * of the process.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";

vi.mock("../../lib/config", () => ({ getGatewayUrl: () => "http://gateway" }));
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

beforeEach(() => {
  vi.stubGlobal("fetch", vi.fn(async () => ({ ok: true, blob: async () => new Blob(["img"]) })));
  (globalThis as { URL?: unknown }).URL = Object.assign(globalThis.URL, {
    createObjectURL: vi.fn(() => "blob:thumb"),
    revokeObjectURL: vi.fn(),
  });
});

describe("image upload thumbnail", () => {
  it("revokes its object URL when the row unmounts", async () => {
    const { unmount } = render(<AttachmentChipRow item={imageItem} agentId="agent-1" />);
    await screen.findByRole("img", { name: "screenshot.png" });

    unmount();
    expect(vi.mocked(URL.revokeObjectURL)).toHaveBeenCalledWith("blob:thumb");
  });
});
