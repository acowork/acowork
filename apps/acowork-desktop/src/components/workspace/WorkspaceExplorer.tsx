import { useState, useCallback, useRef, useEffect } from "react";
import { Search, RefreshCw, FolderOpen, FilePlus, FolderPlus, X } from "lucide-react";
import { useAgentStore } from "../../stores/agentStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import { useFileTreeStore, treeKey, isReadyNode, type TreeEntry } from "../../stores/fileTree";
import { useChatStore } from "../../stores/chatStore";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { useLayoutStore } from "../../stores/layoutStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { FileTree } from "./FileTree/FileTree";
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
    const currentWorkspaceId = activeSessionId
        ? (sessionWorkspaceMap[activeSessionId] ?? "__agent_home__")
        : "__agent_home__";

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
    const [searchFocused, setSearchFocused] = useState(false);
    const [focusedIdx, setFocusedIdx] = useState(0);
    const [searchLoading, setSearchLoading] = useState(false);
    const [searchResults, setSearchResults] = useState<
        Array<{ name: string; relPath: string; dir: string; score: number }>
    >([]);
    const searchInputRef = useRef<HTMLInputElement>(null);

    // Server-side filename search — one request per debounced query.
    // Previous in-flight requests are cancelled via AbortController so
    // the latest keystroke always wins, even on a slow network.
    useEffect(() => {
        if (!searchQuery.trim() || !selectedAgentId) {
            setSearchResults([]);
            setSearchLoading(false);
            return;
        }
        const controller = new AbortController();
        setSearchLoading(true);
        const timer = setTimeout(() => {
            void useWorkspaceStore
                .getState()
                .findFiles(
                    selectedAgentId,
                    currentWorkspaceId,
                    searchQuery,
                    50,
                    controller.signal,
                )
                .then((resp) => {
                    if (controller.signal.aborted) return;
                    if (!resp) {
                        setSearchResults([]);
                        setSearchLoading(false);
                        return;
                    }
                    const rows = resp.matches.map((m) => {
                        const slash = m.relPath.lastIndexOf("/");
                        return {
                            name: m.name,
                            relPath: m.relPath,
                            dir: slash >= 0 ? m.relPath.slice(0, slash) : "",
                            score: m.score,
                        };
                    });
                    setSearchResults(rows);
                    setSearchLoading(false);
                });
        }, 150);
        return () => {
            clearTimeout(timer);
            controller.abort();
        };
    }, [searchQuery, selectedAgentId, currentWorkspaceId]);

    const matchingFiles = searchQuery ? searchResults : [];

    // Clamp focused index when the result list changes
    useEffect(() => {
        setFocusedIdx((i) => (i >= matchingFiles.length ? 0 : i));
    }, [searchQuery, matchingFiles.length]);

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

    const handleSearchKeyDown = useCallback((e: React.KeyboardEvent<HTMLInputElement>) => {
        if (e.key === "Escape") {
            e.preventDefault();
            setSearchQuery("");
            setSearchFocused(false);
            searchInputRef.current?.blur();
        } else if (e.key === "ArrowDown") {
            e.preventDefault();
            setFocusedIdx((i) => Math.min(i + 1, matchingFiles.length - 1));
        } else if (e.key === "ArrowUp") {
            e.preventDefault();
            setFocusedIdx((i) => Math.max(i - 1, 0));
        } else if (e.key === "Enter") {
            e.preventDefault();
            const item = matchingFiles[focusedIdx];
            if (item) handleSearchSelect(item.relPath);
        }
    }, [matchingFiles, focusedIdx, handleSearchSelect]);

    const showDropdown = searchFocused && searchQuery.trim().length > 0;

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
                <WorkspaceSelector dropDirection="down" />
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
                                className="inline-flex items-center justify-center rounded h-6 w-6 transition-colors text-text-tertiary hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300"
                            >
                                <FilePlus className="h-3.5 w-3.5" />
                            </button>
                        </Tooltip>
                    </div>
                    <div className="relative inline-flex">
                        <Tooltip content={t("workspace.newFolder")} variant="plain">
                            <button
                                onClick={handleNewFolder}
                                className="inline-flex items-center justify-center rounded h-6 w-6 transition-colors text-text-tertiary hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300"
                            >
                                <FolderPlus className="h-3.5 w-3.5" />
                            </button>
                        </Tooltip>
                    </div>
                    <div className="relative inline-flex">
                        <Tooltip content={t("workspace.refresh")} variant="plain">
                            <button
                                onClick={handleRefresh}
                                className="inline-flex items-center justify-center rounded h-6 w-6 transition-colors text-text-tertiary hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300"
                            >
                                <RefreshCw className="h-3.5 w-3.5" />
                            </button>
                        </Tooltip>
                    </div>
                </div>
            </div>

            {/* Search box with file-search dropdown (Ctrl+P-style).
                Vertical padding (`py-1.5`) and min-h (`2.5rem`) are kept in
                sync with the workspace-selector toolbar row above so the
                two header strips read as the same height — otherwise the
                selector row visually dwarfs the search row (~40px vs ~28px). */}
            <div className="relative border-b border-border-divider">
                <div className="flex items-center gap-1.5 px-3 py-1.5 min-h-[2.5rem]">
                    <Search className="h-3 w-3 shrink-0 text-text-tertiary" />
                    <input
                        ref={searchInputRef}
                        type="text"
                        value={searchQuery}
                        onChange={(e) => setSearchQuery(e.target.value)}
                        onFocus={() => setSearchFocused(true)}
                        onBlur={() => setTimeout(() => setSearchFocused(false), 150)}
                        onKeyDown={handleSearchKeyDown}
                        placeholder={t("workspace.explorer.searchPlaceholder")}
                        className="flex-1 bg-transparent text-xs text-text-secondary outline-none placeholder:text-text-tertiary  dark:placeholder:text-text-tertiary"
                    />
                    {searchQuery && (
                        <button
                            onClick={() => {
                                setSearchQuery("");
                                searchInputRef.current?.focus();
                            }}
                            className="text-10 text-text-tertiary hover:text-zinc-600 dark:hover:text-zinc-300"
                            title={t("workspace.ariaLabelClearSearch")}
                        >
                            <X className="h-3 w-3" />
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
                            ) : matchingFiles.length > 0 ? (
                                <span>{matchingFiles.length} matching file{matchingFiles.length === 1 ? "" : "s"}</span>
                            ) : (
                                <span>No matching files</span>
                            )}
                        </div>

                        {/* Items */}
                        <div className="max-h-80 overflow-y-auto py-1">
                            {matchingFiles.length === 0 ? (
                                <div className="px-3 py-4 text-center text-xs text-text-tertiary ">
                                    {searchLoading ? "Searching workspace…" : "No matching files"}
                                </div>
                            ) : (
                                matchingFiles.map((f, idx) => {
                                    const focused = idx === focusedIdx;
                                    return (
                                        <div
                                            key={f.relPath}
                                            onMouseDown={(e) => e.preventDefault()}
                                            onMouseEnter={() => setFocusedIdx(idx)}
                                            onClick={() => handleSearchSelect(f.relPath)}
                                            className={cn(
                                                "group flex items-center gap-2 px-3 py-1.5 transition-colors cursor-pointer",
                                                focused
                                                    ? `${listRowSelected} text-text `
                                                    : listRowHover,
                                            )}
                                        >
                                            <div className="h-3.5 w-3.5 shrink-0 flex items-center justify-center">
                                                <SetiIcon {...getFileIcon(f.name)} size={14} />
                                            </div>
                                            <span className="shrink-0 text-xs text-text-secondary ">
                                                {f.name}
                                            </span>
                                            <span className="min-w-0 truncate text-10 text-text-tertiary  ml-3">
                                                {abbreviatePath(f.dir)}
                                            </span>
                                        </div>
                                    );
                                })
                            )}
                        </div>
                    </div>
                )}
            </div>

            {/* File tree (normal mode, no search filtering) */}
            {selectedAgentId && activeSessionId && (
                <FileTree
                    key={fontSize}
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

            {/* ADR-078 decision 6 (2026-XX revision) — GitStatusBar lives at the
                bottom of the workspace panel. Source = currently selected
                session workspace (independent of any open editor file). Hidden
                when: agent not running (above early-return), workspace is
                __agent_home__ (no repo context), or this panel itself is not
                the active right-panel tab (parent RightPanel handles that).
                The wrapper reserves `pb-3` of empty panel-background below
                the git strip so neither the collapsed bar nor the expanded
                panel touches RightPanel's `rounded-xl` (12px) bottom corners
                — matches the chat input's `mb-3` buffer (ChatPanel.tsx
                L2467). FileTree (flex-1) is unaffected since the padding
                lives on the wrapper, not on the panel.
                `max-h-1/2` caps the strip at half the workspace area — up
                from the prior `max-h-[200px]` cap which surfaced too
                little on repos with many uncommitted files. The cap
                resolves against WorkspaceExplorer's definite flex-1
                height (parent chain: RightPanel clip → tab content). */}
            {gitContext && (
                <div className="flex min-h-0 flex-col pb-3 max-h-1/2">
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
        </div>
    );
}
