/**
 * PdfViewer — workspace PDF preview, rendered by us.
 *
 * The bytes come from the Gateway raw route (already authorised through the
 * fetch interceptor) or from the Office → PDF converter, and are drawn by
 * `PdfColumnViewer`: one continuous column of pdf.js pages, our own toolbar,
 * our own typography. It used to be an `<iframe>` of the WebView's built-in
 * viewer, which cost more than it saved — that chrome is not our DOM (its 15
 * px UI can never match the app's 11 px one), its page / zoom / scroll offset
 * cannot be read back, and framing a region therefore needed a SECOND renderer
 * whose arrival threw away where the user was looking. One surface now answers
 * both, and `framing` is a prop rather than a mode switch.
 *
 * `external` is the second byte source: PDFs produced on this machine by the
 * Office → PDF converter (`lib/docConvert.ts`). Supplying it skips the blob
 * fetch entirely (the tab's own bytes are a .docx, not a PDF). Everything else
 * — header, save-as, open-externally, the loading / error gate — is the
 * frame's, unchanged.
 *
 * The two "show the agent this file" entries are what the rest of this
 * component is about. Both need the PDF *bytes*, and this component reads them
 * once, up front: the preview needs them too now, so there is no "only when the
 * user asks" path left to optimise. Sending the whole document hands the file
 * to the panel's callback and needs no renderer at all — the agent is being
 * given the file, not a picture of it.
 *
 * Neither entry appears when there are no PDF bytes: with the converter missing
 * or the conversion failed, `external.state` is not `ready`, and promising the
 * agent a look at the print layout of something we cannot even preview would be
 * a promise we cannot keep. The degraded JS renderers stay on their own path.
 */
import { useCallback, useEffect, useMemo, useState, type ReactNode } from "react";
import { Crop, Loader2, MessageSquarePlus, X } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { log } from "../../lib/logger";
import { useWorkspaceFileBlob, type WorkspaceFileBlobState } from "../../hooks/useWorkspaceFileBlob";
import { DocumentViewerFrame, type ViewerFileProps } from "./DocumentViewerFrame";
import { PdfColumnViewer } from "./PdfColumnViewer";

/** Pre-converted PDF bytes, owned by the caller (see OfficePdfViewer). */
export interface ExternalPdfState {
    state: "loading" | "ready" | "error";
    /** Required when `state === "ready"`. */
    bytes?: Uint8Array | null;
    error?: string | null;
    retry: () => void;
}

export interface PdfViewerProps extends ViewerFileProps, PdfChatProps {
    external?: ExternalPdfState;
    /** Notice slot forwarded to the frame — used by OfficePdfViewer to
     *  label a converted Office document as print-layout output. */
    banner?: ReactNode;
}

/**
 * The chat hand-off a PDF surface takes, all three parts supplied by the
 * panel that mounted it (see `FileEditorPanel`'s `docChat`).
 *
 * Every entry is optional on purpose: a viewer mounted somewhere without a
 * composer to feed must not offer a button that goes nowhere, so the presence
 * of the callback IS the affordance's gate. The byte check in `PdfViewer`
 * covers the other half.
 */
export interface PdfChatProps {
    /** Tooltip while the hand-off is unavailable, `null` when it is live.
     *  Owned by the panel: one rule, so the three "Add to Chat" entries in
     *  the editor (image region, PDF region, PDF document) can never
     *  disagree about whether the session can take anything. */
    addToChatDisabledTitle?: string | null;
    /** A cropped page region as PNG, plus which page it came from. */
    onAddRegionToChat?: (file: File, info: { page: number }) => void;
    /** The whole PDF as a file attachment. Deliberately no page count: the
     *  viewer would have to load the PDF engine to produce one, and an
     *  attachment must not depend on a renderer the user is not asking for. */
    onAddDocumentToChat?: (file: File) => void;
}

/** Attachment name: the tab's stem, always re-typed to `.pdf`. A converted
 *  `deck.pptx` is being sent as the print-layout export the agent receives,
 *  and calling it `deck.pptx` would invite the agent to treat it as an OOXML
 *  archive. A native PDF keeps its own name. */
export function pdfAttachmentName(fileName: string): string {
    return `${fileName.replace(/\.[^./\\]+$/, "") || fileName}.pdf`;
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

    const [picking, setPicking] = useState(false);
    /** The PDF the column draws. Read once per source and kept in state,
     *  because the column re-opens its document whenever the array identity
     *  changes — so the value has to be stable across renders. It may be the
     *  converter's cached array: `pdfRasterize` takes its own copy at the pdf.js
     *  transfer point, so opening a document never eats the bytes the tab still
     *  holds. */
    const [bytes, setBytes] = useState<Uint8Array | null>(null);
    /** The read settled with nothing in hand. Distinct from `bytes === null`
     *  (still reading) because the body has to say which of the two it is: a
     *  spinner that never ends is how a dead document gets presented. */
    const [readFailed, setReadFailed] = useState(false);
    const [readKey, setReadKey] = useState(0);

    // Read the PDF behind the preview, from whichever source fed it. Every
    // path is same-origin (`blob:` of our own object URL), so this never
    // leaves the machine and never re-authorises anything.
    const readBytes = useCallback(async (): Promise<Uint8Array | null> => {
        const direct = external?.state === "ready" ? (external.bytes ?? null) : null;
        if (direct && direct.length > 0) return direct;
        if (state.blob) return new Uint8Array(await state.blob.arrayBuffer());
        if (state.blobUrl) {
            const response = await fetch(state.blobUrl);
            return new Uint8Array(await response.arrayBuffer());
        }
        return null;
    }, [external, state.blob, state.blobUrl]);

    // Framing is a prop on the surface that is already showing the document, so
    // entering it cannot move the user: no read, no remount, no second renderer.
    const startPicking = useCallback(() => setPicking(true), []);
    const stopPicking = useCallback(() => setPicking(false), []);

    // The preview and both hand-offs read from one array, taken once per source.
    // Every path is same-origin (`blob:` of our own object URL), so this never
    // leaves the machine and never re-authorises anything.
    useEffect(() => {
        let cancelled = false;
        void readBytes()
            .then((next) => {
                if (cancelled) return;
                // An empty read is a failed read: a zero-length array is truthy
                // and would otherwise be handed to the column as a document.
                const ok = next !== null && next.length > 0;
                setBytes(ok ? next : null);
                setReadFailed(!ok);
            })
            .catch((err) => {
                log.error("[PdfViewer] reading the PDF bytes failed:", err);
                if (!cancelled) {
                    setBytes(null);
                    setReadFailed(true);
                }
            });
        return () => {
            cancelled = true;
        };
    }, [readBytes, readKey]);

    const sendDocument = useCallback(() => {
        const onAddDocumentToChat = props.onAddDocumentToChat;
        if (!onAddDocumentToChat || !bytes) return;
        // `File` copies its parts, so `bytes` stays usable afterwards — and
        // pdf.js, which *transfers* whatever it is given, only ever sees the
        // copy `pdfRasterize` takes at its own transfer point.
        onAddDocumentToChat(
            new File([bytes as Uint8Array<ArrayBuffer>], pdfAttachmentName(props.fileName), {
                type: "application/pdf",
            }),
        );
    }, [bytes, props.fileName, props.onAddDocumentToChat]);

    const headerBtn =
        "inline-flex h-6 w-6 shrink-0 items-center justify-center rounded text-text-tertiary transition-colors hover:bg-hover hover:text-text disabled:opacity-50";
    const toolBtn =
        "inline-flex h-6 shrink-0 items-center rounded border border-border-default px-2 text-11 transition-colors hover:bg-hover";
    const onAddRegionToChat = props.onAddRegionToChat;
    // One slot, two entries: framing needs the picker, the whole document
    // needs the bytes — neither is offered before the preview itself has
    // succeeded, and neither is offered at all without a callback to take it.
    const extraActions =
        bytes !== null && (onAddRegionToChat || props.onAddDocumentToChat) ? (
            <>
                {onAddRegionToChat && (
                    <button
                        type="button"
                        className={headerBtn}
                        onClick={() => (picking ? stopPicking() : startPicking())}
                        aria-pressed={picking}
                        data-testid="pdf-region-toggle"
                        aria-label={t("fileEditor.pdfViewer.selectRegion")}
                        title={t("fileEditor.pdfViewer.selectRegion")}
                    >
                        {picking ? <X className="h-3.5 w-3.5" /> : <Crop className="h-3.5 w-3.5" />}
                    </button>
                )}
                {props.onAddDocumentToChat && (
                    <button
                        type="button"
                        className={headerBtn}
                        onClick={sendDocument}
                        disabled={picking}
                        aria-label={t("fileEditor.pdfViewer.addDocument")}
                        title={t("fileEditor.pdfViewer.addDocument")}
                        data-testid="pdf-add-document"
                    >
                        <MessageSquarePlus className="h-3.5 w-3.5" />
                    </button>
                )}
            </>
        ) : undefined;

    return (
        <DocumentViewerFrame file={props} state={state} banner={props.banner} extraActions={extraActions}>
            {bytes ? (
                /*  One surface for both jobs: the column stays mounted, and
                 *  `framing` decides whether a pointer draws a box or pans the
                 *  document. The region callback is only ever handed to the
                 *  surface that can produce a region, and leaving framing must
                 *  not lose it — the column would remount and the user would be
                 *  back at the top of the document. */
                <PdfColumnViewer
                    bytes={bytes}
                    framing={picking}
                    onExitFraming={stopPicking}
                    {...(onAddRegionToChat ? { onAddRegionToChat } : {})}
                    fileName={pdfAttachmentName(props.fileName)}
                    addToChatDisabledTitle={props.addToChatDisabledTitle ?? null}
                />
            ) : (
                <div className="flex h-full flex-col items-center justify-center gap-2 text-xs text-text-tertiary">
                    {readFailed ? (
                        <>
                            <span>{t("fileEditor.docViewer.loadFailed")}</span>
                            <button
                                type="button"
                                className={toolBtn}
                                onClick={() => setReadKey((k) => k + 1)}
                            >
                                {t("fileEditor.docViewer.retry")}
                            </button>
                        </>
                    ) : (
                        <>
                            <Loader2 className="h-4 w-4 animate-spin" />
                            {t("fileEditor.previewLoading")}
                        </>
                    )}
                </div>
            )}
        </DocumentViewerFrame>
    );
}
