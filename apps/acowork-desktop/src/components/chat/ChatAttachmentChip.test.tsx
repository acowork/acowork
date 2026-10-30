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

  // ── P1: the capsule frame and the click contract ──────────────────
  // A chat attachment used to be a borderless grey `span`: it read as a
  // caption, not as a file, and nothing about it said "you can open this".
  // These three guard the frame and the two interactive forms.

  it("renders a document as a capsule with its size, not as a bare caption", () => {
    render(
      wrap(
        <ChatAttachmentChip
          chatSessionId="cs-1"
          attachment={{
            id: "att-5",
            filename: "spec.pdf",
            mime: "application/pdf",
            size: 12800,
          }}
        />,
      ),
    );
    expect(screen.getByText("spec.pdf")).toBeTruthy();
    expect(screen.getByText("12.5 KB")).toBeTruthy();
    const capsule = screen.getByTitle("spec.pdf (application/pdf, 12.5 KB)");
    expect(capsule.className).toContain("border-border-outer");
  });

  it("omits the size rather than inventing one when the row has no metadata", () => {
    render(
      wrap(
        <ChatAttachmentChip
          chatSessionId="cs-1"
          attachment={{ id: "att-6", filename: "legacy.doc", mime: "application/msword" }}
        />,
      ),
    );
    expect(screen.getByTitle("legacy.doc (application/msword)")).toBeTruthy();
  });

  it("is clickable only when the row hands it an open action", () => {
    const onClick = vi.fn();
    render(
      wrap(
        <ChatAttachmentChip
          chatSessionId="cs-1"
          attachment={{ id: "att-7", filename: "a.pdf", mime: "application/pdf" }}
          onClick={onClick}
        />,
      ),
    );
    screen.getByRole("button", { name: "a.pdf" }).click();
    expect(onClick).toHaveBeenCalledTimes(1);
  });

  it("stays inert in the composer tray, where the only control is remove", () => {
    const onRemove = vi.fn();
    render(
      wrap(
        <ChatAttachmentChip
          chatSessionId="cs-1"
          attachment={{ id: "att-8", filename: "b.pdf", mime: "application/pdf" }}
          onRemove={onRemove}
          removeLabel="remove attachment"
        />,
      ),
    );
    expect(screen.queryByRole("button", { name: "b.pdf" })).toBeNull();
    screen.getByRole("button", { name: "remove attachment" }).click();
    expect(onRemove).toHaveBeenCalledTimes(1);
  });

  it("never nests one control in another when a caller passes open and remove", () => {
    // The composer tray removes, a message row opens — no caller does both
    // today. If one ever does, the rule is "the remove button owns the click":
    // a `<button>` inside a `<button>` is invalid HTML and two tab stops for
    // one chip.
    const onClick = vi.fn();
    const onRemove = vi.fn();
    const { container } = render(
      wrap(
        <ChatAttachmentChip
          chatSessionId="cs-1"
          attachment={{ id: "att-10", filename: "c.pdf", mime: "application/pdf" }}
          onClick={onClick}
          onRemove={onRemove}
          removeLabel="remove attachment"
        />,
      ),
    );
    expect(container.querySelector("button button")).toBeNull();
    screen.getByRole("button", { name: "remove attachment" }).click();
    expect(onRemove).toHaveBeenCalledTimes(1);
    expect(onClick).not.toHaveBeenCalled();
  });

  it("makes the image thumbnail openable, and keeps remove a single control", async () => {
    const onClick = vi.fn();
    render(
      wrap(
        <ChatAttachmentChip
          chatSessionId="cs-1"
          attachment={{ id: "att-9", filename: "shot.png", mime: "image/png" }}
          onClick={onClick}
        />,
      ),
    );
    // The thumbnail form only exists once the blob resolves; before that the
    // chip is the filename fallback (also a button, hence the ordering).
    await screen.findByRole("img", { name: "shot.png" });
    const button = screen.getByRole("button", { name: "shot.png" });
    button.click();
    expect(onClick).toHaveBeenCalledTimes(1);
    // One interactive element: no nested button inside the thumbnail.
    expect(button.querySelector("button")).toBeNull();
  });
});
