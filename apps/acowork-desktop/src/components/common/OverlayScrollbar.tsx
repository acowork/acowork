import { forwardRef, useCallback, useEffect, useId, useRef, useState, type KeyboardEvent, type PointerEvent, type ReactNode, type UIEvent, type Ref, type MouseEvent, type WheelEvent } from "react";
import { cn } from "../../lib/utils";

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
  onContextMenu?: (event: MouseEvent<HTMLDivElement>) => void;
  onViewportWheel?: (event: WheelEvent<HTMLDivElement>) => void;
}

interface ThumbMetrics {
  height: number;
  top: number;
  maxScroll: number;
  maxTop: number;
}

const MIN_THUMB_HEIGHT = 20;

export function getOverlayThumbMetrics(clientHeight: number, scrollHeight: number, scrollTop: number): ThumbMetrics {
  const maxScroll = Math.max(0, scrollHeight - clientHeight);
  const height = maxScroll === 0 ? clientHeight : Math.max(MIN_THUMB_HEIGHT, clientHeight * clientHeight / scrollHeight);
  const maxTop = Math.max(0, clientHeight - height);
  return { height, top: maxScroll === 0 ? 0 : scrollTop / maxScroll * maxTop, maxScroll, maxTop };
}

export const OverlayScrollbar = forwardRef<HTMLDivElement, OverlayScrollbarProps>(function OverlayScrollbar(
  { children, className, role, "aria-label": ariaLabel, viewportRef, onViewportScroll, onViewportPointerDown, dataTabScroll, containerClassName, maxHeight, fill = "height", onContextMenu, onViewportWheel },
  forwardedRef,
) {
  const innerViewportRef = useRef<HTMLDivElement>(null);
  const setViewportRef = useCallback((node: HTMLDivElement | null) => {
    innerViewportRef.current = node;
    if (typeof viewportRef === "function") viewportRef(node);
    else if (viewportRef) viewportRef.current = node;
    if (typeof forwardedRef === "function") forwardedRef(node);
    else if (forwardedRef) forwardedRef.current = node;
  }, [forwardedRef, viewportRef]);
  const dragRef = useRef<{ pointerY: number; scrollTop: number } | null>(null);
  const viewportId = useId();
  const [metrics, setMetrics] = useState<ThumbMetrics>({ height: 0, top: 0, maxScroll: 0, maxTop: 0 });

  const updateMetrics = useCallback(() => {
    const viewport = innerViewportRef.current;
    if (!viewport) return;
    const { clientHeight, scrollHeight, scrollTop } = viewport;
    setMetrics(getOverlayThumbMetrics(clientHeight, scrollHeight, scrollTop));
  }, []);

  useEffect(() => {
    const viewport = innerViewportRef.current;
    if (!viewport) return;
    const observer = new ResizeObserver(updateMetrics);
    observer.observe(viewport);
    if (viewport.firstElementChild) observer.observe(viewport.firstElementChild);
    const mutations = new MutationObserver(updateMetrics);
    mutations.observe(viewport, { childList: true, subtree: true, characterData: true });
    updateMetrics();
    return () => {
      observer.disconnect();
      mutations.disconnect();
    };
  }, [updateMetrics, children]);

  const onThumbPointerDown = (event: PointerEvent<HTMLDivElement>) => {
    if (event.button !== 0 || metrics.maxScroll === 0) return;
    event.preventDefault();
    dragRef.current = { pointerY: event.clientY, scrollTop: innerViewportRef.current?.scrollTop ?? 0 };
    event.currentTarget.setPointerCapture(event.pointerId);
  };

  const onThumbPointerMove = (event: PointerEvent<HTMLDivElement>) => {
    const drag = dragRef.current;
    const viewport = innerViewportRef.current;
    if (!drag || !viewport || metrics.maxTop === 0) return;
    viewport.scrollTop = drag.scrollTop + (event.clientY - drag.pointerY) * metrics.maxScroll / metrics.maxTop;
  };

  const onWheel = (event: WheelEvent<HTMLDivElement>) => {
    const viewport = innerViewportRef.current;
    if (!viewport || event.deltaY === 0) return;
    const maxScroll = viewport.scrollHeight - viewport.clientHeight;
    const canScroll = event.deltaY < 0 ? viewport.scrollTop > 0 : viewport.scrollTop < maxScroll;
    if (canScroll) event.stopPropagation();
  };

  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    const viewport = innerViewportRef.current;
    if (!viewport) return;
    const step = Math.max(40, viewport.clientHeight * 0.9);
    switch (event.key) {
      case "ArrowDown": viewport.scrollTop += 40; break;
      case "ArrowUp": viewport.scrollTop -= 40; break;
      case "PageDown": viewport.scrollTop += step; break;
      case "PageUp": viewport.scrollTop -= step; break;
      case "Home": viewport.scrollTop = 0; break;
      case "End": viewport.scrollTop = viewport.scrollHeight; break;
      default: return;
    }
    event.preventDefault();
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
        id={viewportId}
        ref={setViewportRef}
        data-tab-scroll={dataTabScroll ? "" : undefined}
        className={cn(
          "overlay-scrollbar-viewport overflow-y-auto overflow-x-hidden",
          maxHeight === undefined && (fill === "flex" ? "min-h-0 flex-1" : "h-full"),
          className,
        )}
        style={maxHeight !== undefined ? { maxHeight } : undefined}
        role={role}
        aria-label={ariaLabel}
        onScroll={(event) => {
          updateMetrics();
          onViewportScroll?.(event);
        }}
        onPointerDown={onViewportPointerDown}
        onWheel={(event) => {
          onWheel(event);
          onViewportWheel?.(event);
        }}
        onContextMenu={onContextMenu}
      >
        {children}
      </div>
      {metrics.maxScroll > 0 && (
        <div className="overlay-scrollbar-track" aria-hidden="true">
          <div
            className="overlay-scrollbar-thumb"
            style={{ height: metrics.height, transform: `translateY(${metrics.top}px)` }}
            onPointerDown={onThumbPointerDown}
            onPointerMove={onThumbPointerMove}
            onPointerUp={() => { dragRef.current = null; }}
            onPointerCancel={() => { dragRef.current = null; }}
            onKeyDown={onKeyDown}
            role="scrollbar"
            aria-controls={viewportId}
            aria-orientation="vertical"
            aria-valuemin={0}
            aria-valuemax={Math.round(metrics.maxScroll)}
            aria-valuenow={Math.round(innerViewportRef.current?.scrollTop ?? 0)}
            tabIndex={0}
          />
        </div>
      )}
    </div>
  );
});

// Minimal math check for thumb sizing and travel mapping.
const checkOverlayScrollbarMath = () => {
  const viewportHeight = 100;
  const scrollHeight = 500;
  const maxScroll = scrollHeight - viewportHeight;
  const thumbHeight = Math.max(MIN_THUMB_HEIGHT, viewportHeight * viewportHeight / scrollHeight);
  const maxTop = viewportHeight - thumbHeight;
  if (thumbHeight !== 20 || maxTop !== 80 || maxTop / maxScroll !== 0.2) {
    throw new Error("Overlay scrollbar thumb geometry is inconsistent");
  }
};
void checkOverlayScrollbarMath;
