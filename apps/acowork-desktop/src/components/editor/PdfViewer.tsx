/**
 * PdfViewer — workspace PDF preview via the WebView's native viewer.
 *
 * The blob URL from the Gateway raw route (already authorised through
 * the fetch interceptor) is handed to an `<iframe>`; WebView2 (Windows)
 * and WKWebView (macOS) render PDFs natively with their own page
 * controls, so the component itself is just chrome + a frame. This is
 * the P0 decision from the preview plan — pdfjs-dist (selection
 * gestures, text layer) is a deliberate later step, not a missing piece
 * here.
 *
 * `external` is the second byte source: PDFs produced on this machine by
 * the Office → PDF converter (`lib/docConvert.ts`). Supplying it skips the
 * blob fetch entirely (the tab's own bytes are a .docx, not a PDF) and
 * feeds the same iframe from an object URL minted from the returned bytes,
 * revoked on unmount. Everything else — header, save-as, open-externally,
 * the loading / error gate — is the frame's, unchanged.
 */
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { useWorkspaceFileBlob, type WorkspaceFileBlobState } from "../../hooks/useWorkspaceFileBlob";
import { DocumentViewerFrame, type ViewerFileProps } from "./DocumentViewerFrame";

/** Pre-converted PDF bytes, owned by the caller (see OfficePdfViewer). */
export interface ExternalPdfState {
    state: "loading" | "ready" | "error";
    /** Required when `state === "ready"`. */
    bytes?: Uint8Array | null;
    error?: string | null;
    retry: () => void;
}

export interface PdfViewerProps extends ViewerFileProps {
    external?: ExternalPdfState;
    /** Notice slot forwarded to the frame — used by OfficePdfViewer to
     *  label a converted Office document as print-layout output. */
    banner?: ReactNode;
}

/**
 * Object URL for the external bytes. Created in an effect (not during
 * render) so a StrictMode double mount cannot leak the first URL: only the
 * URL this effect produced is revoked, and a re-mount creates a fresh one.
 */
function useExternalPdfUrl(external?: ExternalPdfState): string | null {
    const bytes = external?.state === "ready" ? (external.bytes ?? null) : null;
    const [url, setUrl] = useState<string | null>(null);
    useEffect(() => {
        if (!bytes || bytes.length === 0) {
            setUrl(null);
            return;
        }
        // The bytes always wrap a freshly allocated ArrayBuffer (they come
        // off the IPC boundary), which is precisely what `Blob` wants — TS
        // cannot see through the `Uint8Array<ArrayBufferLike>` generic.
        const next = URL.createObjectURL(
            new Blob([bytes as Uint8Array<ArrayBuffer>], { type: "application/pdf" }),
        );
        setUrl(next);
        return () => URL.revokeObjectURL(next);
    }, [bytes]);
    return url;
}

export default function PdfViewer(props: PdfViewerProps) {
    const { t } = useTranslation();
    const external = props.external;
    const externalUrl = useExternalPdfUrl(external);
    // Disabled in external mode: the tab's raw bytes are the SOURCE
    // document, and converting them is the caller's job.
    const feed = useWorkspaceFileBlob(
        { agentId: props.agentId, workspaceId: props.workspaceId, relPath: props.relPath },
        { mode: "url", reloadSeq: props.previewSeq, enabled: !external },
    );
    // The frame gates on the blob-feed shape; an external feed is mapped
    // onto it so both sources share one chrome and one retry affordance.
    const state = useMemo<WorkspaceFileBlobState>(
        () =>
            external
                ? {
                    blob: null,
                    blobUrl: externalUrl,
                    mime: externalUrl ? "application/pdf" : null,
                    // `error` must win over the frame's loading branch —
                    // the gate checks loading first.
                    loading: external.state === "loading",
                    error: external.state === "error" ? (external.error ?? null) : null,
                    retry: external.retry,
                }
                : feed,
        [external, externalUrl, feed],
    );
    return (
        <DocumentViewerFrame file={props} state={state} banner={props.banner}>
            {state.blobUrl && (
                <iframe
                    src={state.blobUrl}
                    className="h-full w-full border-0 bg-modal-surface"
                    title={props.fileName || t("fileEditor.pdfViewer.title")}
                />
            )}
        </DocumentViewerFrame>
    );
}
