/**
 * The PDF preview surface — the gesture, not the engine.
 *
 * The same column answers two questions here: whether a document lays its
 * pages out as one scrollable run (it does, and only a few pages are ever
 * painted), and whether a pointer becomes a region on the page it started
 * on (it does, and only while framing is on).
 *
 * pdf.js is never loaded here: `openDocument` is injectable exactly so this
 * file can drive the interaction over a stub page. What is worth pinning is
 * the wiring between a pointer and a pixel, because that mapping is the whole
 * claim the feature makes ("the agent sees what you framed"):
 *
 *   1. the document is opened once, EVERY page is measured and laid out in one
 *      column, and only the pages the scroll window covers are painted — and a
 *      remount that re-opens the SAME bytes (what React StrictMode does in dev)
 *      still ends on a painted page, with the abandoned document released;
 *   2. a drag frames the page it STARTED on, wherever in the column that is, and
 *      a drag that wanders past an edge is clamped back onto that page — a crop
 *      is one bitmap and cannot straddle two;
 *   3. the hand-off is a PNG `File` named after the source plus the region, with
 *      that page number beside it, and it still works after the page has been
 *      evicted from the paint window;
 *   4. the arrows and the page readout follow the column's scroll; nothing about
 *      framing switches the user into a one-page viewer;
 *   5. leaving the mode releases the document — a 30-page PDF must not stay
 *      parsed because someone previewed a corner of it;
 *   6. an unreachable composer disables the hand-off instead of swallowing the
 *      gesture, and a document that will not open says so in the body.
 *
 * Column arithmetic is `pdfPageStack.test.ts`'s business and the per-page
 * numbers are `imageViewerGeometry.test.ts`'s; this file is the wiring between
 * a pointer event and those two.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { PdfColumnViewer, type PdfColumnViewerProps } from "./PdfColumnViewer";
import type { PdfPageRenderer } from "./pdfRasterize";
import { PAGE_GAP_PX } from "./pdfPageStack";
import type { ImageSize } from "./imageViewerGeometry";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string, params?: Record<string, unknown>) => {
      const table: Record<string, string> = {
        "fileEditor.pdfViewer.regionPickerTitle": "Select a region",
        "fileEditor.pdfViewer.selectRegion": "Frame a region",
        "fileEditor.pdfViewer.prevPage": "Previous page",
        "fileEditor.pdfViewer.nextPage": "Next page",
        "fileEditor.pdfViewer.pageIndicator": `${params?.page} / ${params?.pages}`,
        "fileEditor.pdfViewer.addRegion": "Add selection to Chat",
        "fileEditor.pdfViewer.rasterizeFailed": "Could not read this PDF",
        "fileEditor.imageViewer.zoomIn": "Zoom in",
        "fileEditor.imageViewer.zoomOut": "Zoom out",
        "fileEditor.imageViewer.fit": "Fit to panel",
        "fileEditor.imageViewer.addRegionToChat": "Add selection to Chat",
        "fileEditor.imageViewer.clearSelection": "Clear selection",
        "fileEditor.imageViewer.captureFailed": "Could not capture the selection",
        "fileEditor.previewLoading": "Loading preview...",
        "fileEditor.close": "Close",
      };
      return table[key] ?? key;
    },
  }),
}));

const toast = vi.hoisted(() => vi.fn());
vi.mock("../common/ToastProvider", () => ({ showToast: toast }));

// jsdom ships no `PointerEvent`, and its `Event` fallback silently drops
// `clientX` / `button` — the data every handler here reads. Same polyfill and
// same reasoning as `ImageViewer.test.tsx`.
if (typeof window.PointerEvent === "undefined") {
  class PointerEventPolyfill extends MouseEvent {
    readonly pointerId: number;
    constructor(type: string, init: PointerEventInit = {}) {
      super(type, init);
      this.pointerId = init.pointerId ?? 1;
    }
  }
  // @ts-expect-error — assigning a polyfill over a DOM global jsdom omits.
  window.PointerEvent = PointerEventPolyfill;
}

/** Stub page bitmaps: 400 × 300 each, three pages. At the fitted scale these
 *  tests produce (1 — a 400 px page in an 800 px panel is never enlarged), a
 *  drag maps 1:1 onto bitmap pixels. */
const PAGE_W = 400;
const PAGE_H = 300;
const PAGES = 3;
/** One page's band, gutter included — the column's pitch. */
const PITCH = PAGE_H + PAGE_GAP_PX;

function rect(left: number, top: number, width: number, height: number): DOMRect {
  return {
    left,
    top,
    right: left + width,
    bottom: top + height,
    width,
    height,
    x: left,
    y: top,
    toJSON: () => ({}),
  } as DOMRect;
}

/** The column, laid out at 1:1: its top-left is the origin every pointer
 *  offset is measured from, so `clientY === 320` is column y 320. */
const COLUMN_RECT = rect(0, 0, PAGE_W, PAGE_H * PAGES + PAGE_GAP_PX * (PAGES - 1));

/**
 * jsdom never resizes anything, yet the paint window is derived from the
 * viewport's height — so without a controllable observer the column would
 * forever believe it had nothing to show. This stub keeps the instances the
 * component created (and drops the ones it disconnected) so a test can report a
 * size through the real code path instead of poking component state.
 */
const observers: ResizeObserver[] = [];
class TestResizeObserver {
  private readonly callback: ResizeObserverCallback;
  constructor(callback: ResizeObserverCallback) {
    this.callback = callback;
    observers.push(this);
  }
  observe(): void {}
  unobserve(): void {}
  disconnect(): void {
    const index = observers.indexOf(this as unknown as ResizeObserver);
    if (index >= 0) observers.splice(index, 1);
  }
}

beforeEach(() => {
  cleanup();
  toast.mockClear();
  observers.length = 0;
  vi.stubGlobal("ResizeObserver", TestResizeObserver as unknown as typeof ResizeObserver);
  // jsdom gives every canvas a 2D context of the same name, but no painting:
  // stub `getContext` the way the image viewer's tests do — the crop under
  // test is real code.
  vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue({
    drawImage: vi.fn(),
  } as unknown as CanvasRenderingContext2D);
  vi.spyOn(HTMLCanvasElement.prototype, "toBlob").mockImplementation(function (
    this: HTMLCanvasElement,
    callback: BlobCallback,
  ) {
    callback(new Blob(["png"], { type: "image/png" }));
  });
  document.body.innerHTML = "";
});

function stubRenderer(sizes: ImageSize[] = Array.from({ length: PAGES }, () => ({ width: PAGE_W, height: PAGE_H }))) {
  const rendered: number[] = [];
  const renderer: PdfPageRenderer = {
    numPages: sizes.length,
    measure: vi.fn(async () => sizes),
    async render(pageNumber) {
      rendered.push(pageNumber);
      const canvas = document.createElement("canvas");
      canvas.width = sizes[pageNumber - 1]?.width ?? PAGE_W;
      canvas.height = sizes[pageNumber - 1]?.height ?? PAGE_H;
      return canvas;
    },
    destroy: vi.fn(async () => {}),
  };
  return { renderer, rendered };
}

async function renderPicker(
  overrides: Partial<PdfColumnViewerProps> = {},
  options: { waitForPage?: boolean; strictMode?: boolean } = {},
) {
  const bytes = new Uint8Array([0x25, 0x50, 0x44, 0x46]);
  const { renderer, rendered } = stubRenderer();
  const openDocument = vi.fn(async () => renderer);
  const onAddRegionToChat = vi.fn();
  const onClose = vi.fn();
  const props: PdfColumnViewerProps = {
    bytes,
    framing: true,
    onExitFraming: onClose,
    fileName: "report.pdf",
    addToChatDisabledTitle: null,
    onAddRegionToChat,
    openDocument,
    ...overrides,
  };
  const view = render(<PdfColumnViewer {...props} />, { reactStrictMode: options.strictMode });
  const stage =
    // The column only exists once the pages have been measured: awaiting it is
    // the test's version of "the document is on screen". A test that mounts a
    // document which never opens has nothing to wait for.
    options.waitForPage === false
      ? null
      : await (async () => {
          const found = await screen.findByTestId("pdf-column");
          vi.spyOn(found, "getBoundingClientRect").mockReturnValue(COLUMN_RECT);
          return found;
        })();
  return {
    bytes,
    renderer,
    rendered,
    openDocument,
    stage,
    onAddRegionToChat,
    onClose,
    view,
    /** Re-render with a different prop, same `bytes` identity — the
     *  framing toggle is a prop change on one column, not a remount. */
    setFraming: (framing: boolean) => view.rerender(<PdfColumnViewer {...props} framing={framing} />),
    ...view,
  };
}

/** Give the viewport a size through the observer, the way a real panel does on
 *  its first layout. 800 × 300 shows one page at the fitted 1:1 scale. */
async function measureViewport(height = PAGE_H, width = PAGE_W * 2) {
  const viewport = screen.getByTestId("pdf-column-viewport");
  vi.spyOn(viewport, "getBoundingClientRect").mockReturnValue(rect(0, 0, width, height));
  for (const observer of [...observers]) {
    (observer as unknown as TestResizeObserver).callback([], observer);
  }
  await waitFor(() => expect(screen.getByTestId("pdf-zoom-level").textContent).toBe("100%"));
  return viewport;
}

/** jsdom's `scrollTop` is a fixed 0 — no layout, no scroll. Redefine it on the
 *  node so the component's own writes and reads are observable, then deliver a
 *  real `scroll` event the way the browser would. */
async function scrollColumn(top: number) {
  const viewport = screen.getByTestId("pdf-column-viewport");
  if (!Object.getOwnPropertyDescriptor(viewport, "scrollTop")?.configurable) {
    let value = 0;
    Object.defineProperty(viewport, "scrollTop", {
      configurable: true,
      get: () => value,
      set: (next: number) => {
        value = next;
      },
    });
  }
  viewport.scrollTop = top;
  fireEvent.scroll(viewport);
  await waitFor(() => expect(viewport.scrollTop).toBe(top));
  return viewport;
}

/** Drag from one column point to another. */
const drag = (stage: HTMLElement, from: [number, number], to: [number, number]) => {
  fireEvent.pointerDown(stage, { clientX: from[0], clientY: from[1], button: 0, pointerId: 1 });
  fireEvent.pointerMove(stage, { clientX: to[0], clientY: to[1] });
  fireEvent.pointerUp(stage, { clientX: to[0], clientY: to[1], pointerId: 1 });
};

/** The framing gesture on page 1, in the coordinates the old single-page
 *  picker used — kept as a helper because half these tests only need "a box". */
const dragRegion = (stage: HTMLElement) => drag(stage, [10, 10], [110, 60]);

describe("PdfColumnViewer page column", () => {
  it("measures every page and lays them out as one column", async () => {
    const { bytes, renderer, rendered, openDocument } = await renderPicker();

    expect(openDocument).toHaveBeenCalledTimes(1);
    expect(openDocument).toHaveBeenCalledWith(bytes);
    // The scroll geometry exists before any bitmap does — that is what makes a
    // continuous column affordable.
    expect(renderer.measure).toHaveBeenCalledTimes(1);
    const boxes = screen.getAllByTestId("pdf-column-page");
    expect(boxes).toHaveLength(PAGES);
    expect(boxes.map((box) => box.getAttribute("data-page"))).toEqual(["1", "2", "3"]);
    expect(boxes[1].style.top).toBe(`${PITCH}px`);

    const stage = screen.getByTestId("pdf-column");
    expect(stage.style.width).toBe(`${PAGE_W}px`);
    expect(stage.style.height).toBe(`${PAGE_H * PAGES + PAGE_GAP_PX * 2}px`);
    expect(screen.getByTestId("pdf-page-indicator").textContent).toBe("1 / 3");

    // Nothing is painted twice, and the page that is painted is painted at the
    // size the document reported. The paint is a promise, so the canvas is
    // what the test waits on — `rendered` is only complete once it is parked.
    await waitFor(() =>
      expect(screen.getAllByTestId("pdf-column-canvas-holder")[0].firstElementChild).not.toBeNull(),
    );
    expect(rendered).toEqual([1]);
    const canvas = screen
      .getAllByTestId("pdf-column-canvas-holder")[0]
      .firstElementChild as HTMLCanvasElement;
    expect(canvas.width).toBe(PAGE_W);
    expect(canvas.height).toBe(PAGE_H);
  });

  it("stretches the painted bitmap into its page box", async () => {
    // pdf.js hands back a bitmap at `PDF_OUTPUT_SCALE` (a slide is ~1900 px
    // wide); the box is that bitmap times the display scale. If the canvas is
    // parked at its own size it overflows the box and the holder's
    // `overflow-hidden` crops the page to its top-left corner — the column
    // looks magnified and cut off. jsdom lays nothing out, so the class that
    // does the stretching is what a test can actually observe.
    await renderPicker();
    await waitFor(() =>
      expect(screen.getAllByTestId("pdf-column-canvas-holder")[0].firstElementChild).not.toBeNull(),
    );
    const canvas = screen.getAllByTestId("pdf-column-canvas-holder")[0]
      .firstElementChild as HTMLCanvasElement;

    expect(canvas.className).toBe("block h-full w-full select-none");
    // The crop is measured in bitmap pixels, so the stretch must not touch the
    // canvas' own dimensions.
    expect(canvas.width).toBe(PAGE_W);
    expect(canvas.height).toBe(PAGE_H);
  });

  it("paints what a measured viewport can see, and evicts what it scrolled past", async () => {
    const { rendered } = await renderPicker();
    const viewport = await measureViewport(PAGE_H);

    // A 300 px viewport plus one viewport of overscan reaches page 2, not 3.
    await waitFor(() => expect(rendered).toEqual([1, 2]));

    await scrollColumn(PITCH * 2);
    await waitFor(() => expect(rendered).toContain(3));
    await waitFor(() => {
      const first = screen.getAllByTestId("pdf-column-canvas-holder")[0];
      expect(first.firstElementChild).toBeNull();
    });
    expect(viewport.scrollTop).toBe(PITCH * 2);
  });

  it("keeps the document and the scroll position across the framing toggle", async () => {
    // The complaint that started all of this: entering framing swapped the
    // renderer, so the user lost the page they were looking at. One column
    // serves both modes now, so toggling is a prop change — the document is
    // not re-opened and the scroll does not move.
    const { openDocument, setFraming } = await renderPicker();
    await measureViewport(PAGE_H);
    await scrollColumn(PITCH);
    await waitFor(() => expect(screen.getByTestId("pdf-page-indicator").textContent).toBe("2 / 3"));

    setFraming(false);

    expect(openDocument).toHaveBeenCalledTimes(1);
    expect(screen.getByTestId("pdf-page-indicator").textContent).toBe("2 / 3");
    expect(screen.getByTestId("pdf-column-viewport").scrollTop).toBe(PITCH);
  });

  it("survives a remount that re-opens the same bytes (React StrictMode)", async () => {
    // Dev React mounts, cleans up and re-mounts, so the open effect runs twice
    // over ONE `bytes` array. The second open is the one the user sees: the
    // orphan must be released and the failure body must not appear. This is
    // also why `pdfRasterize` copies at the pdf.js transfer point — an engine
    // that ate the caller's buffer would make this very call throw.
    const docs: PdfPageRenderer[] = [];
    const openDocument = vi.fn(async () => {
      const { renderer } = stubRenderer();
      docs.push(renderer);
      return renderer;
    });

    await renderPicker({ openDocument }, { strictMode: true });

    expect(openDocument.mock.calls.length).toBe(2);
    expect(openDocument.mock.calls[0][0]).toBe(openDocument.mock.calls[1][0]);
    expect(screen.queryByText("Could not read this PDF")).toBeNull();
    expect(screen.getByTestId("pdf-page-indicator").textContent).toBe("1 / 3");
    await waitFor(() => expect(docs[0].destroy).toHaveBeenCalled());
  });

  it("says what went wrong when the document will not open", async () => {
    const openDocument = vi.fn(async () => {
      throw new Error("Invalid PDF structure");
    });
    await renderPicker({ openDocument }, { waitForPage: false });
    expect(await screen.findByText("Could not read this PDF")).toBeTruthy();
    expect(screen.queryByTestId("pdf-column")).toBeNull();
  });

  it("leaves the mode through the close button", async () => {
    const { onClose } = await renderPicker();
    fireEvent.click(screen.getByRole("button", { name: "Close" }));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("releases the document when the mode is unmounted", async () => {
    const { renderer, view } = await renderPicker();
    view.unmount();
    await waitFor(() => expect(renderer.destroy).toHaveBeenCalledTimes(1));
  });
});

describe("PdfColumnViewer framing in the column", () => {
  it("is a plain preview when framing is off", async () => {
    // Same column, no gesture: the pointer must not draw a box, and the
    // framing-only controls must not be there to be pressed.
    const { stage } = await renderPicker({ framing: false });

    drag(stage!, [10, 10], [110, 60]);

    expect(screen.queryByTestId("pdf-column-selection")).toBeNull();
    expect(screen.queryByRole("button", { name: "Add selection to Chat" })).toBeNull();
    expect(screen.getByTestId("pdf-page-indicator").textContent).toBe("1 / 3");
  });

  it("frames the page the pointer went down on and hands its number over", async () => {
    const { stage, onAddRegionToChat } = await renderPicker();
    dragRegion(stage!);

    const box = screen.getByTestId("pdf-column-selection");
    expect(box.style.left).toBe("10px");
    expect(box.style.width).toBe("101px");

    fireEvent.click(screen.getByRole("button", { name: "Add selection to Chat" }));
    await waitFor(() => expect(onAddRegionToChat).toHaveBeenCalledTimes(1));
    const [file, info] = onAddRegionToChat.mock.calls[0];
    expect(file).toBeInstanceOf(File);
    expect(file.type).toBe("image/png");
    expect(file.name).toBe("report-region-101x51.png");
    expect(info).toEqual({ page: 1 });
  });

  it("frames a lower page without leaving the column", async () => {
    // The whole point of the continuous layout: page 2 is reached by scrolling,
    // not by a mode switch. Column y 320 is 8 px down page 2 (its band starts
    // at 312), and the crop must carry page 2's number.
    const { stage, rendered, onAddRegionToChat } = await renderPicker();
    await measureViewport(PAGE_H);
    await waitFor(() => expect(rendered).toContain(2));

    drag(stage!, [10, 320], [110, 370]);

    const box = screen.getByTestId("pdf-column-selection");
    // The overlay is positioned inside ITS OWN page box, so these are page-2
    // pixels, not column pixels.
    expect(box.style.left).toBe("10px");
    expect(box.style.top).toBe("8px");
    expect(box.style.width).toBe("101px");
    expect(box.style.height).toBe("51px");

    fireEvent.click(screen.getByRole("button", { name: "Add selection to Chat" }));
    await waitFor(() => expect(onAddRegionToChat).toHaveBeenCalledTimes(1));
    expect(onAddRegionToChat.mock.calls[0][1]).toEqual({ page: 2 });
  });

  it("clamps a drag that wanders past its page's edge back onto that page", async () => {
    // Start on page 1, release at column y 500 — which is page 2's band. A crop
    // is one bitmap, so the selection stays on page 1 and stops at its last
    // addressable row (299), and the page number that travels is 1.
    const { stage, onAddRegionToChat } = await renderPicker();
    drag(stage!, [10, 10], [110, 500]);

    const box = screen.getByTestId("pdf-column-selection");
    expect(box.style.top).toBe("10px");
    expect(box.style.height).toBe(`${PAGE_H - 1 - 10 + 1}px`);

    fireEvent.click(screen.getByRole("button", { name: "Add selection to Chat" }));
    await waitFor(() => expect(onAddRegionToChat).toHaveBeenCalledTimes(1));
    expect(onAddRegionToChat.mock.calls[0][1]).toEqual({ page: 1 });
  });

  it("crops a page that has scrolled out of the paint window", async () => {
    // The framing outlives its page's canvases: scroll far enough and page 1 is
    // evicted, but the selection stands. A button that quietly did nothing was
    // the bug this pins — the page is rendered on demand instead.
    const { stage, rendered } = await renderPicker();
    await measureViewport(PAGE_H);
    dragRegion(stage!);
    await scrollColumn(PITCH * 2);
    await waitFor(() =>
      expect(screen.getAllByTestId("pdf-column-canvas-holder")[0].firstElementChild).toBeNull(),
    );

    fireEvent.click(screen.getByRole("button", { name: "Add selection to Chat" }));
    await waitFor(() => expect(screen.queryByTestId("pdf-column-selection")).toBeNull());
    expect(rendered.filter((page) => page === 1)).toHaveLength(2);
  });

  it("treats a released wobble as a click, not a crop", async () => {
    const { stage } = await renderPicker();
    drag(stage!, [10, 10], [11, 11]);
    expect(screen.queryByTestId("pdf-column-selection")).toBeNull();
  });

  it("keeps the mode open after a hand-off, so a page can be shown twice", async () => {
    const { stage, onAddRegionToChat } = await renderPicker();
    dragRegion(stage!);
    fireEvent.click(screen.getByRole("button", { name: "Add selection to Chat" }));
    await waitFor(() => expect(onAddRegionToChat).toHaveBeenCalledTimes(1));
    // The framing is dropped (the next thing on the page is a new region), but
    // the column stays where it was.
    expect(screen.queryByTestId("pdf-column-selection")).toBeNull();
    expect(screen.getByTestId("pdf-column")).toBeTruthy();
  });

  it("disables the hand-off — toolbar and menu alike — when no composer can take it", async () => {
    const { stage } = await renderPicker({ addToChatDisabledTitle: "No chat session" });
    dragRegion(stage!);
    const button = screen.getByRole("button", { name: "Add selection to Chat" }) as HTMLButtonElement;
    expect(button.disabled).toBe(true);
    expect(button.getAttribute("title")).toBe("No chat session");

    fireEvent.contextMenu(stage!);
    // Do not carry that label while the menu is open — the toolbar one
    // and the item; the menu renders last.
    const item = screen
      .getAllByText("Add selection to Chat")
      .at(-1)!
      .closest("button") as HTMLButtonElement;
    expect(item.disabled).toBe(true);
    expect(item.getAttribute("title")).toBe("No chat session");
  });

  it("clears a committed selection with Escape before leaving the mode", async () => {
    const { stage, onClose } = await renderPicker();
    dragRegion(stage!);
    fireEvent.keyDown(window, { key: "Escape" });
    expect(screen.queryByTestId("pdf-column-selection")).toBeNull();
    expect(onClose).not.toHaveBeenCalled();
    fireEvent.keyDown(window, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("reports a failed crop instead of attaching an empty file", async () => {
    vi.spyOn(HTMLCanvasElement.prototype, "toBlob").mockImplementation(function (
      this: HTMLCanvasElement,
      callback: BlobCallback,
    ) {
      callback(null);
    });
    const { stage, onAddRegionToChat } = await renderPicker();
    dragRegion(stage!);
    fireEvent.click(screen.getByRole("button", { name: "Add selection to Chat" }));
    await waitFor(() => expect(toast).toHaveBeenCalled());
    expect(onAddRegionToChat).not.toHaveBeenCalled();
  });
});

describe("PdfColumnViewer column scrolling", () => {
  it("reads the page at the top of the viewport", async () => {
    await renderPicker();
    await measureViewport(PAGE_H);

    await scrollColumn(PITCH);
    await waitFor(() => expect(screen.getByTestId("pdf-page-indicator").textContent).toBe("2 / 3"));
  });

  it("scrolls on a plain wheel and zooms only on ctrl + wheel", async () => {
    await renderPicker();
    const viewport = await measureViewport();
    const zoom = () => screen.getByTestId("pdf-zoom-level").textContent;
    expect(zoom()).toBe("100%");

    const wheel = new WheelEvent("wheel", { deltaY: 120, bubbles: true, cancelable: true });
    await act(async () => {
        viewport.dispatchEvent(wheel);
    });
    // Not cancelled means the browser scrolls it — the column is the only
    // surface here with somewhere to go.
    expect(wheel.defaultPrevented).toBe(false);
    expect(zoom()).toBe("100%");

    const pinch = new WheelEvent("wheel", {
        deltaY: -120,
        ctrlKey: true,
        bubbles: true,
        cancelable: true,
    });
    await act(async () => {
        viewport.dispatchEvent(pinch);
    });
    expect(pinch.defaultPrevented).toBe(true);
    expect(zoom()).not.toBe("100%");
  });

  it("moves the column to the next page instead of swapping it", async () => {
    // The arrows are a shortcut to a page, not a different viewer: after the
    // click, every page box is still in the DOM and the one above it is where
    // the user was.
    const { stage } = await renderPicker();
    await measureViewport(PAGE_H);
    dragRegion(stage!);

    fireEvent.click(screen.getByRole("button", { name: "Next page" }));

    await waitFor(() => expect(screen.getByTestId("pdf-page-indicator").textContent).toBe("2 / 3"));
    expect(screen.getAllByTestId("pdf-column-page")).toHaveLength(PAGES);
    expect(screen.getByTestId("pdf-column-viewport").scrollTop).toBe(PITCH);
    // The framing was on page 1 and page 1 still exists — the selection is not
    // a paging artefact any more.
    expect(screen.getByTestId("pdf-column-selection")).toBeTruthy();
  });

  it("cannot walk past either end of the column", async () => {
    await renderPicker();
    await measureViewport(PAGE_H);
    const prev = () => screen.getByRole("button", { name: "Previous page" }) as HTMLButtonElement;
    const next = () => screen.getByRole("button", { name: "Next page" }) as HTMLButtonElement;
    expect(prev().disabled).toBe(true);

    fireEvent.click(next());
    await waitFor(() => expect(screen.getByTestId("pdf-page-indicator").textContent).toBe("2 / 3"));
    fireEvent.click(next());
    await waitFor(() => expect(screen.getByTestId("pdf-page-indicator").textContent).toBe("3 / 3"));
    expect(next().disabled).toBe(true);
    expect(prev().disabled).toBe(false);
  });
});
