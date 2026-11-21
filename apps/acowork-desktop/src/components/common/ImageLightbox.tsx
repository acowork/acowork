/**
 * Image lightbox for chat attachments.
 *
 * Why a bridge and not a prop: the click that opens this lives in
 * `lib/openChatAttachment.ts`, a plain module — it must stay callable from a
 * row component, a store action, or a future surface without every caller
 * having to thread state down. Same pattern as `showToast` (ADR-038): fire a
 * `CustomEvent`, and one mounted host renders the overlay.
 *
 * Why one host and not per-chip state: chat rows live inside a virtualized
 * list, so an overlay rendered inside a row would be clipped by the scroll
 * container and unmounted the moment the row leaves the viewport. The host
 * sits above the app shell.
 *
 * Scope: display, zoom (buttons + wheel), drag-to-pan, copy to clipboard,
 * "save to disk". It never fetches *for its own sake* — the caller resolves
 * the blob URL (the chip already knows how) and hands it over, so this
 * component has no auth or Gateway knowledge; the one `fetch` is inside
 * `lib/clipboard.copyImageUrl`, which re-reads the bytes the caller already
 * owns. Because the bytes are the caller's, so is their lifetime: `onClose` is
 * the release hook, and the host calls it exactly once per request, whether
 * the overlay was dismissed, replaced by another image, or unmounted.
 *
 * Geometry is NOT measured off the DOM: the fitted size is derived from the
 * image's natural size and the viewport's content box through the same
 * `imageViewerGeometry` helpers the workspace preview uses, so a window resize
 * while zoomed re-derives instead of keeping a stale pixel base.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type {
  KeyboardEvent as ReactKeyboardEvent,
  PointerEvent as ReactPointerEvent,
} from "react";
import { Copy, Download, X, ZoomIn, ZoomOut } from "lucide-react";
import {
  MAX_ZOOM,
  MIN_ZOOM,
  clampZoom,
  fitScale,
  wheelZoomFactor,
  zoomStepFactor,
} from "../editor/imageViewerGeometry";
import { copyImageUrl } from "../../lib/clipboard";
import { showToast } from "./ToastProvider";
import {
  OVERLAY_SCROLL_HOST_CLASS,
  OVERLAY_SCROLL_VIEWPORT_CLASS,
} from "./overlayScrollThumb";
import { OverlayScrollTarget } from "./OverlayScrollbar";

export interface ImageLightboxRequest {
  /** Object URL or data URL for the full-size image. */
  src: string;
  /** Shown in the header and used as the alt text. */
  filename: string;
  /** Optional save action; the button is hidden when absent. */
  onDownload?: () => void;
  /**
   * Release hook for whatever `src` holds — a caller-owned object URL is the
   * case this exists for. Called once, when this request stops being shown;
   * omitted when the URL is shared (a cached thumbnail, a data URL).
   */
  onClose?: () => void;
}

const LIGHTBOX_EVENT = "acowork:image-lightbox";

/** One icon-button shape for the whole toolbar — the header carries no text
 *  labels at all, so `aria-label` + `title` are the only names. */
const TOOL_BUTTON =
  "inline-flex h-7 w-7 items-center justify-center rounded text-white transition-colors hover:bg-white/10 disabled:cursor-default disabled:opacity-40 disabled:hover:bg-transparent";

/** Open the lightbox from anywhere (no React context required). */
export function openImageLightbox(request: ImageLightboxRequest): void {
  window.dispatchEvent(new CustomEvent(LIGHTBOX_EVENT, { detail: request }));
}

/** Everything inside the overlay the keyboard can reach. */
const FOCUSABLE = 'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])';

/** A measured box, in CSS pixels. */
interface Size {
  width: number;
  height: number;
}

/** The scroller's content box — `clientWidth` includes the `p-4` padding, and
 *  the image is fitted to the content box, not to the padding edge. */
function contentBox(el: HTMLElement): Size {
  const style = getComputedStyle(el);
  const px = (value: string) => Number.parseFloat(value) || 0;
  return {
    width: el.clientWidth - px(style.paddingLeft) - px(style.paddingRight),
    height: el.clientHeight - px(style.paddingTop) - px(style.paddingBottom),
  };
}

export function ImageLightboxHost() {
  const [request, setRequest] = useState<ImageLightboxRequest | null>(null);
  /** The request currently on screen. Held outside state so the release hook
   *  fires exactly once per request even if React renders twice. */
  const current = useRef<ImageLightboxRequest | null>(null);
  const dialogRef = useRef<HTMLDivElement | null>(null);
  /** The scrolling viewport around the image; wheel zoom needs it as a DOM
   *  node (React's `onWheel` is passive, so `preventDefault` is a no-op). */
  const scrollerRef = useRef<HTMLDivElement | null>(null);
  /** 1 = fit to the viewport. Zoom multiplies from the fitted size. */
  const [zoom, setZoom] = useState(1);
  /** The loaded image's intrinsic size. */
  const [natural, setNatural] = useState<Size | null>(null);
  /** The scrolling viewport's content box (kept current by a ResizeObserver). */
  const [viewport, setViewport] = useState<Size | null>(null);
  /** Live drag state for panning; `null` when the pointer is up. */
  const pan = useRef<{ pointerId: number; x: number; y: number; left: number; top: number } | null>(
    null,
  );
  /** Where focus was when the overlay opened — a modal that does not give it
   *  back strands the keyboard at the top of the document. */
  const restoreFocus = useRef<HTMLElement | null>(null);

  useEffect(() => {
    const handler = (e: Event) => {
      const detail = (e as CustomEvent<ImageLightboxRequest>).detail;
      if (!detail?.src) return;
      const previous = current.current;
      if (!previous) restoreFocus.current = document.activeElement as HTMLElement | null;
      current.current = detail;
      setRequest(detail);
      // A second image replaces the first: the first one's bytes are no
      // longer on screen, so this is where its object URL goes back.
      previous?.onClose?.();
    };
    window.addEventListener(LIGHTBOX_EVENT, handler);
    return () => window.removeEventListener(LIGHTBOX_EVENT, handler);
  }, []);

  const close = useCallback(() => {
    const live = current.current;
    current.current = null;
    setRequest(null);
    live?.onClose?.();
    const back = restoreFocus.current;
    restoreFocus.current = null;
    back?.focus?.();
  }, []);

  // Move focus into the overlay for as long as it is up.
  useEffect(() => {
    if (!request) return;
    dialogRef.current?.focus();
  }, [request]);

  useEffect(() => {
    if (!request) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") close();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [request, close]);

  // A new image starts at fit size: the previous zoom means nothing here.
  useEffect(() => {
    setZoom(1);
    setNatural(null);
  }, [request]);

  // Track the viewport box. The fitted base is derived from it, so resizing
  // the window while zoomed re-derives instead of keeping a stale pixel size.
  useEffect(() => {
    const el = scrollerRef.current;
    if (!request || !el) return;
    const measure = () => setViewport(contentBox(el));
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    return () => observer.disconnect();
  }, [request, natural]);

  /** The image's size at zoom 1 (fitted, never enlarged past 1:1). `null`
   *  until both the image and the viewport are known. */
  const base = useMemo(() => {
    if (!natural || !viewport) return null;
    const scale = fitScale(natural, viewport);
    return { width: natural.width * scale, height: natural.height * scale };
  }, [natural, viewport]);

  // Wheel = zoom. The base above is derived, so this only scales.
  useEffect(() => {
    const el = scrollerRef.current;
    if (!request || !el) return;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      setZoom((z) => clampZoom(z * wheelZoomFactor(e.deltaY)));
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, [request]);

  // Keep what grew in the middle of the viewport instead of the top-left
  // corner. `ponytail:` centre-anchored only — cursor-anchored zoom needs the
  // pointer→pixel math in `imageViewerGeometry.ts`; add it if users complain.
  useEffect(() => {
    const el = scrollerRef.current;
    if (!el) return;
    el.scrollLeft = (el.scrollWidth - el.clientWidth) / 2;
    el.scrollTop = (el.scrollHeight - el.clientHeight) / 2;
  }, [zoom, base]);

  /** Drag to pan. Zoom without a way to move the picture is a dead feature:
   *  the wheel is taken by zoom and the overlay thumb is a 6 px target. */
  const startPan = (e: ReactPointerEvent<HTMLDivElement>) => {
    const el = scrollerRef.current;
    if (!el || zoom <= 1 || e.button !== 0) return;
    pan.current = {
      pointerId: e.pointerId,
      x: e.clientX,
      y: e.clientY,
      left: el.scrollLeft,
      top: el.scrollTop,
    };
    el.setPointerCapture?.(e.pointerId);
  };

  const movePan = (e: ReactPointerEvent<HTMLDivElement>) => {
    const el = scrollerRef.current;
    const drag = pan.current;
    if (!el || !drag || drag.pointerId !== e.pointerId) return;
    el.scrollLeft = drag.left - (e.clientX - drag.x);
    el.scrollTop = drag.top - (e.clientY - drag.y);
  };

  const endPan = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (pan.current?.pointerId !== e.pointerId) return;
    scrollerRef.current?.releasePointerCapture?.(e.pointerId);
    pan.current = null;
  };

  /** Clipboard write. Goes through `lib/clipboard.copyImageUrl`, which owns
   *  the PNG repaint, the `ClipboardItem` construction — and, crucially, the
   *  unpatched `navigator.clipboard.write`, so a refused write reports
   *  failure instead of a swallowed success. */
  const copy = useCallback(async () => {
    const live = current.current;
    if (!live) return;
    const ok = await copyImageUrl(live.src);
    showToast(
      ok
        ? { type: "success", message: "图片已复制" }
        : { type: "error", message: "复制图片失败" },
    );
  }, []);

  if (!request) return null;

  /** Keep Tab inside the overlay: `aria-modal` alone does not. */
  const trapTab = (e: ReactKeyboardEvent<HTMLDivElement>) => {
    if (e.key !== "Tab") return;
    const root = dialogRef.current;
    if (!root) return;
    const focusable = Array.from(root.querySelectorAll<HTMLElement>(FOCUSABLE));
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (!first || !last) return;
    const active = document.activeElement;
    if (e.shiftKey && (active === first || active === root)) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && active === last) {
      e.preventDefault();
      first.focus();
    }
  };

  const rendered = base ? { width: base.width * zoom, height: base.height * zoom } : null;

  return (
    <div
      ref={dialogRef}
      tabIndex={-1}
      className="fixed inset-0 z-[200] flex flex-col bg-black/80 backdrop-blur-sm outline-none"
      role="dialog"
      aria-modal="true"
      aria-label={request.filename}
      onClick={close}
      onKeyDown={trapTab}
    >
      <div
        className="flex shrink-0 items-center gap-1 px-4 py-3 text-white"
        onClick={(e) => e.stopPropagation()}
      >
        <span className="mr-2 min-w-0 flex-1 truncate text-sm" title={request.filename}>
          {request.filename}
        </span>
        <button
          type="button"
          aria-label="缩小"
          title="缩小"
          disabled={zoom <= MIN_ZOOM}
          className={TOOL_BUTTON}
          onClick={() => setZoom((z) => clampZoom(z * zoomStepFactor(-1)))}
        >
          <ZoomOut className="h-4 w-4" />
        </button>
        <button
          type="button"
          aria-label="放大"
          title="放大"
          disabled={zoom >= MAX_ZOOM}
          className={TOOL_BUTTON}
          onClick={() => setZoom((z) => clampZoom(z * zoomStepFactor(1)))}
        >
          <ZoomIn className="h-4 w-4" />
        </button>
        <button
          type="button"
          aria-label="复制"
          title="复制"
          className={TOOL_BUTTON}
          onClick={copy}
        >
          <Copy className="h-4 w-4" />
        </button>
        {request.onDownload && (
          <button
            type="button"
            aria-label="下载"
            title="下载"
            className={TOOL_BUTTON}
            onClick={request.onDownload}
          >
            <Download className="h-4 w-4" />
          </button>
        )}
        <button
          type="button"
          aria-label="关闭"
          title="关闭"
          className={TOOL_BUTTON}
          onClick={close}
        >
          <X className="h-4 w-4" />
        </button>
      </div>
      {/* `m-auto` centres until the image outgrows the viewport, then it
          collapses to 0 and the overflow scrolls from the edge — which is
          exactly the centring/clipping rule a zoomed image wants. The attached
          form of the overlay scrollbar (not the wrapper): a zoomed image pans
          on BOTH axes, and the wrapper form pins `overflow-x-hidden`. */}
      <div className={`${OVERLAY_SCROLL_HOST_CLASS} flex min-h-0 flex-1`}>
        <div
          ref={scrollerRef}
          className={`${OVERLAY_SCROLL_VIEWPORT_CLASS} flex flex-1 overflow-auto p-4 ${
            zoom > 1 ? "cursor-grab active:cursor-grabbing" : ""
          }`}
          onPointerDown={startPan}
          onPointerMove={movePan}
          onPointerUp={endPan}
          onPointerCancel={endPan}
        >
          <img
            src={request.src}
            alt={request.filename}
            // Native image dragging would fight the pan gesture.
            draggable={false}
            className={
              rendered
                ? "m-auto max-w-none shrink-0 select-none object-contain"
                : "m-auto max-h-full max-w-full shrink-0 object-contain"
            }
            style={rendered ?? undefined}
            onLoad={(e) => {
              const img = e.currentTarget;
              // A not-yet-decoded image reports 0×0; keeping the CSS-fit
              // fallback is better than deriving a zero base.
              if (img.naturalWidth > 0 && img.naturalHeight > 0) {
                setNatural({ width: img.naturalWidth, height: img.naturalHeight });
              }
            }}
            onClick={(e) => e.stopPropagation()}
          />
        </div>
        <OverlayScrollTarget targetRef={scrollerRef} showXThumb />
      </div>
    </div>
  );
}