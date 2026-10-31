// @vitest-environment jsdom
/**
 * The workspace panel's image preview.
 *
 * What this pins, in the order the user meets it:
 *
 *   1. the image opens fitted, and the zoom controls (buttons AND wheel) move
 *      the percentage the user reads;
 *   2. the pointer readout is in IMAGE pixels — the numbers the agent's tools
 *      address — not screen pixels;
 *   3. right-click offers "Add coordinates to Chat", and choosing it hands the
 *      caller the point under the cursor;
 *   4. a drag draws a box and right-click then offers "Add selection to Chat",
 *      which hands over a cropped PNG `File` (canvas is stubbed — jsdom has no
 *      2-D context; what matters here is that the region reaches the crop);
 *   5. an unreachable composer disables both chat items instead of silently
 *      dropping the gesture.
 *
 * Geometry itself is covered by `imageViewerGeometry.test.ts`; this file is
 * about the wiring between the DOM and those functions.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { ImageViewer, type ImageViewerProps } from "./ImageViewer";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string, params?: Record<string, unknown>) => {
      const table: Record<string, string> = {
        "fileEditor.imageViewer.zoomIn": "Zoom in",
        "fileEditor.imageViewer.zoomOut": "Zoom out",
        "fileEditor.imageViewer.fit": "Fit to panel",
        "fileEditor.imageViewer.cursorPoint": `x: ${params?.x}, y: ${params?.y}`,
        "fileEditor.imageViewer.cursorIdle": "—",
        "fileEditor.imageViewer.selectionSize": `${params?.width} × ${params?.height}`,
        "fileEditor.imageViewer.addPointToChat": "Add coordinates to Chat",
        "fileEditor.imageViewer.addRegionToChat": "Add selection to Chat",
        "fileEditor.imageViewer.clearSelection": "Clear selection",
        "fileEditor.imageViewer.captureFailed": "Could not capture the selection",
      };
      return table[key] ?? key;
    },
  }),
}));

const toast = vi.hoisted(() => vi.fn());
vi.mock("../common/ToastProvider", () => ({ showToast: toast }));

// jsdom ships no `PointerEvent`, and its `Event` fallback silently drops
// `clientX` / `button` — which is exactly the data every handler here reads.
// A MouseEvent subclass is enough: the component needs coordinates, button and
// pointerId, and pointer capture is optional-chained away.
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

/** jsdom has no canvas: stub the 2-D context and the blob encoder. */
beforeEach(() => {
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

const IMAGE_RECT = {
  left: 0,
  top: 0,
  right: 800,
  bottom: 600,
  width: 800,
  height: 600,
  x: 0,
  y: 0,
  toJSON: () => ({}),
} as DOMRect;

function renderViewer(overrides: Partial<ImageViewerProps> = {}) {
  const onAddPointToChat = vi.fn();
  const onAddRegionToChat = vi.fn();
  const props: ImageViewerProps = {
    src: "data:image/png;base64,AAAA",
    fileName: "photo.png",
    addToChatDisabledTitle: null,
    onAddPointToChat,
    onAddRegionToChat,
    ...overrides,
  };
  const view = render(<ImageViewer {...props} />);
  const image = screen.getByTestId("image-viewer-image") as HTMLImageElement;
  // jsdom never decodes: give the element the size the component reads.
  Object.defineProperty(image, "naturalWidth", { value: 800, configurable: true });
  Object.defineProperty(image, "naturalHeight", { value: 600, configurable: true });
  vi.spyOn(image, "getBoundingClientRect").mockReturnValue(IMAGE_RECT);
  fireEvent.load(image);
  return { ...view, image, onAddPointToChat, onAddRegionToChat };
}

const zoomLevel = () => screen.getByTestId("image-viewer-zoom-level").textContent;
const cursorReadout = () => screen.getByTestId("image-viewer-cursor").textContent;

describe("ImageViewer zoom", () => {
  it("opens fitted and shows the resulting percentage", () => {
    renderViewer();
    // 800×600 into an unmeasured (0×0) viewport fits at 1:1 — the state a
    // freshly mounted panel is in before its first layout pass.
    expect(zoomLevel()).toBe("100%");
  });

  it("steps with the buttons", () => {
    renderViewer();
    fireEvent.click(screen.getByRole("button", { name: "Zoom in" }));
    expect(zoomLevel()).toBe("125%");
    fireEvent.click(screen.getByRole("button", { name: "Zoom out" }));
    expect(zoomLevel()).toBe("100%");
  });

  it("zooms on a mouse wheel notch", () => {
    renderViewer();
    const viewport = screen.getByTestId("image-viewer-viewport");
    fireEvent.wheel(viewport, { deltaY: -100, clientX: 10, clientY: 10 });
    expect(zoomLevel()).toBe("125%");
    fireEvent.wheel(viewport, { deltaY: 100, clientX: 10, clientY: 10 });
    expect(zoomLevel()).toBe("100%");
  });

  it("returns to the fitted scale from the fit button", () => {
    renderViewer();
    fireEvent.click(screen.getByRole("button", { name: "Zoom in" }));
    fireEvent.click(screen.getByRole("button", { name: "Fit to panel" }));
    expect(zoomLevel()).toBe("100%");
  });
});

describe("ImageViewer cursor readout", () => {
  it("reports the pointer position in image pixels", () => {
    const { image } = renderViewer();
    fireEvent.pointerMove(image, { clientX: 100, clientY: 50 });
    expect(cursorReadout()).toContain("x: 100, y: 50");
  });

  it("divides by the current scale, so the number stays the image pixel", () => {
    const { image } = renderViewer();
    // 125%: 250 screen px into the image is pixel 200.
    fireEvent.click(screen.getByRole("button", { name: "Zoom in" }));
    expect(zoomLevel()).toBe("125%");
    fireEvent.pointerMove(image, { clientX: 250, clientY: 150 });
    expect(cursorReadout()).toContain("x: 200, y: 120");
  });

  it("clamps a pointer outside the image to the nearest edge pixel", () => {
    const { image } = renderViewer();
    fireEvent.pointerMove(image, { clientX: 5000, clientY: -40 });
    expect(cursorReadout()).toContain("x: 799, y: 0");
  });
});

describe("ImageViewer add-to-chat gestures", () => {
  it("sends the right-clicked coordinate", () => {
    const { image, onAddPointToChat } = renderViewer();
    fireEvent.contextMenu(image, { clientX: 10, clientY: 20 });
    fireEvent.click(screen.getByText("Add coordinates to Chat"));
    expect(onAddPointToChat).toHaveBeenCalledWith({ x: 10, y: 20 });
  });

  it("disables both chat items when no composer can take them", () => {
    const { image, onAddPointToChat } = renderViewer({
      addToChatDisabledTitle: "No chat session",
    });
    fireEvent.contextMenu(image, { clientX: 10, clientY: 20 });
    const item = screen.getByText("Add coordinates to Chat").closest("button");
    expect(item?.disabled).toBe(true);
    expect(item?.getAttribute("title")).toBe("No chat session");
    fireEvent.click(screen.getByText("Add coordinates to Chat"));
    expect(onAddPointToChat).not.toHaveBeenCalled();
  });

  it("sends a dragged region as a cropped PNG", async () => {
    const { image, onAddRegionToChat } = renderViewer();
    fireEvent.pointerDown(image, { clientX: 10, clientY: 10, button: 0, pointerId: 1 });
    fireEvent.pointerMove(image, { clientX: 110, clientY: 60 });
    fireEvent.pointerUp(image, { clientX: 110, clientY: 60, pointerId: 1 });

    const box = screen.getByTestId("image-viewer-selection");
    expect(box.style.left).toBe("10px");
    expect(box.style.width).toBe("101px");
    expect(cursorReadout()).toContain("101 × 51");

    fireEvent.contextMenu(image, { clientX: 60, clientY: 40 });
    fireEvent.click(screen.getByText("Add selection to Chat"));

    await waitFor(() => expect(onAddRegionToChat).toHaveBeenCalledTimes(1));
    const [file, region] = onAddRegionToChat.mock.calls[0];
    expect(region).toEqual({ x: 10, y: 10, width: 101, height: 51 });
    expect(file).toBeInstanceOf(File);
    expect(file.name).toBe("photo-region-101x51.png");
    expect(file.type).toBe("image/png");
  });

  it("drops a selection that was only a wobble", () => {
    const { image } = renderViewer();
    fireEvent.pointerDown(image, { clientX: 10, clientY: 10, button: 0, pointerId: 1 });
    fireEvent.pointerUp(image, { clientX: 11, clientY: 11, pointerId: 1 });
    expect(screen.queryByTestId("image-viewer-selection")).toBeNull();
  });

  it("clears a committed selection from the menu", () => {
    const { image } = renderViewer();
    fireEvent.pointerDown(image, { clientX: 10, clientY: 10, button: 0, pointerId: 1 });
    fireEvent.pointerMove(image, { clientX: 110, clientY: 60 });
    fireEvent.pointerUp(image, { clientX: 110, clientY: 60, pointerId: 1 });
    expect(screen.getByTestId("image-viewer-selection")).toBeTruthy();

    fireEvent.contextMenu(image, { clientX: 60, clientY: 40 });
    fireEvent.click(screen.getByText("Clear selection"));
    expect(screen.queryByTestId("image-viewer-selection")).toBeNull();
  });

  it("reports a failed crop instead of attaching an empty file", async () => {
    vi.spyOn(HTMLCanvasElement.prototype, "toBlob").mockImplementation(function (
      this: HTMLCanvasElement,
      callback: BlobCallback,
    ) {
      callback(null);
    });
    const { image, onAddRegionToChat } = renderViewer();
    fireEvent.pointerDown(image, { clientX: 10, clientY: 10, button: 0, pointerId: 1 });
    fireEvent.pointerMove(image, { clientX: 110, clientY: 60 });
    fireEvent.pointerUp(image, { clientX: 110, clientY: 60, pointerId: 1 });
    fireEvent.contextMenu(image, { clientX: 60, clientY: 40 });
    fireEvent.click(screen.getByText("Add selection to Chat"));

    await waitFor(() => expect(toast).toHaveBeenCalledTimes(1));
    expect(onAddRegionToChat).not.toHaveBeenCalled();
  });
});

describe("ImageViewer source toggle", () => {
  it("only wires double-click when the caller offers it", () => {
    const onOpenSource = vi.fn();
    const { rerender } = render(
      <ImageViewer
        src="data:image/svg+xml;base64,AAAA"
        fileName="logo.svg"
        addToChatDisabledTitle={null}
        onAddPointToChat={vi.fn()}
        onAddRegionToChat={vi.fn()}
      />,
    );
    fireEvent.doubleClick(screen.getByTestId("image-viewer-image"));
    expect(onOpenSource).not.toHaveBeenCalled();

    rerender(
      <ImageViewer
        src="data:image/svg+xml;base64,AAAA"
        fileName="logo.svg"
        onOpenSource={onOpenSource}
        openSourceHint="Double-click to open in editor"
        addToChatDisabledTitle={null}
        onAddPointToChat={vi.fn()}
        onAddRegionToChat={vi.fn()}
      />,
    );
    fireEvent.doubleClick(screen.getByTestId("image-viewer-image"));
    expect(onOpenSource).toHaveBeenCalledTimes(1);
  });
});