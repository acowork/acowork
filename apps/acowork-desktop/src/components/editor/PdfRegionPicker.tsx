/**
 * PdfRegionPicker — the "frame a region and send it to the agent" mode over a
 * PDF preview.
 *
 * Why a second renderer instead of drawing a rectangle over the native viewer:
 * the `<iframe>` PDF is painted by the WebView's own plugin, so its page, zoom
 * and scroll offset are not readable from our document. A rectangle laid over
 * it would have no defined mapping onto the file, and the crop would be a
 * guess. Here the page is rendered by pdf.js at a fixed output scale, so every
 * pixel the user frames is a pixel we own.
 *
 * Layer split (identical to `ImageViewer`, whose geometry and crop this reuses
 * verbatim — neither was ever image-specific):
 *
 *   - `pdfRasterize.ts` owns talking to pdf.js (injectable, so jsdom never
 *     loads the engine).
 *   - `imageViewerGeometry.ts` owns every number (scale, page pixel, drag rect).
 *   - `imageCrop.ts` owns turning a rectangle into PNG bytes.
 *   - the panel that mounted us owns the prompt text and the workspace path.
 *
 * Layout: the rendered canvas is laid out at exactly `natural × scale` inside
 * an `overflow: auto` viewport, so the pointer→pixel conversion is one
 * division by `scale` against the stage's own bounding box — the same "no
 * transform stack to invert, panning is native scrolling" contract
 * `ImageViewer` documents. `natural` is the *rendered bitmap*, which is
 * `PDF_OUTPUT_SCALE` times the page's print size: 100% here means "one bitmap
 * pixel per screen pixel", and it is the same space the crop is measured in.
 *
 * What this component deliberately does NOT do: decide whether the reference
 * may be sent (`addToChatDisabledTitle` carries that answer), build the chat
 * prompt, or know the workspace-relative path.
 */
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { MouseEvent as ReactMouseEvent, PointerEvent as ReactPointerEvent } from "react";
import {
    ChevronLeft,
    ChevronRight,
    Crop,
    Loader2,
    Maximize2,
    MessageSquarePlus,
    X,
    XSquare,
    ZoomIn,
    ZoomOut,
} from "lucide-react";
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
    zoomAnchorDrift,
    zoomPercent,
    zoomStepFactor,
    type ImagePoint,
    type ImageRect,
    type ImageSize,
} from "./imageViewerGeometry";
import { createPdfPageRenderer, type PdfPageRenderer } from "./pdfRasterize";

/** Gap kept between the page and the viewport edge when it fits. */
const VIEWPORT_PADDING_PX = 16;

export interface PdfRegionPickerProps {
    /** PDF bytes for the whole document (converted Office file, or a native
     *  PDF). Identity must be stable — a new array re-opens the document. */
    bytes: Uint8Array;
    /** Source file name — stems the cropped attachment's name. */
    fileName: string;
    /** Tooltip when the hand-off is unavailable, `null` when it is live.
     *  Resolved by the panel, the same rule every other "Add to Chat" uses. */
    addToChatDisabledTitle: string | null;
    /** A cropped page region, ready to attach. */
    onAddRegionToChat: (file: File, info: { page: number }) => void;
    /** Leave the picker; the native viewer takes the surface back. */
    onClose: () => void;
    /**
     * Page opener. Injectable because jsdom can neither spawn a pdf.js worker
     * nor parse a PDF: the test hands back a stub and asserts on the gesture,
     * which is the only part of this component that can be wrong.
     */
    openDocument?: (bytes: Uint8Array) => Promise<PdfPageRenderer>;
}

type MenuPayload = { selection: ImageRect | null };

export function PdfRegionPicker({
    bytes,
    fileName,
    addToChatDisabledTitle,
    onAddRegionToChat,
    onClose,
    openDocument = createPdfPageRenderer,
}: PdfRegionPickerProps) {
    const { t } = useTranslation();
    const viewportRef = useRef<HTMLDivElement | null>(null);
    const stageRef = useRef<HTMLDivElement | null>(null);
    /** pdf.js owns the canvas, so it is appended into this holder rather than
     *  declared in JSX — the page size is only known once it has rendered. */
    const holderRef = useRef<HTMLDivElement | null>(null);
    const canvasRef = useRef<HTMLCanvasElement | null>(null);

    const [renderer, setRenderer] = useState<PdfPageRenderer | null>(null);
    const [page, setPage] = useState(1);
    /** A render is in flight — gates the page buttons so two of them cannot
     *  fight over the same holder. */
    const [busy, setBusy] = useState(true);
    const [failed, setFailed] = useState(false);
    const [natural, setNatural] = useState<ImageSize | null>(null);
    const [scale, setScale] = useState(1);
    const [selection, setSelection] = useState<ImageRect | null>(null);
    const [drag, setDrag] = useState<{ anchor: ImagePoint; rect: ImageRect } | null>(null);

    // Mirrors of what the imperative paths (wheel listener, ResizeObserver,
    // pointer handlers) read. Written only from those paths.
    const scaleRef = useRef(1);
    const naturalRef = useRef<ImageSize | null>(null);
    const zoomModeRef = useRef<"fit" | "manual">("fit");
    const pendingAnchor = useRef<
        { pointer: ImagePoint; origin: ImagePoint; prevScale: number } | null
    >(null);
    /** Generation guard: a slow render of page 3 must not overwrite page 4
     *  after the user moved on, and nothing may touch a disposed document. */
    const generationRef = useRef(0);

    const menu = useContextMenu<MenuPayload>();

    // ── Document open ────────────────────────────────────────────────
    useEffect(() => {
        const generation = ++generationRef.current;
        let disposed = false;
        let opened: PdfPageRenderer | null = null;
        setRenderer(null);
        setBusy(true);
        setFailed(false);
        void (async () => {
            try {
                const next = await openDocument(bytes);
                if (disposed || generation !== generationRef.current) {
                    // Lost interest (unmount, or a newer `bytes` arrived):
                    // release what we just paid to open.
                    await next.destroy();
                    return;
                }
                opened = next;
                setRenderer(next);
            } catch (err) {
                log.error("[PdfRegionPicker] opening the PDF failed:", err);
                if (!disposed && generation === generationRef.current) {
                    setBusy(false);
                    setFailed(true);
                }
            }
        })();
        return () => {
            disposed = true;
            void opened?.destroy();
        };
    }, [bytes, openDocument]);

    // ── Scale plumbing (as `ImageViewer`) ────────────────────────────
    const commitScale = useCallback((next: number, mode: "fit" | "manual") => {
        scaleRef.current = next;
        zoomModeRef.current = mode;
        setScale(next);
    }, []);

    const fitToViewport = useCallback(() => {
        const size = naturalRef.current;
        const rect = viewportRef.current?.getBoundingClientRect();
        if (!size || !rect) return;
        pendingAnchor.current = null;
        commitScale(
            fitScale(size, {
                width: Math.max(0, rect.width - VIEWPORT_PADDING_PX * 2),
                height: Math.max(0, rect.height - VIEWPORT_PADDING_PX * 2),
            }),
            "fit",
        );
    }, [commitScale]);

    // ── Page rendering ───────────────────────────────────────────────
    const drawPage = useCallback(
        async (pageNumber: number) => {
            if (!renderer) return;
            const generation = generationRef.current;
            setBusy(true);
            try {
                const canvas = await renderer.render(pageNumber);
                if (generation !== generationRef.current) return;
                // Parked, not appended: the holder lives inside the page box,
                // and on the very first page that box does not exist yet — it
                // is created by the `natural` state set below. The layout
                // effect underneath is the one place that knows the node is on
                // screen.
                canvasRef.current = canvas;
                const size = { width: canvas.width, height: canvas.height };
                naturalRef.current = size;
                setNatural(size);
                setSelection(null);
                setDrag(null);
                fitToViewport();
            } catch (err) {
                log.error("[PdfRegionPicker] rendering the page failed:", err);
                if (generation === generationRef.current) {
                    // The body already says why: `failed` swaps the viewport
                    // for the message. A toast on top of that would be the
                    // same sentence twice — and keeping `t` out of this
                    // callback is what makes it safe as an effect dependency.
                    setFailed(true);
                }
            } finally {
                if (generation === generationRef.current) setBusy(false);
            }
        },
        [renderer, fitToViewport],
    );

    useEffect(() => {
        if (!renderer) return;
        void drawPage(page);
    }, [renderer, page, drawPage]);

    // Mount the page that was just rendered. `natural` gets a fresh object on
    // every draw — including a re-draw of the same page size — so this runs
    // exactly once per painted page.
    useLayoutEffect(() => {
        const holder = holderRef.current;
        const canvas = canvasRef.current;
        if (!holder || !canvas || holder.firstElementChild === canvas) return;
        // The bitmap is the page's own size × OUTPUT_SCALE; the CSS box the
        // holder sits in is what makes it `natural × scale` on screen.
        canvas.className = "block h-full w-full select-none";
        holder.replaceChildren(canvas);
    }, [natural]);

    const zoomTo = useCallback(
        (nextScale: number, anchorClient?: ImagePoint) => {
            const prev = scaleRef.current;
            const next = clampZoom(nextScale);
            if (Math.abs(next - prev) < 1e-6) {
                zoomModeRef.current = "manual";
                return;
            }
            const stage = stageRef.current;
            pendingAnchor.current =
                anchorClient && stage
                    ? (() => {
                          const rect = stage.getBoundingClientRect();
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
            const rect = viewportRef.current?.getBoundingClientRect();
            const centre = rect
                ? { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 }
                : undefined;
            zoomTo(scaleRef.current * factor, centre);
        },
        [zoomTo],
    );

    // Cursor-anchored zoom is corrected AFTER the re-layout, for the reason
    // `ImageViewer` spells out: the drift is expressed in screen pixels, which
    // only exist once the resized page has been laid out.
    useLayoutEffect(() => {
        const anchor = pendingAnchor.current;
        if (!anchor) return;
        pendingAnchor.current = null;
        const viewport = viewportRef.current;
        const stage = stageRef.current;
        if (!viewport || !stage) return;
        const rect = stage.getBoundingClientRect();
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

    // Wheel zoom: a native non-passive listener, because React's synthetic
    // `onWheel` is passive and would let the panel scroll while the page zooms.
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

    // ── Keyboard ─────────────────────────────────────────────────────
    useEffect(() => {
        const onKey = (e: KeyboardEvent) => {
            if (e.key !== "Escape") return;
            // Two-stage: first the framing, then the mode. Leaving the picker
            // on a stray Esc would be the worse default.
            if (selection) {
                setSelection(null);
                return;
            }
            onClose();
        };
        window.addEventListener("keydown", onKey);
        return () => window.removeEventListener("keydown", onKey);
    }, [onClose, selection]);

    // ── Pointer → page pixels ────────────────────────────────────────
    /** The bitmap pixel under a pointer event. Unclamped: callers decide
     *  whether an out-of-bounds value matters. */
    const pointOf = useCallback(
        (client: { clientX: number; clientY: number }): ImagePoint | null => {
            const stage = stageRef.current;
            if (!stage) return null;
            const rect = stage.getBoundingClientRect();
            return screenToImagePoint(client, { left: rect.left, top: rect.top }, scaleRef.current);
        },
        [],
    );

    const handlePointerDown = useCallback(
        (e: ReactPointerEvent<HTMLDivElement>) => {
            // Right / middle button: leave it to the context menu.
            if (e.button !== 0) return;
            const size = naturalRef.current;
            const raw = pointOf(e);
            if (!size || !raw) return;
            const anchor = clampPoint(raw, size);
            setSelection(null);
            setDrag({ anchor, rect: normalizeDragRect(anchor, anchor, size) });
            // Capture so the drag survives the pointer leaving the page.
            e.currentTarget.setPointerCapture?.(e.pointerId);
            e.preventDefault();
        },
        [pointOf],
    );

    const handlePointerMove = useCallback(
        (e: ReactPointerEvent<HTMLDivElement>) => {
            const size = naturalRef.current;
            const raw = pointOf(e);
            if (!size || !raw) return;
            const point = clampPoint(raw, size);
            setDrag((current) =>
                current
                    ? { anchor: current.anchor, rect: normalizeDragRect(current.anchor, point, size) }
                    : current,
            );
        },
        [pointOf],
    );

    const handlePointerUp = useCallback(
        (e: ReactPointerEvent<HTMLDivElement>) => {
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
        (e: ReactMouseEvent<HTMLDivElement>) => {
            e.preventDefault();
            e.stopPropagation();
            // A drag in progress is not a selection.
            menu.openAt(e, { selection: drag ? null : selection });
        },
        [drag, menu, selection],
    );

    // ── Chat hand-off ────────────────────────────────────────────────
    const addRegion = useCallback(
        async (region: ImageRect) => {
            const canvas = canvasRef.current;
            if (!canvas) return;
            try {
                const blob = await cropImageRegion(canvas, region);
                onAddRegionToChat(
                    new File([blob], regionFileName(fileName, region), { type: "image/png" }),
                    { page },
                );
                // Stay in the mode and drop the framing: one page usually has
                // more than one thing worth showing.
                setSelection(null);
            } catch (err) {
                log.error("[PdfRegionPicker] cropping the region failed:", err);
                showToast({ type: "error", message: t("fileEditor.imageViewer.captureFailed") });
            }
        },
        [fileName, onAddRegionToChat, page, t],
    );

    const menuItems = useMemo<ContextMenuItem<MenuPayload>[]>(() => {
        const region = menu.payload?.selection;
        if (!region) return [];
        return [
            {
                key: "add-region",
                icon: <MessageSquarePlus size={14} />,
                label: t("fileEditor.imageViewer.addRegionToChat"),
                disabled: addToChatDisabledTitle !== null,
                title: addToChatDisabledTitle ?? undefined,
                onClick: () => void addRegion(region),
            },
            {
                key: "clear-selection",
                icon: <XSquare size={14} />,
                label: t("fileEditor.imageViewer.clearSelection"),
                onClick: () => setSelection(null),
            },
        ];
    }, [addRegion, addToChatDisabledTitle, menu.payload, t]);

    const shownSelection = drag?.rect ?? selection;
    /** Selection geometry in SCREEN px inside the stage (bitmap px × scale). */
    const selectionStyle = shownSelection
        ? {
              left: shownSelection.x * scale,
              top: shownSelection.y * scale,
              width: shownSelection.width * scale,
              height: shownSelection.height * scale,
          }
        : null;

    const toolBtn =
        "inline-flex h-6 w-6 shrink-0 items-center justify-center rounded text-text-tertiary transition-colors hover:bg-hover hover:text-text disabled:cursor-not-allowed disabled:opacity-40";
    const divider = <span className="mx-1 h-3.5 w-px shrink-0 bg-border-divider" />;

    return (
        <div className="flex h-full flex-col bg-editor-canvas" data-testid="pdf-region-picker">
            {/* Mode toolbar lives inside the body, so the tab keeps exactly one
                header — the frame's. */}
            <div className="flex shrink-0 flex-wrap items-center gap-1 border-b border-border-divider bg-modal-surface px-2 py-1">
                <Crop className="h-3.5 w-3.5 shrink-0 text-text-tertiary" />
                <span className="shrink-0 text-11 font-medium text-text-secondary">
                    {t("fileEditor.pdfViewer.regionPickerTitle")}
                </span>
                {divider}
                <button
                    type="button"
                    className={toolBtn}
                    onClick={() => setPage((p) => Math.max(1, p - 1))}
                    disabled={busy || !renderer || page <= 1}
                    aria-label={t("fileEditor.pdfViewer.prevPage")}
                    title={t("fileEditor.pdfViewer.prevPage")}
                >
                    <ChevronLeft className="h-3.5 w-3.5" />
                </button>
                <span
                    data-testid="pdf-page-indicator"
                    className="w-20 shrink-0 text-center text-11 tabular-nums text-text-secondary"
                >
                    {renderer
                        ? t("fileEditor.pdfViewer.pageIndicator", {
                            page,
                            pages: renderer.numPages,
                        })
                        : ""}
                </span>
                <button
                    type="button"
                    className={toolBtn}
                    onClick={() => setPage((p) => (renderer ? Math.min(renderer.numPages, p + 1) : p))}
                    disabled={busy || !renderer || page >= (renderer?.numPages ?? 1)}
                    aria-label={t("fileEditor.pdfViewer.nextPage")}
                    title={t("fileEditor.pdfViewer.nextPage")}
                >
                    <ChevronRight className="h-3.5 w-3.5" />
                </button>
                {divider}
                <button
                    type="button"
                    className={toolBtn}
                    onClick={() => zoomFromCentre(zoomStepFactor(-1))}
                    disabled={!natural || scale <= MIN_ZOOM}
                    aria-label={t("fileEditor.imageViewer.zoomOut")}
                    title={t("fileEditor.imageViewer.zoomOut")}
                >
                    <ZoomOut className="h-3.5 w-3.5" />
                </button>
                <span
                    data-testid="pdf-zoom-level"
                    className="w-12 shrink-0 text-center text-11 tabular-nums text-text-secondary"
                >
                    {natural ? `${zoomPercent(scale)}%` : "—"}
                </span>
                <button
                    type="button"
                    className={toolBtn}
                    onClick={() => zoomFromCentre(zoomStepFactor(1))}
                    disabled={!natural || scale >= MAX_ZOOM}
                    aria-label={t("fileEditor.imageViewer.zoomIn")}
                    title={t("fileEditor.imageViewer.zoomIn")}
                >
                    <ZoomIn className="h-3.5 w-3.5" />
                </button>
                <button
                    type="button"
                    className={toolBtn}
                    onClick={fitToViewport}
                    disabled={!natural}
                    aria-label={t("fileEditor.imageViewer.fit")}
                    title={t("fileEditor.imageViewer.fit")}
                >
                    <Maximize2 className="h-3.5 w-3.5" />
                </button>
                <div className="ml-auto flex shrink-0 items-center gap-1">
                    <button
                        type="button"
                        className={cn(
                            "inline-flex h-6 shrink-0 items-center gap-1 rounded border border-border-default px-2 text-11 transition-colors",
                            "hover:bg-hover disabled:cursor-not-allowed disabled:opacity-40",
                        )}
                        onClick={() => selection && void addRegion(selection)}
                        disabled={!selection || busy || addToChatDisabledTitle !== null}
                        title={addToChatDisabledTitle ?? t("fileEditor.pdfViewer.selectRegion")}
                    >
                        <MessageSquarePlus className="h-3.5 w-3.5" />
                        {t("fileEditor.pdfViewer.addRegion")}
                    </button>
                    <button
                        type="button"
                        className={toolBtn}
                        onClick={onClose}
                        aria-label={t("fileEditor.close")}
                        title={t("fileEditor.close")}
                    >
                        <X className="h-3.5 w-3.5" />
                    </button>
                </div>
            </div>

            {failed ? (
                <div className="flex min-h-0 flex-1 flex-col items-center justify-center gap-2 text-xs text-text-tertiary">
                    <XSquare className="h-4 w-4" />
                    {t("fileEditor.pdfViewer.rasterizeFailed")}
                </div>
            ) : (
                <div
                    ref={viewportRef}
                    data-testid="pdf-region-viewport"
                    className={cn(
                        "relative min-h-0 flex-1 overflow-auto",
                        // The drag IS the selection gesture, so the surface
                        // stays crosshair-like; `grabbing` would promise panning.
                        drag ? "cursor-crosshair select-none" : "cursor-crosshair",
                    )}
                >
                    <div className="flex min-h-full min-w-full" style={{ padding: VIEWPORT_PADDING_PX }}>
                        {natural ? (
                            <div
                                ref={stageRef}
                                data-testid="pdf-region-stage"
                                className="relative m-auto shrink-0 bg-white ring-1 ring-zinc-300/60 dark:ring-zinc-600/60"
                                style={{
                                    width: natural.width * scale,
                                    height: natural.height * scale,
                                }}
                                onPointerDown={handlePointerDown}
                                onPointerMove={handlePointerMove}
                                onPointerUp={handlePointerUp}
                                onPointerCancel={() => setDrag(null)}
                                onContextMenu={handleContextMenu}
                            >
                                {/* The canvas pdf.js rendered, filling the
                                    stage box exactly (`h-full w-full`). */}
                                <div
                                    ref={holderRef}
                                    data-testid="pdf-region-canvas-holder"
                                    className="absolute inset-0 overflow-hidden"
                                />
                                {selectionStyle && (
                                    // One element draws both the outline and the
                                    // outside dimming: a 9999px spread shadow is
                                    // clipped by the viewport, so the two visuals
                                    // cost one node and no masking layer.
                                    <div
                                        data-testid="pdf-region-selection"
                                        className="pointer-events-none absolute border border-[var(--color-accent)] bg-[var(--color-accent)]/10"
                                        style={{
                                            ...selectionStyle,
                                            boxShadow: "0 0 0 9999px rgba(0, 0, 0, 0.35)",
                                        }}
                                    />
                                )}
                            </div>
                        ) : (
                            <div
                                data-testid="pdf-region-placeholder"
                                className="m-auto flex items-center gap-2 text-xs text-text-tertiary"
                            >
                                <Loader2 className="h-4 w-4 animate-spin" />
                                {t("fileEditor.previewLoading")}
                            </div>
                        )}
                    </div>
                </div>
            )}

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

export default PdfRegionPicker;
