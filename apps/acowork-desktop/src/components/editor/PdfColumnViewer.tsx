/**
 * PdfColumnViewer — the PDF preview surface: our own page column, its toolbar,
 * and the region-framing mode that rides on top of it.
 *
 * This replaced an `<iframe>` of the WebView's built-in PDF viewer. That was
 * never our DOM: its toolbar, outline panel and thumbnails are painted by the
 * PDF plugin at its own ~15 px UI scale, which no amount of CSS can bring in
 * line with the app's 11 px one, and its page / zoom / scroll offset cannot be
 * read back — so framing a region needed a SECOND renderer, and switching to
 * it threw away where the user was looking. One column now serves both jobs:
 * `framing` turns the pointer into a selection gesture and nothing else moves.
 *
 * Layer split (this component is only the toolbar and the semantics):
 *
 *   - `PdfPageColumn` owns the surface: open, measure, layout, lazy painting,
 *     zoom, scroll, the gesture, and drawing the selection.
 *   - `pdfPageStack.ts` / `imageViewerGeometry.ts` own the arithmetic.
 *   - `pdfRasterize.ts` owns talking to pdf.js, including the copy it hands
 *     over at the transfer point.
 *   - here: what a framed region MEANS — the PNG attachment, its page number,
 *     the context menu, the two-stage Escape, and when the hand-off is allowed.
 *   - the panel that mounted us owns the prompt text and the workspace path.
 *
 * A crop is one bitmap: the column clamps a drag to the page it started on and
 * reports `{ page, rect }` in that page's bitmap pixels, which is also the
 * space the crop is measured in.
 *
 * What this component deliberately does NOT do: decide whether the reference
 * may be sent (`addToChatDisabledTitle` carries that answer), build the chat
 * prompt, know the workspace-relative path, or cache painted canvases across
 * mounts — leaving the surface releases every page it paid to draw.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { MouseEvent as ReactMouseEvent } from "react";
import {
    ChevronLeft,
    ChevronRight,
    Crop,
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
import { regionFileName } from "./imageCrop";
import { MAX_ZOOM, MIN_ZOOM, zoomPercent, zoomStepFactor } from "./imageViewerGeometry";
import {
    PdfPageColumn,
    type PdfColumnView,
    type PdfPageColumnHandle,
    type PdfRegion,
} from "./PdfPageColumn";
import type { PdfPageRenderer } from "./pdfRasterize";

export interface PdfColumnViewerProps {
    /** PDF bytes for the whole document (converted Office file, or a native
     *  PDF). Identity must be stable — a new array re-opens the document.
     *  The array itself may be shared and may be opened more than once:
     *  `pdfRasterize` copies before handing anything to pdf.js, so this stays
     *  readable afterwards. */
    bytes: Uint8Array;
    /** True: the pointer frames a region and the framing controls appear.
     *  False: plain preview — the same column, scrolling and zooming, with the
     *  position and scale the user already had. */
    framing: boolean;
    /** Leave framing (the toolbar's X, and Escape with a framing standing). */
    onExitFraming: () => void;
    /** Source file name — stems the cropped attachment's name. */
    fileName: string;
    /** Tooltip when the hand-off is unavailable, `null` when it is live.
     *  Resolved by the panel, the same rule every other "Add to Chat" uses. */
    addToChatDisabledTitle: string | null;
    /** A cropped page region, ready to attach. Absent means this document
     *  cannot be framed into a chat, and the surface says so: the hand-off
     *  button is not offered at all. */
    onAddRegionToChat?: (file: File, info: { page: number }) => void;
    /**
     * Document opener. Injectable because jsdom can neither spawn a pdf.js
     * worker nor parse a PDF: the test hands back a stub and asserts on the
     * gesture, which is the only part of this surface that can be wrong.
     */
    openDocument?: (bytes: Uint8Array) => Promise<PdfPageRenderer>;
}

type MenuPayload = { selection: PdfRegion | null };

export function PdfColumnViewer({
    bytes,
    framing,
    onExitFraming,
    fileName,
    addToChatDisabledTitle,
    onAddRegionToChat,
    openDocument,
}: PdfColumnViewerProps) {
    const { t } = useTranslation();
    const columnRef = useRef<PdfPageColumnHandle>(null);
    const [selection, setSelection] = useState<PdfRegion | null>(null);
    const [view, setView] = useState<PdfColumnView>({
        status: "loading",
        scale: 1,
        page: 1,
        pageCount: 0,
    });

    const menu = useContextMenu<MenuPayload>();

    const onViewChange = useCallback((next: PdfColumnView) => setView(next), []);

    // Leaving framing must not leave a framing on screen: the box would keep
    // pointing at a page the user can no longer act on.
    useEffect(() => {
        if (!framing) setSelection(null);
    }, [framing]);

    // ── Chat hand-off ────────────────────────────────────────────────
    const addRegion = useCallback(
        async (region: PdfRegion) => {
            if (!onAddRegionToChat) return;
            try {
                const blob = await columnRef.current?.crop(region);
                if (!blob) return;
                onAddRegionToChat(
                    new File([blob], regionFileName(fileName, region.rect), { type: "image/png" }),
                    { page: region.page },
                );
                // Stay in the mode and drop the framing: one page usually has
                // more than one thing worth showing.
                setSelection(null);
            } catch (err) {
                log.error("[PdfColumnViewer] cropping the region failed:", err);
                showToast({
                    type: "error",
                    message: t("fileEditor.imageViewer.captureFailed"),
                });
            }
        },
        [fileName, onAddRegionToChat, t],
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

    const handleContextMenu = useCallback(
        (e: ReactMouseEvent<HTMLDivElement>) => {
            if (!framing) return;
            e.preventDefault();
            e.stopPropagation();
            // A drag in progress is not a selection — and the column clears the
            // committed one when a drag begins, so `selection` already answers
            // this by the time the menu opens.
            menu.openAt(e, { selection });
        },
        [framing, menu, selection],
    );

    // ── Keyboard ────────────────────────────────────────────────────
    useEffect(() => {
        if (!framing) return;
        const onKey = (e: KeyboardEvent) => {
            if (e.key !== "Escape") return;
            // Two-stage: first the framing, then the mode. Leaving framing on a
            // stray Esc would be the worse default.
            if (selection) {
                setSelection(null);
                return;
            }
            onExitFraming();
        };
        window.addEventListener("keydown", onKey);
        return () => window.removeEventListener("keydown", onKey);
    }, [framing, onExitFraming, selection]);

    const toolBtn =
        "inline-flex h-6 w-6 shrink-0 items-center justify-center rounded text-text-tertiary transition-colors hover:bg-hover hover:text-text disabled:cursor-not-allowed disabled:opacity-40";
    const divider = <span className="mx-1 h-3.5 w-px shrink-0 bg-border-divider" />;

    return (
        <div className="flex h-full flex-col bg-editor-canvas" data-testid="pdf-column-viewer">
            {/* The toolbar lives inside the body, so the tab keeps exactly one
                header — the frame's. It is the same row in both modes: what
                changes is which of its buttons exist, not the document's
                position on screen. */}
            <div className="flex shrink-0 flex-wrap items-center gap-1 border-b border-border-divider bg-modal-surface px-2 py-1">
                {framing && (
                    <>
                        <Crop className="h-3.5 w-3.5 shrink-0 text-text-tertiary" />
                        <span className="shrink-0 text-11 font-medium text-text-secondary">
                            {t("fileEditor.pdfViewer.regionPickerTitle")}
                        </span>
                        {divider}
                    </>
                )}
                <button
                    type="button"
                    className={toolBtn}
                    onClick={() => columnRef.current?.scrollToPage(Math.max(1, view.page - 1))}
                    disabled={!view.pageCount || view.page <= 1}
                    aria-label={t("fileEditor.pdfViewer.prevPage")}
                    title={t("fileEditor.pdfViewer.prevPage")}
                >
                    <ChevronLeft className="h-3.5 w-3.5" />
                </button>
                <span
                    data-testid="pdf-page-indicator"
                    className="w-20 shrink-0 text-center text-11 tabular-nums text-text-secondary"
                >
                    {view.pageCount
                        ? t("fileEditor.pdfViewer.pageIndicator", {
                            page: view.page,
                            pages: view.pageCount,
                        })
                        : ""}
                </span>
                <button
                    type="button"
                    className={toolBtn}
                    onClick={() =>
                        columnRef.current?.scrollToPage(Math.min(view.pageCount, view.page + 1))}
                    disabled={!view.pageCount || view.page >= view.pageCount}
                    aria-label={t("fileEditor.pdfViewer.nextPage")}
                    title={t("fileEditor.pdfViewer.nextPage")}
                >
                    <ChevronRight className="h-3.5 w-3.5" />
                </button>
                {divider}
                <button
                    type="button"
                    className={toolBtn}
                    onClick={() => columnRef.current?.zoomBy(zoomStepFactor(-1))}
                    disabled={view.status !== "ready" || view.scale <= MIN_ZOOM}
                    aria-label={t("fileEditor.imageViewer.zoomOut")}
                    title={t("fileEditor.imageViewer.zoomOut")}
                >
                    <ZoomOut className="h-3.5 w-3.5" />
                </button>
                <span
                    data-testid="pdf-zoom-level"
                    className="w-12 shrink-0 text-center text-11 tabular-nums text-text-secondary"
                >
                    {view.status === "ready" ? `${zoomPercent(view.scale)}%` : "—"}
                </span>
                <button
                    type="button"
                    className={toolBtn}
                    onClick={() => columnRef.current?.zoomBy(zoomStepFactor(1))}
                    disabled={view.status !== "ready" || view.scale >= MAX_ZOOM}
                    aria-label={t("fileEditor.imageViewer.zoomIn")}
                    title={t("fileEditor.imageViewer.zoomIn")}
                >
                    <ZoomIn className="h-3.5 w-3.5" />
                </button>
                <button
                    type="button"
                    className={toolBtn}
                    onClick={() => columnRef.current?.fitToWidth()}
                    disabled={view.status !== "ready"}
                    aria-label={t("fileEditor.imageViewer.fit")}
                    title={t("fileEditor.imageViewer.fit")}
                >
                    <Maximize2 className="h-3.5 w-3.5" />
                </button>
                {framing && onAddRegionToChat && (
                    <>
                        {divider}
                        <button
                            type="button"
                            className={cn(
                                "inline-flex h-6 shrink-0 items-center gap-1 rounded border border-border-default px-2 text-11 transition-colors",
                                "hover:bg-hover disabled:cursor-not-allowed disabled:opacity-40",
                            )}
                            onClick={() => selection && void addRegion(selection)}
                            disabled={
                                !selection
                                || view.status !== "ready"
                                || addToChatDisabledTitle !== null
                            }
                            title={addToChatDisabledTitle ?? t("fileEditor.pdfViewer.selectRegion")}
                        >
                            <MessageSquarePlus className="h-3.5 w-3.5" />
                            {t("fileEditor.pdfViewer.addRegion")}
                        </button>
                        <button
                            type="button"
                            className={toolBtn}
                            onClick={onExitFraming}
                            aria-label={t("fileEditor.close")}
                            title={t("fileEditor.close")}
                        >
                            <X className="h-3.5 w-3.5" />
                        </button>
                    </>
                )}
            </div>

            <div className="min-h-0 flex-1">
                <PdfPageColumn
                    ref={columnRef}
                    bytes={bytes}
                    framing={framing}
                    selection={selection}
                    onRegion={setSelection}
                    onViewChange={onViewChange}
                    onContextMenu={handleContextMenu}
                    openDocument={openDocument}
                />
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

export default PdfColumnViewer;
