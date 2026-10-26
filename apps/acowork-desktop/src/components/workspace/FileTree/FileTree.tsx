import { useCallback, useEffect, useMemo, useRef } from "react";
import { useShallow } from "zustand/react/shallow";
import { FilePlus, FolderPlus, ClipboardPaste } from "lucide-react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { useWorkspaceStore } from "../../../stores/workspaceStore";
import { useFileTreeStore, treeKey, isReadyNode, isLoading, type TreeEntry, type TreeNode } from "../../../stores/fileTree";
import { useChatStore } from "../../../stores/chatStore";
import { useFileEditorStore } from "../../../stores/fileEditorStore";
import { useSettingsStore } from "../../../stores/settingsStore";
import { FileTreeNode } from "./FileTreeNode";
import { useTranslation } from "../../../i18n/useTranslation";
import {
    ContextMenu,
    useContextMenu,
    type ContextMenuItem,
} from "../../common/ContextMenu";
import { OverlayScrollbar } from "../../common/OverlayScrollbar";

const EMPTY_ARRAY: string[] = [];

/** Row glyph size as a ratio of `--ui-font-size` — the `--ui-text-size` step,
 * 12px at the default base. MUST stay in sync with `--ui-text-size` in
 * globals.css (which `.file-tree-row` applies to the row); the virtualizer
 * cannot read CSS, so the geometry is restated here. */
export const TREE_ROW_FONT_RATIO = 0.857;

/** Natural row height as a multiple of the row's own font-size:
 * `line-height 1.5 + 2 × py-[0.2em]` = 1.9. */
export const TREE_ROW_LINE_MULTIPLIER = 1.9;

/** Expansion-state bucket used when the tree has no execution session
 *  (Chat-surface fallback view, design 27 §4.5). A real session id can
 *  never collide with it (runtime ids are `s-…`/UUID shaped). */
export const NO_SESSION_TREE_KEY = "__no_session__";

/** Flattened tree node for virtualized rendering */
interface FlatNode {
    entry: TreeEntry;
    depth: number;
    relPath: string;
}

interface FileTreeProps {
    agentId: string;
    workspaceId: string;
    /** Execution session whose expansion state this tree reads/writes.
     *  `null` = no exec session (the panel is mirroring the Chat surface's
     *  draft workspace); expansion then lives under `NO_SESSION_TREE_KEY`
     *  and session-bound actions (Add to Chat) are disabled. */
    sessionId: string | null;
    /** Currently-selected entry path (controlled by parent). `null` = nothing selected. */
    selectedPath?: string | null;
    /** Notify parent of selection changes. `type` is `null` when `path` is `null` (deselect). */
    onSelectPath?: (path: string | null, type: "file" | "dir" | null) => void;
    onFileDoubleClick?: (entry: TreeEntry, relPath: string) => void;
    onContextNewItem?: (type: "file" | "dir", parentPath: string) => void;
    onDelete?: (relPath: string, isDir: boolean) => void;
    onCopy?: (relPath: string, isDir: boolean) => void;
    onPaste?: (parentPath: string) => void;
    onRename?: (relPath: string, newName: string, isDir: boolean) => Promise<boolean>;
    /**
     * Right-click "Reveal in File Explorer" — opens the OS file manager
     * with the entry selected. Local-mode only; the parent supplies a
     * handler that surfaces a toast on failure. The corresponding menu
     * item is hidden entirely when `isGatewayLocal()` is false (matches
     * VSCode's behaviour for remote workspaces — see
     * `FileTreeNode.handleReveal`).
     */
    onReveal?: (relPath: string) => void | Promise<void>;
    /** When this matches a node's `relPath`, that node renders an inline
     * rename input seeded with `renameInitialValue`. Owned by the parent
     * so the same machinery drives right-click "New File / New Folder /
     * Paste" (which create + enter rename) and right-click "Rename" on
     * an existing entry. */
    renameTarget?: string | null;
    renameInitialValue?: string;
    /** Notify the parent that the inline rename finished — either via
     * Enter (commit) or Esc / blur with no change (cancel). The parent
     * then clears `renameTarget`. */
    onCancelRename?: (relPath: string) => void;
    /** Right-click "Rename" on an existing entry — parent decides the
     * target + initial value (typically `entry.name`). */
    onRequestRename?: (relPath: string, initialName: string) => void;
    /** RelPath of the entry currently being dragged, or `null` when no
     * drag is in progress. The matching node renders at reduced opacity
     * (`.file-tree-row-drag-source`). Owned by the parent so the row
     * highlight + the drop target highlight are coordinated from one
     * place — clearing one clears the other. */
    draggingRelPath?: string | null;
    /** RelPath of the directory the cursor is hovering — that node's row
     * gets the accent background (`.file-tree-row-drop-target`). `null`
     * when no drag is in progress or the cursor is over an invalid
     * target. */
    dropTarget?: string | null;
    /** pointerdown on a row - records the drag candidate. The parent's
     * global pointermove/pointerup listeners handle the actual drag. */
    onPointerDownTreeEntry?: (relPath: string, isDir: boolean, e: React.PointerEvent) => void;
}

/**
 * Shape of the workspace-root clipboard — kept loose so this helper does
 * not depend on the full `WorkspaceState`. Mirrors `useWorkspaceStore`'s
 * `copiedEntry` field; the test file uses a minimal stub of the same shape.
 */
export interface RootContextCopiedEntry {
    agentId: string;
    workspaceId: string;
    path: string;
    type: "file" | "directory";
}

export interface RootContextMenuDeps {
    /** File / dir create handler (mirrors FileTreeProps.onContextNewItem). */
    onContextNewItem?: (type: "file" | "dir", parentPath: string) => void;
    /** Paste handler (mirrors FileTreeProps.onPaste). */
    onPaste?: (parentPath: string) => void | Promise<void>;
    /** Workspace-root clipboard entry — when non-null, "Paste" is enabled. */
    copiedEntry?: RootContextCopiedEntry | null;
}

/**
 * Build the workspace-root context menu items. Pure function — extracted
 * from `FileTree` so tests can drive it without rendering the whole
 * virtualised tree.
 *
 * Only operations meaningful at the root are exposed:
 *   - New File   — `parentPath = ""` means the workspace root
 *   - New Folder — `parentPath = ""` means the workspace root
 *   - Paste      — disabled when `copiedEntry` is null
 *
 * Single-entry ops (Copy / Rename / Delete / Reveal / Add to Chat /
 * Preview / Toggle Prompt File) are intentionally absent — they have
 * no meaningful target here.
 *
 * Exported for unit testing only. Not part of the public API.
 */
export function buildRootContextMenuItems(
    t: (key: string) => string,
    deps: RootContextMenuDeps,
): ContextMenuItem[] {
    const { onContextNewItem, onPaste, copiedEntry } = deps;
    return [
        {
            key: "new-file",
            icon: <FilePlus size={14} />,
            label: t("workspace.contextMenu.newFile"),
            onClick: () => {
                onContextNewItem?.("file", "");
            },
        },
        {
            key: "new-folder",
            icon: <FolderPlus size={14} />,
            label: t("workspace.contextMenu.newFolder"),
            onClick: () => {
                onContextNewItem?.("dir", "");
            },
        },
        {
            key: "paste",
            icon: <ClipboardPaste size={14} />,
            label: t("workspace.contextMenu.paste"),
            dividerBefore: true,
            disabled: !copiedEntry,
            onClick: () => {
                void onPaste?.("");
            },
        },
    ];
}

export function FileTree({
    agentId,
    workspaceId,
    sessionId,
    selectedPath = null,
    onSelectPath,
    onFileDoubleClick,
    onContextNewItem,
    onDelete,
    onCopy,
    onPaste,
    onRename,
    onReveal,
    renameTarget = null,
    renameInitialValue = "",
    onCancelRename,
    onRequestRename,
    draggingRelPath = null,
    dropTarget = null,
    onPointerDownTreeEntry,
}: FileTreeProps) {
    // Tree nodes are mirrored into a single Record<key, TreeNode>.
    // Subscribe to ONLY the current (agent, workspace) subtree so tree
    // transitions in other agents/workspaces don't re-render this
    // component. `useShallow` keeps the reference stable while the
    // filtered result is shallow-equal (unrelated updates produce the
    // same object), so effect deps below don't churn either.
    const treeNodes = useFileTreeStore(
        useShallow((s) => {
            const prefix = `${agentId}\u0000${workspaceId}\u0000`;
            const out: Record<string, TreeNode> = {};
            for (const k in s.nodes) {
                if (k.startsWith(prefix)) out[k] = s.nodes[k];
            }
            return out;
        }),
    );
    const fetchTree = useFileTreeStore((s) => s.fetch);
    const copiedEntry = useWorkspaceStore((s) => s.copiedEntry);
    const toggleTreeExpandedPath = useChatStore((s) => s.toggleTreeExpandedPath);
    const expandTreeToPath = useChatStore((s) => s.expandTreeToPath);
    const { t } = useTranslation();

    /** Right-click on the empty area below the rows (the scroller
     * padding / unscrolled region) — semantically equivalent to a
     * right-click on the workspace root, so we offer the same actions
     * that work at the root: New File / New Folder / Paste. Single-entry
     * ops (Copy / Rename / Delete / Reveal / Add to Chat / Preview /
     * Toggle Prompt File) are deliberately excluded — they have no
     * meaningful target here. */
    const ctxMenu = useContextMenu();

    /**
     * Workspace root path — taken from the cached root-path tree node.
     * Empty string until the root fetch resolves (UI shows Loading...).
     */
    const rootNode = treeNodes[treeKey(agentId, workspaceId, "")];
    const workspaceRoot = isReadyNode(rootNode) ? rootNode.root : "";

    // Expanded paths from the session — Zustand selector is reactive
    const expandedPathsArr = useChatStore((s) => {
        const ss = s.agentStates[agentId]?.sessionStates[sessionId ?? NO_SESSION_TREE_KEY];
        return ss?.treeExpandedPaths ?? EMPTY_ARRAY;
    });
    const expandedPaths = useMemo(() => new Set(expandedPathsArr), [expandedPathsArr]);

    // Compute set of directory paths that contain at least one open editor tab.
    // e.g. open file "src/components/Foo.tsx" → dirs: "src", "src/components"
    const openFiles = useFileEditorStore((s) => s.openFiles);
    const openFileDirSet = useMemo(() => {
        const dirs = new Set<string>();
        for (const f of openFiles) {
            const parts = f.relPath.split("/");
            for (let i = 0; i < parts.length - 1; i++) {
                dirs.add(parts.slice(0, i + 1).join("/"));
            }
        }
        return dirs;
    }, [openFiles]);

    // Selection is owned by WorkspaceExplorer now; nothing to reset locally.

    // Fetch root when agent or workspace changes. We deliberately do
    // NOT gate on `meta.lifecycle` (ADR-085; formerly `meta.ready`)
    // here — Bug B v3 fix. The lifecycle push arrives asynchronously to
    // Runtime HTTP readiness; gating on it caused the right pane to
    // flash "Loading…" forever when the user opened the workspace tab
    // during the first second after agent start. The fetcher in
    // `treeClient` now owns the 503 retry loop (see `lib/httpRetry.ts`)
    // so transient 503s recover transparently.
    //
    // Self-healing: the effect ALSO re-fetches when the root node
    // transitions back to `idle` (e.g. an in-flight fetch was aborted
    // by a concurrent `abortAll` from a re-select of the same agent).
    // Without this, the UI would sit on "Loading…" forever — the
    // `[agentId, workspaceId, fetchTree]` deps never change, so the
    // mount-time fetch was the ONLY chance to populate the tree.
    const rootNodeKind = rootNode?.kind;
    useEffect(() => {
        if (!agentId || !workspaceId) return;
        if (rootNodeKind && rootNodeKind !== "idle") return;
        // `undefined` (never fetched) and `idle` (aborted) both start
        // a foreground fetch. `loading` / `ready` / `stale` / `error`
        // are deliberately left alone: loading is already in flight,
        // ready/stale have data, and error must not hot-loop (the
        // error branch renders a manual retry button instead).
        fetchTree(agentId, workspaceId, "");
    }, [agentId, workspaceId, fetchTree, rootNodeKind]);

    // ── Locate-in-tree: expand ancestors, lazy-load, select, scroll ───
    // The FileEditorPanel's "locate" button publishes a request via
    // workspaceStore.requestLocate. We expand all ancestor directories
    // synchronously, kick off `fetchTree` for any not-yet-cached ancestor,
    // then poll flatNodes until the target node appears so we can select
    // and center-scroll it.
    const locateRequest = useWorkspaceStore((s) => s.locateRequest);
    const consumedLocateSeqRef = useRef<number>(-1);

    // Step 1+2: expand ancestors and pre-fetch (only runs once per request).
    useEffect(() => {
        if (!locateRequest) return;
        if (locateRequest.agentId !== agentId) return;
        if (locateRequest.workspaceId !== workspaceId) return;
        if (locateRequest.sessionId !== sessionId) return;
        if (consumedLocateSeqRef.current === locateRequest.seq) return;
        consumedLocateSeqRef.current = locateRequest.seq;

        const { relPath } = locateRequest;

        // Expand ancestor dirs (synchronous Zustand update).
        expandTreeToPath(agentId, sessionId ?? NO_SESSION_TREE_KEY, relPath);

        // Lazy-fetch each ancestor dir that isn't already cached so the
        // target node eventually appears in flatNodes.
        const parts = relPath.split("/");
        const ancestors: string[] = [];
        for (let i = 0; i < parts.length - 1; i++) {
            ancestors.push(parts.slice(0, i + 1).join("/"));
        }
        for (const p of ancestors) {
            if (!isReadyNode(treeNodes[treeKey(agentId, workspaceId, p)])) {
                void fetchTree(agentId, workspaceId, p);
            }
        }
    }, [locateRequest, agentId, workspaceId, sessionId, treeNodes, expandTreeToPath, fetchTree]);

    // Flatten the tree into a list respecting expanded state
    const flatNodes = useMemo<FlatNode[]>(() => {
        const result: FlatNode[] = [];

        function walk(relPath: string, depth: number) {
            const node = treeNodes[treeKey(agentId, workspaceId, relPath)];
            if (!isReadyNode(node)) return;

            for (const entry of node.entries) {
                const childRelPath = relPath ? `${relPath}/${entry.name}` : entry.name;

                result.push({ entry, depth, relPath: childRelPath });

                if (entry.type === "directory" && expandedPaths.has(childRelPath)) {
                    walk(childRelPath, depth + 1);
                }
            }
        }

        walk("", 0);
        return result;
    }, [agentId, workspaceId, treeNodes, expandedPaths]);

    const handleToggle = useCallback(
        (relPath: string) => {
            const isCurrentlyExpanded = expandedPaths.has(relPath);
            toggleTreeExpandedPath(agentId, sessionId ?? NO_SESSION_TREE_KEY, relPath);
            // Lazy-load children when expanding
            if (
                !isCurrentlyExpanded &&
                !isReadyNode(treeNodes[treeKey(agentId, workspaceId, relPath)])
            ) {
                fetchTree(agentId, workspaceId, relPath);
            }
        },
        [agentId, workspaceId, sessionId, expandedPaths, treeNodes, fetchTree, toggleTreeExpandedPath],
    );

    const handleSelect = useCallback(
        (entry: TreeEntry, relPath: string) => {
            onSelectPath?.(relPath, entry.type === "directory" ? "dir" : "file");
        },
        [onSelectPath],
    );

    /** Right-click handler attached to the scroller element. Reacts only
     * when the right-click landed OUTSIDE any row — the row's own
     * `onContextMenu` already calls `e.stopPropagation()` via the
     * shared `useContextMenu.openAt`, so in normal operation this
     * handler only fires for true empty-area clicks. We still defend
     * against the corner case (e.g. if a future refactor drops the
     * stopPropagation) by checking `closest('[data-rel-path]')` so
     * we never double-open the menu.
     *
     * `ctxMenu.openAt` does the preventDefault + stopPropagation itself,
     * so the browser's native context menu never appears here. */
    const handleScrollerContextMenu = useCallback(
        (e: React.MouseEvent<HTMLDivElement>) => {
            const target = e.target as HTMLElement | null;
            if (target?.closest("[data-rel-path]")) {
                // A row was clicked — that row's own handler owns this
                // gesture; don't open the root menu on top of it.
                return;
            }
            ctxMenu.openAt(e);
        },
        [ctxMenu],
    );

    /** Empty-area (workspace-root) menu items. Memoised so React.memo
     * on the wrapped `<ContextMenu>` consumer doesn't churn. `copiedEntry`
     * is subscribed above so the `Paste` disabled flag tracks the
     * clipboard contents in real time. */
    const rootCtxMenuItems = useMemo<ContextMenuItem[]>(
        () =>
            buildRootContextMenuItems(t, {
                onContextNewItem,
                onPaste,
                copiedEntry,
            }),
        [t, onContextNewItem, onPaste, copiedEntry],
    );

    // Virtual scrolling setup.
    //
    // Row height is derived from CSS geometry (`line-height × font-size +
    // vertical padding` = fontSize × 16 × TREE_ROW_FONT_RATIO ×
    // TREE_ROW_LINE_MULTIPLIER = 22.78px at the default base) and NOT rounded
    // — see the e29e5196 commit for the multiplier's derivation.
    //
    // Two constraints that fall out of the row geometry:
    //
    // 1. The row div IS the virtualizer slot (merged, no wrapper layer).
    //    Otherwise `elementFromPoint` lands on a wrapper without
    //    `data-rel-path`, and `closest('[data-rel-path]')` returns null
    //    in WorkspaceExplorer's DnD handler.
    //
    // 2. `rowHeight` must equal the row's natural CSS height exactly.
    //    With `Math.round(26.6) = 27` the slot extends 0.4px past the
    //    rendered row, leaving a sliver where `elementFromPoint` lands
    //    on the slot's outer edge — same null-`closest` failure mode.
    //    Changing the row's font-size in globals.css therefore means
    //    changing TREE_ROW_FONT_RATIO here.
    const scrollRef = useRef<HTMLDivElement | null>(null);
    const fontSize = useSettingsStore((s) => s.fontSize);
    const rowHeight = useMemo(
        () => fontSize * 16 * TREE_ROW_FONT_RATIO * TREE_ROW_LINE_MULTIPLIER,
        [fontSize],
    );
    const virtualizer = useVirtualizer({
        count: flatNodes.length,
        getScrollElement: () => scrollRef.current,
        estimateSize: () => rowHeight,
        overscan: 20,
    });

    // Step 3 of locate-in-tree: select the matched node and center-scroll it.
    // Re-runs whenever flatNodes changes, so once the lazy-loaded children
    // arrive in the cache we'll center-scroll automatically.
    useEffect(() => {
        if (!locateRequest) return;
        if (consumedLocateSeqRef.current !== locateRequest.seq) return;
        const idx = flatNodes.findIndex((n) => n.relPath === locateRequest.relPath);
        if (idx < 0) return;
        onSelectPath?.(locateRequest.relPath, "file");
        // Defer one frame so the virtualizer has updated totalSize for the
        // newly-loaded flatNodes length before we ask it to scroll.
        const frame = requestAnimationFrame(() => {
            virtualizer.scrollToIndex(idx, { align: "center" });
        });
        return () => cancelAnimationFrame(frame);
    }, [flatNodes, locateRequest, virtualizer]);

    // Empty state
    if (flatNodes.length === 0) {
        const rootNode = treeNodes[treeKey(agentId, workspaceId, "")];
        const rootEntries = isReadyNode(rootNode) ? rootNode.entries : undefined;
        if (!rootEntries) {
            // Distinguish failure from progress: an `error` node renders
            // the cause + a manual retry (auto-retry would hot-loop when
            // the Gateway/Runtime is genuinely down), while `loading` /
            // `idle` show the spinner. Previously every non-ready state
            // rendered "Loading…", so a failed fetch looked like a
            // permanent spinner with no way to recover.
            if (rootNode?.kind === "error") {
                const err = rootNode.error;
                const detail =
                    err.cause === "http"
                        ? `${err.status}${err.statusText ? ` ${err.statusText}` : ""}`
                        : err.cause === "network"
                          ? err.message
                          : t("workspace.treeLoadAborted");
                // A 403 is a denial, not a load failure: the tree endpoint
                // requires the `use` tier, so a view-only guest sees the
                // same wall as the workspace list (ADR-087).
                const denied = err.cause === "http" && err.status === 403;
                return (
                    <div
                        className="flex min-h-0 flex-1 flex-col items-center justify-center gap-2 py-8 text-text-tertiary"
                        style={{ fontSize: "var(--ui-text-size)" }}
                    >
                        {denied ? (
                            <>
                                <span className="text-text-secondary">{t("workspace.deniedTitle")}</span>
                                <span className="max-w-sm text-center text-10 leading-relaxed">
                                    {t("workspace.deniedHint")}
                                </span>
                            </>
                        ) : (
                            <span>{t("workspace.treeLoadFailed", { detail })}</span>
                        )}
                        <button
                            type="button"
                            className="rounded border border-zinc-300 px-3 py-1 text-xs hover:bg-zinc-100 dark:border-zinc-600 dark:hover:bg-zinc-800"
                            onClick={() => void fetchTree(agentId, workspaceId, "")}
                        >
                            {t("workspace.retry") ?? "Retry"}
                        </button>
                    </div>
                );
            }
            return (
                <div className="flex min-h-0 flex-1 items-center justify-center py-8 text-text-tertiary" style={{ fontSize: "var(--ui-text-size)" }}>
                    Loading...
                </div>
            );
        }
        if (rootEntries.length === 0) {
            return (
                <div className="flex min-h-0 flex-1 flex-col items-center justify-center py-8 text-text-tertiary" style={{ fontSize: "var(--ui-text-size)" }}>
                    <span>Empty workspace</span>
                </div>
            );
        }
    }

    return (
        <OverlayScrollbar
            viewportRef={scrollRef}
            dataTabScroll
            /* Long paths overflow the panel on the x-axis (`width: fit-content`
             * content wrapper) — the overlay x-thumb is the visible affordance,
             * since the native horizontal scrollbar cannot be re-enabled on just
             * that axis (Chrome 121+ ignores `::-webkit-scrollbar` while
             * `scrollbar-width`/`scrollbar-color` are set; globals.css). */
            showXThumb
            className="file-tree-scroller flex-1 min-h-0 overflow-x-auto"
            containerClassName="flex-1"
            /* Right-click on the empty area (padding / below-the-last-row
             * space) opens the workspace-root menu. */
            onContextMenu={handleScrollerContextMenu}
        >
            <div
                style={{
                    height: `${virtualizer.getTotalSize()}px`,
                    width: "fit-content",
                    minWidth: "100%",
                    position: "relative",
                }}
            >
                {virtualizer.getVirtualItems().map((virtualRow) => {
                    const node = flatNodes[virtualRow.index];
                    const nodeIsLoading = isLoading(treeNodes[treeKey(agentId, workspaceId, node.relPath)]);

                    // FileTreeNode IS the virtualizer slot (one div owns
                    // both the slot geometry and the row semantics —
                    // see the comment above the virtualizer setup).
                    return (
                        <FileTreeNode
                            key={node.relPath}
                            entry={node.entry}
                            depth={node.depth}
                            agentId={agentId}
                            workspaceId={workspaceId}
                            relPath={node.relPath}
                            absPath={workspaceRoot ? `${workspaceRoot}/${node.relPath}` : node.relPath}
                            isExpanded={expandedPaths.has(node.relPath)}
                            isLoading={nodeIsLoading}
                            isSelected={selectedPath === node.relPath}
                            hasOpenDescendant={openFileDirSet.has(node.relPath)}
                            onToggle={handleToggle}
                            onSelect={handleSelect}
                            onDoubleClick={onFileDoubleClick}
                            onContextNewItem={onContextNewItem}
                            onDelete={onDelete}
                            onCopy={onCopy}
                            onPaste={onPaste}
                            onRename={onRename}
                            onReveal={onReveal}
                            renameTarget={renameTarget}
                            renameInitialValue={renameInitialValue}
                            onCancelRename={onCancelRename}
                            onRequestRename={onRequestRename}
                            draggingRelPath={draggingRelPath}
                            dropTarget={dropTarget}
                            onPointerDownTreeEntry={onPointerDownTreeEntry}
                            slotSize={virtualRow.size}
                            slotStart={virtualRow.start}
                            slotIndex={virtualRow.index}
                        />
                    );
                })}
            </div>

            {/* Workspace-root context menu — same renderer as the per-row
                menu (ContextMenu) so we get portal-to-body + outside-click +
                Escape behaviour for free. Renders nothing when closed. */}
            <ContextMenu
                isOpen={ctxMenu.isOpen}
                menuProps={ctxMenu.menuProps}
                items={rootCtxMenuItems}
                payload={undefined}
                selectionAtOpen={ctxMenu.selectionAtOpen}
                onClose={ctxMenu.close}
            />
        </OverlayScrollbar>
    );
}
