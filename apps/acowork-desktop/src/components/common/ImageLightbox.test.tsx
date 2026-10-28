// @vitest-environment jsdom
/**
 * The lightbox host. Three things matter beyond rendering: it is reachable
 * from a plain module (the dispatcher), it closes on the two gestures a user
 * will actually try — Escape and a backdrop click — and it hands the caller's
 * bytes back.
 *
 * The last one is the reason `onClose` exists: the image's object URL belongs
 * to whoever opened the overlay, and an overlay that just unmounts leaks a
 * full-size blob per image viewed.
 *
 * Focus is pinned too: an `aria-modal` dialog that leaves the keyboard in the
 * transcript behind it is only pretending to be modal.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { ImageLightboxHost, openImageLightbox } from "./ImageLightbox";

describe("ImageLightboxHost", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
  });

  it("renders nothing until something opens it", () => {
    render(<ImageLightboxHost />);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("opens over an object URL and shows the filename", () => {
    render(<ImageLightboxHost />);
    act(() => openImageLightbox({ src: "blob:full", filename: "shot.png" }));

    const dialog = screen.getByRole("dialog", { name: "shot.png" });
    expect(dialog).toBeTruthy();
    expect(screen.getByRole("img", { name: "shot.png" }).getAttribute("src")).toBe("blob:full");
  });

  it("offers save only when the opener supplied one", () => {
    render(<ImageLightboxHost />);
    act(() => openImageLightbox({ src: "blob:a", filename: "a.png" }));
    expect(screen.queryByRole("button", { name: /保存/ })).toBeNull();

    const onDownload = vi.fn();
    act(() => openImageLightbox({ src: "blob:b", filename: "b.png", onDownload }));
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    expect(onDownload).toHaveBeenCalledTimes(1);
  });

  it("closes on Escape", () => {
    render(<ImageLightboxHost />);
    act(() => openImageLightbox({ src: "blob:full", filename: "shot.png" }));
    fireEvent.keyDown(window, { key: "Escape" });
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("closes on the backdrop but not on the image itself", () => {
    render(<ImageLightboxHost />);
    act(() => openImageLightbox({ src: "blob:full", filename: "shot.png" }));

    fireEvent.click(screen.getByRole("img", { name: "shot.png" }));
    expect(screen.getByRole("dialog")).toBeTruthy();

    fireEvent.click(screen.getByRole("dialog"));
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  // ── the caller's bytes ────────────────────────────────────────────────

  it("releases the caller's URL exactly once, on dismissal", () => {
    const onClose = vi.fn();
    render(<ImageLightboxHost />);
    act(() => openImageLightbox({ src: "blob:full", filename: "shot.png", onClose }));

    fireEvent.keyDown(window, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);

    // Dismissing again is a no-op: a second release would double-revoke a
    // URL the caller may already have handed to a fresh object.
    fireEvent.keyDown(window, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("releases the previous image when another one replaces it", () => {
    const firstClose = vi.fn();
    render(<ImageLightboxHost />);
    act(() => openImageLightbox({ src: "blob:1", filename: "1.png", onClose: firstClose }));
    act(() => openImageLightbox({ src: "blob:2", filename: "2.png" }));

    expect(firstClose).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("dialog", { name: "2.png" })).toBeTruthy();
  });

  // ── focus ─────────────────────────────────────────────────────────────

  it("takes focus while it is up and gives it back on close", () => {
    const opener = document.createElement("button");
    opener.textContent = "open";
    document.body.appendChild(opener);
    opener.focus();
    expect(document.activeElement).toBe(opener);

    render(<ImageLightboxHost />);
    act(() => openImageLightbox({ src: "blob:full", filename: "shot.png" }));
    expect(document.activeElement).toBe(screen.getByRole("dialog"));

    fireEvent.keyDown(window, { key: "Escape" });
    expect(document.activeElement).toBe(opener);
  });

  it("keeps Tab inside the overlay", () => {
    render(<ImageLightboxHost />);
    act(() =>
      openImageLightbox({ src: "blob:full", filename: "shot.png", onDownload: () => {} }),
    );
    const dialog = screen.getByRole("dialog");
    const save = screen.getByRole("button", { name: /保存/ });
    const closeButton = screen.getByRole("button", { name: /关闭/ });

    // Forward from the last control wraps to the first.
    act(() => closeButton.focus());
    fireEvent.keyDown(dialog, { key: "Tab" });
    expect(document.activeElement).toBe(save);

    // Backwards from the first wraps to the last.
    act(() => save.focus());
    fireEvent.keyDown(dialog, { key: "Tab", shiftKey: true });
    expect(document.activeElement).toBe(closeButton);
  });
});