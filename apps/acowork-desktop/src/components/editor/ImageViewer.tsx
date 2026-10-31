// SPDX-License-Identifier: Apache-2.0
// Copyright the ACowork project contributors.
//
// The workspace panel's image preview — with the affordances a user needs to
// TELL the agent what to change: zoom, a pixel coordinate readout, and two
// "pin it into the chat" gestures (a point, or a dragged region as an actual
// cropped screenshot).
//
// Layer split (why this file is only rendering + event wiring):
//
//   - `imageViewerGeometry.ts` owns every number (scale, image pixel, drag
//     rectangle, cursor-anchored scroll). Pure, unit-tested without a DOM.
//   - `imageCrop.ts` owns turning a rectangle into PNG bytes.
//   - `lib/chatComposerBridge.ts` owns delivery to whichever composer is on
//     screen; this component only hands over a `File` / a point.
//
// What this component deliberately does NOT do: decide whether the reference
// may be sent (the panel resolves that and passes `addToChatDisabledTitle`),
// and build the prompt text (the panel knows the workspace-relative path).
//
// Layout: `img` is laid out at exactly `naturalSize × scale` inside an
// `overflow: auto` viewport, so the pointer→pixel conversion is one division
// by `scale` against the element's own bounding box — no transform stack to
// invert, and panning is native scrolling (no drag-to-pan gesture competing
// with drag-to-select).

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { MouseEvent as ReactMouseEvent, PointerEvent as ReactPointerEvent } from "react";
import { Crosshair, Maximize2, MessageSquarePlus, XSquare, ZoomIn, ZoomOut } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import { log } from "../../lib/logger";
import { showToast } from "../common/ToastProvider";
import { ContextMenu, useContextMenu, type ContextMenuItem } from "../common/ContextMenu";
import { cropImageRegion, regionFileName } from "./imageCrop";
import {
    MAX_ZOOM,
    MIN_ZOOM,
    clampPoint,
    clampZoom,
    dragSelectionRect,
    fitScale,
    normalizeDragRect,
    screenToImagePoint,
    wheelZoomFactor,
    zoomPercent,
    zoomAnchorDrift,
    zoomStepFactor,
    type ImagePoint,
    type ImageRect,
    type ImageSize,
} from "./imageViewerGeometry";

/** Distance kept between the image and the viewport edge when it fits. */
const VIEWPORT_PADDING_PX = 16;

export interface ImageViewerProps {
    /** `data:` URL holding the bytes (raster base64, or SVG re-encoded by the
     *  caller — both are same-origin for the canvas, so cropping is allowed). */
    src: string;
    /** Basename — alt text and the stem of the cropped attachment's name. */
    fileName: string;
    /** Double-click affordance back to the source editor (SVG only). */
    onOpenSource?: () => void;
    /** Tooltip for `onOpenSource`; ignored when the handler is absent. */
    openSourceHint?: string;
    /**
     * Why "Add to Chat" cannot deliver right now, or `null` when it can.
     * Resolved by the panel — the same rule the tab menu and the markdown
     * selection button use — so the three entry points cannot disagree.
     */
    addToChatDisabledTitle: string | null;
    /** Cursor coordinate at the right-click that chose "Add coordinates". */
    onAddPointToChat: (point: ImagePoint) => void;
    /** Cropped region (already a PNG `File`) at the right-click that chose
     *  "Add selection". */
    onAddRegionToChat: (file: File, region: ImageRect) => void;
}

type MenuPayload = { point: ImagePoint; selection: ImageRect | null };

export function ImageViewer({
    src,
    fileName,
    onOpenSource,
    openSourceHint,
    addToChatDisabledTitle,
    onAddPointToChat,
    onAddRegionToChat,
}: ImageViewerProps) {
    const { t } = useTranslation();
    const viewportRef = useRef<HTMLDivElement | null>(null);
    const imgRef = useRef<HTMLImageElement | null>(null);

    /** Natural size of the decoded image; `null` until it loads. */
    const [natural, setNatural] = useState<ImageSize | null>(null);
    const [scale, setScale] = useState(1);
    /** Cursor in image pixels, or `null` when the pointer is off the image. */
    const [cursor, setCursor] = useState<ImagePoint | null>(null);
    /** Committed selection, in image pixels. */
    const [selection, setSelection] = useState<ImageRect | null>(null);
    /** Live drag anchor + the rectangle derived from it, in image pixels. */
    const [drag, setDrag] = useState<{ anchor: ImagePoint; rect: ImageRect } | null>(null);

    // Mirrors of the state the imperative paths (wheel listener, ResizeObserver,
    // pointer handlers) need. Written only from those paths — never during
    // render — so state and refs cannot drift.
    const scaleRef = useRef(1);
    const naturalRef = useRef<ImageSize | null>(null);
    /** "fit" until the user zooms by hand; a resize re-fits only in that mode. */
    const zoomModeRef = useRef<"fit" | "manual">("fit");
    /** Zoom anchor captured before the re-layout, applied after it. */
    const pendingAnchor = useRef<
        { pointer: ImagePoint; origin: ImagePoint; prevScale: number } | null
    >(null);
    /** Natural size that `applyNatural` last acted on, so a duplicate `load`
     *  (cache hit + explicit layout probe) cannot reset a user's zoom. */
    const appliedNaturalRef = useRef("");

    const menu = useContextMenu<MenuPayload>();

    // ── Scale plumbing ───────────────────────────────────────────────
    const commitScale = useCallback((next: number, mode: "fit" | "manual") => {
        scaleRef.current = next;
        zoomModeRef.current = mode;
        setScale(next);
    }, []);

    /** Zoom to `nextScale`, keeping the image pixel under `anchorClient` put. */
    const zoomTo = useCallback(
        (nextScale: number, anchorClient?: ImagePoint) => {
            const prev = scaleRef.current;
            const next = clampZoom(nextScale);
            if (Math.abs(next - prev) < 1e-6) {
                zoomModeRef.current = "manual";
                return;
            }
            const img = imgRef.current;
            pendingAnchor.current =
                anchorClient && img
                    ? (() => {
                          const rect = img.getBoundingClientRect();
                          return {
                              pointer: anchorClient,
                              origin: { x: rect.left, y: rect.top },
                              prevScale: prev,
                          };
                      })()
                    : null;
            commitScale(next, "manual");
        },
        [commitScale],
    );

    /** Zoom around the middle of the viewport — what the buttons do. */
    const zoomFromCentre = useCallback(
        (factor: number) => {
            const viewport = viewportRef.current;
            const rect = viewport?.getBoundingClientRect();
            const centre = rect
                ? { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 }
                : undefined;
            zoomTo(scaleRef.current * factor, centre);
        },
        [zoomTo],
    );

    const fitToViewport = useCallback(() => {
        const size = naturalRef.current;
        const rect = viewportRef.current?.getBoundingClientRect();
        if (!size || !rect) return;
        // `VIEWPORT_PADDING_PX` is applied on both sides: the stage keeps the
        // same breathing room the pre-viewer `max-h-full max-w-full` img had.
        const fit = fitScale(size, {
            width: Math.max(0, rect.width - VIEWPORT_PADDING_PX * 2),
            height: Math.max(0, rect.height - VIEWPORT_PADDING_PX * 2),
        });
        pendingAnchor.current = null;
        commitScale(fit, "fit");
        // A fit always shrinks back inside the viewport, where the browser
        // clamps the scroll offsets itself; no explicit recentring needed.
    }, [commitScale]);

    /** Record the decoded size and fit it — once per distinct natural size. */
    const applyNatural = useCallback(
        (width: number, height: number) => {
            if (!(width > 0) || !(height > 0)) return;
            const key = `${width}x${height}`;
            if (appliedNaturalRef.current === key) return;
            appliedNaturalRef.current = key;
            naturalRef.current = { width, height };
            setNatural({ width, height });
            setCursor(null);
            setSelection(null);
            setDrag(null);
            fitToViewport();
        },
        [fitToViewport],
    );

    // Cursor-anchored zoom needs the NEW layout before it can correct the
    // scroll offsets: the delta is expressed in screen px, which only exist
    // after the browser has relaid the resized image out.
    useLayoutEffect(() => {
        const anchor = pendingAnchor.current;
        if (!anchor) return;
        pendingAnchor.current = null;
        const viewport = viewportRef.current;
        const img = imgRef.current;
        if (!viewport || !img) return;
        // Measured AFTER the re-layout: the element's origin also moves when
        // its auto-margins shrink, which a ratio-only formula cannot see.
        const rect = img.getBoundingClientRect();
        const drift = zoomAnchorDrift({
            pointer: anchor.pointer,
            originBefore: anchor.origin,
            originAfter: { x: rect.left, y: rect.top },
            prevScale: anchor.prevScale,
            nextScale: scale,
        });
        viewport.scrollLeft += drift.x;
        viewport.scrollTop += drift.y;
    }, [scale]);

    // Wheel zoom. A native non-passive listener: React's synthetic `onWheel`
    // is registered passively, so `preventDefault()` there is ignored and the
    // panel scrolls while the image zooms.
    useEffect(() => {
        const viewport = viewportRef.current;
        if (!viewport) return;
        const onWheel = (e: WheelEvent) => {
            if (!naturalRef.current) return;
            const factor = wheelZoomFactor(e.deltaY);
            if (factor === 1) return;
            e.preventDefault();
            zoomTo(scaleRef.current * factor, { x: e.clientX, y: e.clientY });
        };
        viewport.addEventListener("wheel", onWheel, { passive: false });
        return () => viewport.removeEventListener("wheel", onWheel);
    }, [zoomTo]);

    // Keep "fit" actually fitting while the panel is resized.
    useEffect(() => {
        const viewport = viewportRef.current;
        if (!viewport || typeof ResizeObserver === "undefined") return;
        const observer = new ResizeObserver(() => {
            if (zoomModeRef.current !== "fit") return;
            fitToViewport();
        });
        observer.observe(viewport);
        return () => observer.disconnect();
    }, [fitToViewport]);

    // Escape drops the selection — the keyboard twin of the menu's "Clear".
    useEffect(() => {
        if (!selection) return;
        const onKey = (e: KeyboardEvent) => {
            if (e.key === "Escape") setSelection(null);
        };
        window.addEventListener("keydown", onKey);
        return () => window.removeEventListener("keydown", onKey);
    }, [selection]);

    // A different image in the same tab: drop everything that describes the
    // old one. A layout effect (not `useEffect`) so it runs BEFORE the new
    // bytes' `load` event on a cache hit, and the probe below covers the case
    // where that event already fired while this module was parsing.
    const prevSrcRef = useRef<string | null>(null);
    useLayoutEffect(() => {
        if (prevSrcRef.current === src) return;
        prevSrcRef.current = src;
        appliedNaturalRef.current = "";
        naturalRef.current = null;
        zoomModeRef.current = "fit";
        scaleRef.current = 1;
        setNatural(null);
        setScale(1);
        setSelection(null);
        setCursor(null);
        setDrag(null);
        const img = imgRef.current;
        if (img?.complete && img.naturalWidth > 0) {
            applyNatural(img.naturalWidth, img.naturalHeight);
        }
    }, [applyNatural, src]);

    // ── Pointer → image pixels ───────────────────────────────────────
    /** The image pixel under a pointer event, or `null` when nothing is
     *  decoded yet. Unclamped: callers decide whether out-of-bounds matters. */
    const pointOf = useCallback((client: { clientX: number; clientY: number }): ImagePoint | null => {
        const img = imgRef.current;
        if (!img) return null;
        const rect = img.getBoundingClientRect();
        return screenToImagePoint(client, { left: rect.left, top: rect.top }, scaleRef.current);
    }, []);

    const handlePointerDown = useCallback(
        (e: ReactPointerEvent<HTMLImageElement>) => {
            // Right / middle button: leave it to the context menu.
            if (e.button !== 0) return;
            const size = naturalRef.current;
            const raw = pointOf(e);
            if (!size || !raw) return;
            const anchor = clampPoint(raw, size);
            setSelection(null);
            setDrag({ anchor, rect: normalizeDragRect(anchor, anchor, size) });
            // Capture so the drag survives the pointer leaving the image (and
            // the viewport) — without it the box freezes mid-drag.
            e.currentTarget.setPointerCapture?.(e.pointerId);
            e.preventDefault();
        },
        [pointOf],
    );

    const handlePointerMove = useCallback(
        (e: ReactPointerEvent<HTMLImageElement>) => {
            const size = naturalRef.current;
            const raw = pointOf(e);
            if (!size || !raw) return;
            const point = clampPoint(raw, size);
            setCursor(point);
            setDrag((current) =>
                current ? { anchor: current.anchor, rect: normalizeDragRect(current.anchor, point, size) } : current,
            );
        },
        [pointOf],
    );

    const handlePointerUp = useCallback(
        (e: ReactPointerEvent<HTMLImageElement>) => {
            if (e.currentTarget.hasPointerCapture?.(e.pointerId)) {
                e.currentTarget.releasePointerCapture(e.pointerId);
            }
            const size = naturalRef.current;
            const raw = pointOf(e);
            const anchor = drag?.anchor;
            setDrag(null);
            if (!size || !raw || !anchor) return;
            // Too small to be a selection → the box disappears, which is the
            // honest reading of "that was a click, not a drag".
            setSelection(dragSelectionRect(anchor, raw, size));
        },
        [drag, pointOf],
    );

    const handleContextMenu = useCallback(
        (e: ReactMouseEvent<HTMLImageElement>) => {
            e.preventDefault();
            e.stopPropagation();
            const size = naturalRef.current;
            const raw = pointOf(e);
            if (!size || !raw) return;
            menu.openAt(e, {
                point: clampPoint(raw, size),
                // A selection in progress is not a selection: the menu's
                // "Add selection" acts on the committed one only.
                selection: drag ? null : selection,
            });
        },
        [drag, menu, pointOf, selection],
    );

    // ── Chat hand-off ────────────────────────────────────────────────
    const addRegion = useCallback(
        async (region: ImageRect) => {
            const img = imgRef.current;
            if (!img) return;
            try {
                const blob = await cropImageRegion(img, region);
                onAddRegionToChat(
                    new File([blob], regionFileName(fileName, region), { type: "image/png" }),
                    region,
                );
            } catch (err) {
                log.error("[ImageViewer] cropping the selection failed:", err);
                showToast({ type: "error", message: t("fileEditor.imageViewer.captureFailed") });
            }
        },
        [fileName, onAddRegionToChat, t],
    );

    // ── Context menu ─────────────────────────────────────────────────
    const menuItems = useMemo<ContextMenuItem<MenuPayload>[]>(() => {
        const payload = menu.payload;
        if (!payload) return [];
        const disabled = addToChatDisabledTitle !== null;
        const title = addToChatDisabledTitle ?? undefined;
        const items: ContextMenuItem<MenuPayload>[] = [];
        if (payload.selection) {
            const region = payload.selection;
            items.push({
                key: "add-region",
                icon: <MessageSquarePlus size={14} />,
                label: t("fileEditor.imageViewer.addRegionToChat"),
                disabled,
                title,
                onClick: () => void addRegion(region),
            });
        }
        items.push({
            key: "add-point",
            icon: <Crosshair size={14} />,
            label: t("fileEditor.imageViewer.addPointToChat"),
            disabled,
            title,
            onClick: () => onAddPointToChat(payload.point),
        });
        if (payload.selection) {
            items.push({
                key: "clear-selection",
                icon: <XSquare size={14} />,
                label: t("fileEditor.imageViewer.clearSelection"),
                onClick: () => setSelection(null),
            });
        }
        return items;
    }, [addRegion, addToChatDisabledTitle, menu.payload, onAddPointToChat, t]);

    const shownSelection = drag?.rect ?? selection;
    /** Selection geometry in SCREEN px inside the stage (image px × scale). */
    const selectionStyle = shownSelection
        ? {
              left: shownSelection.x * scale,
              top: shownSelection.y * scale,
              width: shownSelection.width * scale,
              height: shownSelection.height * scale,
          }
        : null;

    return (
        <div
            className="relative h-full w-full bg-editor-canvas"
            onDoubleClick={onOpenSource}
            title={onOpenSource ? openSourceHint : undefined}
        >
            <div
                ref={viewportRef}
                data-testid="image-viewer-viewport"
                className={cn(
                    "absolute inset-0 overflow-auto",
                    // The drag is the selection gesture, so the whole surface
                    // stays crosshair-like; `grabbing` would promise panning.
                    drag ? "cursor-crosshair select-none" : "cursor-crosshair",
                )}
            >
                <div
                    className="flex min-h-full min-w-full"
                    style={{ padding: VIEWPORT_PADDING_PX }}
                >
                    <div
                        className="relative m-auto shrink-0"
                        style={
                            natural
                                ? { width: natural.width * scale, height: natural.height * scale }
                                : undefined
                        }
                    >
                        <img
                            ref={imgRef}
                            src={src}
                            alt={fileName}
                            draggable={false}
                            data-testid="image-viewer-image"
                            // Intrinsic size until the natural size is known; the stage
                            // owns the size afterwards, so `scale` is the only thing that
                            // moves pixels.
                            className={
                                natural
                                    ? "block h-full w-full select-none"
                                    : "block max-h-full max-w-full select-none"
                            }
                            onLoad={(e) =>
                                applyNatural(e.currentTarget.naturalWidth, e.currentTarget.naturalHeight)
                            }
                            onPointerDown={handlePointerDown}
                            onPointerMove={handlePointerMove}
                            onPointerUp={handlePointerUp}
                            onPointerCancel={() => setDrag(null)}
                            // Off the image = no pixel to report. Guarded
                            // while dragging, which leaves the image by design.
                            onPointerLeave={() => {
                                if (!drag) setCursor(null);
                            }}
                            onContextMenu={handleContextMenu}
                        />
                        {selectionStyle && (
                            // One element draws both the outline and the outside dimming: a
                            // 9999px spread shadow is clipped by the viewport, so the two
                            // visuals cost one node and no masking layers.
                            <div
                                data-testid="image-viewer-selection"
                                className="pointer-events-none absolute border border-[var(--color-accent)] bg-[var(--color-accent)]/10"
                                style={{
                                    ...selectionStyle,
                                    boxShadow: "0 0 0 9999px rgba(0, 0, 0, 0.35)",
                                }}
                            />
                        )}
                    </div>
                </div>
            </div>

            {/* Zoom cluster — top-right of the preview, same shape as the
                chat's Mermaid viewer toolbar so the two read as one control. */}
            {natural && (
                <div className="absolute right-2 top-2 flex items-center gap-0.5 rounded-md border border-black/10 bg-white/90 px-1 py-0.5 shadow-sm backdrop-blur dark:border-white/10 dark:bg-zinc-800/90">
                    <button
                        type="button"
                        onClick={() => zoomFromCentre(zoomStepFactor(-1))}
                        disabled={scale <= MIN_ZOOM}
                        aria-label={t("fileEditor.imageViewer.zoomOut")}
                        title={t("fileEditor.imageViewer.zoomOut")}
                        className="flex items-center justify-center rounded p-0.5 text-text-tertiary transition-colors hover:bg-zinc-200 hover:text-zinc-700 disabled:cursor-not-allowed disabled:opacity-40 dark:hover:bg-zinc-700 dark:hover:text-zinc-200"
                    >
                        <ZoomOut className="h-3.5 w-3.5" />
                    </button>
                    <span
                        data-testid="image-viewer-zoom-level"
                        className="min-w-[3.25rem] text-center text-11 tabular-nums text-text-secondary"
                    >
                        {zoomPercent(scale)}%
                    </span>
                    <button
                        type="button"
                        onClick={() => zoomFromCentre(zoomStepFactor(1))}
                        disabled={scale >= MAX_ZOOM}
                        aria-label={t("fileEditor.imageViewer.zoomIn")}
                        title={t("fileEditor.imageViewer.zoomIn")}
                        className="flex items-center justify-center rounded p-0.5 text-text-tertiary transition-colors hover:bg-zinc-200 hover:text-zinc-700 disabled:cursor-not-allowed disabled:opacity-40 dark:hover:bg-zinc-700 dark:hover:text-zinc-200"
                    >
                        <ZoomIn className="h-3.5 w-3.5" />
                    </button>
                    <span className="mx-0.5 h-3.5 w-px bg-black/10 dark:bg-white/10" />
                    <button
                        type="button"
                        onClick={fitToViewport}
                        aria-label={t("fileEditor.imageViewer.fit")}
                        title={t("fileEditor.imageViewer.fit")}
                        className="flex items-center justify-center rounded p-0.5 text-text-tertiary transition-colors hover:bg-zinc-200 hover:text-zinc-700 dark:hover:bg-zinc-700 dark:hover:text-zinc-200"
                    >
                        <Maximize2 className="h-3.5 w-3.5" />
                    </button>
                </div>
            )}

            {/* Cursor readout — bottom-right of the preview area, in IMAGE
                pixels so the number is what the agent's tools address. While a
                drag is live it also reports the box being drawn. */}
            <div
                data-testid="image-viewer-cursor"
                className="pointer-events-none absolute bottom-2 right-2 rounded bg-black/60 px-2 py-0.5 font-mono text-11 leading-4 tabular-nums text-white/90"
            >
                {cursor
                    ? t("fileEditor.imageViewer.cursorPoint", { x: cursor.x, y: cursor.y })
                    : t("fileEditor.imageViewer.cursorIdle")}
                {shownSelection && (
                    <span data-testid="image-viewer-selection-size" className="ml-2 text-white/60">
                        {t("fileEditor.imageViewer.selectionSize", {
                            width: shownSelection.width,
                            height: shownSelection.height,
                        })}
                    </span>
                )}
            </div>

            <ContextMenu<MenuPayload>
                isOpen={menu.isOpen}
                menuProps={menu.menuProps}
                items={menuItems}
                payload={menu.payload}
                selectionAtOpen={menu.selectionAtOpen}
                onClose={menu.close}
            />
        </div>
    );
}