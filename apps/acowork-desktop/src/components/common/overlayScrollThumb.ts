//! The app's one scrollbar — geometry, measurement and interaction.
//!
//! `OverlayScrollbar` is the *delivery form*; this module is the behaviour it
//! and every other delivery form share. Two forms exist because a scroll
//! container is not always ours to wrap:
//!
//! - `OverlayScrollbar` (wrapper): the component owns the scrolling element,
//!   so it renders a viewport div + the thumb. Used wherever the scroller is
//!   plain JSX.
//! - `OverlayScrollTarget` (attach): the scrolling element belongs to someone
//!   else — a `<textarea>` whose own content scrolls internally, or a
//!   third-party host (Monaco, panzoom, a virtualizer). It paints the same
//!   thumb over an element the wrapper cannot wrap.
//!
//! Both consume `useOverlayScrollThumb` below, so the thumb geometry, the drag
//! maths and the keyboard stepping are defined exactly once, and a change to
//! the skin (`.overlay-scrollbar*` in globals.css) or to the behaviour lands
//! everywhere at once.
//!
//! ## The class contract (why the constants below exist)
//!
//! The CSS reveal rules are written against an ancestor:
//! `.overlay-scrollbar:hover .overlay-scrollbar-track`. An attach-form host
//! therefore has to carry `OVERLAY_SCROLL_HOST_CLASS` on a positioned ancestor
//! and `OVERLAY_SCROLL_VIEWPORT_CLASS` on the scroller itself. Hard-coding
//! those strings at ~80 call sites is how the previous drift started, so they
//! are exported here.
import { useCallback, useEffect, useId, useRef, useState, type KeyboardEvent, type PointerEvent } from "react";

/** Positioned ancestor of the scroller: reveals the thumb on hover/focus and
 *  anchors it (`.overlay-scrollbar-track` is `position: absolute`). */
export const OVERLAY_SCROLL_HOST_CLASS = "overlay-scrollbar relative";

/** The scroller itself: hides the native bar, keeps wheel/keyboard scrolling. */
export const OVERLAY_SCROLL_VIEWPORT_CLASS = "overlay-scrollbar-viewport";

export interface ThumbMetrics {
  height: number;
  top: number;
  maxScroll: number;
  maxTop: number;
}

export interface XThumbMetrics {
  width: number;
  left: number;
  maxScroll: number;
  maxLeft: number;
}

export const MIN_THUMB_HEIGHT = 20;

export function getOverlayThumbMetrics(clientHeight: number, scrollHeight: number, scrollTop: number): ThumbMetrics {
  const maxScroll = Math.max(0, scrollHeight - clientHeight);
  const height = maxScroll === 0 ? clientHeight : Math.max(MIN_THUMB_HEIGHT, clientHeight * clientHeight / scrollHeight);
  const maxTop = Math.max(0, clientHeight - height);
  return { height, top: maxScroll === 0 ? 0 : scrollTop / maxScroll * maxTop, maxScroll, maxTop };
}

const EMPTY_Y: ThumbMetrics = { height: 0, top: 0, maxScroll: 0, maxTop: 0 };
const EMPTY_X: XThumbMetrics = { width: 0, left: 0, maxScroll: 0, maxLeft: 0 };

function sameMetrics(a: ThumbMetrics, b: ThumbMetrics): boolean {
  return a.height === b.height && a.top === b.top &&
    a.maxScroll === b.maxScroll && a.maxTop === b.maxTop;
}

export interface OverlayScrollThumbOptions {
  /** Also track the horizontal axis (long un-truncated paths, wide tables).
   *  Opt-in: most viewports clip on x (`overflow-x-hidden`), where an x-thumb
   *  would be dead weight. */
  showXThumb?: boolean;
}

export interface OverlayThumbBinding {
  metrics: ThumbMetrics;
  /** Everything to spread on the vertical thumb div. */
  props: {
    style: { height: number; transform: string };
    onPointerDown: (event: PointerEvent<HTMLDivElement>) => void;
    onPointerMove: (event: PointerEvent<HTMLDivElement>) => void;
    onPointerUp: () => void;
    onPointerCancel: () => void;
    onKeyDown: (event: KeyboardEvent<HTMLDivElement>) => void;
    role: "scrollbar";
    "aria-controls": string;
    "aria-orientation": "vertical";
    "aria-valuemin": number;
    "aria-valuemax": number;
    "aria-valuenow": number;
  };
}

export interface OverlayXThumbBinding {
  metrics: XThumbMetrics;
  props: {
    style: { width: number; transform: string };
    onPointerDown: (event: PointerEvent<HTMLDivElement>) => void;
    onPointerMove: (event: PointerEvent<HTMLDivElement>) => void;
    onPointerUp: () => void;
    onPointerCancel: () => void;
    role: "scrollbar";
    "aria-controls": string;
    "aria-orientation": "horizontal";
    "aria-valuemin": number;
    "aria-valuemax": number;
    "aria-valuenow": number;
  };
}

export interface OverlayScrollThumbState {
  /** Pass as a `ref` (or feed with an effect) to the scrolling element. */
  setTarget: (element: HTMLElement | null) => void;
  /** Id of the scrolling element, for `aria-controls`. */
  viewportId: string;
  thumb: OverlayThumbBinding;
  /** `undefined` unless `showXThumb` — render nothing then. */
  thumbX: OverlayXThumbBinding | undefined;
  /** True when the scroller can actually move on the y-axis. */
  scrollable: boolean;
}

/**
 * Measure `target` and expose the thumb's geometry + handlers.
 *
 * Measurement is driven by every signal that can change the answer: the
 * scroller's own size (ResizeObserver), its content (MutationObserver — a
 * message streaming in does not resize the box) and scrolling (native
 * `scroll` listener, so the attach form needs no injected handler). `input` is
 * what covers the attach form's `<textarea>`: typing grows its content without
 * a DOM mutation and, once the box is capped, without a scroll either — the
 * caret only scrolls once it leaves the visible area.
 */
export function useOverlayScrollThumb(options: OverlayScrollThumbOptions = {}): OverlayScrollThumbState {
  const { showXThumb } = options;
  const [target, setTarget] = useState<HTMLElement | null>(null);
  const [metrics, setMetrics] = useState<ThumbMetrics>(EMPTY_Y);
  const [metricsX, setMetricsX] = useState<XThumbMetrics>(EMPTY_X);
  const dragRef = useRef<{ pointerY: number; scrollTop: number } | null>(null);
  const dragXRef = useRef<{ pointerX: number; scrollLeft: number } | null>(null);
  const viewportId = useId();

  const updateMetrics = useCallback(() => {
    const el = target;
    if (!el) return;
    const { clientHeight, scrollHeight, scrollTop, clientWidth, scrollWidth, scrollLeft } = el;
    // Identity-stable writes: the observers below fire in bursts (one mutation
    // per streamed token), and a fresh object per measurement would re-render
    // on every burst with identical numbers. It also makes the post-render
    // re-measure safe — an unconditional `setMetrics` there would loop.
    const y = getOverlayThumbMetrics(clientHeight, scrollHeight, scrollTop);
    setMetrics((prev) => (sameMetrics(prev, y) ? prev : y));
    if (showXThumb) {
      // Same geometry maths, just fed the horizontal axis; the result is
      // remapped into width/left field names for readability at the use site.
      const x = getOverlayThumbMetrics(clientWidth, scrollWidth, scrollLeft);
      const next: XThumbMetrics = { width: x.height, left: x.top, maxScroll: x.maxScroll, maxLeft: x.maxTop };
      setMetricsX((prev) =>
        prev.width === next.width && prev.left === next.left &&
        prev.maxScroll === next.maxScroll && prev.maxLeft === next.maxLeft
          ? prev
          : next,
      );
    }
  }, [showXThumb, target]);

  useEffect(() => {
    if (!target) return;
    const resize = new ResizeObserver(updateMetrics);
    resize.observe(target);
    // The first child is the content box for the wrapper form; observing it
    // catches "content grew, scroller box did not", which a box-only observer
    // misses.
    if (target.firstElementChild) resize.observe(target.firstElementChild);
    const mutations = new MutationObserver(updateMetrics);
    mutations.observe(target, { childList: true, subtree: true, characterData: true });
    // A virtualizer grows its total-size wrapper by writing inline `height`
    // (attribute-only, no child mutation). The content-box ResizeObserver above
    // already reports the resulting resize; observing the style attribute too
    // covers hosts where the write lands on a box we do not observe. This is
    // the event-driven signal the old per-commit re-measure was hacking around.
    if (target.firstElementChild) {
      mutations.observe(target.firstElementChild, { attributes: true, attributeFilter: ["style"] });
    }
    target.addEventListener("scroll", updateMetrics, { passive: true });
    target.addEventListener("input", updateMetrics, { passive: true });
    updateMetrics();
    return () => {
      resize.disconnect();
      mutations.disconnect();
      target.removeEventListener("scroll", updateMetrics);
      target.removeEventListener("input", updateMetrics);
    };
  }, [target, updateMetrics]);

  // NO per-commit re-measure here. A deps-less `useEffect(updateMetrics)` looks
  // harmless because `updateMetrics` writes identity-stable metrics — but that
  // only bails while geometry is STILL. Mid-stream the virtualizer's directDom
  // height writes and the bottom-pin's scrollTop writes keep geometry in flux,
  // so every commit measured a NEW value and setMetrics re-rendered. Under a
  // discrete event (workspace dblclick) React runs passive effects at sync lane,
  // so that setState spawned a synchronous commit → new geometry → new setState:
  // the 23s frozen cascade. The thumb is a read-only projection of geometry; it
  // must be driven ONLY by the observers above (resize / mutation / scroll /
  // input), which fire when geometry actually changes and settle to silence.

  const onThumbPointerDown = (event: PointerEvent<HTMLDivElement>) => {
    if (event.button !== 0 || metrics.maxScroll === 0 || !target) return;
    event.preventDefault();
    dragRef.current = { pointerY: event.clientY, scrollTop: target.scrollTop };
    event.currentTarget.setPointerCapture(event.pointerId);
  };

  const onThumbPointerMove = (event: PointerEvent<HTMLDivElement>) => {
    const drag = dragRef.current;
    if (!drag || !target || metrics.maxTop === 0) return;
    target.scrollTop = drag.scrollTop + (event.clientY - drag.pointerY) * metrics.maxScroll / metrics.maxTop;
  };

  const onThumbXPointerDown = (event: PointerEvent<HTMLDivElement>) => {
    if (event.button !== 0 || metricsX.maxScroll === 0 || !target) return;
    event.preventDefault();
    dragXRef.current = { pointerX: event.clientX, scrollLeft: target.scrollLeft };
    event.currentTarget.setPointerCapture(event.pointerId);
  };

  const onThumbXPointerMove = (event: PointerEvent<HTMLDivElement>) => {
    const drag = dragXRef.current;
    if (!drag || !target || metricsX.maxLeft === 0) return;
    target.scrollLeft = drag.scrollLeft + (event.clientX - drag.pointerX) * metricsX.maxScroll / metricsX.maxLeft;
  };

  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (!target) return;
    const step = Math.max(40, target.clientHeight * 0.9);
    switch (event.key) {
      case "ArrowDown": target.scrollTop += 40; break;
      case "ArrowUp": target.scrollTop -= 40; break;
      case "PageDown": target.scrollTop += step; break;
      case "PageUp": target.scrollTop -= step; break;
      case "Home": target.scrollTop = 0; break;
      case "End": target.scrollTop = target.scrollHeight; break;
      default: return;
    }
    event.preventDefault();
  };

  return {
    setTarget,
    viewportId,
    scrollable: metrics.maxScroll > 0,
    thumb: {
      metrics,
      props: {
        style: { height: metrics.height, transform: `translateY(${metrics.top}px)` },
        onPointerDown: onThumbPointerDown,
        onPointerMove: onThumbPointerMove,
        onPointerUp: () => { dragRef.current = null; },
        onPointerCancel: () => { dragRef.current = null; },
        onKeyDown,
        role: "scrollbar",
        "aria-controls": viewportId,
        "aria-orientation": "vertical",
        "aria-valuemin": 0,
        "aria-valuemax": Math.round(metrics.maxScroll),
        "aria-valuenow": Math.round(target?.scrollTop ?? 0),
      },
    },
    thumbX: showXThumb
      ? {
        metrics: metricsX,
        props: {
          style: { width: metricsX.width, transform: `translateX(${metricsX.left}px)` },
          onPointerDown: onThumbXPointerDown,
          onPointerMove: onThumbXPointerMove,
          onPointerUp: () => { dragXRef.current = null; },
          onPointerCancel: () => { dragXRef.current = null; },
          role: "scrollbar",
          "aria-controls": viewportId,
          "aria-orientation": "horizontal",
          "aria-valuemin": 0,
          "aria-valuemax": Math.round(metricsX.maxScroll),
          "aria-valuenow": Math.round(target?.scrollLeft ?? 0),
        },
      }
      : undefined,
  };
}

/**
 * Resolve an element that a consumer owns through a ref (`<textarea ref={r}>`).
 *
 * The read runs after *every* render on purpose: a ref holds no signal we can
 * depend on, the node is populated during the consumer's commit (not ours),
 * and a remount swaps the node without changing any prop. The `seen` guard
 * keeps the steady state free — same node, no state write, no extra render.
 */
export function useRefElement(targetRef: { current: HTMLElement | null }): HTMLElement | null {
  const [element, setElement] = useState<HTMLElement | null>(null);
  const seenRef = useRef<HTMLElement | null>(null);
  // deps-less-ok: late-ref read guarded by seenRef — same node bails, no state
  // write, no extra render in steady state.
  useEffect(() => {
    const node = targetRef.current;
    if (seenRef.current === node) return;
    seenRef.current = node;
    setElement(node);
  });
  return element;
}
