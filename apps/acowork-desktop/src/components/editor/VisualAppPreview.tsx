/**
 * Editor-pane preview of a visual hosted-app workspace file (ADR-118).
 *
 * Lives next to ImageViewer / PDF viewers — never inside VirtualRowList.
 * Bytes come from the workspace file API; the iframe is a viewer, not the
 * MCP backend.
 */
import { useEffect, useState } from "react";
import { Loader2, AlertCircle } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { listApps, type AppDto } from "../../lib/apps-api";
import { useWorkspaceFileBlob } from "../../hooks/useWorkspaceFileBlob";
import type { OpenFile } from "../../stores/fileEditorStore";
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
            openFile={bytes ? { name: file.fileName, bytes } : null}
        />
    );
}
