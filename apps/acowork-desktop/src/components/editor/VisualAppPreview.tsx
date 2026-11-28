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
import { DOC_FILE_AGENT_ID } from "../../lib/doc-api";
import { primeDocFileBlobFromFileName } from "../../lib/docFileBlob";
import { saveDocFile } from "../../lib/doc-api";
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
     * Write-back (ADR-121): the app's Save lands on whatever the surface
     * opened — a workspace file in the editor tab, a library entry in the doc
     * pane. Save As / Export always asks the user where the new file goes, so
     * that stays the workspace dialog on both surfaces.
     */
    const docId = file.agentId === DOC_FILE_AGENT_ID ? file.workspaceId : null;
    /** Only an editor tab needs the save-bounce stamp; a doc entry has no fs
     *  event behind it to bounce off. */
    const editorTabId = file.id;
    const onSaveFile = useCallback(
        async ({ name, bytes, saveAs }: { name: string; bytes: Uint8Array; saveAs: boolean }) => {
            if (docId && !saveAs) {
                // The document library owns these bytes: save back onto the
                // entry, not into a workspace the entry does not live in.
                await saveDocFile(docId, bytes);
                // 缓存刷新由 doc 侧代劳：三元组怎么拼、key 怎么算，是它的契约。
                primeDocFileBlobFromFileName(docId, file.fileName, bytes);
                return;
            }
            await saveHostedAppFile({
                // A doc entry has no workspace file to default the Save As
                // dialog to; `null` lets it use the selected agent's workspace.
                bound: docId ? null : { agentId: file.agentId, workspaceId: file.workspaceId, relPath: file.relPath },
                saveAs,
                name,
                bytes,
            });
            // Our own write bounces back as an fs event. Without this stamp the
            // tab reloads the blob, the app is handed the document again, and it
            // re-opens it on screen — the "save refreshes the whole app" bug.
            if (editorTabId) useFileEditorStore.getState().noteHostedAppSave(editorTabId);
        },
        [file.agentId, file.workspaceId, file.relPath, docId, editorTabId],
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
            onSaveFile={onSaveFile}
        />
    );
}
