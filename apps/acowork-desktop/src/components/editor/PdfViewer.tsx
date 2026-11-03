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
 *
 * The two "show the agent this file" entries are what the rest of this
 * component is about. Both need the PDF *bytes*, and only both of them ever
 * touch them: a normal preview never loads pdf.js. Framing a region mounts
 * `PdfRegionPicker` (lazy, and its engine import is lazy inside it) over a
 * canvas of its own, because the native viewer's page / zoom / scroll offset
 * cannot be read back — see the note in `pdfRasterize.ts`. Sending the whole
 * document hands the file to the panel's callback and needs no renderer at
 * all: the bytes are already in hand, and the agent is being given the file,
 * not a picture of it.
 *
 * Neither entry appears when there are no PDF bytes: with the converter
 * missing or the conversion failed, `external.state` is not `ready`, and
 * promising the agent a look at the print layout of something we cannot even
 * preview would be a promise we cannot keep. The degraded JS renderers stay
 * on their own path.
 */
import {
    useCallback,
    useEffect,
    useMemo,
    useState,
    lazy,
    Suspense,
    type ReactNode,
} from "react";
import { Crop, Loader2, MessageSquarePlus, X } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { showToast } from "../common/ToastProvider";
import { log } from "../../lib/logger";
import { useWorkspaceFileBlob, type WorkspaceFileBlobState } from "../../hooks/useWorkspaceFileBlob";
import { DocumentViewerFrame, type ViewerFileProps } from "./DocumentViewerFrame";

/** The crop mode is mounted on demand: 650 lines of gesture code plus an
 *  engine nobody pays for until they ask to frame something. */
const PdfRegionPicker = lazy(() => import("./PdfRegionPicker"));

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

    const canReadBytes = state.blobUrl !== null || state.blob !== null;
    const [picking, setPicking] = useState(false);
    /** Private copy handed to pdf.js. Kept in state rather than derived
     *  during render because `PdfRegionPicker` re-opens its document whenever
     *  the array identity changes — and because `getDocument` *transfers*
     *  whatever buffer it is given, so the copy must not be the converter's
     *  cached bytes nor the ones the attachment is built from. */
    const [pickerBytes, setPickerBytes] = useState<Uint8Array | null>(null);
    const [sending, setSending] = useState(false);

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

    const startPicking = useCallback(async () => {
        try {
            const bytes = await readBytes();
            if (!bytes || bytes.length === 0) {
                showToast({ type: "error", message: t("fileEditor.pdfViewer.rasterizeFailed") });
                return;
            }
            setPickerBytes(bytes.slice(0));
            setPicking(true);
        } catch (err) {
            log.error("[PdfViewer] reading the PDF bytes failed:", err);
            showToast({ type: "error", message: t("fileEditor.pdfViewer.rasterizeFailed") });
        }
    }, [readBytes, t]);

    const stopPicking = useCallback(() => {
        setPicking(false);
        setPickerBytes(null);
    }, []);

    const sendDocument = useCallback(async () => {
        const onAddDocumentToChat = props.onAddDocumentToChat;
        if (!onAddDocumentToChat) return;
        setSending(true);
        try {
            const bytes = await readBytes();
            if (!bytes || bytes.length === 0) {
                showToast({ type: "error", message: t("fileEditor.pdfViewer.rasterizeFailed") });
                return;
            }
            // `File` copies its parts, so `bytes` stays usable afterwards —
            // pdf.js, which *transfers* whatever it is given, only ever sees a
            // private copy handed over elsewhere.
            onAddDocumentToChat(
                new File([bytes as Uint8Array<ArrayBuffer>], pdfAttachmentName(props.fileName), {
                    type: "application/pdf",
                }),
            );
        } catch (err) {
            log.error("[PdfViewer] sending the PDF to the chat failed:", err);
            showToast({ type: "error", message: t("fileEditor.pdfViewer.rasterizeFailed") });
        } finally {
            setSending(false);
        }
    }, [props.fileName, props.onAddDocumentToChat, readBytes, t]);

    const headerBtn =
        "inline-flex h-6 w-6 shrink-0 items-center justify-center rounded text-text-tertiary transition-colors hover:bg-hover hover:text-text disabled:opacity-50";
    const onAddRegionToChat = props.onAddRegionToChat;
    // One slot, two entries: framing needs the picker, the whole document
    // needs the bytes — neither is offered before the preview itself has
    // succeeded, and neither is offered at all without a callback to take it.
    const extraActions =
        canReadBytes && (onAddRegionToChat || props.onAddDocumentToChat) ? (
            <>
                {onAddRegionToChat && (
                    <button
                        type="button"
                        className={headerBtn}
                        onClick={() => (picking ? stopPicking() : void startPicking())}
                        disabled={!picking && sending}
                        aria-label={t("fileEditor.pdfViewer.selectRegion")}
                        title={t("fileEditor.pdfViewer.selectRegion")}
                        data-testid="pdf-region-toggle"
                    >
                        {picking ? <X className="h-3.5 w-3.5" /> : <Crop className="h-3.5 w-3.5" />}
                    </button>
                )}
                {props.onAddDocumentToChat && (
                    <button
                        type="button"
                        className={headerBtn}
                        onClick={() => void sendDocument()}
                        disabled={sending || picking}
                        aria-label={t("fileEditor.pdfViewer.addDocument")}
                        title={t("fileEditor.pdfViewer.addDocument")}
                        data-testid="pdf-add-document"
                    >
                        {sending ? (
                            <Loader2 className="h-3.5 w-3.5 animate-spin" />
                        ) : (
                            <MessageSquarePlus className="h-3.5 w-3.5" />
                        )}
                    </button>
                )}
            </>
        ) : undefined;

    return (
        <DocumentViewerFrame file={props} state={state} banner={props.banner} extraActions={extraActions}>
            {picking && pickerBytes && onAddRegionToChat ? (
                <Suspense
                    fallback={
                        <div className="flex h-full items-center justify-center gap-2 text-xs text-text-tertiary">
                            <Loader2 className="h-4 w-4 animate-spin" />
                            {t("fileEditor.previewLoading")}
                        </div>
                    }
                >
                    <PdfRegionPicker
                        bytes={pickerBytes}
                        fileName={pdfAttachmentName(props.fileName)}
                        addToChatDisabledTitle={props.addToChatDisabledTitle ?? null}
                        onAddRegionToChat={onAddRegionToChat}
                        onClose={stopPicking}
                    />
                </Suspense>
            ) : (
                state.blobUrl && (
                    <iframe
                        src={state.blobUrl}
                        className="h-full w-full border-0 bg-modal-surface"
                        title={props.fileName || t("fileEditor.pdfViewer.title")}
                    />
                )
            )}
        </DocumentViewerFrame>
    );
}
