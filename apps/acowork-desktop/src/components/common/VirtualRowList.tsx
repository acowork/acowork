/**
 * VirtualRowList — the generic virtualized-row shell (chat-transcript
 * render-perf plan L2).
 *
 * Why this exists: the execution surface proved the mechanism works
 * (`VirtualMessageList`), but its props are welded to the exec data model
 * (adapter / MessageBlock / toolProgress / isThinking…), so each new
 * chat-like surface re-invented the list with a plain `.map` — and a long
 * transcript then re-renders every loaded row on every keystroke. The shell
 * extracts ONLY the reusable mechanism:
 *
 *   - viewport-only rendering via `useVirtualizer` (@tanstack/react-virtual);
 *   - measurement discipline: `measureElement` on mount + a per-element
 *     ResizeObserver re-measure for async content (markdown, images);
 *   - a module-level measured-height cache keyed by the adapter's item key,
 *     so estimates converge to real heights across remounts and re-opens;
 *   - bottom-pin convergence for `scrollToBottom` (a single jump cannot land
 *     on the bottom while never-measured rows above are under-estimated);
 *   - the WKWebView-safe `scrollToFn` (synchronous scrollTop assignment +
 *     forced scroll event) that `VirtualMessageList` had to learn the hard
 *     way;
 *   - an imperative handle with data-derived queries (first/last visible
 *     index/key, scrollToIndex/Key/Bottom/Top, isKeyInViewport).
 *
 * What the shell deliberately does NOT own:
 *   - row semantics — the adapter supplies `keyOf` / `renderItem` /
 *     `estimateSize` / per-row attrs;
 *   - store subscriptions, pagination triggers, follow-to-bottom policy —
 *     those live in the surface (`useStickToBottom`, `useScrollController`);
 *   - prepend scroll anchoring — it is a scroll-container-level
 *     scrollHeight-delta concern and already lives in the surface's follow
 *     controller; virtualization does not move it (scrollHeight equals the
 *     virtualizer's total size either way).
 *
 * Session switching: the parent MUST key this component by the session
 * identity (same contract `VirtualMessageList` documents at its header) —
 * a fresh mount creates a fresh virtualizer with `scrollOffset = 0`;
 * otherwise the retained offset can exceed the new session's total size and
 * `getVirtualItems()` renders an empty list (white pane).
 */
import React, {
  useCallback,
  useEffect,
  useImperativeHandle,
  useLayoutEffect,
  useReducer,
  useRef,
  useState,
} from "react";
import { useVirtualizer, type Virtualizer } from "@tanstack/react-virtual";
import { decideBottomPin } from "./bottomAnchor";

/** Fallback row height when neither the measured cache nor the adapter's
 *  estimator has an opinion. Generous enough that a short chat row
 *  under-fills rather than over-fills the estimate; measurements correct it
 *  on first mount. */
const DEFAULT_ROW_HEIGHT_PX = 64;

/** Bottom-pin tuning — the anchor re-pins on every commit until the layout
 *  settles (measurements land). MAX is a safety valve; the release rule
 *  lives in `bottomAnchor.ts`. */
const BOTTOM_PIN_MAX_MS = 2000;

// ResizeObserver instances per element. WeakMap so they're GC'd when the
// element is removed from the DOM (virtual list recycling).
const resizeObservers = new WeakMap<HTMLElement, ResizeObserver>();

// Module-level measured-height cache, keyed by the adapter's item key.
// Once an item has been in view, its real height is preserved across
// re-mounts (recycling) and re-opens of the same window — the same trick
// `blockHeightEstimator.ts` performs for exec blocks, generalized. Keys
// must be session-qualified by the adapter (e.g. `${sessionId}:${seq}`) so
// two sessions never share a height. Never cleared: bounded by the union
// of windows opened in the page lifetime (~MB worst case).
const measuredHeights = new Map<string, number>();

export interface VirtualRowListHandle {
  /** Index (into `items`) of the first row currently visible, or null while
   *  the virtualizer has no measured layout (immediately after mount). */
  getFirstVisibleIndex: () => number | null;
  /** `keyOf` of the first visible row, or null. Stable across re-renders. */
  getFirstVisibleKey: () => string | null;
  /** Index of the last row currently visible, or null. */
  getLastVisibleIndex: () => number | null;
  /** True when the row with this key is within the rendered window. Lets a
   *  surface gate per-row work (e.g. streaming refresh) on actual visibility. */
  isKeyInViewport: (key: string) => boolean;
  /** Jump to the last row and arm the bottom-pin convergence. Arms even when
   *  the virtualizer has not attached its scroll element yet (the late-ref
   *  window right after a session-switch remount): the pin effect reads the
   *  surface's container ref directly and rides every commit until the
   *  element exists and measurements settle, so an armed request is honoured
   *  without a caller-side retry loop. Returns false only on zero items. */
  scrollToBottom: () => boolean;
  /** Jump to the first row (align: start). No-op on zero items. */
  scrollToTop: () => void;
  /** Jump to an index (default align: start). */
  scrollToIndex: (index: number, align?: "start" | "center" | "end") => void;
  /** Jump to the row whose `keyOf` matches. Returns true only when the row
   *  is present AND the virtualizer has a scroll element (a false return
   *  means the viewport did NOT move — do not treat the pane as positioned). */
  scrollToKey: (key: string, align?: "start" | "center" | "end") => boolean;
}

export interface VirtualRowListProps<T> {
  items: readonly T[];
  /** Stable, session-qualified identity for one row. Feeds the virtualizer's
   *  measurement cache AND the measured-height cache. */
  keyOf: (item: T, index: number) => string;
  /** Row content. The shell owns the positioned wrapper; this returns what
   *  goes inside it. */
  renderItem: (item: T, index: number) => React.ReactNode;
  /** Height estimate for a never-measured row. Omit for the fixed default;
   *  `measureElement` corrects it on first mount either way. */
  estimateSize?: (item: T, index: number, containerWidth: number) => number;
  overscan?: number;
  gap?: number;
  /** The scroll container (owned by the surface — same element the follow
   *  controller binds). */
  scrollContainerRef: React.RefObject<HTMLElement | null>;
  /** Extra attributes for the positioned wrapper (e.g. `data-seq` so
   *  locate-and-flash can query mounted rows). */
  rowAttrs?: (item: T, index: number) => Record<string, string | number>;
  /** Extra classes for the positioned wrapper (e.g. a locate tint). */
  rowClassName?: (item: T, index: number) => string | undefined;
  /** Extra inline styles for the positioned wrapper (merged over the
   *  position/transform the shell owns). */
  rowStyle?: (item: T, index: number) => React.CSSProperties | undefined;
  /** React 19 ref-as-prop: the imperative handle (visible-range queries,
   *  scroll actions). */
  ref?: React.Ref<VirtualRowListHandle>;
}

export function VirtualRowList<T>({
  items,
  keyOf,
  renderItem,
  estimateSize,
  overscan = 5,
  gap = 4,
  scrollContainerRef,
  rowAttrs,
  rowClassName,
  rowStyle,
  ref,
}: VirtualRowListProps<T>) {
  // Latest-value refs: every callback below is created once and must read the
  // current items/props without re-creating the virtualizer's closures.
  const itemsRef = useRef(items);
  itemsRef.current = items;
  const keyOfRef = useRef(keyOf);
  keyOfRef.current = keyOf;

  // ── Container width (self-measured) ──────────────────────────────
  // Copied from VirtualMessageList: setState in useLayoutEffect flushes
  // before paint, so estimateSize sees the real width on the first measured
  // pass without flushSync or rAF.
  const [containerWidth, setContainerWidth] = useState(0);
  useLayoutEffect(() => {
    const el = scrollContainerRef.current;
    if (el && el.clientWidth > 0) {
      setContainerWidth((prev) => {
        const w = el.clientWidth;
        return prev === w ? prev : w;
      });
    }
  });
  useEffect(() => {
    const el = scrollContainerRef.current;
    if (!el) return;
    const ro = new ResizeObserver((entries) => {
      for (const entry of entries) {
        const w = Math.round(entry.contentRect.width);
        setContainerWidth((prev) => (prev === w ? prev : w));
      }
    });
    ro.observe(el);
    return () => ro.disconnect();
  });
  const containerWidthRef = useRef(containerWidth);
  containerWidthRef.current = containerWidth;

  // ── estimateSize: measured cache → adapter estimate → default ────
  const estimate = useCallback(
    (index: number) => {
      const item = itemsRef.current[index];
      if (item === undefined) return DEFAULT_ROW_HEIGHT_PX;
      const key = keyOfRef.current(item, index);
      const measured = measuredHeights.get(key);
      if (measured !== undefined) return measured;
      const est = estimateSize?.(item, index, containerWidthRef.current);
      return est !== undefined && est > 0 ? est : DEFAULT_ROW_HEIGHT_PX;
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [estimateSize],
  );

  const virtualizer = useVirtualizer({
    count: items.length,
    getScrollElement: () => scrollContainerRef.current,
    estimateSize: estimate,
    overscan,
    gap,
    initialOffset: 0,
    // Viewport measurement fallback (render-perf L2). The library's default
    // observer reads offsetWidth/offsetHeight, which are 0 in environments
    // without layout (jsdom) — scrollRect then pins viewportSize to 0 and the
    // range stays empty (white pane). Fall back to clientWidth/clientHeight,
    // which layout-less test setups mock directly. In real browsers the
    // primary path always yields a positive size, so behavior is unchanged.
    observeElementRect: (
      instance: Virtualizer<HTMLElement, HTMLElement>,
      cb: (rect: { width: number; height: number }) => void,
    ) => {
      const el = instance.scrollElement;
      if (!el) return () => {};
      const read = () => {
        cb({
          width: el.offsetWidth || el.clientWidth,
          height: el.offsetHeight || el.clientHeight,
        });
      };
      read();
      if (typeof ResizeObserver === "undefined") return () => {};
      const ro = new ResizeObserver(read);
      ro.observe(el, { box: "border-box" });
      return () => ro.disconnect();
    },
    // Content-stable keys so the internal measurement cache survives
    // prepend/append (index keys shift every measurement on a prepend).
    getItemKey: (index: number) => {
      const item = itemsRef.current[index];
      return item === undefined ? index : keyOfRef.current(item, index);
    },
    // WKWebView fix (inherited from VirtualMessageList): synchronous
    // scrollTop assignment instead of element.scrollTo(), which can resolve
    // asynchronously on macOS and delay the scroll event. When the position
    // does not actually change (already clamped), toggle to force a scroll
    // event — otherwise the virtualizer's internal scrollOffset stays stale.
    scrollToFn: (offset, options, instance) => {
      const element = instance.scrollElement;
      if (!element) return;
      const target = offset + (options.adjustments ?? 0);
      if (options.behavior === "smooth") {
        element.scrollTo({
          [instance.options.horizontal ? "left" : "top"]: target,
          behavior: "smooth",
        });
      } else {
        const axis = instance.options.horizontal ? "scrollLeft" : "scrollTop";
        const current = element[axis] as number;
        element[axis] = target;
        if (element[axis] === current && target === current) {
          element[axis] = current + 1;
          element[axis] = target;
        }
      }
    },
  });

  // ── Late-ref self-heal ───────────────────────────────────────────
  // React attaches the parent's scroll-container ref AFTER this child's
  // layout effects run (verified on React 19 + testing-library: refs are
  // attached in the parent's layout phase, passive effects run after it).
  // The virtualizer discovers its scroll element during render/effect
  // passes only, so the first commit can miss it and nothing re-checks —
  // leaving an empty range (white pane). The passive effect below bumps
  // exactly one re-render per element identity so _willUpdate re-runs with
  // the ref populated; the guard prevents any loop.
  const [, remeasure] = useReducer((n: number) => n + 1, 0);
  const attachedForEl = useRef<HTMLElement | null>(null);
  useEffect(() => {
    const el = scrollContainerRef.current;
    if (el && !virtualizer.scrollElement && attachedForEl.current !== el) {
      attachedForEl.current = el;
      remeasure();
    }
  });

  // ── measureElement + ResizeObserver discipline ───────────────────
  // measureElement fires on mount, but async content (images, code
  // highlighting) changes height AFTER mount; a per-element ResizeObserver
  // re-measures when the row actually settles, so totalSize converges
  // instead of jumping.
  const measureElementRef = useRef<((el: HTMLElement | null) => void) | null>(null);
  measureElementRef.current = virtualizer.measureElement;

  /**
   * Wraps virtualizer.measureElement to catch NotAllowedError from
   * @tanstack/virtual-core's internal ResizeObserver: in WKWebView (Tauri),
   * observing a recycled element can throw — harmless (the measurement is
   * moot), but it surfaces as console "ERR – NotAllowedError".
   */
  const safeMeasureElement = useCallback((el: HTMLElement | null) => {
    const fn = measureElementRef.current;
    if (!fn) return;
    try {
      fn(el);
    } catch (e) {
      if (e instanceof DOMException && e.name === "NotAllowedError") return;
      throw e;
    }
  }, []);

  const measureRef = useCallback((el: HTMLElement | null) => {
    safeMeasureElement(el);
    if (!el) return;
    const record = () => {
      const idxAttr = el.dataset.index;
      if (idxAttr === undefined) return;
      const idx = parseInt(idxAttr, 10);
      const item = itemsRef.current[idx];
      if (item === undefined) return;
      const h = el.offsetHeight;
      if (h > 0) measuredHeights.set(keyOfRef.current(item, idx), h);
    };
    record();
    if (resizeObservers.has(el)) return;
    const ro = new ResizeObserver(() => {
      safeMeasureElement(el);
      record();
    });
    ro.observe(el);
    resizeObservers.set(el, ro);
  }, [safeMeasureElement]);

  // ── Bottom-pin convergence (event-driven) ────────────────────────
  // While armed, re-pin scrollTop to the (still growing) bottom on every
  // commit: scrollToIndex lands short while never-measured rows above are
  // under-estimated, and the virtualizer re-renders as measurements land.
  // Disarms when the user scrolls away (never fights a deliberate scroll-up)
  // or after BOTTOM_PIN_MAX_MS.
  const pinToBottomRef = useRef(false);
  const lastPinTopRef = useRef(0);
  const pinDeadlineRef = useRef(0);
  useLayoutEffect(() => {
    if (!pinToBottomRef.current) return;
    const el = scrollContainerRef.current;
    if (!el) return;
    if (performance.now() > pinDeadlineRef.current) {
      pinToBottomRef.current = false;
      return;
    }
    const max = el.scrollHeight - el.clientHeight;
    // See ./bottomAnchor.ts: a layout shrink clamps scrollTop too, and must
    // not be mistaken for the user scrolling away.
    if (decideBottomPin({ scrollTop: el.scrollTop, max, lastPinTop: lastPinTopRef.current }) === "disarm") {
      pinToBottomRef.current = false;
      return;
    }
    el.scrollTop = max;
    lastPinTopRef.current = max;
  });

  // ── Imperative handle ────────────────────────────────────────────
  // Data-derived queries only — no scrollTop pixel guessing.
  useImperativeHandle(
    ref,
    () => ({
      getFirstVisibleIndex: () => {
        const vis = virtualizer.getVirtualItems();
        return vis.length > 0 ? vis[0].index : null;
      },
      getFirstVisibleKey: () => {
        const vis = virtualizer.getVirtualItems();
        if (vis.length === 0) return null;
        const item = itemsRef.current[vis[0].index];
        return item === undefined ? null : keyOfRef.current(item, vis[0].index);
      },
      getLastVisibleIndex: () => {
        const vis = virtualizer.getVirtualItems();
        return vis.length > 0 ? vis[vis.length - 1].index : null;
      },
      isKeyInViewport: (key: string) => {
        const vis = virtualizer.getVirtualItems();
        if (vis.length === 0) return false;
        const idx = itemsRef.current.findIndex(
          (it, i) => keyOfRef.current(it, i) === key,
        );
        if (idx < 0) return false;
        return idx >= vis[0].index && idx <= vis[vis.length - 1].index;
      },
      scrollToBottom: () => {
        const count = virtualizer.options.count;
        if (count === 0) return false;
        // Arm the convergence anchor FIRST, before the attach check: a
        // session-switch refresh calls this from the surface's layout effect,
        // which can run while the virtualizer has not discovered its scroll
        // element yet (the late-ref window). The pin effect reads the
        // surface's scrollContainerRef directly and re-pins on every commit,
        // and the self-heal bump guarantees a commit once the element appears
        // — so an armed request lands without any caller-side retry.
        pinToBottomRef.current = true;
        pinDeadlineRef.current = performance.now() + BOTTOM_PIN_MAX_MS;
        const el = virtualizer.scrollElement;
        if (!el) {
          lastPinTopRef.current = 0;
          return true;
        }
        // Mount the last item first; the resulting DOM growth is asynchronous
        // (scroll → re-evaluate → commit → ResizeObserver measures →
        // scrollHeight grows again). The armed anchor below rides the bottom
        // through those rounds instead of stopping at the first estimate.
        virtualizer.scrollToIndex(count - 1, { align: "end" });
        lastPinTopRef.current = el.scrollHeight - el.clientHeight;
        el.scrollTop = lastPinTopRef.current;
        return true;
      },
      scrollToTop: () => {
        const count = virtualizer.options.count;
        if (count === 0) return;
        // No scroll-element guard: nothing reports back, and every caller
        // that needs to know whether the viewport moved goes through
        // scrollToBottom / scrollToKey (both return booleans).
        virtualizer.scrollToIndex(0, { align: "start" });
      },
      scrollToIndex: (index: number, align: "start" | "center" | "end" = "start") => {
        const count = virtualizer.options.count;
        if (count === 0) return;
        const clamped = Math.max(0, Math.min(index, count - 1));
        virtualizer.scrollToIndex(clamped, { align });
      },
      scrollToKey: (key: string, align: "start" | "center" | "end" = "start") => {
        const idx = itemsRef.current.findIndex(
          (it, i) => keyOfRef.current(it, i) === key,
        );
        if (idx < 0) return false;
        // virtualizer.scrollToIndex is a SILENT no-op while no scroll element
        // is attached — report false so the caller does not treat the pane
        // as positioned (the locate path depends on this).
        if (!virtualizer.scrollElement) return false;
        virtualizer.scrollToIndex(idx, { align });
        return true;
      },
    }),
    [virtualizer, items],
  );

  // ── Render ───────────────────────────────────────────────────────
  // Three states:
  //   1. scroll container not yet attached (first commit — the parent's
  //      ref lags this child's effects, see Late-ref self-heal): render
  //      the sizing pane only. One frame of no rows, then the bump
  //      re-renders virtualized. Cheaper than mounting the whole
  //      transcript on the first paint of a huge session.
  //   2. container attached but viewport measures 0 (jsdom without
  //      layout mocks, or a hidden container): fall back to every row in
  //      normal flow — pre-virtualization behavior, so degraded
  //      environments can never produce a white pane.
  //   3. normal: the virtualized window.
  const container = scrollContainerRef.current;
  const viewportPx = container ? container.offsetHeight || container.clientHeight : 0;
  const virtualized = container !== null && viewportPx > 0;

  const rowDiv = (
    key: string | number,
    item: T,
    index: number,
    posStyle: React.CSSProperties,
    measured: boolean,
  ) => (
    <div
      key={key}
      {...(measured ? { ref: measureRef, "data-index": index } : {})}
      {...(rowAttrs?.(item, index) ?? {})}
      className={rowClassName?.(item, index)}
      style={posStyle}
    >
      {renderItem(item, index)}
    </div>
  );

  return (
    <div
      style={{
        height: virtualized ? virtualizer.getTotalSize() : undefined,
        width: "100%",
        position: "relative",
      }}
    >
      {virtualized
        ? virtualizer.getVirtualItems().map((virtualRow) => {
            const item = items[virtualRow.index];
            if (item === undefined) return null;
            return rowDiv(
              virtualRow.key as string | number,
              item,
              virtualRow.index,
              {
                position: "absolute",
                top: 0,
                left: 0,
                width: "100%",
                transform: `translateY(${virtualRow.start}px)`,
                ...(rowStyle?.(item, virtualRow.index) ?? {}),
              },
              true,
            );
          })
        : container
          ? items.map((item, index) =>
              rowDiv(
                keyOf(item, index),
                item,
                index,
                { ...(rowStyle?.(item, index) ?? {}) },
                false,
              ),
            )
          : null}
    </div>
  );
}
