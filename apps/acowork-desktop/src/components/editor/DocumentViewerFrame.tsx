/**
 * DocumentViewerFrame — the shared chrome around every workspace document
 * preview (PDF / sheet / docx / pptx).
 *
 * Mirrors the search pane's two-segment PreviewHeader look (icon + name +
 * relative path, actions right) so a document tab reads like the rest of
 * the app, and owns the three states each viewer would otherwise
 * re-implement: loading spinner, error with retry, and the two escape
 * hatches that always work regardless of renderer support —
 * "save as" (Rust streams the Gateway bytes to a picked path) and
 * "open externally" (temp download + OS default app).
 *
 * Viewers render inside it only once their bytes are in hand; everything
 * feed-related (the fetch state) is passed in, so the frame never touches
 * the network itself.
 */
import { useState, type ReactNode } from "react";
import { AlertCircle, ExternalLink, FileText, Loader2, RefreshCw, Save } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { showToast } from "../common/ToastProvider";
import {
    openWorkspaceFileExternally,
    saveWorkspaceFileToDisk,
} from "../../lib/workspaceFileBlob";
import type { WorkspaceFileBlobState } from "../../hooks/useWorkspaceFileBlob";

/** The slice of an open tab a viewer needs. Deliberately NOT `OpenFile`
 *  — viewers stay decoupled from the editor store's shape (and its
 *  test mocks). */
export interface ViewerFileProps {
    agentId: string;
    workspaceId: string;
    relPath: string;
    fileName: string;
    /** Bumped by the store when an fs event invalidated the cached bytes;
     *  viewers put it in their fetch-effect deps to re-read the file. */
    previewSeq?: number;
}

/** Derive the viewer-ready props from a tab-shaped object. */
export function viewerFileProps(file: ViewerFileProps): ViewerFileProps {
    return {
        agentId: file.agentId,
        workspaceId: file.workspaceId,
        relPath: file.relPath,
        fileName: file.fileName,
        previewSeq: file.previewSeq,
    };
}

interface DocumentViewerFrameProps {
    file: ViewerFileProps;
    /** Blob-feed state from `useWorkspaceFileBlob`. Optional because a
     *  `bypassFeed` frame has no feed to mirror. */
    state?: WorkspaceFileBlobState;
    /** Degradation / fallback notice rendered above the body even when
     *  the frame is in its ready state (e.g. "previewing first N rows"). */
    banner?: ReactNode;
    /** Viewer-specific header actions, placed left of the two escape hatches.
     *  A slot rather than a second header: the frame owns the single row of
     *  chrome every document tab shares, and a viewer that needs a button
     *  ("frame a region", "send this file") supplies it here. */
    extraActions?: ReactNode;
    /** Skip the blob-feed gating entirely and render `children`.
     *  For surfaces that carry no bytes of their own (the unsupported
     *  legacy-format card) — the header actions still work because they
     *  re-address the Gateway route directly. */
    bypassFeed?: boolean;
    /** Body content — rendered only when the feed is ready. */
    children?: ReactNode;
}

/** Stand-in for frames without a feed (see `state` above). */
const IDLE_FEED: WorkspaceFileBlobState = {
    blob: null,
    blobUrl: null,
    mime: null,
    loading: false,
    error: null,
    retry: () => {},
};

export function DocumentViewerFrame({ file, state = IDLE_FEED, banner, extraActions, bypassFeed, children }: DocumentViewerFrameProps) {
    const { t } = useTranslation();
    const [saving, setSaving] = useState(false);
    const [opening, setOpening] = useState(false);

    const handleSave = async () => {
        setSaving(true);
        try {
            const ok = await saveWorkspaceFileToDisk(
                file.agentId,
                file.workspaceId,
                file.relPath,
                file.fileName,
            );
            // `false` = the user cancelled the dialog — not an error.
            if (!ok) return;
        } catch (e) {
            showToast({ type: "error", message: `${t("fileEditor.docViewer.saveFailed")}: ${String(e)}` });
        } finally {
            setSaving(false);
        }
    };

    const handleOpenExternal = async () => {
        setOpening(true);
        try {
            await openWorkspaceFileExternally(
                file.agentId,
                file.workspaceId,
                file.relPath,
                file.fileName,
            );
        } catch (e) {
            showToast({ type: "error", message: `${t("fileEditor.docViewer.openExternalFailed")}: ${String(e)}` });
        } finally {
            setOpening(false);
        }
    };

    const actionBtn =
        "inline-flex h-6 w-6 shrink-0 items-center justify-center rounded text-text-tertiary transition-colors hover:bg-hover hover:text-text disabled:opacity-50";

    return (
        <div className="flex h-full flex-col">
            <div className="flex shrink-0 items-center gap-2 border-b border-border-divider px-4 py-2">
                <FileText className="h-4 w-4 shrink-0 text-text-tertiary" />
                <span className="truncate text-xs font-medium text-text">{file.fileName}</span>
                <span className="truncate font-mono text-10 text-text-tertiary">{file.relPath}</span>
                <div className="ml-auto flex shrink-0 items-center gap-1">
                    {extraActions}
                    <button
                        type="button"
                        className={actionBtn}
                        title={t("fileEditor.docViewer.saveAs")}
                        aria-label={t("fileEditor.docViewer.saveAs")}
                        onClick={handleSave}
                        disabled={saving}
                    >
                        {saving ? (
                            <Loader2 className="h-3.5 w-3.5 animate-spin" />
                        ) : (
                            <Save className="h-3.5 w-3.5" />
                        )}
                    </button>
                    <button
                        type="button"
                        className={actionBtn}
                        title={t("fileEditor.docViewer.openExternally")}
                        aria-label={t("fileEditor.docViewer.openExternally")}
                        onClick={handleOpenExternal}
                        disabled={opening}
                    >
                        {opening ? (
                            <Loader2 className="h-3.5 w-3.5 animate-spin" />
                        ) : (
                            <ExternalLink className="h-3.5 w-3.5" />
                        )}
                    </button>
                </div>
            </div>
            {banner}
            {bypassFeed ? (
                <div className="min-h-0 flex-1">{children}</div>
            ) : state.loading || (!state.blob && !state.blobUrl && !state.error) ? (
                <div className="flex min-h-0 flex-1 items-center justify-center gap-2 text-xs text-text-tertiary">
                    <Loader2 className="h-4 w-4 animate-spin" />
                    {t("fileEditor.previewLoading")}
                </div>
            ) : state.error ? (
                <div className="flex min-h-0 flex-1 flex-col items-center justify-center gap-2 text-xs text-text-tertiary">
                    <AlertCircle className="h-4 w-4" />
                    {t("fileEditor.docViewer.loadFailed")}
                    <span className="max-w-[80%] truncate font-mono text-10">{state.error}</span>
                    <button
                        type="button"
                        className="mt-1 inline-flex items-center gap-1 rounded border border-border-default px-2 py-1 hover:bg-hover"
                        onClick={state.retry}
                    >
                        <RefreshCw className="h-3 w-3" />
                        {t("fileEditor.docViewer.retry")}
                    </button>
                </div>
            ) : (
                <div className="min-h-0 flex-1">{children}</div>
            )}
        </div>
    );
}
