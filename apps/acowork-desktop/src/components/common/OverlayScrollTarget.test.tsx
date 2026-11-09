/**
 * The attach form of the app's scrollbar — the one that paints the floating
 * thumb over a scroller it does not own.
 *
 * Why this test exists: jsdom has no layout, so every other overlay-scroll
 * test mounts a component and asserts nothing about the thumb (the setup stub
 * keeps `ResizeObserver` inert and the metrics stay at zero). That leaves the
 * *new* delivery form — the one a `<textarea>` composer depends on, because its
 * content scrolls inside the element and there is no child box to wrap —
 * completely unverified: a broken measurement, a wrong axis or a thumb that
 * never renders would all pass. Here the target's geometry is stubbed, the
 * native scroll/input signals are fired for real, and the rendered thumb is
 * checked against the shared geometry function.
 */
import { act, render, screen } from "@testing-library/react";
import { useRef } from "react";
import { describe, expect, it, vi } from "vitest";
import { OverlayScrollTarget } from "./OverlayScrollbar";
import { OVERLAY_SCROLL_HOST_CLASS, OVERLAY_SCROLL_VIEWPORT_CLASS, getOverlayThumbMetrics } from "./overlayScrollThumb";

/** Give an element a fixed scroll geometry (jsdom reports 0 for all of it). */
function stubGeometry(el: HTMLElement, box: { clientHeight: number; scrollHeight: number; scrollTop: number }) {
  for (const [key, value] of Object.entries(box)) {
    Object.defineProperty(el, key, { configurable: true, value });
  }
}

function Harness({ clientHeight, scrollHeight }: { clientHeight: number; scrollHeight: number }) {
  const ref = useRef<HTMLDivElement | null>(null);
  return (
    <div className={OVERLAY_SCROLL_HOST_CLASS}>
      <div
        ref={(node) => {
          ref.current = node;
          if (node) stubGeometry(node, { clientHeight, scrollHeight, scrollTop: 0 });
        }}
        data-testid="scroller"
        className={OVERLAY_SCROLL_VIEWPORT_CLASS}
      />
      <OverlayScrollTarget targetRef={ref} />
    </div>
  );
}

describe("OverlayScrollTarget", () => {
  it("renders no thumb when the target does not overflow", () => {
    render(<Harness clientHeight={200} scrollHeight={200} />);
    expect(screen.queryByRole("scrollbar", { hidden: true })).toBeNull();
  });

  it("sizes the thumb with the shared geometry and follows the target's scroll", () => {
    render(<Harness clientHeight={100} scrollHeight={500} />);
    const scroller = screen.getByTestId("scroller");
    const thumb = screen.getByRole("scrollbar", { hidden: true });

    const expected = getOverlayThumbMetrics(100, 500, 0);
    expect(thumb.style.height).toBe(`${expected.height}px`);
    expect(thumb.getAttribute("aria-controls")).toBe(scroller.id);
    expect(thumb.getAttribute("aria-orientation")).toBe("vertical");

    // Scroll → the native listener re-measures and moves the thumb.
    Object.defineProperty(scroller, "scrollTop", { configurable: true, value: 200 });
    act(() => {
      scroller.dispatchEvent(new Event("scroll"));
    });
    expect(thumb.style.transform).toBe(`translateY(${getOverlayThumbMetrics(100, 500, 200).top}px)`);
  });

  it("re-measures when the target's content grows without scrolling (typing)", () => {
    render(<Harness clientHeight={100} scrollHeight={500} />);
    const scroller = screen.getByTestId("scroller");
    const thumb = screen.getByRole("scrollbar", { hidden: true });

    // A textarea at its height cap grows its content without a box resize and
    // without a scroll — `input` is the only signal.
    Object.defineProperty(scroller, "scrollHeight", { configurable: true, value: 1000 });
    act(() => {
      scroller.dispatchEvent(new Event("input"));
    });
    expect(thumb.style.height).toBe(`${getOverlayThumbMetrics(100, 1000, 0).height}px`);
  });

  it("drag maps pointer travel onto scroll position", () => {
    render(<Harness clientHeight={100} scrollHeight={500} />);
    const scroller = screen.getByTestId("scroller");
    const thumb = screen.getByRole("scrollbar", { hidden: true });
    const scrollTo = vi.fn();
    Object.defineProperty(scroller, "scrollTop", { configurable: true, get: () => 0, set: scrollTo });
    // jsdom has no pointer capture; the handler calls it unconditionally.
    (thumb as HTMLElement).setPointerCapture = vi.fn();

    act(() => {
      thumb.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true, clientY: 10, button: 0 }));
      thumb.dispatchEvent(new MouseEvent("pointermove", { bubbles: true, clientY: 50 }));
    });
    // maxScroll 400 spread over maxTop 80 → 5 px of scroll per px dragged.
    expect(scrollTo).toHaveBeenCalledWith(200);
  });
});
