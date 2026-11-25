import { useState, useCallback, useRef, useEffect } from "react";
import { Search, RefreshCw, FolderOpen, FilePlus, FolderPlus, X, MessageSquarePlus, FileSearch, TextSearch, CaseSensitive, WholeWord } from "lucide-react";
import { useAgentStore } from "../../stores/agentStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import { useFileTreeStore, treeKey, isReadyNode, type TreeEntry, getCachedWorkspaceRoot } from "../../stores/fileTree";
import { useChatStore } from "../../stores/chatStore";
import { useChatSessionStore } from "../../stores/chatSessionStore";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { useLayoutStore } from "../../stores/layoutStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { useAddToChatTarget, attachWorkspaceRef } from "../../lib/addToChat";
import { FileTree, fileTreeViewKey } from "./FileTree/FileTree";
import { WorkspaceSelector } from "./WorkspaceSelector";
import { SetiIcon } from "../common/SetiIcon";
import { getFileIcon } from "./FileTree/fileIcons";
import { useTranslation } from "../../i18n/useTranslation";
import { Tooltip } from "../common/Tooltip";
import { cn } from "../../lib/utils";
import { listRowHover, listRowSelected } from "../../lib/ui-styles";
import { log } from "../../lib/logger";
import { useToast } from "../common/ToastProvider";
import { isWorkspacePreviewable } from "../editor/filePreview";
import { GitStatusBar } from "./git/GitStatusBar";
import { GitStatusPanel } from "./git/GitStatusPanel";
import { useGitStore } from "../../stores/gitStore";
import { getGatewayUrl } from "../../lib/config";
import { fileContentSearchUrl } from "../search/GlobalSearchDialog";

/** Search box mode — the two halves of the box are mutually exclusive
 *  (a radio pair, not two filters): `files` answers "where is this file",
 *  `content` answers "where is this text". */
export type SearchMode = "files" | "content";

/** One dropdown row. Content mode adds the match location (`line`) and the
 *  matched source line (`text`); file mode has neither and shows name+dir. */
export interface SearchRow {
    name: string;
    relPath: string;
    dir: string;
    line?: number;
    text?: string;
}

const AGENT_HOME_WORKSPACE = "__agent_home__";

/** The search box's radio pair: file name vs content. Icons only — the row
 *  already carries a placeholder and a mode-dependent tooltip. */
const SEARCH_MODES: Array<{
    mode: SearchMode;
    Icon: typeof FileSearch;
    labelKey: string;
}> = [
    { mode: "files", Icon: FileSearch, labelKey: "workspace.explorer.searchFilesMode" },
    { mode: "content", Icon: TextSearch, labelKey: "workspace.explorer.searchContentMode" },
];

/** The search row's icon buttons — the mode radio pair and the two ripgrep
 *  flags. All four share ONE box (`h-6 w-6`) and ONE gap, because a row of
 *  icons in different boxes / different gaps reads as "big icon, small
 *  icon, big icon" no matter how carefully each glyph is chosen.
 *
 *  The selected state differs by the background chip alone. A darker text
 *  color on top of it made the active glyph read as "bold", which is a
 *  second, louder signal than the chip already gives — same weight as its
 *  neighbours, background does the work.
 *
 *  `disabled` (the flags in file mode) greys the glyph AND drops the chip:
 *  a switch that is ON but not applicable must not keep its accent. */
function searchToggleClass(active: boolean, disabled = false): string {
    return cn(
        "flex h-6 w-6 shrink-0 items-center justify-center rounded transition-colors",
        disabled
            ? "text-text-disabled hover:text-text-disabled"
            : active
              ? "bg-[var(--color-accent)]/15 text-text-secondary"
              : "text-text-tertiary hover:text-text-secondary",
    );
}

function splitRelPath(relPath: string): Pick<SearchRow, "name" | "relPath" | "dir"> {
    const slash = relPath.lastIndexOf("/");
    return {
        name: slash >= 0 ? relPath.slice(slash + 1) : relPath,
        relPath,
        dir: slash >= 0 ? relPath.slice(0, slash) : "",
    };
}

/**
 * Rows for the search dropdown, one endpoint per mode.
 *
 *  - `files`   → `/workspaces/find` (fuzzy file **name**, what the box has
 *                always done).
 *  - `content` → `/workspaces/search` with `match_file_names=false`: the
 *                same ripgrep backend the global search dialog's 文件 tab
 *                uses, but content-only, so "content" mode cannot quietly
 *                answer with file-name hits.
 *
 * Exported for its own test; abort/network failures read as "no rows" here —
 * the caller drops the result anyway once the signal is aborted.
 */
export async function searchRows(
    mode: SearchMode,
    agentId: string,
    workspaceId: string,
    query: string,
    signal: AbortSignal,
    opts: { caseSensitive?: boolean; wholeWord?: boolean } = {},
): Promise<SearchRow[]> {
    if (mode === "files") {
        const resp = await useWorkspaceStore
            .getState()
            .findFiles(agentId, workspaceId, query, 50, signal);
        return (resp?.matches ?? []).map((m) => ({ ...splitRelPath(m.relPath), name: m.name }));
    }
    try {
        const resp = await fetch(
            fileContentSearchUrl(getGatewayUrl(), agentId, query, {
                workspaceId: workspaceId === AGENT_HOME_WORKSPACE ? undefined : workspaceId,
                caseSensitive: opts.caseSensitive,
                wholeWord: opts.wholeWord,
                maxResults: 50,
            }),
            { signal },
        );
        if (!resp.ok) return [];
        const data = (await resp.json()) as {
            matches?: Array<{ file: string; line?: number; text: string }>;
        };
        return (data.matches ?? []).map((m) => ({
            ...splitRelPath(m.file),
            line: m.line,
            text: m.text,
        }));
    } catch {
        return [];
    }
}

/** Abbreviate a file path from the left: "…parent/filename.ext" */
function abbreviatePath(path: string): string {
    const maxLen = 38;
    if (path.length <= maxLen) return path;
    const parts = path.split("/");
    const filename = parts[parts.length - 1];
    for (let i = parts.length - 2; i >= 1; i--) {
        const abbreviated = `…${parts.slice(i).join("/")}`;
        if (abbreviated.length <= maxLen) return abbreviated;
    }
    return `…${filename}`;
}

/**
 * Workspace the right panel shows. The panel belongs to whichever surface is
 * on screen (same two-composers rule as "Add to Chat", see `lib/addToChat`):
 *
 *  - **Chat surface** → the Chat session's OWN workspace: its
 *    `session_config` draft pick, falling back to the execution session's
 *    (as a read-only display default when the chat has never picked) and then
 *    agent home. Picking here writes the Chat session's config and never a
 *    session workspace — an execution session is not re-pointed by a
 *    Chat-surface choice; the pick only seeds a session the router creates.
 *  - **Execution surface** → the active session's workspace (the real working
 *    directory), else agent home.
 *
 * Exported for its own test — the fallback chain is the fix for "closing every
 * exec tab blanks the panel", and the Chat-surface branch is the fix for "the
 * composer chip and the panel show different workspaces".
 */
export function resolveExplorerWorkspaceId(
    activeSessionId: string | null,
    sessionWorkspaceMap: Record<string, string>,
    chatDraftWorkspace: string | null | undefined,
    onChatSurface: boolean,
): string {
    const sessionWs = activeSessionId
        ? (sessionWorkspaceMap[activeSessionId] ?? "__agent_home__")
        : "__agent_home__";
    if (onChatSurface) return chatDraftWorkspace ?? sessionWs;
    return sessionWs;
}

export function WorkspaceExplorer() {
    const { t } = useTranslation();
    const { addToast } = useToast();
    const selectedAgentId = useAgentStore((s) => s.selectedAgentId);
    const fontSize = useSettingsStore((s) => s.fontSize);
    const selectedAgent = useAgentStore((s) => s.selectedAgentId ? s.agents[s.selectedAgentId]?.meta : undefined);
    const invalidateTreeCache = useFileTreeStore((s) => s.invalidate);
    const fetchTree = useFileTreeStore((s) => s.fetch);
    // Authoritative refresh after a successful server-side mutation (create /
    // rename / move / delete / paste). Bypasses SWR so the UI immediately
    // reflects the new state instead of racing the SWR freshness window
    // (regression introduced when fileTreeStore adopted stale-while-
    // revalidate — see ea819127 fix).
    const refreshTree = useFileTreeStore((s) => s.refresh);
    const sessionWorkspaceMap = useWorkspaceStore((s) => s.sessionWorkspaceMap);
    const createFile = useWorkspaceStore((s) => s.createFile);
    const createDir = useWorkspaceStore((s) => s.createDir);
    const deleteFile = useWorkspaceStore((s) => s.deleteFile);
    const deleteDir = useWorkspaceStore((s) => s.deleteDir);
    const copyItem = useWorkspaceStore((s) => s.copyItem);
    const renameItem = useWorkspaceStore((s) => s.renameItem);
    const revealItem = useWorkspaceStore((s) => s.revealItem);
    const setCopiedEntry = useWorkspaceStore((s) => s.setCopiedEntry);
    const openFile = useFileEditorStore((s) => s.openFile);
    const openPreview = useFileEditorStore((s) => s.openPreview);

    // Get the current workspace ID for the active session
    const activeSessionId = useChatStore((s) =>
        selectedAgentId ? s.getActiveSessionId(selectedAgentId) : null,
    );
    // The Chat surface owns its workspace (design 27 §4.5): the panel mirrors
    // the Chat session's own pick, not the execution session's working
    // directory. Picking there writes the Chat session's config and never a
    // session workspace, so an execution session is untouched by it. This is
    // also what makes Chat-surface "Add to Chat" coherent — the tree and the
    // composer chip are the same workspace. (Before this, the tree blanked the
    // moment the last exec tab closed, and with a session open the panel
    // silently disagreed with the chip.)
    const chatSessionId = useChatSessionStore((s) =>
        selectedAgentId ? s.byAgent[selectedAgentId]?.chatSessionId ?? null : null,
    );
    const onChatSurface = useChatSessionStore((s) =>
        selectedAgentId ? s.byAgent[selectedAgentId]?.mode === "chat" : false,
    );
    // Whichever surface the panel belongs to. A closed exec tab leaves the
    // panel on the Chat surface even if the tab strip still says "exec", so
    // with no session to mirror the Chat's own pick is the only honest source
    // (this is also what keeps the tree from blanking when the last exec tab
    // closes).
    const chatSurface = (onChatSurface || activeSessionId === null) && chatSessionId !== null;
    const chatDraft = useChatSessionStore((s) =>
        chatSessionId ? s.draft[chatSessionId] : undefined,
    );
    const currentWorkspaceId = resolveExplorerWorkspaceId(
        activeSessionId, sessionWorkspaceMap, chatDraft?.workspace,
        chatSurface,
    );
    // ADR-076 + design 27 Wave 5: a reference goes to the composer that is on
    // screen (Chat session or execution session). Read-only execution sessions
    // cannot send the next message, so they are not a target — but a Chat
    // surface next to one still is. Mirrors FileEditorPanel's
    // `addToChatDisabledReason` gating so both "add to chat" entries behave alike.
    const addToChatTarget = useAddToChatTarget(selectedAgentId);

    // Search-result "add to chat" eligibility. The search hits the currently
    // selected workspace, so "wrong agent/workspace" reasons from the editor
    // panel don't apply — only the session-side gates remain.
    const searchAddToChatDisabledReason: "no-agent" | "no-session" | "read-only-session" | null =
        !selectedAgentId ? "no-agent"
            : addToChatTarget === null ? (activeSessionId ? "read-only-session" : "no-session")
                : null;

    // ADR-078 (2026-XX revision) — git strip source: (agent, workspace) of
    // the currently selected session workspace. Independent of whether any
    // file is open in the editor (git is a workspace-level property).
    // `__agent_home__` is a virtual home workspace with no repo context.
    const gitContext =
        selectedAgentId && currentWorkspaceId !== "__agent_home__"
            ? { agentId: selectedAgentId, workspaceId: currentWorkspaceId }
            : null;
    const gitExpanded = useGitStore((s) =>
        gitContext ? s.isExpanded(gitContext.agentId, gitContext.workspaceId) : false,
    );

    // Parent-controlled inline rename — when `renameTarget` matches a
    // tree node's relPath, that node swaps its name span for an input
    // and the user types the new name. The actual `rename_item` call
    // lives in `handleRename` below; here we only own the trigger.
    //
    // macOS Finder-style flows: right-click → "New File" / "New Folder"
    // / "Paste" all call `requestRenameFor(relPath, initialValue)` after
    // the create succeeds so the user can immediately retype the name.
    const [renameTarget, setRenameTarget] = useState<string | null>(null);
    const [renameInitialValue, setRenameInitialValue] = useState<string>("");

    const requestRenameFor = useCallback((relPath: string, initialValue: string) => {
        setRenameTarget(relPath);
        setRenameInitialValue(initialValue);
    }, []);

    const cancelExternalRename = useCallback(() => {
        setRenameTarget(null);
        setRenameInitialValue("");
    }, []);

    // ── Pointer-based Drag & Drop ─────────────────────────────────────
    //
    // HTML5 DnD (dragstart/dragover/drop) is unreliable in macOS
    // WKWebView: `dragover`/`drop` events don't fire on child elements
    // during internal drags, and state updates during dragstart cause
    // Virtualizer DOM re-layout which cancels the native drag session.
    // We bypass the HTML5 DnD API entirely and implement dragging with
    // Pointer Events - the same approach used by dnd-kit's PointerSensor.
    //
    // Flow:
    //   1. `pointerdown` on a row -> record drag candidate (path + start coords)
    //   2. `pointermove` (global) -> if moved > 5px, enter drag mode,
    //      use `elementFromPoint` to find target row, update `dropTarget`
    //   3. `pointerup` (global) -> if on valid target, call `handleMoveItem`
    //
    // `draggingRelPath` drives the `.file-tree-row-drag-source` visual
    // (opacity 0.5 + pointer-events:none so elementFromPoint sees through).
    // `dropTarget` drives the `.file-tree-row-drop-target` highlight.
    const [draggingRelPath, setDraggingRelPath] = useState<string | null>(null);
    const [dropTarget, setDropTarget] = useState<string | null>(null);

    /** Mutable drag state - survives re-renders without triggering them. */
    const dragStateRef = useRef<{
        relPath: string;
        isDir: boolean;
        startX: number;
        startY: number;
        active: boolean;
    } | null>(null);

    const clearDragState = useCallback(() => {
        dragStateRef.current = null;
        setDraggingRelPath(null);
        setDropTarget(null);
    }, []);

    /** Returns `true` when `destParent === sourceRelPath` OR
     * `sourceRelPath.startsWith(destParent + "/")` - i.e. the move is
     * to itself or to one of its own ancestors. We must refuse because
     * it would either be a no-op (self) or create a cycle (ancestor). */
    const isSelfOrAncestor = useCallback((sourceRelPath: string, destParent: string) => {
        if (destParent === sourceRelPath) return true;
        if (destParent === "") return false; // dropping onto workspace root is always valid
        return sourceRelPath === destParent || sourceRelPath.startsWith(`${destParent}/`);
    }, []);

    /** Move a tree entry to a new parent directory. */
    const handleMoveItem = useCallback(
        async (sourceRelPath: string, destParentRelPath: string, isDir: boolean): Promise<boolean> => {
            if (!selectedAgentId) return false;

            if (isSelfOrAncestor(sourceRelPath, destParentRelPath)) {
                log.warn(
                    "[WorkspaceExplorer] drop rejected (self/ancestor): source=%s destParent=%s",
                    sourceRelPath,
                    destParentRelPath,
                );
                return false;
            }

            const basename = sourceRelPath.split("/").pop() ?? sourceRelPath;
            const dest =
                destParentRelPath === ""
                    ? basename
                    : `${destParentRelPath}/${basename}`;

            log.debug(
                "[WorkspaceExplorer] DnD move: source=%s dest=%s (dir=%s)",
                sourceRelPath,
                dest,
                isDir,
            );

            const ok = await renameItem(selectedAgentId, currentWorkspaceId, sourceRelPath, dest);
            if (!ok) {
                addToast({ type: "error", message: `Move failed: ${basename}` });
                return false;
            }
            // Refresh both ends so the source disappears from its
            // old parent and the entry appears in the destination.
            // Use `refreshTree` (bypass SWR) because we just mutated
            // the server and need the UI to converge immediately —
            // otherwise the SWR fresh window will keep showing the
            // pre-move tree and any subsequent operation against the
            // source path will 404 (regression: ea819127).
            const sourceParent =
                sourceRelPath.includes("/")
                    ? sourceRelPath.substring(0, sourceRelPath.lastIndexOf("/"))
                    : "";
            const srcRefresh =
                sourceParent
                    ? refreshTree(selectedAgentId, currentWorkspaceId, sourceParent)
                    : refreshTree(selectedAgentId, currentWorkspaceId, "");
            const dstRefresh = refreshTree(selectedAgentId, currentWorkspaceId, destParentRelPath);
            void Promise.all([srcRefresh, dstRefresh]).then((nodes) => {
                if (nodes.some((n) => n.kind === "error")) {
                    addToast({
                        type: "error",
                        message: `Moved ${basename}, but the tree refresh failed — the list may be stale.`,
                    });
                }
            });
            return true;
        },
        [selectedAgentId, currentWorkspaceId, renameItem, refreshTree, isSelfOrAncestor, addToast],
    );

    /** `pointerdown` on a tree row - records the drag candidate but
     * does NOT start dragging yet. The actual drag begins only after
     * the pointer moves beyond DRAG_THRESHOLD pixels. */
    const onPointerDownTreeEntry = useCallback(
        (relPath: string, isDir: boolean, e: React.PointerEvent) => {
            if (e.button !== 0) return; // only left button
            dragStateRef.current = {
                relPath,
                isDir,
                startX: e.clientX,
                startY: e.clientY,
                active: false,
            };
        },
        [],
    );

    /** Global pointermove/pointerup listeners - registered once.
     * These handle the entire drag lifecycle without HTML5 DnD. */
    useEffect(() => {
        const DRAG_THRESHOLD = 5;

        const findRowUnderCursor = (x: number, y: number): { relPath: string; isDir: boolean } | null => {
            const el = document.elementFromPoint(x, y);
            const row = el?.closest('[data-rel-path]') as HTMLElement | null;
            if (!row) return null;
            return {
                relPath: row.getAttribute('data-rel-path') ?? '',
                isDir: row.getAttribute('data-is-dir') === 'true',
            };
        };

        const handlePointerMove = (e: PointerEvent) => {
            const ds = dragStateRef.current;
            if (!ds) return;

            if (!ds.active) {
                const dx = e.clientX - ds.startX;
                const dy = e.clientY - ds.startY;
                if (Math.abs(dx) < DRAG_THRESHOLD && Math.abs(dy) < DRAG_THRESHOLD) return;
                ds.active = true;
                setDraggingRelPath(ds.relPath);
            }

            const target = findRowUnderCursor(e.clientX, e.clientY);
            // No row under cursor -> blank space = workspace root ("")
            // which is always a valid drop target (unless source IS root).
            if (!target) {
                setDropTarget(isSelfOrAncestor(ds.relPath, "") ? null : "");
                return;
            }
            if (!target.isDir || isSelfOrAncestor(ds.relPath, target.relPath)) {
                setDropTarget(null);
            } else {
                setDropTarget(target.relPath);
            }
        };

        const handlePointerUp = (e: PointerEvent) => {
            const ds = dragStateRef.current;
            if (!ds) return;

            if (ds.active) {
                const target = findRowUnderCursor(e.clientX, e.clientY);
                // No row -> drop to workspace root ("")
                const destRelPath = target ? target.relPath : "";
                const destIsDir = target ? target.isDir : true;
                if (destIsDir && !isSelfOrAncestor(ds.relPath, destRelPath)) {
                    void handleMoveItem(ds.relPath, destRelPath, ds.isDir);
                }
            }

            clearDragState();
        };

        const handlePointerCancel = () => clearDragState();

        window.addEventListener('pointermove', handlePointerMove);
        window.addEventListener('pointerup', handlePointerUp);
        window.addEventListener('pointercancel', handlePointerCancel);
        return () => {
            window.removeEventListener('pointermove', handlePointerMove);
            window.removeEventListener('pointerup', handlePointerUp);
            window.removeEventListener('pointercancel', handlePointerCancel);
        };
    }, [isSelfOrAncestor, handleMoveItem, clearDragState]);


    // Selected tree entry — drives the toolbar's "create in selected dir"
    // behaviour. `null` means nothing is selected, so the toolbar falls
    // back to the workspace root.
    const [selectedEntry, setSelectedEntry] = useState<{ path: string; type: "file" | "dir" } | null>(null);

    // Reset selection when agent or workspace changes — the previously-
    // selected path is not guaranteed to exist in the new tree.
    useEffect(() => {
        setSelectedEntry(null);
    }, [selectedAgentId, currentWorkspaceId]);

    /* ── Selection bridging from FileTree ──────────────────────────────── */
    const handleSelectPath = useCallback(
        (path: string | null, type: "file" | "dir" | null) => {
            if (path === null || type === null) {
                setSelectedEntry(null);
            } else {
                setSelectedEntry({ path, type });
            }
        },
        [],
    );

    /* ── Rename handler ──────────────────────────────────────────────────
     * `newName` must be just the basename — the parent path is taken from
     * the entry's current location. The desktop appends `-copy N` only for
     * paste; renames use exactly what the user types. */
    const handleRename = useCallback(
        async (relPath: string, newName: string, isDir: boolean): Promise<boolean> => {
            const trimmed = newName.trim();
            if (!trimmed) return false;
            const oldName = relPath.split("/").pop() ?? "";
            if (trimmed === oldName) {
                // No-op rename — close the rename input even though the
                // server wasn't called.
                cancelExternalRename();
                return true;
            }
            const parentPath = relPath.substring(0, relPath.lastIndexOf("/"));
            const dest = parentPath ? `${parentPath}/${trimmed}` : trimmed;
            const ok = await renameItem(selectedAgentId ?? "", currentWorkspaceId, relPath, dest);
            if (!ok) {
                addToast({ type: "error", message: `Rename failed: ${oldName} → ${trimmed}` });
                return false;
            }
            // Close the rename input — the node at `relPath` will unmount
            // and a fresh one will mount at `dest` (the tree re-flattens
            // on the next render of `flatNodes`).
            cancelExternalRename();
            // Force-refresh the parent so the renamed entry shows up at
            // `dest` and disappears from `relPath`. `refreshTree` bypasses
            // SWR — a stale-while-revalidate `fetch()` would happily
            // return the pre-rename entries within the freshMs window and
            // leave the UI showing the old relPath until the SWR background
            // fetch eventually lands. That race broke file rename in this
            // workspace after ea819127 introduced SWR semantics.
            const parentRefresh =
                parentPath
                    ? refreshTree(selectedAgentId ?? "", currentWorkspaceId, parentPath)
                    : refreshTree(selectedAgentId ?? "", currentWorkspaceId, "");
            void parentRefresh.then((node) => {
                if (node.kind === "error") {
                    addToast({
                        type: "error",
                        message: `Renamed to ${trimmed}, but the tree refresh failed — the list may be stale.`,
                    });
                }
            });
            // Move selection to the new path so the toolbar's "create in
            // selected dir" target stays sensible (only meaningful for
            // directories).
            if (isDir) {
                setSelectedEntry({ path: dest, type: "dir" });
            } else {
                setSelectedEntry(null);
            }
            return true;
        },
        [selectedAgentId, currentWorkspaceId, renameItem, refreshTree, addToast, cancelExternalRename],
    );

    /** Right-click "Rename" on an existing entry — single source of truth
     * lives in the parent so the same `renameTarget` machinery drives both
     * the "New File / Paste" flow and the explicit Rename menu item. */
    const handleRequestRename = useCallback(
        (relPath: string, initialName: string) => {
            requestRenameFor(relPath, initialName);
        },
        [requestRenameFor],
    );

    /* ── Reveal in File Explorer ──────────────────────────────────────
     *
     * Opens the OS file manager (Finder / Explorer / xdg-open) with the
     * given entry selected. The actual spawn happens in the desktop
     * Rust process via the `reveal_in_file_explorer` Tauri command; the
     * store handles gateway-mode guarding + path resolution.
     *
     * Local-mode only by design. The `revealItem` action refuses remote
     * gateways and returns false; we surface the result as a toast so
     * the user knows why nothing happened. The menu item itself is also
     * disabled with an explanatory tooltip when `isGatewayLocal()` is
     * false — this handler is the second line of defence. */
    const handleReveal = useCallback(
        async (relPath: string): Promise<void> => {
            if (!selectedAgentId) return;
            const ok = await revealItem(selectedAgentId, currentWorkspaceId, relPath);
            if (!ok) {
                // The store already logged the underlying cause (remote
                // mode, no cached root, spawn failure, …). Surface a
                // user-facing hint here so the action is not silent.
                addToast({
                    type: "error",
                    message: t("workspace.contextMenu.revealDisabled"),
                });
            }
        },
        [selectedAgentId, currentWorkspaceId, revealItem, addToast, t],
    );

    /* ── Search box state (Ctrl+P-style file search above the file tree) ── */
    const [searchQuery, setSearchQuery] = useState("");
    const [searchMode, setSearchMode] = useState<SearchMode>("files");
    /** Content-mode only: case-sensitive and whole-word matching. The file
     *  mode has no use for them — a fuzzy name search is case-insensitive by
     *  definition — so they are hidden, not merely ignored, there. */
    const [caseSensitive, setCaseSensitive] = useState(false);
    const [wholeWord, setWholeWord] = useState(false);
    const [searchFocused, setSearchFocused] = useState(false);
    const [focusedIdx, setFocusedIdx] = useState(0);
    const [searchLoading, setSearchLoading] = useState(false);
    const [searchResults, setSearchResults] = useState<SearchRow[]>([]);
    const searchInputRef = useRef<HTMLInputElement>(null);

    // Server-side search — one request per debounced query, endpoint picked
    // by `searchMode` (see `searchRows`). Previous in-flight requests are
    // cancelled via AbortController so the latest keystroke always wins,
    // even on a slow network.
    useEffect(() => {
        if (!searchQuery.trim() || !selectedAgentId) {
            setSearchResults([]);
            setSearchLoading(false);
            return;
        }
        const controller = new AbortController();
        setSearchLoading(true);
        const timer = setTimeout(() => {
            void searchRows(
                searchMode,
                selectedAgentId,
                currentWorkspaceId,
                searchQuery,
                controller.signal,
                { caseSensitive, wholeWord },
            ).then((rows) => {
                if (controller.signal.aborted) return;
                setSearchResults(rows);
                setSearchLoading(false);
            });
        }, 150);
        return () => {
            clearTimeout(timer);
            controller.abort();
        };
    }, [searchQuery, searchMode, caseSensitive, wholeWord, selectedAgentId, currentWorkspaceId]);

    const matchingRows = searchQuery ? searchResults : [];

    // Clamp focused index when the result list changes
    useEffect(() => {
        setFocusedIdx((i) => (i >= matchingRows.length ? 0 : i));
    }, [searchQuery, matchingRows.length]);

    // Ctrl+F (no file-tab focus) — AppLayout requests that the workspace
    // search input take focus. Consume each new seq once so a repeat press
    // re-focuses even when the user is already inside the input.
    const workspaceSearchFocusSeq = useLayoutStore((s) => s.workspaceSearchFocusSeq);
    const consumedSearchFocusSeqRef = useRef(-1);
    useEffect(() => {
        if (workspaceSearchFocusSeq <= consumedSearchFocusSeqRef.current) return;
        consumedSearchFocusSeqRef.current = workspaceSearchFocusSeq;
        const el = searchInputRef.current;
        if (!el) return;
        el.focus();
        // Select any existing query so a second Ctrl+F starts with a
        // clean slate that the user can immediately overwrite.
        el.select();
    }, [workspaceSearchFocusSeq]);

    const handleSearchSelect = useCallback((relPath: string) => {
        if (!selectedAgentId) return;
        // Previewable files (images, SVG, Markdown, HTML) open in preview;
        // everything else opens in Monaco. Single rule shared with
        // `handleFileDoubleClick` so both entry points can't drift.
        if (isWorkspacePreviewable(relPath)) {
            void openPreview(selectedAgentId, currentWorkspaceId, relPath);
        } else {
            void openFile(selectedAgentId, currentWorkspaceId, relPath);
        }
        setSearchQuery("");
        setSearchFocused(false);
        searchInputRef.current?.blur();
    }, [selectedAgentId, currentWorkspaceId, openFile, openPreview]);

    /**
     * Attach a search-result file to the active conversation — mirrors
     * `FileEditorPanel.handleTabAddToChat` so the two entry points produce
     * an identical context chip (same id, same absPath resolution). Unlike
     * `handleSearchSelect`, this does NOT close the search dropdown: the
     * user may want to add several files in a row (the store de-dupes by
     * id, so re-adding the same file is a no-op).
     *
     * `searchAddToChatDisabledReason` is checked by the caller (the
     * rendered button + the Ctrl/Cmd+Enter shortcut) so this function can
     * stay a pure store mutation.
     */
    const handleSearchAddToChat = useCallback((item: { name: string; relPath: string }) => {
        if (!selectedAgentId || searchAddToChatDisabledReason !== null) return;
        const root = getCachedWorkspaceRoot(selectedAgentId, currentWorkspaceId);
        const absPath = root ? `${root}/${item.relPath}` : item.relPath;
        attachWorkspaceRef(addToChatTarget, {
            id: `${selectedAgentId}:${item.relPath}`,
            type: "file",
            name: item.name,
            absPath,
        });
        // Toast so the user gets feedback when the search dropdown stays
        // open and the new chip is not in the immediate field of view.
        addToast({
            type: "info",
            message: t("workspace.search.addedToChat", { name: item.name }),
        });
    }, [selectedAgentId, activeSessionId, currentWorkspaceId, searchAddToChatDisabledReason, addToChatTarget, addToast, t]);

    const handleSearchKeyDown = useCallback((e: React.KeyboardEvent<HTMLInputElement>) => {
        if (e.key === "Escape") {
            e.preventDefault();
            setSearchQuery("");
            setSearchFocused(false);
            searchInputRef.current?.blur();
        } else if (e.key === "ArrowDown") {
            e.preventDefault();
            setFocusedIdx((i) => Math.min(i + 1, matchingRows.length - 1));
        } else if (e.key === "ArrowUp") {
            e.preventDefault();
            setFocusedIdx((i) => Math.max(i - 1, 0));
        } else if (e.key === "Enter") {
            e.preventDefault();
            const item = matchingRows[focusedIdx];
            if (!item) return;
            // Ctrl/Cmd+Enter attaches to chat; bare Enter opens the file —
            // splits the two actions so a keyboard-only flow can either
            // "open to inspect" or "attach to chat" without leaving the
            // input.
            if (e.ctrlKey || e.metaKey) {
                handleSearchAddToChat(item);
            } else {
                handleSearchSelect(item.relPath);
            }
        }
    }, [matchingRows, focusedIdx, handleSearchSelect, handleSearchAddToChat]);

    const showDropdown = searchFocused && searchQuery.trim().length > 0;
    const noResultsLabel =
        searchMode === "content" ? "No matching content" : "No matching files";

    const handleRefresh = useCallback(() => {
        if (!selectedAgentId) return;
        invalidateTreeCache(selectedAgentId);
        fetchTree(selectedAgentId, currentWorkspaceId, "");
    }, [selectedAgentId, currentWorkspaceId, invalidateTreeCache, fetchTree]);

    /**
     * Resolve the parent path that the toolbar's "New File / New Folder"
     * buttons should target. Priority:
     *   1. The currently-selected **directory** in the tree.
     *   2. Workspace root (empty string) when nothing is selected or the
     *      selection is a file.
     */
    const selectedDirParent = selectedEntry && selectedEntry.type === "dir"
        ? selectedEntry.path
        : "";

    /** Build a deduplicated basename that does not collide with anything
     * already in `parentPath`. Used by `quickCreateAndRename` for the
     * macOS Finder-style `untitled` / `untitled 2` / `untitled 3` … naming
     * and shared between files and dirs (the basename is identical for
     * both — macOS Finder shows e.g. `untitled 2` for a folder). */
    const buildUniqueName = useCallback(
        (baseName: string, existingNames: Set<string>): string => {
            if (!existingNames.has(baseName)) return baseName;
            for (let i = 2; i < 1000; i++) {
                const candidate = `${baseName} ${i}`;
                if (!existingNames.has(candidate)) return candidate;
            }
            // Pathological fallback — give up and let the server return
            // an error; we don't want to silently pick `untitled 1000`
            // and confuse the user.
            return baseName;
        },
        [],
    );

    /** macOS Finder-style quick-create: create an empty file/dir with
     * the next free `untitled[N]` name, refresh the parent so the new
     * entry appears, and immediately enter inline rename mode with
     * that basename seeded in the input.
     *
     * Replaces the old two-step "type a name in a top-of-tree prompt
     * then create" flow. The user can either keep the suggested name
     * (just blur / Esc) or type a new one. */
    const quickCreateAndRename = useCallback(
        async (parentPath: string, type: "file" | "dir") => {
            if (!selectedAgentId) return;
            const siblingNode = useFileTreeStore.getState().getNode(
                treeKey(selectedAgentId, currentWorkspaceId, parentPath),
            );
            const siblingNames = new Set<string>(
                isReadyNode(siblingNode) ? siblingNode.entries.map((e: TreeEntry) => e.name) : [],
            );
            const baseName = "untitled";
            const name = buildUniqueName(baseName, siblingNames);
            const relPath = parentPath ? `${parentPath}/${name}` : name;

            log.debug("[WorkspaceExplorer] quickCreate", type, "at", relPath, "workspace:", currentWorkspaceId);

            const ok =
                type === "file"
                    ? await createFile(selectedAgentId, currentWorkspaceId, relPath)
                    : await createDir(selectedAgentId, currentWorkspaceId, relPath);

            if (!ok) {
                addToast({ type: "error", message: `Create failed: ${name}` });
                return;
            }

            // Force-refresh the parent so the new entry shows up before we
            // ask for rename. If we used SWR `fetch()` here and the cache
            // entry was still fresh, we'd be served the pre-creation
            // entries — the FileTreeNode with the new `relPath` would not
            // exist yet and the rename input would never appear (or would
            // appear on the wrong node), and the rename commit against the
            // server would target a `relPath` that is not yet in the local
            // cache. Bypassing SWR is mandatory here.
            //
            // `refresh` resolves to `kind:"error"` on failure instead of
            // throwing; if the tree could not be re-fetched we must NOT
            // enter rename mode — the target node isn't in `flatNodes`, so
            // the rename input would silently never render.
            const refreshed =
                parentPath
                    ? await refreshTree(selectedAgentId, currentWorkspaceId, parentPath)
                    : await refreshTree(selectedAgentId, currentWorkspaceId, "");
            if (refreshed.kind === "error") {
                addToast({
                    type: "error",
                    message: `Created ${name}, but the tree refresh failed — please retry or use the Refresh button.`,
                });
                return;
            }
            requestRenameFor(relPath, name);
        },
        [selectedAgentId, currentWorkspaceId, createFile, createDir, refreshTree, buildUniqueName, requestRenameFor, addToast],
    );

    const handleNewFile = useCallback(() => {
        log.debug("[WorkspaceExplorer] handleNewFile clicked, agent:", selectedAgentId, "workspace:", currentWorkspaceId, "parent:", selectedDirParent);
        void quickCreateAndRename(selectedDirParent, "file");
    }, [selectedAgentId, currentWorkspaceId, selectedDirParent, quickCreateAndRename]);

    const handleNewFolder = useCallback(() => {
        log.debug("[WorkspaceExplorer] handleNewFolder clicked, parent:", selectedDirParent);
        void quickCreateAndRename(selectedDirParent, "dir");
    }, [selectedDirParent, quickCreateAndRename]);

    const handleFileDoubleClick = useCallback((_entry: TreeEntry, relPath: string) => {
        if (!selectedAgentId) return;
        // Previewable files (images, SVG, Markdown, HTML) open in preview;
        // everything else opens in Monaco for source editing. Single rule
        // shared with `handleSearchSelect` and `FileTreeNode.isPreviewable`
        // via `isWorkspacePreviewable` so the right-click menu, the search
        // picker and the double-click handler can't drift apart.
        if (isWorkspacePreviewable(relPath)) {
            void openPreview(selectedAgentId, currentWorkspaceId, relPath);
        } else {
            void openFile(selectedAgentId, currentWorkspaceId, relPath);
        }
    }, [selectedAgentId, currentWorkspaceId, openFile, openPreview]);

    /** Called from FileTree context menu to create item at a specific path —
     * matches the toolbar flow so right-click "New File" / "New Folder"
     * behaves identically: create immediately, then enter rename mode. */
    const handleContextNewItem = useCallback(
        (type: "file" | "dir", parentPath: string) => {
            void quickCreateAndRename(parentPath, type);
        },
        [quickCreateAndRename],
    );

    const handleDelete = useCallback(async (relPath: string, isDir: boolean) => {
        if (!selectedAgentId) return;
        const ok = isDir
            ? await deleteDir(selectedAgentId, currentWorkspaceId, relPath)
            : await deleteFile(selectedAgentId, currentWorkspaceId, relPath);
        if (ok) {
            // Authoritative refresh (bypass SWR) — the entry is gone on
            // disk and the cached pre-delete entries must NOT be served.
            const parentPath = relPath.substring(0, relPath.lastIndexOf("/"));
            const parentRefresh =
                parentPath
                    ? refreshTree(selectedAgentId, currentWorkspaceId, parentPath)
                    : refreshTree(selectedAgentId, currentWorkspaceId, "");
            void parentRefresh.then((node) => {
                if (node.kind === "error") {
                    addToast({
                        type: "error",
                        message: `Deleted ${relPath.split("/").pop()}, but the tree refresh failed — the list may be stale.`,
                    });
                }
            });
        }
    }, [selectedAgentId, currentWorkspaceId, deleteFile, deleteDir, refreshTree, addToast]);

    const handleCopy = useCallback((relPath: string, isDir: boolean) => {
        if (!selectedAgentId) return;
        setCopiedEntry({
            agentId: selectedAgentId,
            workspaceId: currentWorkspaceId,
            path: relPath,
            type: isDir ? "directory" : "file",
        });
    }, [selectedAgentId, currentWorkspaceId, setCopiedEntry]);

    const handlePaste = useCallback(async (parentPath: string) => {
        if (!selectedAgentId) return;
        const entry = useWorkspaceStore.getState().copiedEntry;
        if (!entry || entry.agentId !== selectedAgentId || entry.workspaceId !== currentWorkspaceId) return;

        const name = entry.path.split("/").pop() || entry.path;

        /**
         * Build a deduplicated `-copy` suffix that does not collide with
         * anything already in `parentPath`. The desktop walks the parent's
         * tree cache first (cheap, in-memory) and falls back to the source
         * dir when pasting back into the same place — both produce a name
         * the user can recognise.
         *
         *   "aaa.txt"     → "aaa-copy.txt", "aaa-copy 2.txt", …
         *   "bbbb"        → "bbbb-copy",    "bbbb-copy 2",    …
         *   ".gitignore"  → ".gitignore-copy"     (the leading dot is preserved as the stem)
         *
         * The trailing suffix starts at `-copy` (not `-copy 2`) so the
         * first paste looks visually distinct from the source — that is
         * what makes the action feel like an actual copy, addressing the
         * "looks like nothing was copied" UX bug.
         */
        const buildCopyName = (existingNames: Set<string>): string => {
            const dotIdx = name.lastIndexOf(".");
            // A leading dot (e.g. ".gitignore") is the stem, not an extension.
            const hasExtension = dotIdx > 0;
            const stem = hasExtension ? name.slice(0, dotIdx) : name;
            const ext = hasExtension ? name.slice(dotIdx) : "";
            const baseCandidate = `${stem}-copy${ext}`;
            if (!existingNames.has(baseCandidate)) return baseCandidate;
            for (let i = 2; i < 1000; i++) {
                const candidate = `${stem}-copy ${i}${ext}`;
                if (!existingNames.has(candidate)) return candidate;
            }
            // Pathological fallback — give up and let the server 400.
            return baseCandidate;
        };

        // Collect existing sibling names from the tree cache so we don't
        // race the server's "Destination already exists" 400.
        const siblingNode = useFileTreeStore.getState().getNode(
            treeKey(selectedAgentId, currentWorkspaceId, parentPath),
        );
        const siblingNames = new Set<string>(
            isReadyNode(siblingNode) ? siblingNode.entries.map((e: TreeEntry) => e.name) : [],
        );
        const uniqueName = buildCopyName(siblingNames);
        const dest = parentPath ? `${parentPath}/${uniqueName}` : uniqueName;

        const ok = await copyItem(selectedAgentId, currentWorkspaceId, entry.path, dest);
        setCopiedEntry(null); // clear clipboard after paste (one-shot)
        if (ok) {
            // Force-refresh parent BEFORE requesting rename so the new
            // node is mounted in `flatNodes` — `FileTreeNode` with
            // `relPath === dest` must exist for the rename input to
            // render. Bypass SWR for the same reason as quickCreate:
            // a stale pre-copy cache would leave the rename input
            // targeting a node that's not in the local tree.
            // If the refresh fails (resolves to `kind:"error"`), the
            // node isn't in `flatNodes` — skip rename mode and tell the
            // user the copy succeeded but the UI can't show it yet.
            const refreshed = await refreshTree(selectedAgentId, currentWorkspaceId, parentPath || "");
            if (refreshed.kind === "error") {
                addToast({
                    type: "error",
                    message: `Pasted as ${uniqueName}, but the tree refresh failed — please retry or use the Refresh button.`,
                });
                return;
            }
            requestRenameFor(dest, uniqueName);
        } else {
            // Surface the silent failure so users know why nothing
            // appeared. The store already logged the underlying error;
            // here we just give a recoverable hint.
            log.warn(
                "[WorkspaceExplorer] paste failed; clipboard cleared. source=%s dest=%s",
                entry.path,
                dest,
            );
            addToast({ type: "error", message: `Paste failed: ${name}` });
        }
    }, [selectedAgentId, currentWorkspaceId, copyItem, refreshTree, setCopiedEntry, requestRenameFor, addToast]);

    if (!selectedAgent?.alive) {
        return (
            <div className="flex flex-1 flex-col items-center justify-center gap-2 bg-right-panel p-6 text-xs text-text-tertiary ">
                <FolderOpen className="h-6 w-6" />
                <span>{t("workspace.explorer.agentNotRunning")}</span>
            </div>
        );
    }

    return (
        <div className="flex min-h-0 flex-1 flex-col overflow-hidden bg-right-panel">
            {/* Workspace selector + action buttons */}
            <div className="flex items-center gap-0.5 border-b border-border-divider px-1.5 py-1.5">
                {/* On the Chat surface the panel mirrors the Chat session's
                    own workspace — the selector edits that config, exactly
                    like the composer chip, and never a session workspace. */}
                <WorkspaceSelector
                    dropDirection="down"
                    {...(chatSurface && selectedAgentId
                        ? {
                            value: currentWorkspaceId,
                            onChange: (id: string) =>
                                void useChatSessionStore
                                    .getState()
                                    .saveDraft(selectedAgentId, { ...(chatDraft ?? {}), workspace: id }),
                        }
                        : {})}
                />
                <div className="ml-auto flex items-center gap-1 pr-1 shrink-0">
                    {/* Toolbar action buttons. The class string + Tooltip
                        wrap mirror SessionTabBar's session-history button
                        (and the git banner's history/refresh buttons) so
                        the three toolbar strips at the top, right, and
                        bottom of the workspace panel read as a family.
                        The previous per-icon accent / yellow hover colors
                        were dropped here — visual differentiation was
                        costing family parity for negligible gain. */}
                    <div className="relative inline-flex">
                        <Tooltip content={t("workspace.newFile")} variant="plain">
                            <button
                                onClick={handleNewFile}
                                className="inline-flex items-center justify-center rounded h-6 w-6 transition-colors text-text-tertiary hover:bg-[var(--color-accent)]/10 hover:text-text-tertiary dark:hover:bg-[var(--color-accent)]/15"
                            >
                                <FilePlus className="h-3.5 w-3.5" />
                            </button>
                        </Tooltip>
                    </div>
                    <div className="relative inline-flex">
                        <Tooltip content={t("workspace.newFolder")} variant="plain">
                            <button
                                onClick={handleNewFolder}
                                className="inline-flex items-center justify-center rounded h-6 w-6 transition-colors text-text-tertiary hover:bg-[var(--color-accent)]/10 hover:text-text-tertiary dark:hover:bg-[var(--color-accent)]/15"
                            >
                                <FolderPlus className="h-3.5 w-3.5" />
                            </button>
                        </Tooltip>
                    </div>
                    <div className="relative inline-flex">
                        <Tooltip content={t("workspace.refresh")} variant="plain">
                            <button
                                onClick={handleRefresh}
                                className="inline-flex items-center justify-center rounded h-6 w-6 transition-colors text-text-tertiary hover:bg-[var(--color-accent)]/10 hover:text-text-tertiary dark:hover:bg-[var(--color-accent)]/15"
                            >
                                <RefreshCw className="h-3.5 w-3.5" />
                            </button>
                        </Tooltip>
                    </div>
                </div>
            </div>

            {/* ADR-078 decision 6 — the git strip sits BETWEEN the workspace
                selector row and the file-search row, not at the bottom of
                the panel: the bottom strip read as a status bar of the
                whole right panel, and it pushed the file tree's own
                scroll area to the bottom edge. Here the strip is one more
                header strip in the panel's header stack, and the search
                row + file tree simply flow below it — an expanded list
                pushes them down instead of squeezing them.
                Collapsed by default (the store's `expandedKey` starts
                null); expanded, the strip is capped at half the panel
                height so a long change list cannot swallow the tree.
                Hidden when the workspace is not a git repository
                (`isRepo === false`) — the strip has nothing to say there. */}
            {gitContext && (
                /* `shrink-0` + `max-h-1/2`: the strip takes its natural
                   height and never gives ground to the file tree below it;
                   when expanded, the change list is capped at half the
                   panel height and scrolls inside it. */
                <div className="flex min-h-0 shrink-0 flex-col max-h-1/2">
                    <GitStatusBar
                        agentId={gitContext.agentId}
                        workspaceId={gitContext.workspaceId}
                    />
                    {gitExpanded && (
                        <GitStatusPanel
                            agentId={gitContext.agentId}
                            workspaceId={gitContext.workspaceId}
                        />
                    )}
                </div>
            )}

            {/* Search box with file-search dropdown (Ctrl+P-style).
                Vertical padding (`py-1.5`) and min-h (`2.5rem`) are kept in
                sync with the workspace-selector toolbar row above so the
                two header strips read as the same height — otherwise the
                selector row visually dwarfs the search row (~40px vs ~28px). */}
            <div className="relative border-b border-border-divider">
                <div className="flex items-center gap-1.5 px-3 py-1.5 min-h-[2.5rem]">
                    <Search className="h-4 w-4 shrink-0 text-text-tertiary" strokeWidth={1.5} />
                    <input
                        ref={searchInputRef}
                        type="text"
                        value={searchQuery}
                        onChange={(e) => setSearchQuery(e.target.value)}
                        onFocus={() => setSearchFocused(true)}
                        onBlur={() => setTimeout(() => setSearchFocused(false), 150)}
                        onKeyDown={handleSearchKeyDown}
                        placeholder={
                            searchMode === "content"
                                ? t("workspace.explorer.searchContentPlaceholder")
                                : t("workspace.explorer.searchPlaceholder")
                        }
                        className="min-w-0 flex-1 bg-transparent text-xs text-text-secondary outline-none placeholder:text-text-tertiary  dark:placeholder:text-text-tertiary"
                    />
                    {/* The row's four icon controls — the mode radio pair
                        and the two ripgrep flags — live in ONE cluster, one
                        pixel apart.

                        They were two groups before (a radiogroup and a
                        flag pair), and the seam between them showed up as
                        an uneven gap: the icons inside each group were
                        tight, the gap across the seam was the row's own
                        `gap-1.5`. Four boxes of the same size at the same
                        distance is the whole fix — the glyphs differ in
                        ink (a file outline is lighter than three text
                        lines), but the boxes they sit in do not.

                        `gap-px`, not `gap-1`: at 6px apart four separate
                        buttons read as four separate buttons. Touching, they
                        read as ONE control that happens to be split — which
                        is what they are: a mode and two modifiers on the
                        same search. The selected chip then reads as a
                        segment of that control.

                        The mode pair is a radio pair, not two filters —
                        "where is this file" and "where is this text" are
                        different questions with different endpoints.
                        `onMouseDown` preventDefault keeps focus in the box
                        so switching mode does not blur the row and close
                        the dropdown. */}
                    <div className="flex shrink-0 items-center gap-px">
                        <div
                            role="radiogroup"
                            aria-label={t("workspace.explorer.searchMode")}
                            className="flex items-center gap-px"
                        >
                            {SEARCH_MODES.map(({ mode, Icon, labelKey }) => (
                                <button
                                    key={mode}
                                    type="button"
                                    role="radio"
                                    aria-checked={searchMode === mode}
                                    aria-label={t(labelKey)}
                                    title={t(labelKey)}
                                    onMouseDown={(e) => e.preventDefault()}
                                    onClick={() => {
                                        setSearchMode(mode);
                                        searchInputRef.current?.focus();
                                    }}
                                    className={searchToggleClass(searchMode === mode)}
                                >
                                    {/* `strokeWidth={1.5}` — one size and one
                                        weight for every icon in this row.
                                        Lucide's default 2 on a 16px glyph
                                        lands at ~1.3px of stroke and reads
                                        heavier than its neighbours; a
                                        thinner stroke on a smaller glyph
                                        made the pair shout instead. 16px
                                        at 1.5 is ~1px — legible, quiet. */}
                                    <Icon className="h-4 w-4 shrink-0" strokeWidth={1.5} />
                                </button>
                            ))}
                        </div>
                        {/* Ripgrep flags (`/workspaces/search`'s case /
                            whole-word switches). They are DISABLED in file
                            mode, not hidden: a fuzzy file-name search has
                            no case/word boundary to toggle, so a
                            live-looking switch would be lying. Greyed out
                            says "not applicable here" without the row
                            changing shape when the mode flips. */}
                        {[
                            {
                                Icon: CaseSensitive,
                                labelKey: "workspace.explorer.matchCase",
                                on: caseSensitive,
                                set: setCaseSensitive,
                            },
                            {
                                Icon: WholeWord,
                                labelKey: "workspace.explorer.matchWholeWord",
                                on: wholeWord,
                                set: setWholeWord,
                            },
                        ].map(({ Icon, labelKey, on, set }) => (
                            <button
                                key={labelKey}
                                type="button"
                                aria-pressed={on}
                                aria-label={t(labelKey)}
                                title={t(labelKey)}
                                disabled={searchMode !== "content"}
                                onMouseDown={(e) => e.preventDefault()}
                                onClick={() => set(!on)}
                                className={searchToggleClass(on, searchMode !== "content")}
                            >
                                <Icon className="h-4 w-4 shrink-0" strokeWidth={1.5} />
                            </button>
                        ))}
                    </div>
                    {searchQuery && (
                        <button
                            onClick={() => {
                                setSearchQuery("");
                                searchInputRef.current?.focus();
                            }}
                            className="text-10 text-text-tertiary hover:text-text-tertiary"
                            title={t("workspace.ariaLabelClearSearch")}
                        >
                            <X className="h-4 w-4" strokeWidth={1.5} />
                        </button>
                    )}
                </div>

                {/* Dropdown results */}
                {showDropdown && (
                    <div className="absolute left-0 right-0 top-full z-50 mt-1 rounded-lg border border-border-outer bg-modal-surface shadow-lg">
                        {/* Header with count */}
                        <div className="flex items-center justify-between border-b border-border-divider px-3 py-1.5 text-11 text-text-tertiary">
                            {searchLoading ? (
                                <span>Searching…</span>
                            ) : matchingRows.length > 0 ? (
                                <span>
                                    {matchingRows.length}{" "}
                                    {searchMode === "content"
                                        ? `match${matchingRows.length === 1 ? "" : "es"}`
                                        : `matching file${matchingRows.length === 1 ? "" : "s"}`}
                                </span>
                            ) : (
                                <span>{noResultsLabel}</span>
                            )}
                        </div>

                        {/* Items */}
                        <div className="max-h-80 overflow-y-auto py-1">
                            {matchingRows.length === 0 ? (
                                <div className="px-3 py-4 text-center text-xs text-text-tertiary ">
                                    {searchLoading ? "Searching workspace…" : noResultsLabel}
                                </div>
                            ) : (
                                matchingRows.map((f, idx) => {
                                    const focused = idx === focusedIdx;
                                    const addDisabled = searchAddToChatDisabledReason !== null;
                                    return (
                                        <div
                                            key={`${f.relPath}:${f.line ?? 0}`}
                                            onMouseDown={(e) => e.preventDefault()}
                                            onMouseEnter={() => setFocusedIdx(idx)}
                                            onClick={() => handleSearchSelect(f.relPath)}
                                            // Names clip on very narrow panes, so the
                                            // full path stays reachable on hover.
                                            title={f.relPath}
                                            className={cn(
                                                // Three-column grid:
                                                //   col 1 — icon (auto)
                                                //   col 2 — name + dir (minmax(0, 1fr);
                                                //           overflow-hidden is on the
                                                //           inner wrapper below, so only
                                                //           the text region clips)
                                                //   col 3 — "add to chat" button (auto)
                                                // Why grid instead of flex row: a flat
                                                // flex row with `ml-auto` on the button
                                                // depends on free space in the row to pin
                                                // the button to the trailing edge. With
                                                // `shrink-0` on the name, the row would
                                                // push wider than the pane on long names,
                                                // the dir span would absorb the overflow
                                                // down to zero, and `ml-auto` would lose
                                                // its free space — the button rides out
                                                // past the dropdown's right edge and only
                                                // reappears once the pane is dragged
                                                // wide enough. Putting the button in its
                                                // own auto column decouples it from that
                                                // race: it always sits in column 3,
                                                // regardless of name length. The text
                                                // column is the only thing that gets
                                                // squeezed, and within it `shrink-0`
                                                // on the name pins it at content width
                                                // while the dir absorbs the rest. The
                                                // full path is still recoverable from
                                                // `title={f.relPath}` even when the
                                                // name itself clips.
                                                "group grid items-center gap-2 overflow-hidden px-3 py-1.5 transition-colors cursor-pointer",
                                                "grid-cols-[auto_minmax(0,1fr)_auto]",
                                                focused
                                                    ? `${listRowSelected} text-text `
                                                    : listRowHover,
                                            )}
                                        >
                                            <div className="h-3.5 w-3.5 shrink-0 flex items-center justify-center">
                                                <SetiIcon {...getFileIcon(f.name)} size={14} />
                                            </div>
                                            <div className="flex min-w-0 flex-col overflow-hidden">
                                                <div className="flex items-center min-w-0 overflow-hidden">
                                                    <span className="shrink-0 whitespace-nowrap text-xs text-text-secondary ">
                                                        {f.name}
                                                    </span>
                                                    {/* Content hits carry their location:
                                                        the same file can match on many
                                                        lines, so without the number two
                                                        rows for one file read as duplicates. */}
                                                    {f.line !== undefined && (
                                                        <span className="ml-1 shrink-0 text-10 text-text-tertiary">
                                                            :{f.line}
                                                        </span>
                                                    )}
                                                    <span className="ml-3 min-w-0 truncate text-10 text-text-tertiary ">
                                                        {abbreviatePath(f.dir)}
                                                    </span>
                                                </div>
                                                {/* Content mode: the matched source line. */}
                                                {f.text !== undefined && (
                                                    <span className="truncate text-10 text-text-tertiary">
                                                        {f.text.trim()}
                                                    </span>
                                                )}
                                            </div>
                                            {/* Hover/focus-reveal "add to chat" button.
                                                Mirrors the floating button on the editor
                                                selection (handleTabAddToChat) so the two
                                                entry points feel like one feature.
                                                `e.preventDefault()` on mousedown keeps
                                                focus on the search input (matches the
                                                outer row); `e.stopPropagation()` on click
                                                prevents the row's onClick from also
                                                opening the file. `ml-auto` is no longer
                                                needed — the grid places the button in
                                                column 3 regardless of how much content
                                                the text column holds. */}
                                            <Tooltip
                                                content={
                                                    addDisabled
                                                        ? t(`workspace.search.addToChatDisabled.${searchAddToChatDisabledReason}`)
                                                        : `${t("workspace.contextMenu.addToChat")} (${t("workspace.search.addToChatShortcut")})`
                                                }
                                                variant="plain"
                                            >
                                                <button
                                                    type="button"
                                                    aria-label={t("workspace.contextMenu.addToChat")}
                                                    disabled={addDisabled}
                                                    onMouseDown={(e) => e.preventDefault()}
                                                    onClick={(e) => {
                                                        e.stopPropagation();
                                                        handleSearchAddToChat(f);
                                                    }}
                                                    className={cn(
                                                        "flex h-5 w-5 shrink-0 items-center justify-center rounded text-text-secondary",
                                                        "opacity-0 transition-opacity hover:bg-[var(--color-accent)]/10 hover:text-text",
                                                        // Reveal on hover or when the row is
                                                        // keyboard-focused (ArrowUp/Down
                                                        // drives `focused`).
                                                        "group-hover:opacity-100",
                                                        focused && "opacity-100",
                                                        addDisabled && "cursor-not-allowed",
                                                    )}
                                                >
                                                    <MessageSquarePlus className="h-3.5 w-3.5" />
                                                </button>
                                            </Tooltip>
                                        </div>
                                    );
                                })
                            )}
                        </div>
                    </div>
                )}
            </div>

            {/* File tree (normal mode, no search filtering). Rendered even
                without an execution session — `currentWorkspaceId` falls
                back to the Chat draft / agent home, so closing the last
                exec tab no longer blanks the panel. */}
            {selectedAgentId && (
                <FileTree
                    // View identity, not just the font size (see
                    // `fileTreeViewKey`): the tree's virtualizer offset belongs
                    // to one (agent, workspace, session), and reusing the
                    // instance across a switch carried the previous tree's
                    // offset into the next one.
                    key={fileTreeViewKey({
                        agentId: selectedAgentId,
                        workspaceId: currentWorkspaceId,
                        sessionId: activeSessionId,
                        fontSize,
                    })}
                    agentId={selectedAgentId}
                    workspaceId={currentWorkspaceId}
                    sessionId={activeSessionId}
                    selectedPath={selectedEntry?.path ?? null}
                    onSelectPath={handleSelectPath}
                    onFileDoubleClick={handleFileDoubleClick}
                    onContextNewItem={handleContextNewItem}
                    onDelete={handleDelete}
                    onCopy={handleCopy}
                    onPaste={handlePaste}
                    onRename={handleRename}
                    onReveal={handleReveal}
                    renameTarget={renameTarget}
                    renameInitialValue={renameInitialValue}
                    onCancelRename={cancelExternalRename}
                    onRequestRename={handleRequestRename}
                    /* DnD wiring - pointer-event based, no HTML5 DnD.
                     * draggingRelPath + dropTarget are visual-only state;
                     * all drag logic lives in the global pointermove/
                     * pointerup listeners above. */
                    draggingRelPath={draggingRelPath}
                    dropTarget={dropTarget}
                    onPointerDownTreeEntry={onPointerDownTreeEntry}
                />
            )}

        </div>
    );
}
