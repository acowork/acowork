import { useState, useEffect, useCallback, useMemo } from "react";
import { ChevronRight, ChevronDown, Folder, FolderOpen, HardDrive } from "lucide-react";
import { useSettingsStore } from "../../stores/settingsStore";
import { useTranslation } from "../../i18n/useTranslation";
import { ErrorBox } from "../common/ErrorBox";
import { Switch } from "../common/Switch";
import { cn } from "../../lib/utils";
import { listRowHover, listRowSelected } from "../../lib/ui-styles";
import { DEFAULT_GATEWAY_URL } from "../../lib/config";

interface FsBrowseEntry {
    name: string;
    type: string;
    path: string;
    size?: number;
    childrenCount?: number;
}

interface FsBrowseResponse {
    path: string;
    entries: FsBrowseEntry[];
}

interface RemoteFolderPickerProps {
    /** Called when user selects a directory path */
    onSelect: (path: string) => void;
    /** Called when user cancels */
    onCancel: () => void;
    /**
     * Node id of the machine whose filesystem to browse (ADR-055 L7-1).
     * Forwarded as `?target=` to the Gateway, which reverse-proxies to
     * that node's `/fs/browse`. Omit to browse the Gateway machine.
     */
    target?: string;
}

export function RemoteFolderPicker({ onSelect, onCancel, target }: RemoteFolderPickerProps) {
    const { t } = useTranslation();
    const { gatewayUrl } = useSettingsStore();
    const baseUrl = gatewayUrl || DEFAULT_GATEWAY_URL;

    // Navigation history (breadcrumb)
    const [breadcrumbs, setBreadcrumbs] = useState<string[]>([]);
    const [currentPath, setCurrentPath] = useState<string>("");
    const [entries, setEntries] = useState<FsBrowseEntry[]>([]);
    const [loading, setLoading] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const [selectedPath, setSelectedPath] = useState<string | null>(null);

    // Expanded directories (for inline expansion)
    const [expandedDirs, setExpandedDirs] = useState<Set<string>>(new Set());
    const [expandedEntries, setExpandedEntries] = useState<Map<string, FsBrowseEntry[]>>(new Map());

    // Opt-in toggle for hidden entries (names starting with '.'). Off
    // by default to preserve the historical behaviour; on, the backend
    // returns every direct child so users can pick a `.config` / dotfile
    // workspace that the default filter used to silently hide.
    const [showHidden, setShowHidden] = useState(false);

    // Build a `/api/fs/browse` URL with `target` forwarded when set, so
    // the Gateway reverse-proxies to the node that actually owns the
    // filesystem instead of returning its own machine's tree. The
    // `show_hidden` flag is appended when the picker has it on.
    const browseUrl = useCallback(
        (path: string) => {
            const qs = new URLSearchParams({ path });
            if (target) qs.set("target", target);
            if (showHidden) qs.set("show_hidden", "true");
            return `${baseUrl}/api/fs/browse?${qs.toString()}`;
        },
        [baseUrl, target, showHidden],
    );

    const fetchEntries = useCallback(async (path: string) => {
        setLoading(true);
        setError(null);
        try {
            const resp = await fetch(browseUrl(path));
            if (!resp.ok) {
                const err = await resp.json().catch(() => null);
                setError(err?.error || `Failed to browse: ${resp.status}`);
                return;
            }
            const data: FsBrowseResponse = await resp.json();
            setEntries(data.entries);
            setCurrentPath(data.path || path);
        } catch (e) {
            setError(String(e));
        } finally {
            setLoading(false);
        }
    }, [browseUrl]);

    // Load root on mount
    useEffect(() => {
        void fetchEntries("");
    }, [fetchEntries]);

    const navigateTo = useCallback(async (path: string) => {
        setExpandedDirs(new Set());
        setExpandedEntries(new Map());
        setSelectedPath(null);

        // Build breadcrumbs
        if (path === "" || path === "/") {
            setBreadcrumbs([]);
        } else {
            const parts = path.split("/").filter(Boolean);
            // Handle Windows paths like C:/Users/...
            const crumbs: string[] = [];
            if (path.match(/^\w:\//)) {
                // Windows drive root
                crumbs.push(path.slice(0, 3).replace("/", ":/") + "/");
            }
            // Build incremental paths
            let accumulated = path.match(/^\w:\//) ? path.slice(0, 3) : "";
            for (const part of parts) {
                accumulated = accumulated ? `${accumulated}/${part}` : `/${part}`;
                crumbs.push(accumulated);
            }
            setBreadcrumbs(crumbs);
        }

        await fetchEntries(path);
    }, [fetchEntries]);

    const handleExpand = useCallback(async (entry: FsBrowseEntry) => {
        if (entry.type !== "directory") return;

        const newExpanded = new Set(expandedDirs);
        if (newExpanded.has(entry.path)) {
            newExpanded.delete(entry.path);
            setExpandedDirs(newExpanded);
            return;
        }
        newExpanded.add(entry.path);
        setExpandedDirs(newExpanded);

        // Fetch sub-entries if not cached
        if (!expandedEntries.has(entry.path)) {
            try {
                const resp = await fetch(browseUrl(entry.path));
                if (resp.ok) {
                    const data: FsBrowseResponse = await resp.json();
                    setExpandedEntries(new Map(expandedEntries).set(entry.path, data.entries));
                }
            } catch {
                // ignore
            }
        }
    }, [browseUrl, expandedDirs, expandedEntries]);

    const handleConfirm = () => {
        if (selectedPath) {
            onSelect(selectedPath);
        }
    };

    // Toggling `showHidden` flips the query string, but the inline
    // expansion cache (expandedEntries) is keyed by `path` and holds
    // the previous listing — drop it so the next expand re-fetches
    // with the new flag and the chevron count agrees with what's on
    // screen. We also clear the selection since the previous pick
    // may not exist in the new listing.
    const handleToggleHidden = (next: boolean) => {
        setShowHidden(next);
        setExpandedDirs(new Set());
        setExpandedEntries(new Map());
        setSelectedPath(null);
    };

    const handleSelectDir = (entry: FsBrowseEntry) => {
        if (entry.type === "directory") {
            setSelectedPath(entry.path);
        }
    };

    const handleDoubleClick = (entry: FsBrowseEntry) => {
        if (entry.type === "directory") {
            void navigateTo(entry.path);
        }
    };

    // Flatten the visible tree into a flat list respecting expandedDirs.
    //
    // ponytail: recursive rendering (renderEntry → renderEntry) makes
    // the fiber tree depth equal to the user's expanded level count.
    // macOS WKWebView's JSCore stack overflows under deep
    // `commitLayoutEffectOnFiber` traversal even for shallow user
    // input, so we render a flat list instead. Matches the pattern
    // already used in FileTree.tsx (see walk() there).
    //
    // Defense in depth: skip any child whose `path` equals the
    // parent's `path`. The backend's `/` listing was once returning
    // `/` itself as a child (root_entries() quirk), and that looped
    // this walker forever — the backend no longer does that, but if
    // any future symlink/cross-mount case sends us back the parent
    // path, we'd rather render one stray row than stack-overflow.
    const flatEntries = useMemo(() => {
        const out: Array<{ entry: FsBrowseEntry; depth: number }> = [];
        const walk = (entry: FsBrowseEntry, depth: number): void => {
            out.push({ entry, depth });
            if (expandedDirs.has(entry.path)) {
                const children = expandedEntries.get(entry.path);
                if (children) {
                    for (const child of children) {
                        if (child.path === entry.path) continue;
                        walk(child, depth + 1);
                    }
                }
            }
        };
        for (const entry of entries) walk(entry, 0);
        return out;
    }, [entries, expandedDirs, expandedEntries]);

    // Render a single directory entry row. Now non-recursive — the
    // caller flattens the tree first, so this just emits one row.
    const renderEntry = ({ entry, depth }: { entry: FsBrowseEntry; depth: number }) => {
        const isExpanded = expandedDirs.has(entry.path);
        const isSelected = selectedPath === entry.path;
        const isDir = entry.type === "directory";
        const hasChildren = isDir && (entry.childrenCount ?? 0) > 0;

        return (
            <div key={entry.path}>
                <div
                    className={cn(
                        "flex items-center gap-1 px-2 py-1 cursor-pointer transition-colors text-xs",
                        isSelected
                            ? listRowSelected
                            : `${listRowHover} text-text-secondary `,
                    )}
                    style={{ paddingLeft: `${depth * 16 + 8}px` }}
                    onClick={() => handleSelectDir(entry)}
                    onDoubleClick={() => handleDoubleClick(entry)}
                >
                    {isDir ? (
                        <button
                            onClick={(e) => { e.stopPropagation(); void handleExpand(entry); }}
                            className="flex items-center"
                        >
                            {hasChildren ? (
                                isExpanded ? <ChevronDown className="h-3 w-3 shrink-0" /> : <ChevronRight className="h-3 w-3 shrink-0" />
                            ) : (
                                <span className="w-3" />
                            )}
                        </button>
                    ) : (
                        <span className="w-3" />
                    )}
                    {isDir ? (
                        isExpanded ? <FolderOpen className="h-3.5 w-3.5 shrink-0 text-text-tertiary" /> : <Folder className="h-3.5 w-3.5 shrink-0 text-text-tertiary" />
                    ) : null}
                    <span className="truncate min-w-0 flex-1">{entry.name}</span>
                    {!isDir && entry.size != null && (
                        <span className="text-10 text-text-tertiary shrink-0">
                            {entry.size < 1024 ? `${entry.size} B`
                                : entry.size < 1024 * 1024 ? `${(entry.size / 1024).toFixed(1)} KB`
                                    : `${(entry.size / (1024 * 1024)).toFixed(1)} MB`}
                        </span>
                    )}
                </div>
            </div>
        );
    };

    return (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-modal-overlay">
            <div className="w-full max-w-lg rounded-md bg-modal-surface shadow-xl flex flex-col max-h-[80vh]">
                {/* Header */}
                <div className="flex items-center justify-between border-b border-border-divider px-4 py-3 min-h-[var(--ui-dialog-zone-h)]">
                    <h3 className="text-sm font-semibold text-text ">
                        {t("workspace.remoteBrowseTitle")}
                    </h3>
                    <button onClick={onCancel} className="rounded-md p-1 text-text-tertiary hover:bg-zinc-100 dark:hover:bg-zinc-800">
                        ✕
                    </button>
                </div>

                {/* Breadcrumb navigation */}
                {breadcrumbs.length > 0 && (
                    <div className="flex items-center gap-1 px-4 py-2 border-b border-border-divider overflow-x-auto text-10">
                        <button
                            onClick={() => void navigateTo("")}
                            className="flex items-center gap-0.5 text-text-tertiary hover:text-zinc-600 dark:hover:text-zinc-300"
                        >
                            <HardDrive className="h-3 w-3" />
                        </button>
                        {breadcrumbs.map((crumb, i) => (
                            <span key={crumb} className="flex items-center gap-1">
                                <span className="text-text-tertiary">/</span>
                                <button
                                    onClick={() => void navigateTo(crumb)}
                                    className={cn(
                                        "truncate hover:text-zinc-600 dark:hover:text-zinc-300",
                                        i === breadcrumbs.length - 1 ? "text-text-secondary  font-medium" : "text-text-tertiary",
                                    )}
                                >
                                    {crumb.split("/").filter(Boolean).pop() || crumb}
                                </button>
                            </span>
                        ))}
                    </div>
                )}

                {/* Directory tree */}
                <div className="flex-1 overflow-y-auto min-h-0 py-1">
                    {loading ? (
                        <div className="flex items-center justify-center py-8 text-xs text-text-tertiary">
                            {t("workspace.explorer.loading")}
                        </div>
                    ) : error ? (
                        <div className="flex flex-col items-center justify-center gap-3 py-8 text-xs text-text-tertiary">
                            <ErrorBox message={error} className="max-w-md" />
                            <button
                                onClick={() => void fetchEntries(currentPath)}
                                className="rounded-md px-3 py-1 text-xs bg-zinc-100 hover:bg-zinc-200 dark:bg-zinc-700 dark:hover:bg-zinc-600"
                            >
                                {t("common.retry")}
                            </button>
                        </div>
                    ) : entries.length === 0 ? (
                        <div className="flex items-center justify-center py-8 text-xs text-text-tertiary">
                            {t("workspace.remoteBrowseEmpty")}
                        </div>
                    ) : (
                        flatEntries.map((node) => renderEntry(node))
                    )}
                </div>

                {/* Selected path display + action buttons */}
                <div className="border-t border-border-divider px-4 py-3">
                    {selectedPath && (
                        <div className="mb-2 text-xs text-text-tertiary  truncate">
                            {t("workspace.remoteBrowseSelected")}: <span className="font-mono text-text-secondary ">{selectedPath}</span>
                        </div>
                    )}
                    <div className="flex items-center justify-between gap-2">
                        <Switch
                            checked={showHidden}
                            onChange={handleToggleHidden}
                            size="sm"
                            label={t("workspace.remoteBrowseShowHidden")}
                            // Material layout so the switch doesn't try to
                            // stretch to the full footer width (the default
                            // `labelPosition="left"` applies `w-full` and
                            // would shove the action buttons off-screen).
                            labelPosition="right"
                        />
                        <div className="flex items-center gap-2">
                            <button
                                onClick={onCancel}
                                className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-100  dark:hover:bg-zinc-800"
                            >
                                {t("common.cancel")}
                            </button>
                            <button
                                onClick={handleConfirm}
                                disabled={!selectedPath}
                                className="rounded-md px-3 py-1.5 text-xs font-medium text-white disabled:opacity-50 disabled:cursor-not-allowed"
                                style={{ backgroundColor: "var(--color-accent-strong)" }}
                            >
                                {t("workspace.remoteBrowseSelect")}
                            </button>
                        </div>
                    </div>
                </div>
            </div>
        </div>
    );
}
