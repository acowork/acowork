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
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { ImageLightboxHost, openImageLightbox } from "./ImageLightbox";
import { showToast } from "./ToastProvider";

// The copy path reports through a toast; mocking it is how the test observes
// success vs. failure without rendering a provider.
vi.mock("./ToastProvider", () => ({ showToast: vi.fn() }));

describe("ImageLightboxHost", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
    vi.mocked(showToast).mockClear();
  });

  afterEach(() => {
    // Every test below that stubs a global or the clipboard restores it here
    // too: a failing assertion must not leak a stub into the next test.
    vi.unstubAllGlobals();
    delete (navigator as { clipboard?: unknown }).clipboard;
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

  it("offers download only when the opener supplied one", () => {
    render(<ImageLightboxHost />);
    act(() => openImageLightbox({ src: "blob:a", filename: "a.png" }));
    expect(screen.queryByRole("button", { name: "下载" })).toBeNull();

    const onDownload = vi.fn();
    act(() => openImageLightbox({ src: "blob:b", filename: "b.png", onDownload }));
    fireEvent.click(screen.getByRole("button", { name: "下载" }));
    expect(onDownload).toHaveBeenCalledTimes(1);
  });

  it("labels every toolbar control by icon only", () => {
    render(<ImageLightboxHost />);
    act(() =>
      openImageLightbox({ src: "blob:full", filename: "shot.png", onDownload: () => {} }),
    );
    for (const name of ["缩小", "放大", "复制", "下载", "关闭"]) {
      expect(screen.getByRole("button", { name }).textContent).toBe("");
    }
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
    const zoomOut = screen.getByRole("button", { name: "缩小" });
    const closeButton = screen.getByRole("button", { name: "关闭" });

    // Forward from the last control wraps to the first.
    act(() => closeButton.focus());
    fireEvent.keyDown(dialog, { key: "Tab" });
    expect(document.activeElement).toBe(zoomOut);

    // Backwards from the first wraps to the last.
    act(() => zoomOut.focus());
    fireEvent.keyDown(dialog, { key: "Tab", shiftKey: true });
    expect(document.activeElement).toBe(closeButton);
  });

  /** jsdom does no layout, so the two measurements the geometry needs are
   *  declared: the viewport's content box on the scroller, the intrinsic size
   *  on the image. The fitted base is derived from them (400 px wide box × a
   *  800×400 image = 400×200), exactly like the real thing. */
  function openWithGeometry(src = "blob:full") {
    render(<ImageLightboxHost />);
    act(() => openImageLightbox({ src, filename: "shot.png" }));
    const img = screen.getByRole("img", { name: "shot.png" }) as HTMLImageElement;
    const scroller = img.parentElement as HTMLElement;
    Object.defineProperty(scroller, "clientWidth", { value: 432, configurable: true });
    Object.defineProperty(scroller, "clientHeight", { value: 232, configurable: true });
    Object.defineProperty(img, "naturalWidth", { value: 800, configurable: true });
    Object.defineProperty(img, "naturalHeight", { value: 400, configurable: true });
    act(() => {
      fireEvent.load(img);
    });
    return { img, scroller };
  }

  it("zooms by button and by wheel, clamped to the shared range", () => {
    const { img, scroller } = openWithGeometry();

    // Fitted: 800×400 into the 432×232 content box = 54% → 432×216. (jsdom's
    // `getComputedStyle` reports no padding, so the box is the clientWidth.)
    expect(img.style.width).toBe("432px");
    expect(img.style.height).toBe("216px");

    fireEvent.click(screen.getByRole("button", { name: "放大" }));
    expect(img.style.width).toBe(`${432 * 1.25}px`);

    fireEvent.wheel(scroller, { deltaY: -100 });
    expect(img.style.width).toBe(`${432 * 1.25 * 1.25}px`);
    fireEvent.wheel(scroller, { deltaY: 100 });
    expect(img.style.width).toBe(`${432 * 1.25}px`);

    // At the clamp the button stops, so the image cannot vanish or explode.
    for (let i = 0; i < 40; i++) fireEvent.click(screen.getByRole("button", { name: "放大" }));
    expect(screen.getByRole("button", { name: "放大" })).toHaveProperty("disabled", true);
  });

  it("pans by dragging once zoomed in", () => {
    const { img, scroller } = openWithGeometry();

    // At fit size there is nothing to pan: dragging must not scroll.
    scroller.scrollLeft = 0;
    pointer(scroller, "pointerdown", 1, { button: 0, clientX: 100, clientY: 100 });
    pointer(scroller, "pointermove", 1, { clientX: 40, clientY: 100 });
    pointer(scroller, "pointerup", 1, { clientX: 40, clientY: 100 });
    expect(scroller.scrollLeft).toBe(0);

    // Zoomed in, the content follows the pointer.
    fireEvent.click(screen.getByRole("button", { name: "放大" }));
    scroller.scrollLeft = 200;
    scroller.scrollTop = 200;
    pointer(scroller, "pointerdown", 2, { button: 0, clientX: 100, clientY: 100 });
    pointer(scroller, "pointermove", 2, { clientX: 60, clientY: 130 });
    expect(scroller.scrollLeft).toBe(240);
    expect(scroller.scrollTop).toBe(170);

    // Releasing stops it: a later move is ignored.
    pointer(scroller, "pointerup", 2, { clientX: 60, clientY: 130 });
    pointer(scroller, "pointermove", 2, { clientX: 0, clientY: 0 });
    expect(scroller.scrollLeft).toBe(240);
    expect(img.style.width).not.toBe("");
  });

  /** jsdom 25 ships no `PointerEvent`, and the handlers only read `button` /
   *  `clientX` / `clientY`. A `MouseEvent` dispatched under the pointer event
   *  type carries exactly that; `pointerId` is stamped on by hand. */
  function pointer(
    el: Element,
    type: "pointerdown" | "pointermove" | "pointerup",
    pointerId: number,
    init: { button?: number; clientX: number; clientY: number },
  ) {
    const event = new MouseEvent(type, { bubbles: true, cancelable: true, ...init });
    Object.defineProperty(event, "pointerId", { value: pointerId });
    fireEvent(el, event);
  }

  it("copies the bytes to the clipboard as PNG", async () => {
    const { write, items } = stubClipboard({});
    render(<ImageLightboxHost />);
    act(() => openImageLightbox({ src: "blob:full", filename: "shot.png" }));
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "复制" }));
    });

    expect(write).toHaveBeenCalledTimes(1);
    expect(items[0]["image/png"]).toBeInstanceOf(Blob);
    expect(showToast).toHaveBeenCalledWith({ type: "success", message: "图片已复制" });
  });

  it("reports a refused clipboard write as a failure, not a success", async () => {
    // `navigator.clipboard.write` is patched in production to swallow
    // NotAllowedError; the helper calls the unpatched original precisely so
    // this rejection is visible. A success toast here would claim a copy that
    // never happened.
    const { write } = stubClipboard({ reject: true });
    render(<ImageLightboxHost />);
    act(() => openImageLightbox({ src: "blob:full", filename: "shot.png" }));
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "复制" }));
    });

    expect(write).toHaveBeenCalledTimes(1);
    expect(showToast).toHaveBeenCalledWith({ type: "error", message: "复制图片失败" });
  });
});

/** Install the clipboard + fetch stubs the copy path needs, remembering the
 *  written items. `reject` makes the write fail the way WKWebView does. */
function stubClipboard({ reject = false }: { reject?: boolean }) {
  const items: Record<string, Blob>[] = [];
  const write = vi.fn(() =>
    reject ? Promise.reject(new DOMException("denied", "NotAllowedError")) : Promise.resolve(),
  );
  vi.stubGlobal(
    "ClipboardItem",
    class {
      constructor(public data: Record<string, Blob>) {
        items.push(data);
      }
    },
  );
  Object.defineProperty(navigator, "clipboard", {
    value: { write },
    configurable: true,
  });
  vi.stubGlobal(
    "fetch",
    vi.fn().mockResolvedValue({ blob: async () => new Blob(["x"], { type: "image/png" }) }),
  );
  return { write, items };
}
