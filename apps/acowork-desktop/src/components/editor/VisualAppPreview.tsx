/**
 * Editor-pane preview of a visual hosted-app workspace file (ADR-121).
 *
 * Lives next to ImageViewer / PDF viewers — never inside VirtualRowList.
 * Bytes come from the workspace file API; the iframe is a viewer, not the
 * MCP backend.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Loader2, AlertCircle } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { listApps, type AppDto } from "../../lib/apps-api";
import { saveHostedAppFile } from "../../lib/workspaceFileSave";
import { useWorkspaceFileBlob } from "../../hooks/useWorkspaceFileBlob";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { AppFrame } from "../apps/AppFrame";
import { pickVisualApp, visualPreviewSearch } from "./visualCanvas";

/**
 * Everything this pane needs from the file it previews — the same triple the
 * byte source and the app's `openFile` channel use. Deliberately not `OpenFile`:
 * the doc library previews its imported files through the same component, and a
 * document entry has no editor tab behind it.
 */
export interface VisualPreviewFile {
    /** Editor tab id — only the save-bounce stamp needs it. Absent outside the
     *  editor (doc library), where a write-back has nowhere to land anyway: an
     *  imported file entry is read-only by design. */
    id?: string;
    agentId: string;
    workspaceId: string;
    relPath: string;
    fileName: string;
    previewSeq?: number;
}

export function VisualAppPreview({ file }: { file: VisualPreviewFile }) {
    const { t } = useTranslation();
    const [app, setApp] = useState<AppDto | null>(null);
    const [appError, setAppError] = useState<string | null>(null);
    // Bytes live in a REF, not state: React's dev performance track walks every
    // state/prop value, and a large PDF's Uint8Array would be iterated index by
    // index on every render (20s+ freeze). A version counter re-renders when
    // they arrive; the getter below hands them to AppFrame without React ever
    // seeing the array.
    const bytesRef = useRef<Uint8Array | null>(null);
    const [bytesVersion, setBytesVersion] = useState(0);

    const blobState = useWorkspaceFileBlob(
        {
            agentId: file.agentId,
            workspaceId: file.workspaceId,
            relPath: file.relPath,
        },
        { mode: "blob", reloadSeq: file.previewSeq },
    );

    /**
     * Write-back (ADR-121): the app's Save lands on the workspace file the tab
     * was opened on; a Save As / Export asks the user where the new file goes.
     */
    /** `undefined` outside the editor: an imported doc entry has no writable
     *  workspace file behind it, so the app must not be offered a Save that
     *  would 404. */
    const editorTabId = file.id;
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
            if (editorTabId) useFileEditorStore.getState().noteHostedAppSave(editorTabId);
        },
        [file.agentId, file.workspaceId, file.relPath, editorTabId],
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
            bytesRef.current = null;
            setBytesVersion((v) => v + 1);
            return;
        }
        let cancelled = false;
        void blobState.blob.arrayBuffer().then((buf) => {
            if (cancelled) return;
            bytesRef.current = new Uint8Array(buf);
            setBytesVersion((v) => v + 1);
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
    const openFile = useMemo(() => {
        const bytes = bytesRef.current;
        return bytes ? { name: file.fileName, getBytes: () => bytes } : null;
    }, [file.fileName, bytesVersion]);

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
            onSaveFile={editorTabId ? onSaveFile : undefined}
        />
    );
}
