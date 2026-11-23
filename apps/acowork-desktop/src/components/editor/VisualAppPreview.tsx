/**
 * Editor-pane preview of a visual hosted-app workspace file (ADR-118).
 *
 * Lives next to ImageViewer / PDF viewers — never inside VirtualRowList.
 * Bytes come from the workspace file API; the iframe is a viewer, not the
 * MCP backend.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { Loader2, AlertCircle } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { listApps, type AppDto } from "../../lib/apps-api";
import { saveHostedAppFile } from "../../lib/workspaceFileSave";
import { useWorkspaceFileBlob } from "../../hooks/useWorkspaceFileBlob";
import { useFileEditorStore, type OpenFile } from "../../stores/fileEditorStore";
import { AppFrame } from "../apps/AppFrame";
import { pickVisualApp, visualPreviewSearch } from "./visualCanvas";

export function VisualAppPreview({ file }: { file: OpenFile }) {
    const { t } = useTranslation();
    const [app, setApp] = useState<AppDto | null>(null);
    const [appError, setAppError] = useState<string | null>(null);
    const [bytes, setBytes] = useState<Uint8Array | null>(null);

    const blobState = useWorkspaceFileBlob(
        {
            agentId: file.agentId,
            workspaceId: file.workspaceId,
            relPath: file.relPath,
        },
        { mode: "blob", reloadSeq: file.previewSeq },
    );

    /**
     * Write-back (ADR-118): the app's Save lands on the workspace file the tab
     * was opened on; a Save As / Export asks the user where the new file goes.
     */
    const onSaveFile = useCallback(
        async ({ name, bytes, saveAs }: { name: string; bytes: Uint8Array; saveAs: boolean }) => {
            await saveHostedAppFile({
                bound: { agentId: file.agentId, workspaceId: file.workspaceId, relPath: file.relPath },
                saveAs,
                name,
                bytes,
            });
            // Our own write bounces back as an fs event. Without this stamp the
            // tab reloads the blob, the app is handed the document again, and it
            // re-opens it on screen — the "save refreshes the whole app" bug.
            useFileEditorStore.getState().noteHostedAppSave(file.id);
        },
        [file.agentId, file.workspaceId, file.relPath, file.id],
    );

    useEffect(() => {
        let cancelled = false;
        void listApps()
            .then((apps) => {
                if (cancelled) return;
                setApp(pickVisualApp(apps, file.fileName));
            })
            .catch((e) => {
                if (!cancelled) setAppError(e instanceof Error ? e.message : String(e));
            });
        return () => {
            cancelled = true;
        };
    }, [file.fileName]);

    useEffect(() => {
        if (!blobState.blob) {
            setBytes(null);
            return;
        }
        let cancelled = false;
        void blobState.blob.arrayBuffer().then((buf) => {
            if (!cancelled) setBytes(new Uint8Array(buf));
        });
        return () => {
            cancelled = true;
        };
    }, [blobState.blob]);

    /**
     * Stable identity: `AppFrame` re-sends the document whenever this prop
     * changes, and the app treats a re-sent document as a fresh open (it
     * reloads, dropping cursor and undo). A bare object literal made every
     * re-render of this pane — a file-tree refresh, a status tick — look like
     * an edit from outside.
     */
    const openFile = useMemo(
        () => (bytes ? { name: file.fileName, bytes } : null),
        [file.fileName, bytes],
    );

    if (appError) {
        return (
            <div className="flex h-full flex-col items-center justify-center gap-2 p-6 text-xs text-text-tertiary">
                <AlertCircle className="h-4 w-4" />
                <span>{appError}</span>
            </div>
        );
    }
    if (blobState.error) {
        return (
            <div className="flex h-full flex-col items-center justify-center gap-2 p-6 text-xs text-text-tertiary">
                <AlertCircle className="h-4 w-4" />
                <span>{t("fileEditor.previewLoadFailed")}</span>
                <button
                    type="button"
                    className="rounded border border-border-default px-2 py-1 hover:bg-[var(--color-accent)]/10"
                    onClick={blobState.retry}
                >
                    {t("fileEditor.previewRetry")}
                </button>
            </div>
        );
    }
    if (!app || blobState.loading) {
        return (
            <div className="flex h-full items-center justify-center gap-2 text-xs text-text-tertiary">
                <Loader2 className="h-4 w-4 animate-spin" />
                {t("fileEditor.previewLoading")}
            </div>
        );
    }

    return (
        <AppFrame
            app={app}
            search={visualPreviewSearch(app, file.relPath)}
            openFile={openFile}
            onSaveFile={onSaveFile}
        />
    );
}
