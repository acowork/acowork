// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";

// jsdom does not implement these; the component only touches them on the
// image path, and the mock hands out a ready-made URL.
(globalThis as { URL?: unknown }).URL = Object.assign(globalThis.URL, {
  createObjectURL: vi.fn(() => "blob:mock"),
  revokeObjectURL: vi.fn(),
});

const objectUrlMock = vi.fn(async () => "blob:mock");
vi.mock("../../stores/chatSessionStore", () => ({
  chatAttachmentObjectUrl: (...args: unknown[]) => objectUrlMock(...(args as [])),
}));

import { ChatAttachmentChip } from "./ChatAttachmentChip";

const wrap = (node: ReactNode) => <div>{node}</div>;

describe("ChatAttachmentChip", () => {
  beforeEach(() => {
    objectUrlMock.mockReset();
    objectUrlMock.mockImplementation(async () => "blob:mock");
  });

  it("renders an image attachment as a thumbnail once the blob resolves", async () => {
    render(
      wrap(
        <ChatAttachmentChip
          chatSessionId="cs-1"
          attachment={{ id: "att-1", filename: "shot.png", mime: "image/png" }}
        />,
      ),
    );
    await waitFor(() => expect(objectUrlMock).toHaveBeenCalledWith("cs-1", "att-1", { thumb: true }));
    const img = await screen.findByRole("img", { name: "shot.png" });
    expect(img.getAttribute("src")).toBe("blob:mock");
  });

  it("falls back to the full blob when the thumb sidecar is missing", async () => {
    objectUrlMock.mockImplementationOnce(async () => {
      throw new Error("attachment → 404");
    });
    render(
      wrap(
        <ChatAttachmentChip
          chatSessionId="cs-1"
          attachment={{ id: "att-2", filename: "pic.jpg", mime: "image/jpeg" }}
        />,
      ),
    );
    await waitFor(() => {
      const fullBlobCall = objectUrlMock.mock.calls.find(
        (c) => c[2] === undefined,
      );
      expect(fullBlobCall).toEqual(["cs-1", "att-2"]);
    });
    await screen.findByRole("img", { name: "pic.jpg" });
  });

  it("stays a filename chip when every fetch fails", async () => {
    objectUrlMock.mockRejectedValueOnce(new Error("404"));
    objectUrlMock.mockRejectedValueOnce(new Error("offline"));
    render(
      wrap(
        <ChatAttachmentChip
          chatSessionId="cs-1"
          attachment={{ id: "att-3", filename: "broken.png", mime: "image/png" }}
        />,
      ),
    );
    await waitFor(() => expect(screen.getByText("broken.png")).toBeTruthy());
    expect(screen.queryByRole("img")).toBeNull();
  });

  it("never fetches for documents", async () => {
    render(
      wrap(
        <ChatAttachmentChip
          chatSessionId="cs-1"
          attachment={{ id: "att-4", filename: "spec.pdf", mime: "application/pdf" }}
        />,
      ),
    );
    expect(screen.getByText("spec.pdf")).toBeTruthy();
    expect(objectUrlMock).not.toHaveBeenCalled();
  });
});
