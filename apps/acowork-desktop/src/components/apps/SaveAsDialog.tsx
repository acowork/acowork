/**
 * Save As target picker for a hosted app's save (ADR-118 §6.4).
 *
 * The app hands bytes to the host; the host is the only side that can reach the
 * workspace, so the user picks where the file goes here — workspace, directory,
 * name — instead of the save silently landing in the session's workspace root.
 * Cancel settles the awaiting save as cancelled (nothing is written, the app
 * hears `saveFailed`).
 */
import { useEffect, useMemo, useRef, useState } from "react";
import { ChevronRight, Folder, Loader2 } from "lucide-react";
import { OverlayScrollbar } from "../common/OverlayScrollbar";
import { useEscapeClose } from "../../hooks/useEscapeClose";
import { useTranslation } from "../../i18n/useTranslation";
import { useSaveAsStore } from "../../stores/saveAsStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import { fetchTreeNode } from "../../stores/fileTree/fileTreeStore";
import { saveNameSegment } from "../../lib/workspaceFileSave";
import { cn } from "../../lib/utils";

export function SaveAsDialog() {
    const { t } = useTranslation();
    const request = useSaveAsStore((s) => s.request);
    const settle = useSaveAsStore((s) => s.settle);
    const workspaces = useWorkspaceStore((s) => s.workspaces);
    const workspacesAgentId = useWorkspaceStore((s) => s.workspacesAgentId);
    const fetchWorkspaces = useWorkspaceStore((s) => s.fetchWorkspaces);

    const [workspaceId, setWorkspaceId] = useState("");
    const [dir, setDir] = useState("");
    const [name, setName] = useState("");
    const [dirs, setDirs] = useState<string[]>([]);
    const [loadingDirs, setLoadingDirs] = useState(false);
    const nameRef = useRef<HTMLInputElement>(null);

    useEffect(() => {
        if (!request) return;
        setWorkspaceId(request.defaultWorkspaceId);
        setDir("");
        setName(saveNameSegment(request.name, "untitled"));
        void fetchWorkspaces(request.agentId);
    }, [request, fetchWorkspaces]);

    useEffect(() => {
        if (!request) return;
        let cancelled = false;
        setLoadingDirs(true);
        void fetchTreeNode(request.agentId, workspaceId, dir)
            .then((node) => {
                if (cancelled) return;
                const entries = node.kind === "ready" || node.kind === "stale" ? node.entries : [];
                setDirs(entries.filter((e) => e.type === "directory").map((e) => e.name));
            })
            .finally(() => {
                if (!cancelled) setLoadingDirs(false);
            });
        return () => {
            cancelled = true;
        };
    }, [request, workspaceId, dir]);

    useEffect(() => {
        if (!request) return;
        nameRef.current?.focus();
        nameRef.current?.select();
    }, [request]);

    const cancel = () => settle(null);
    useEscapeClose(request !== null, cancel);

    const choices = useMemo(
        () => (workspacesAgentId === request?.agentId ? workspaces : []),
        [workspaces, workspacesAgentId, request?.agentId],
    );

    if (!request) return null;

    const trimmed = saveNameSegment(name, "");
    const relPath = dir ? `${dir}/${trimmed}` : trimmed;
    const commit = () => {
        if (!trimmed || !workspaceId) return;
        settle({ agentId: request.agentId, workspaceId, relPath });
    };

    return (
        <div className="fixed inset-0 z-50 flex items-center justify-center">
            <div className="absolute inset-0 bg-modal-overlay" onClick={cancel} />
            <div
                className="relative z-10 flex w-full max-w-md flex-col rounded-md border border-border-outer bg-modal-surface shadow-xl"
                role="dialog"
                aria-modal="true"
                aria-labelledby="save-as-title"
            >
                <h3
                    id="save-as-title"
                    className="flex items-center border-b border-border-divider px-5 py-3 min-h-[var(--ui-dialog-zone-h)] text-sm font-semibold text-text"
                >
                    {t("saveAs.title")}
                </h3>

                <div className="flex flex-col gap-3 px-5 py-4 text-xs text-text-secondary">
                    <label className="flex flex-col gap-1">
                        <span className="text-text-tertiary">{t("saveAs.location")}</span>
                        <select
                            value={workspaceId}
                            onChange={(e) => {
                                setWorkspaceId(e.target.value);
                                setDir("");
                            }}
                            className="rounded-md border border-border-outer bg-modal-surface px-2 py-1.5 text-xs text-text"
                        >
                            {choices.length === 0 && <option value={workspaceId}>{workspaceId}</option>}
                            {choices.map((w) => (
                                <option key={w.id} value={w.id}>
                                    {w.alias || w.path || w.id}
                                </option>
                            ))}
                        </select>
                    </label>

                    <div className="flex flex-col gap-1">
                        <span className="text-text-tertiary">{t("saveAs.folder")}</span>
                        <div className="flex items-center gap-1 rounded-md border border-border-outer px-2 py-1.5">
                            <button
                                type="button"
                                onClick={() => setDir("")}
                                className="text-text-secondary hover:underline"
                            >
                                {t("saveAs.root")}
                            </button>
                            {dir.split("/").filter(Boolean).map((part, i, all) => (
                                <span key={`${part}-${i}`} className="flex items-center gap-1">
                                    <ChevronRight className="h-3 w-3 text-text-tertiary" aria-hidden />
                                    <button
                                        type="button"
                                        onClick={() => setDir(all.slice(0, i + 1).join("/"))}
                                        className="text-text-secondary hover:underline"
                                    >
                                        {part}
                                    </button>
                                </span>
                            ))}
                        </div>
                        <OverlayScrollbar
                            maxHeight={128}
                            className="flex flex-col"
                            containerClassName="rounded-md border border-border-outer"
                        >
                            {loadingDirs && (
                                <span className="flex items-center gap-1 px-2 py-1.5 text-text-tertiary">
                                    <Loader2 className="h-3 w-3 animate-spin" aria-hidden />
                                </span>
                            )}
                            {!loadingDirs && dirs.length === 0 && (
                                <span className="px-2 py-1.5 text-text-tertiary">{t("saveAs.noSubfolders")}</span>
                            )}
                            {dirs.map((d) => (
                                <button
                                    key={d}
                                    type="button"
                                    onClick={() => setDir(dir ? `${dir}/${d}` : d)}
                                    className={cn(
                                        "flex items-center gap-1.5 px-2 py-1.5 text-left text-text-secondary",
                                        "hover:bg-[var(--color-accent)]/10 dark:hover:bg-[var(--color-accent)]/15",
                                    )}
                                >
                                    <Folder className="h-3 w-3 text-text-tertiary" aria-hidden />
                                    {d}
                                </button>
                            ))}
                        </OverlayScrollbar>
                    </div>

                    <label className="flex flex-col gap-1">
                        <span className="text-text-tertiary">{t("saveAs.name")}</span>
                        <input
                            ref={nameRef}
                            value={name}
                            onChange={(e) => setName(e.target.value)}
                            onKeyDown={(e) => {
                                if (e.key === "Enter") commit();
                            }}
                            className="rounded-md border border-border-outer bg-modal-surface px-2 py-1.5 text-xs text-text"
                        />
                    </label>
                    <span className="truncate text-text-tertiary">{relPath || "—"}</span>
                </div>

                <div className="flex items-center justify-end gap-2 border-t border-border-divider px-5 min-h-[var(--ui-dialog-zone-h)]">
                    <button
                        onClick={cancel}
                        className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-[var(--color-accent)]/10 dark:hover:bg-[var(--color-accent)]/15"
                    >
                        {t("common.cancel")}
                    </button>
                    <button
                        onClick={commit}
                        disabled={!trimmed || !workspaceId}
                        className="btn-accent rounded-md px-3 py-1.5 text-xs font-medium disabled:opacity-50"
                    >
                        {t("saveAs.save")}
                    </button>
                </div>
            </div>
        </div>
    );
}