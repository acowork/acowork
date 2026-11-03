/**
 * OfficePdfViewer — .docx / .doc / .pptx / .ppt / .xlsx / .xls preview at
 * the layout the user would print.
 *
 * The in-app JS renderers (docx-preview, SheetJS, the pptx card) re-layout
 * OOXML into HTML with a hand-rolled engine, so they show *content*, not
 * the page the Agent actually produced. This wrapper converts the file on
 * the user's own machine (LibreOffice, or MS Office through COM on Windows
 * — see `src-tauri/src/commands/doc_convert.rs`) and hands the PDF to the
 * native WebView viewer. Nothing is uploaded anywhere; the bytes travel the
 * same authenticated Gateway route every preview uses.
 *
 * The `fallback` element is the renderer this tab used before — it stays
 * reachable and is what gets shown when conversion cannot happen:
 *
 *   - `ready`          → PdfViewer (print layout), with a quiet note
 *   - `detecting` / `converting` / first render → the frame's loading body
 *   - `unavailable`    → fallback + "no engine, this is an approximate
 *                        layout" (the honest state of a bare machine)
 *   - `failed`         → fallback + the failure, with a retry
 *
 * The degraded banner is rendered BELOW the fallback because the fallback
 * owns its own `DocumentViewerFrame` header — a notice cannot be injected
 * into another component's chrome, and stacking a second header above it
 * would read as two documents.
 */
import { Loader2 } from "lucide-react";
import type { ReactNode } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { useConvertedPdf } from "../../hooks/useConvertedPdf";
import {
    DocumentViewerFrame,
    viewerFileProps,
    type ViewerFileProps,
} from "./DocumentViewerFrame";
import PdfViewer from "./PdfViewer";

export interface OfficePdfViewerProps extends ViewerFileProps {
    /** The existing JS renderer for this format, shown while conversion is
     *  unavailable or failed. Passed as an element so this component stays
     *  free of format-specific imports (and lazy-loading stays in
     *  FileEditorPanel). */
    fallback: ReactNode;
    /** File version captured by the editor store when the tab opened —
     *  the converter's cache key. Optional: without it every open
     *  re-converts, which is correct but slower. */
    size?: number;
    mtimeMs?: number;
}

export default function OfficePdfViewer(props: OfficePdfViewerProps) {
    const { t } = useTranslation();
    const viewerFile = viewerFileProps(props);
    const conv = useConvertedPdf(
        {
            agentId: props.agentId,
            workspaceId: props.workspaceId,
            relPath: props.relPath,
            fileName: props.fileName,
            size: props.size,
            mtimeMs: props.mtimeMs,
        },
        { reloadSeq: props.previewSeq },
    );

    if (conv.status === "ready") {
        return (
            <PdfViewer
                {...viewerFile}
                external={{ state: "ready", bytes: conv.pdfBytes, retry: conv.retry }}
                banner={
                    <div className="shrink-0 border-b border-border-divider bg-input px-3 py-1 text-11 text-text-tertiary">
                        {t("fileEditor.officePdf.printLayoutNote")}
                    </div>
                }
            />
        );
    }

    // `idle` is the single render before the effect runs — showing the
    // loading body for it avoids flashing the fallback on every mount.
    if (conv.status === "detecting" || conv.status === "converting" || conv.status === "idle") {
        return (
            <DocumentViewerFrame file={viewerFile} bypassFeed>
                <div className="flex h-full flex-col items-center justify-center gap-2 text-xs text-text-tertiary">
                    <Loader2 className="h-4 w-4 animate-spin" />
                    {t("fileEditor.officePdf.converting")}
                </div>
            </DocumentViewerFrame>
        );
    }

    const degraded =
        conv.status === "unavailable"
            ? t("fileEditor.officePdf.engineMissing")
            : `${t("fileEditor.officePdf.convertFailed")}${conv.error ? `: ${conv.error}` : ""}`;
    return (
        <div className="flex h-full flex-col">
            <div className="min-h-0 flex-1">{props.fallback}</div>
            <div className="flex shrink-0 flex-col gap-0.5 border-t border-amber-300/60 bg-amber-50/95 px-3 py-1 text-11 text-amber-900 dark:bg-amber-950/60 dark:text-amber-100">
                <div className="flex items-center gap-2">
                    <span className="min-w-0 truncate">{degraded}</span>
                    {conv.status === "failed" && (
                        <button
                            type="button"
                            className="ml-auto shrink-0 rounded border border-border-default px-2 py-0.5 hover:bg-hover"
                            onClick={conv.retry}
                        >
                            {t("fileEditor.docViewer.retry")}
                        </button>
                    )}
                </div>
                {/* The one actionable half of "no engine": say what would
                     turn this approximation into the real layout. */}
                {conv.status === "unavailable" && (
                    <span>{t("fileEditor.officePdf.installHint")}</span>
                )}
            </div>
        </div>
    );
}
