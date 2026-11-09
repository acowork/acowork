//! `OverlayScrollbar` — the app's scrollbar, in its two delivery forms.
//!
//! Geometry, measurement and the thumb interaction live in
//! `./overlayScrollThumb` (shared); this module only owns the two DOM shapes.
//!
//! - `OverlayScrollbar`: we own the scrolling element, so it renders
//!   `outer > viewport(overflow) > children` plus the floating thumb.
//! - `OverlayScrollTarget`: someone else owns the scrolling element (a
//!   `<textarea>`, a Monaco/virtualizer host) and it cannot be wrapped — it
//!   paints the identical thumb over that element.
//!
//! Both share one skin (`.overlay-scrollbar*` in globals.css), so a converted
//! scroller is visually indistinguishable regardless of which form it uses.
import { forwardRef, useCallback, useEffect, useRef, type PointerEvent, type ReactNode, type Ref, type RefObject, type UIEvent, type MouseEvent, type WheelEvent } from "react";
import { cn } from "../../lib/utils";
import {
  OVERLAY_SCROLL_VIEWPORT_CLASS,
  getOverlayThumbMetrics,
  useOverlayScrollThumb,
  useRefElement,
  type OverlayScrollThumbState,
} from "./overlayScrollThumb";

// The geometry function is re-exported for `OverlayScrollbar.test.ts`; the
// rest of the primitive is imported straight from `./overlayScrollThumb`.
export { getOverlayThumbMetrics };

interface OverlayScrollbarProps {
  children: ReactNode;
  className?: string;
  role?: string;
  "aria-label"?: string;
  viewportRef?: Ref<HTMLDivElement>;
  onViewportScroll?: (event: UIEvent<HTMLDivElement>) => void;
  onViewportPointerDown?: (event: PointerEvent<HTMLDivElement>) => void;
  dataTabScroll?: boolean;
  containerClassName?: string;
  /** Cap the viewport height. A plain number is treated as px; a string is
   *  passed straight to CSS (`maxHeight`), so vh / `min(...)` caps also work.
   *  When set, the built-in `flex-1` / `h-full` fill is dropped and the
   *  viewport scrolls at this fixed cap instead — for dialog/list boxes that
   *  are not themselves flex children. */
  maxHeight?: number | string;
  /** How the viewport fills its container when no `maxHeight` cap is given.
   *  - `"height"` (default): viewport is `h-full`. This is the original
   *    behaviour and what the chat message list / AgentList / RightPanel
   *    rely on; it resolves correctly wherever the ancestor chain has a
   *    definite height.
   *  - `"flex"`: outer becomes a flex column and the viewport `flex-1
   *    min-h-0`. Use this only inside a `max-h-*` dialog body, where the
   *    container height is indefinite and `height:100%` would not resolve
   *    (the content would overflow unscrolled). Never the default, so
   *    existing definite-height consumers are untouched. */
  fill?: "height" | "flex";
  /** Draw a floating overlay thumb for the HORIZONTAL axis as well as the
   *  vertical one. Opt-in: most viewports clip their content
   *  (`overflow-x-hidden`), so an x-thumb would be dead weight — only scrollers
   *  that genuinely overflow on the x-axis (the file tree: long paths) should
   *  request it. The native scrollbar cannot be re-enabled on just that axis:
   *  Chrome 121+ ignores the whole `::-webkit-scrollbar` family whenever the
   *  standard `scrollbar-width`/`scrollbar-color` are non-auto/non-normal (see
   *  the note in globals.css), so the overlay thumb is the only reliable
   *  x-axis affordance. */
  showXThumb?: boolean;
  onContextMenu?: (event: MouseEvent<HTMLDivElement>) => void;
  onViewportWheel?: (event: WheelEvent<HTMLDivElement>) => void;
}

/** The floating thumb(s) + their tracks. Purely presentational: every bit of
 *  geometry and interaction arrives pre-bound in `state`, and the host element
 *  (the wrapper's outer box, or the attach form's positioned ancestor) owns
 *  the positioning context and the `:hover` reveal. */
export function OverlayScrollThumbLayer({ state }: { state: OverlayScrollThumbState }) {
  return (
    <>
      {state.scrollable && (
        <div className="overlay-scrollbar-track" aria-hidden="true">
          <div className="overlay-scrollbar-thumb" {...state.thumb.props} />
        </div>
      )}
      {state.thumbX && state.thumbX.metrics.maxScroll > 0 && (
        <div className="overlay-scrollbar-track-x" aria-hidden="true">
          {/* Carries `.overlay-scrollbar-thumb` for the skin; the `-x` class
              only re-orients it (the inline `style.width` wins). */}
          <div className="overlay-scrollbar-thumb overlay-scrollbar-thumb-x" {...state.thumbX.props} />
        </div>
      )}
    </>
  );
}

export const OverlayScrollbar = forwardRef<HTMLDivElement, OverlayScrollbarProps>(function OverlayScrollbar(
  { children, className, role, "aria-label": ariaLabel, viewportRef, onViewportScroll, onViewportPointerDown, dataTabScroll, containerClassName, maxHeight, fill = "height", showXThumb, onContextMenu, onViewportWheel },
  forwardedRef,
) {
  const viewportNodeRef = useRef<HTMLDivElement>(null);
  const state = useOverlayScrollThumb({ showXThumb });
  const { setTarget } = state;

  const setViewportRef = useCallback((node: HTMLDivElement | null) => {
    viewportNodeRef.current = node;
    // Feed the shared measurement layer, then the consumer's own refs — the
    // wrapper answers to three callers, and `setTarget` is the one that makes
    // the thumb exist at all.
    setTarget(node);
    if (typeof viewportRef === "function") viewportRef(node);
    else if (viewportRef) viewportRef.current = node;
    if (typeof forwardedRef === "function") forwardedRef(node);
    else if (forwardedRef) forwardedRef.current = node;
  }, [forwardedRef, setTarget, viewportRef]);

  // Wheel containment stays here, not in the shared layer: it is about a
  // *nested* scroller not scrolling the surface behind it, which is a property
  // of this viewport's surroundings, not of the thumb.
  const onWheel = (event: WheelEvent<HTMLDivElement>) => {
    const viewport = viewportNodeRef.current;
    if (!viewport || event.deltaY === 0) return;
    const maxScroll = viewport.scrollHeight - viewport.clientHeight;
    const canScroll = event.deltaY < 0 ? viewport.scrollTop > 0 : viewport.scrollTop < maxScroll;
    if (canScroll) event.stopPropagation();
  };

  return (
    <div
      className={cn(
        "overlay-scrollbar relative min-h-0 flex-1",
        maxHeight === undefined && fill === "flex" && "flex flex-col",
        containerClassName,
      )}
      style={maxHeight !== undefined ? { maxHeight, flex: "none" } : undefined}
    >
      <div
        id={state.viewportId}
        ref={setViewportRef}
        data-tab-scroll={dataTabScroll ? "" : undefined}
        className={cn(
          `${OVERLAY_SCROLL_VIEWPORT_CLASS} overflow-y-auto overflow-x-hidden`,
          maxHeight === undefined && (fill === "flex" ? "min-h-0 flex-1" : "h-full"),
          className,
        )}
        style={maxHeight !== undefined ? { maxHeight } : undefined}
        role={role}
        aria-label={ariaLabel}
        onScroll={onViewportScroll}
        onPointerDown={onViewportPointerDown}
        onWheel={(event) => {
          onWheel(event);
          onViewportWheel?.(event);
        }}
        onContextMenu={onContextMenu}
      >
        {children}
      </div>
      <OverlayScrollThumbLayer state={state} />
    </div>
  );
});

interface OverlayScrollTargetProps {
  /** The scrolling element, already attached to the caller's own ref. */
  targetRef: RefObject<HTMLElement | null>;
  showXThumb?: boolean;
}

/**
 * Attach form: paint the app's floating thumb over a scroller we do not own.
 *
 * Two class obligations at the call site (constants exported by
 * `./overlayScrollThumb`):
 *  - a **positioned ancestor** carrying `OVERLAY_SCROLL_HOST_CLASS` — the
 *    thumb is absolutely positioned and revealed by the ancestor's `:hover`
 *    / `:focus-within`;
 *  - the **scroller** itself carrying `OVERLAY_SCROLL_VIEWPORT_CLASS` so its
 *    native bar is hidden while wheel/keyboard scrolling keeps working.
 *
 * Use it only where the wrapper form is impossible: a `<textarea>` (its
 * content scrolls *inside* the element, so there is no child box to wrap), or
 * a host whose scroll element belongs to a third-party library. A plain JSX
 * container uses `OverlayScrollbar` instead.
 */
export function OverlayScrollTarget({ targetRef, showXThumb }: OverlayScrollTargetProps) {
  const element = useRefElement(targetRef);
  const state = useOverlayScrollThumb({ showXThumb });
  const { setTarget, viewportId } = state;

  useEffect(() => {
    setTarget(element);
  }, [element, setTarget]);

  // `role="scrollbar"` points at its viewport through `aria-controls`; the
  // wrapper form owns that id, an attached element does not. Stamp it on when
  // the element has none (never overwrite one the owner set) so the reference
  // resolves instead of dangling, and take it back off on unmount.
  useEffect(() => {
    if (!element || element.id) return;
    element.id = viewportId;
    return () => {
      if (element.id === viewportId) element.removeAttribute("id");
    };
  }, [element, viewportId]);

  return <OverlayScrollThumbLayer state={state} />;
}
