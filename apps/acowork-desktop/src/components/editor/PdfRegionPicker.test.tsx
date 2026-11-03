/**
 * The PDF region picker — the gesture, not the engine.
 *
 * pdf.js is never loaded here: `openDocument` is injectable exactly so this
 * file can drive the interaction over a stub page. What is worth pinning is
 * the wiring between a pointer and a pixel, because that mapping is the whole
 * claim the feature makes ("the agent sees what you framed"):
 *
 *   1. the document is opened once, page 1 is painted into a canvas, and the
 *      page readout shows what the document reported — and a remount that
 *      re-opens the SAME bytes (what React StrictMode does in dev) still ends
 *      on a painted page, with the abandoned document released;
 *   2. a drag becomes a selection box in the SAME space the crop is measured
 *      in, and releasing under `MIN_SELECTION_PX` is treated as a click;
 *   3. the hand-off is a PNG `File` named after the source plus the region,
 *      with the page number beside it;
 *   4. paging re-renders and drops the framing (a selection has no meaning on
 *      a page that is gone);
 *   5. leaving the mode releases the document — a 30-page PDF must not stay
 *      parsed because someone previewed a corner of it;
 *   6. an unreachable composer disables the hand-off instead of swallowing the
 *      gesture, and a document that will not open says so in the body.
 *
 * Geometry itself is `imageViewerGeometry.test.ts`'s business; this is the
 * same contract `ImageViewer.test.tsx` pins, applied to a rendered page.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { PdfRegionPicker, type PdfRegionPickerProps } from "./PdfRegionPicker";
import type { PdfPageRenderer } from "./pdfRasterize";

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

/** The page bitmap the stub renders: 400 × 300, so a drag maps 1:1 at the
 *  fitted scale jsdom produces (an unmeasured viewport fits at 1). */
const PAGE_W = 400;
const PAGE_H = 300;

const STAGE_RECT = {
  left: 0,
  top: 0,
  right: PAGE_W,
  bottom: PAGE_H,
  width: PAGE_W,
  height: PAGE_H,
  x: 0,
  y: 0,
  toJSON: () => ({}),
} as DOMRect;

beforeEach(() => {
  cleanup();
  toast.mockClear();
  // jsdom has no canvas: stub the 2-D context and the PNG encoder, the same
  // way the image viewer's tests do — the crop under test is real code.
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

function stubRenderer(numPages = 3) {
  const rendered: number[] = [];
  const renderer: PdfPageRenderer = {
    numPages,
    async render(pageNumber) {
      rendered.push(pageNumber);
      const canvas = document.createElement("canvas");
      canvas.width = PAGE_W;
      canvas.height = PAGE_H;
      return canvas;
    },
    destroy: vi.fn(async () => {}),
  };
  return { renderer, rendered };
}

async function renderPicker(
  overrides: Partial<PdfRegionPickerProps> = {},
  options: { waitForPage?: boolean; strictMode?: boolean } = {},
) {
  const bytes = new Uint8Array([0x25, 0x50, 0x44, 0x46]);
  const { renderer, rendered } = stubRenderer();
  const openDocument = vi.fn(async () => renderer);
  const onAddRegionToChat = vi.fn();
  const onClose = vi.fn();
  const props: PdfRegionPickerProps = {
    bytes,
    fileName: "report.pdf",
    addToChatDisabledTitle: null,
    onAddRegionToChat,
    onClose,
    openDocument,
    ...overrides,
  };
  const view = render(<PdfRegionPicker {...props} />, { reactStrictMode: options.strictMode });
  const stage =
    // The page box only exists once the stub has painted: awaiting it is the
    // test's version of "the first page is on screen". A test that mounts a
    // document which never opens has nothing to wait for.
    options.waitForPage === false
      ? null
      : await (async () => {
          const found = await screen.findByTestId("pdf-region-stage");
          vi.spyOn(found, "getBoundingClientRect").mockReturnValue(STAGE_RECT);
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
    ...view,
  };
}

const dragRegion = (stage: HTMLElement) => {
  fireEvent.pointerDown(stage, { clientX: 10, clientY: 10, button: 0, pointerId: 1 });
  fireEvent.pointerMove(stage, { clientX: 110, clientY: 60 });
  fireEvent.pointerUp(stage, { clientX: 110, clientY: 60, pointerId: 1 });
};

describe("PdfRegionPicker page rendering", () => {
  it("opens the document once and paints page 1 at its own size", async () => {
    const { bytes, rendered, openDocument } = await renderPicker();
    expect(openDocument).toHaveBeenCalledTimes(1);
    expect(openDocument).toHaveBeenCalledWith(bytes);
    expect(rendered).toEqual([1]);
    expect(screen.getByTestId("pdf-page-indicator").textContent).toBe("1 / 3");
    const canvas = screen.getByTestId("pdf-region-canvas-holder").firstElementChild as HTMLCanvasElement;
    expect(canvas.width).toBe(PAGE_W);
    expect(canvas.height).toBe(PAGE_H);
    // The stage is the bitmap box, so pointer→pixel is one division by scale.
    const stage = screen.getByTestId("pdf-region-stage");
    expect(stage.style.width).toBe(`${PAGE_W}px`);
    expect(stage.style.height).toBe(`${PAGE_H}px`);
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
    expect(screen.queryByTestId("pdf-region-stage")).toBeNull();
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

describe("PdfRegionPicker page navigation", () => {
  it("renders the next page and clears the framing with it", async () => {
    const { rendered, stage } = await renderPicker();
    dragRegion(stage);
    expect(screen.getByTestId("pdf-region-selection")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Next page" }));
    await waitFor(() => expect(rendered).toEqual([1, 2]));
    expect(screen.getByTestId("pdf-page-indicator").textContent).toBe("2 / 3");
    // A selection measured on page 1 means nothing on page 2.
    expect(screen.queryByTestId("pdf-region-selection")).toBeNull();
  });

  it("cannot walk past either end of the document", async () => {
    await renderPicker();
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

describe("PdfRegionPicker selection and hand-off", () => {
  it("hands over a PNG crop of the framed area, with its page", async () => {
    const { stage, onAddRegionToChat } = await renderPicker();
    dragRegion(stage);

    const box = screen.getByTestId("pdf-region-selection");
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

  it("treats a released wobble as a click, not a crop", async () => {
    const { stage, onAddRegionToChat } = await renderPicker();
    fireEvent.pointerDown(stage, { clientX: 10, clientY: 10, button: 0, pointerId: 1 });
    fireEvent.pointerUp(stage, { clientX: 11, clientY: 11, pointerId: 1 });
    expect(screen.queryByTestId("pdf-region-selection")).toBeNull();

    const add = screen.getByRole("button", { name: "Add selection to Chat" }) as HTMLButtonElement;
    expect(add.disabled).toBe(true);
    fireEvent.click(add);
    expect(onAddRegionToChat).not.toHaveBeenCalled();
  });

  it("keeps the framing after a successful hand-off, so a page can be shown twice", async () => {
    const { stage, onAddRegionToChat } = await renderPicker();
    dragRegion(stage);
    fireEvent.click(screen.getByRole("button", { name: "Add selection to Chat" }));
    await waitFor(() => expect(onAddRegionToChat).toHaveBeenCalledTimes(1));
    // The box is dropped but the mode stays: one page usually holds more than
    // one thing worth sending.
    expect(screen.queryByTestId("pdf-region-selection")).toBeNull();
    expect(screen.getByTestId("pdf-region-stage")).toBeTruthy();
  });

  it("disables the hand-off — toolbar and menu alike — when no composer can take it", async () => {
    const { stage, onAddRegionToChat } = await renderPicker({
      addToChatDisabledTitle: "No chat session",
    });
    dragRegion(stage);
    const add = screen.getByRole("button", { name: "Add selection to Chat" }) as HTMLButtonElement;
    expect(add.disabled).toBe(true);
    expect(add.getAttribute("title")).toBe("No chat session");
    fireEvent.click(add);
    expect(onAddRegionToChat).not.toHaveBeenCalled();

    fireEvent.contextMenu(stage, { clientX: 60, clientY: 40 });
    // Two buttons carry that label while the menu is open — the toolbar one
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
    dragRegion(stage);
    fireEvent.keyDown(window, { key: "Escape" });
    expect(screen.queryByTestId("pdf-region-selection")).toBeNull();
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
    dragRegion(stage);
    fireEvent.click(screen.getByRole("button", { name: "Add selection to Chat" }));
    await waitFor(() => expect(toast).toHaveBeenCalledTimes(1));
    expect(onAddRegionToChat).not.toHaveBeenCalled();
  });
});
