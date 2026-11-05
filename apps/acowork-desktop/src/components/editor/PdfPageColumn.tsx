/**
 * PdfPageColumn — the continuous page column: what a PDF looks like when we
 * render it ourselves instead of handing the surface to the WebView's built-in
 * viewer.
 *
 * Why this exists as its own component: the preview and the region-framing
 * mode are the same document. Two renderers meant two toolbars, two zoom
 * models, and a mode switch that threw away where the user was looking — the
 * complaint that started this. One column serves both: `framing` turns the
 * pointer into a selection gesture, everything else (scroll, zoom, which pages
 * are painted) is shared state that survives the toggle.
 *
 * It is also the only way the native chrome can be made to match the rest of
 * the app: the WebView viewer's toolbar, outline panel and thumbnails are not
 * our DOM, so their 15 px UI can never be brought in line with our 11 px one.
 * Here every pixel is ours.
 *
 * What it owns, and what it deliberately does not:
 *
 *   - owns: opening the document, measuring every page, laying the column out,
 *     painting the pages the viewport can see, the zoom/scroll maths, the
 *     framing gesture, and drawing the selection.
 *   - does not own: what a framed region MEANS (the attachment, the prompt, the
 *     context menu, the toolbar's buttons). Those live in the parent, which
 *     reads view state through `onViewChange` and drives the column through the
 *     handle.
 *
 * Lazy painting is not an optimisation here but a requirement: a page bitmap at
 * `PDF_OUTPUT_SCALE` is ~8 MB of canvas, so thirty pages cannot all exist at
 * once. `pdfPageStack.liveWindow` decides which few do, and canvases are
 * evicted as they leave.
 *
 * Geometry contract: each page box is laid out at exactly `bitmap × scale`
 * inside one `overflow: auto` viewport, so a pointer offset divided by `scale`
 * lands on a bitmap pixel — the same "no transform stack to invert, panning is
 * native scrolling" rule `ImageViewer` documents. The canvas is stretched to
 * its box by CSS (`h-full w-full`), never by resizing the bitmap: the crop is
 * measured in bitmap pixels and must not care what it is displayed at.
 */
import {
    forwardRef,
    useCallback,
    useEffect,
    useImperativeHandle,
    useLayoutEffect,
    useMemo,
    useRef,
    useState,
} from "react";
import type { MouseEvent as ReactMouseEvent, PointerEvent as ReactPointerEvent } from "react";
import { Loader2, XSquare } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import { log } from "../../lib/logger";
import {
    clampZoom,
    dragSelectionRect,
    fitWidthScale,
    normalizeDragRect,
    wheelZoomFactor,
    zoomAnchorDrift,
    type ImagePoint,
    type ImageRect,
    type ImageSize,
} from "./imageViewerGeometry";
import {
    boxOf,
    layoutStack,
    liveWindow,
    pageAtColumnPoint,
    pageAtScroll,
    pointOnPage,
    scrollTargetForPage,
    widestPage,
} from "./pdfPageStack";
import { cropImageRegion } from "./imageCrop";
import { createPdfPageRenderer, type PdfPageRenderer } from "./pdfRasterize";

/** Gap kept between the page column and the viewport edge when it fits. */
const VIEWPORT_PADDING_PX = 16;

/** A framed region. The page is part of the selection, not decoration: a crop
 *  reads one page's bitmap and cannot straddle two. */
export interface PdfRegion {
    page: number;
    rect: ImageRect;
}

/** What the parent's toolbar needs to know. Emitted only when one of these
 *  values actually changes — a scroll event that stays on the same page must
 *  not re-render the header. */
export interface PdfColumnView {
    status: "loading" | "ready" | "failed";
    scale: number;
    /** The page at the top of the viewport (1-based). */
    page: number;
    pageCount: number;
}

/** Imperative surface for the parent's toolbar and hand-off. A ref handle
 *  rather than lifted state because the column owns the numbers (scale, scroll,
 *  painted canvases) and a parent that re-derived them would only be able to
 *  disagree with them. */
export interface PdfPageColumnHandle {
    /** Multiply the current scale, anchored on the column's centre. */
    zoomBy(factor: number): void;
    /** Back to fit-to-width. */
    fitToWidth(): void;
    /** Scroll the page's own top to the viewport's top. */
    scrollToPage(page: number): void;
    /** PNG bytes of a region, rendering its page on demand if it has been
     *  evicted from the paint window in the meantime. */
    crop(region: PdfRegion): Promise<Blob>;
}

export interface PdfPageColumnProps {
    /** PDF bytes for the whole document. Identity must be stable — a new array
     *  re-opens the document. The array may be shared and may be opened more
     *  than once: `pdfRasterize` copies at the pdf.js transfer point. */
    bytes: Uint8Array;
    /** Capture the pointer as a framing gesture. Off = plain preview. */
    framing: boolean;
    /** The committed region to keep drawn. Owned by the parent, so it survives
     *  scrolling and is cleared by the parent's own rules. */
    selection: PdfRegion | null;
    /** A drag finished: the region it produced, or `null` when it was too
     *  small to be a selection (a click). Also called with `null` when a new
     *  drag begins, so the parent never holds a region the canvas has lost. */
    onRegion: (region: PdfRegion | null) => void;
    onViewChange?: (view: PdfColumnView) => void;
    /** Right-click over the surface. The parent owns the menu; this only says
     *  where it happened. */
    onContextMenu?: (event: ReactMouseEvent<HTMLDivElement>) => void;
    /** Document opener. Injectable because jsdom can neither spawn a pdf.js
     *  worker nor parse a PDF. */
    openDocument?: (bytes: Uint8Array) => Promise<PdfPageRenderer>;
}

export const PdfPageColumn = forwardRef<PdfPageColumnHandle, PdfPageColumnProps>(
    function PdfPageColumn(
        {
            bytes,
            framing,
            selection,
            onRegion,
            onViewChange,
            onContextMenu,
            openDocument = createPdfPageRenderer,
        },
        ref,
    ) {
        const { t } = useTranslation();
        const viewportRef = useRef<HTMLDivElement | null>(null);
        const columnRef = useRef<HTMLDivElement | null>(null);
        /** One holder per page: pdf.js owns the canvas node, so it is appended
         *  into the holder rather than declared in JSX — the page's size is
         *  only known once it has measured, its bitmap only once it has
         *  painted. */
        const holdersRef = useRef<Map<number, HTMLDivElement>>(new Map());

        const [renderer, setRenderer] = useState<PdfPageRenderer | null>(null);
        /** Bitmap size of every page, in document order. The column's whole
         *  scroll geometry comes from this, so it exists before anything is
         *  painted. */
        const [sizes, setSizes] = useState<ImageSize[] | null>(null);
        /** Painted canvases by page — the pool `liveWindow` fills and evicts. */
        const [painted, setPainted] = useState<Record<number, HTMLCanvasElement>>({});
        const [failed, setFailed] = useState(false);
        const [scale, setScale] = useState(1);
        const [scrollTop, setScrollTop] = useState(0);
        const [viewportHeight, setViewportHeight] = useState(0);
        /** The in-flight drag. The committed selection is the parent's, so it
         *  is not duplicated here. */
        const [drag, setDrag] = useState<(PdfRegion & { anchor: ImagePoint }) | null>(null);

        // Mirrors of what the imperative paths (wheel listener, ResizeObserver,
        // pointer handlers) read. Written only from those paths.
        const scaleRef = useRef(1);
        const sizesRef = useRef<ImageSize[] | null>(null);
        const layoutRef = useRef<ReturnType<typeof layoutStack> | null>(null);
        const zoomModeRef = useRef<"fit" | "manual">("fit");
        const dragRef = useRef<(PdfRegion & { anchor: ImagePoint }) | null>(null);
        /** Canvases as the crop path sees them, synchronously. */
        const paintedRef = useRef<Record<number, HTMLCanvasElement>>({});
        /** Pages whose render is in flight, so a scroll flick does not ask
         *  twice. */
        const inflightRef = useRef<Set<number>>(new Set());
        const pendingAnchor = useRef<
            { pointer: ImagePoint; origin: ImagePoint; prevScale: number } | null
        >(null);
        /** Generation guard: a slow render of page 3 must not be parked into a
         *  document the user has since replaced, and nothing may touch a
         *  disposed one. */
        const generationRef = useRef(0);

        const layout = useMemo(() => layoutStack(sizes ?? [], scale), [sizes, scale]);
        layoutRef.current = layout;

        // ── Scale plumbing (as `ImageViewer`, against the column) ─────
        const commitScale = useCallback((next: number, mode: "fit" | "manual") => {
            scaleRef.current = next;
            zoomModeRef.current = mode;
            setScale(next);
        }, []);

        /** Fit-to-width: a column is fitted by how wide its widest page is,
         *  never by its total height (which would shrink every page into
         *  mush). */
        const fitToViewport = useCallback(() => {
            const rect = viewportRef.current?.getBoundingClientRect();
            const widest = widestPage(sizesRef.current ?? []);
            if (!rect || widest <= 0) return;
            pendingAnchor.current = null;
            commitScale(
                fitWidthScale(
                    { width: widest, height: 1 },
                    { width: Math.max(0, rect.width - VIEWPORT_PADDING_PX * 2), height: 1 },
                ),
                "fit",
            );
        }, [commitScale]);

        // ── Document open + measure ───────────────────────────────────
        useEffect(() => {
            const generation = ++generationRef.current;
            let disposed = false;
            let opened: PdfPageRenderer | null = null;
            setRenderer(null);
            setSizes(null);
            setPainted({});
            paintedRef.current = {};
            inflightRef.current.clear();
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
                    // Measure before painting: the column needs its total
                    // height (and every page's gutter position) to be
                    // scrollable at all, and a page's viewport is a cheap read,
                    // not a render.
                    const measured = await next.measure();
                    if (disposed || generation !== generationRef.current) {
                        await next.destroy();
                        return;
                    }
                    sizesRef.current = measured;
                    setSizes(measured);
                    setRenderer(next);
                    // The observer's first callback ran before any page had a
                    // size (nothing to scale against), so fitting has to be
                    // asked for here too. Without this the column opens at
                    // whatever scale a later resize happened to compute.
                    fitToViewport();
                } catch (err) {
                    log.error("[PdfPageColumn] opening the PDF failed:", err);
                    if (!disposed && generation === generationRef.current) setFailed(true);
                }
            })();
            return () => {
                disposed = true;
                void opened?.destroy();
            };
        }, [bytes, fitToViewport, openDocument]);

        // ── Painting the live window ─────────────────────────────────
        useEffect(() => {
            if (!renderer || !sizes) return;
            const generation = generationRef.current;
            const want = liveWindow(layout, scrollTop, viewportHeight);
            const keep = new Set(want);

            for (const page of want) {
                if (paintedRef.current[page] || inflightRef.current.has(page)) continue;
                inflightRef.current.add(page);
                renderer
                    .render(page)
                    .then((canvas) => {
                        inflightRef.current.delete(page);
                        if (generation !== generationRef.current) return;
                        paintedRef.current = { ...paintedRef.current, [page]: canvas };
                        setPainted(paintedRef.current);
                    })
                    .catch((err) => {
                        inflightRef.current.delete(page);
                        if (generation !== generationRef.current) return;
                        // One page that will not draw must not take the whole
                        // column down with it: it stays a blank page box, which
                        // is visibly honest, and the reason goes to the log.
                        log.error(`[PdfPageColumn] rendering page ${page} failed:`, err);
                    });
            }

            // Evict what the user has scrolled away from. The bitmap is the
            // expensive part — a page box with no canvas costs a div.
            const dropped = Object.keys(paintedRef.current)
                .map(Number)
                .filter((page) => !keep.has(page));
            if (dropped.length) {
                paintedRef.current = Object.fromEntries(
                    Object.entries(paintedRef.current).filter(([page]) => keep.has(Number(page))),
                );
                setPainted(paintedRef.current);
            }
        }, [layout, renderer, sizes, scrollTop, viewportHeight]);

        /** Park each painted canvas in its page's holder, and empty the holders
         *  of pages that have been evicted. Runs after layout, so the holder
         *  exists by the time a canvas is parked into it.
         *
         *  The canvas is stretched by its holder rather than shown at its own
         *  bitmap size: pdf.js paints at `PDF_OUTPUT_SCALE` (a slide is ~1900
         *  px wide), the box is that bitmap times the display scale, and only
         *  CSS belongs in between. A canvas parked without it overflows the box
         *  and the holder's `overflow-hidden` crops the page to its top-left
         *  corner — the column looks magnified and cut off. */
        useLayoutEffect(() => {
            for (const [page, holder] of holdersRef.current) {
                const canvas = painted[page] ?? null;
                if (holder.firstElementChild === canvas) continue;
                if (!canvas) {
                    holder.replaceChildren();
                    continue;
                }
                canvas.className = "block h-full w-full select-none";
                holder.replaceChildren(canvas);
            }
        }, [painted, sizes]);

        // ── Zoom ──────────────────────────────────────────────────────
        const zoomTo = useCallback(
            (nextScale: number, anchorClient?: ImagePoint) => {
                const prev = scaleRef.current;
                const next = clampZoom(nextScale);
                if (Math.abs(next - prev) < 1e-6) {
                    zoomModeRef.current = "manual";
                    return;
                }
                const column = columnRef.current;
                pendingAnchor.current =
                    anchorClient && column
                        ? {
                            pointer: anchorClient,
                            origin: { x: column.getBoundingClientRect().left, y: column.getBoundingClientRect().top },
                            prevScale: prev,
                        }
                        : {
                            // No pointer given: anchor on the viewport centre,
                            // so the button zoom keeps the page you are looking
                            // at where it is.
                            pointer: {
                                x: (viewportRef.current?.clientWidth ?? 0) / 2,
                                y: (viewportRef.current?.clientHeight ?? 0) / 2,
                            },
                            origin: column
                                ? { x: column.getBoundingClientRect().left, y: column.getBoundingClientRect().top }
                                : { x: 0, y: 0 },
                            prevScale: prev,
                        };
                commitScale(next, "manual");
            },
            [commitScale],
        );

        /** Correct the scroll for the zoom that just happened, before the
         *  browser paints: the point under the cursor must still be under it. */
        useLayoutEffect(() => {
            const anchor = pendingAnchor.current;
            if (!anchor) return;
            pendingAnchor.current = null;
            const viewport = viewportRef.current;
            const column = columnRef.current;
            if (!viewport || !column) return;
            const rect = column.getBoundingClientRect();
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

        // A wheel moves through the document; Ctrl + wheel (and a trackpad
        // pinch, which Chromium reports the same way) makes the pages bigger.
        // The unmodified wheel is left to the browser — this is a document with
        // somewhere to scroll, and stealing that would be the surprising
        // version of the gesture. Native non-passive listener, because React's
        // synthetic `onWheel` is passive and could not cancel the ctrl case.
        useEffect(() => {
            const viewport = viewportRef.current;
            if (!viewport) return;
            const onWheel = (e: WheelEvent) => {
                if (!sizesRef.current) return;
                if (!e.ctrlKey) return;
                const factor = wheelZoomFactor(e.deltaY);
                if (factor === 1) return;
                // The WebView's own ctrl + wheel is page zoom: without this it
                // would scale the whole shell on top of our column.
                e.preventDefault();
                zoomTo(scaleRef.current * factor, { x: e.clientX, y: e.clientY });
            };
            viewport.addEventListener("wheel", onWheel, { passive: false });
            return () => viewport.removeEventListener("wheel", onWheel);
        }, [zoomTo]);

        // The paint window and the page readout both live off the scroll
        // position.
        useEffect(() => {
            const viewport = viewportRef.current;
            if (!viewport) return;
            const onScroll = () => setScrollTop(viewport.scrollTop);
            viewport.addEventListener("scroll", onScroll, { passive: true });
            return () => viewport.removeEventListener("scroll", onScroll);
        }, []);

        // Keep "fit" actually fitting while the panel is resized — and tell the
        // paint window how much column the viewport can now see.
        useEffect(() => {
            const viewport = viewportRef.current;
            if (!viewport || typeof ResizeObserver === "undefined") return;
            const observer = new ResizeObserver(() => {
                const rect = viewport.getBoundingClientRect();
                setViewportHeight(rect.height);
                if (zoomModeRef.current !== "fit") return;
                fitToViewport();
            });
            observer.observe(viewport);
            return () => observer.disconnect();
        }, [fitToViewport]);

        // ── View state out to the parent ──────────────────────────────
        const lastViewRef = useRef<PdfColumnView | null>(null);
        // A layout effect, not a passive one: the parent's toolbar renders the
        // page count from this, and handing it over after paint would show an
        // empty readout for one frame every time a document opens.
        useLayoutEffect(() => {
            const next: PdfColumnView = {
                status: failed ? "failed" : sizes ? "ready" : "loading",
                scale,
                page: pageAtScroll(layout, scrollTop),
                pageCount: sizes?.length ?? 0,
            };
            const prev = lastViewRef.current;
            if (
                prev
                && prev.status === next.status
                && prev.scale === next.scale
                && prev.page === next.page
                && prev.pageCount === next.pageCount
            ) {
                return;
            }
            lastViewRef.current = next;
            onViewChange?.(next);
        }, [failed, layout, onViewChange, scale, scrollTop, sizes]);

        // ── Handle ────────────────────────────────────────────────────
        const scrollToPage = useCallback((page: number) => {
            const viewport = viewportRef.current;
            const layoutNow = layoutRef.current;
            if (!viewport || !layoutNow) return;
            // Direct `scrollTop`, not `scrollTo()`: the same rule
            // `VirtualRowList` had to learn for WKWebView, and the version a
            // jsdom test can observe.
            const target = scrollTargetForPage(layoutNow, page);
            viewport.scrollTop = target;
            setScrollTop(target);
        }, []);

        useImperativeHandle(
            ref,
            () => ({
                zoomBy: (factor: number) => zoomTo(scaleRef.current * factor),
                fitToWidth: () => fitToViewport(),
                scrollToPage,
                async crop(region: PdfRegion) {
                    const paintedCanvas = paintedRef.current[region.page];
                    // The framing outlives its page's stay in the paint window:
                    // the user can scroll away and still press the button.
                    // Rendering the page on demand is the honest answer — the
                    // alternative is a control that silently does nothing.
                    const canvas = paintedCanvas ?? (await renderer?.render(region.page));
                    if (!canvas) throw new Error("No PDF document is open to crop");
                    return cropImageRegion(canvas, region.rect);
                },
            }),
            [fitToViewport, renderer, scrollToPage, zoomTo],
        );

        // ── Pointer → page pixels ─────────────────────────────────────
        /** The raw screen-px offset of a pointer event inside the column. */
        const columnPointOf = useCallback(
            (client: { clientX: number; clientY: number }): ImagePoint | null => {
                const column = columnRef.current;
                if (!column) return null;
                const rect = column.getBoundingClientRect();
                return { x: client.clientX - rect.left, y: client.clientY - rect.top };
            },
            [],
        );

        /** Where a pointer event lands on the page a drag is locked to. */
        const pointOnDragPage = useCallback(
            (client: { clientX: number; clientY: number }, page: number): ImagePoint | null => {
                const column = columnPointOf(client);
                return column ? pointOnPage(layoutRef.current ?? layout, page, column) : null;
            },
            [columnPointOf, layout],
        );

        const handlePointerDown = useCallback(
            (e: ReactPointerEvent<HTMLDivElement>) => {
                // Right / middle button: leave it to the context menu.
                if (e.button !== 0) return;
                const column = columnPointOf(e);
                if (!column) return;
                const hit = pageAtColumnPoint(layout, column);
                if (!hit) return;
                const size = boxOf(layout, hit.page)?.natural ?? { width: 0, height: 0 };
                const next = {
                    page: hit.page,
                    anchor: hit.point,
                    rect: normalizeDragRect(hit.point, hit.point, size),
                };
                dragRef.current = next;
                setDrag(next);
                onRegion(null);
                // Capture so the drag survives the pointer leaving the page —
                // and the column, which is what the arrows scroll past.
                e.currentTarget.setPointerCapture?.(e.pointerId);
                e.preventDefault();
            },
            [columnPointOf, layout, onRegion],
        );

        const handlePointerMove = useCallback(
            (e: ReactPointerEvent<HTMLDivElement>) => {
                const current = dragRef.current;
                if (!current) return;
                const point = pointOnDragPage(e, current.page);
                if (!point) return;
                const size = boxOf(layout, current.page)?.natural ?? { width: 0, height: 0 };
                // The page stays the one the drag started on; the point is
                // clamped onto it. A crop cannot straddle two bitmaps.
                const next = { ...current, rect: normalizeDragRect(current.anchor, point, size) };
                dragRef.current = next;
                setDrag(next);
            },
            [layout, pointOnDragPage],
        );

        const handlePointerUp = useCallback(
            (e: ReactPointerEvent<HTMLDivElement>) => {
                if (e.currentTarget.hasPointerCapture?.(e.pointerId)) {
                    e.currentTarget.releasePointerCapture(e.pointerId);
                }
                const current = dragRef.current;
                dragRef.current = null;
                setDrag(null);
                if (!current) return;
                const point = pointOnDragPage(e, current.page);
                if (!point) return;
                const size = boxOf(layout, current.page)?.natural ?? { width: 0, height: 0 };
                // Too small to be a selection → `null`, which is the honest
                // reading of "that was a click, not a drag".
                const rect = dragSelectionRect(current.anchor, point, size);
                onRegion(rect ? { page: current.page, rect } : null);
            },
            [layout, onRegion, pointOnDragPage],
        );

        // ── Selection rendering ───────────────────────────────────────
        const shown = drag ?? selection;
        /** Selection geometry in SCREEN px inside its own page box (bitmap px ×
         *  scale) — which is why it is positioned against the page, not the
         *  column: the page's box already carries its own centring and offset. */
        const selectionStyle = shown
            ? {
                left: shown.rect.x * scale,
                top: shown.rect.y * scale,
                width: shown.rect.width * scale,
                height: shown.rect.height * scale,
            }
            : null;

        if (failed) {
            return (
                <div className="flex h-full flex-col items-center justify-center gap-2 text-xs text-text-tertiary">
                    <XSquare className="h-4 w-4" />
                    {t("fileEditor.pdfViewer.rasterizeFailed")}
                </div>
            );
        }

        return (
            <div
                ref={viewportRef}
                data-testid="pdf-column-viewport"
                className={cn(
                    "relative h-full min-h-0 w-full overflow-auto",
                    // The drag IS the framing gesture, so the surface stays
                    // crosshair-like; `grabbing` would promise panning.
                    framing && "cursor-crosshair",
                )}
                onContextMenu={onContextMenu}
            >
                <div className="flex min-h-full min-w-full" style={{ padding: VIEWPORT_PADDING_PX }}>
                    {sizes ? (
                        <div
                            ref={columnRef}
                            data-testid="pdf-column"
                            className="relative m-auto shrink-0"
                            style={{ width: layout.width, height: layout.height }}
                            onPointerDown={framing ? handlePointerDown : undefined}
                            onPointerMove={framing ? handlePointerMove : undefined}
                            onPointerUp={framing ? handlePointerUp : undefined}
                            onPointerCancel={
                                framing
                                    ? () => {
                                        dragRef.current = null;
                                        setDrag(null);
                                    }
                                    : undefined
                            }
                        >
                            {layout.boxes.map((box) => (
                                <div
                                    key={box.page}
                                    data-testid="pdf-column-page"
                                    data-page={box.page}
                                    className="absolute bg-white ring-1 ring-zinc-300/60 dark:ring-zinc-600/60"
                                    style={{
                                        left: box.left,
                                        top: box.top,
                                        width: box.width,
                                        height: box.height,
                                    }}
                                >
                                    {/* The canvas pdf.js painted, filling the
                                        page box exactly. */}
                                    <div
                                        ref={(el) => {
                                            if (el) holdersRef.current.set(box.page, el);
                                            else holdersRef.current.delete(box.page);
                                        }}
                                        data-testid="pdf-column-canvas-holder"
                                        className="absolute inset-0 overflow-hidden"
                                    />
                                    {shown && shown.page === box.page && selectionStyle && (
                                        // One element draws both the outline and
                                        // the outside dimming: a 9999px spread
                                        // shadow is clipped by the viewport, so
                                        // the two views cannot disagree about
                                        // what is selected.
                                        <div
                                            data-testid="pdf-column-selection"
                                            className="pointer-events-none absolute border border-sky-500 bg-sky-500/10 shadow-[0_0_0_9999px_rgba(0,0,0,0.35)]"
                                            style={selectionStyle}
                                        />
                                    )}
                                </div>
                            ))}
                        </div>
                    ) : (
                        <div
                            data-testid="pdf-column-placeholder"
                            className="m-auto flex items-center gap-2 text-xs text-text-tertiary"
                        >
                            <Loader2 className="h-4 w-4 animate-spin" />
                            {t("fileEditor.previewLoading")}
                        </div>
                    )}
                </div>
            </div>
        );
    },
);

export default PdfPageColumn;
