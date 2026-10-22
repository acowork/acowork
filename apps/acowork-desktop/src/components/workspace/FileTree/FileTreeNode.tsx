import { memo, useCallback, useState, useRef, useEffect, useMemo } from "react";
import { ChevronRight, FilePlus, FolderPlus, MessageSquarePlus, Trash2, Copy, ClipboardPaste, Eye, Check, Code, Pencil, ExternalLink } from "lucide-react";
import { cn } from "../../../lib/utils";
import { listRowHover } from "../../../lib/ui-styles";
import { getFileIcon } from "./fileIcons";
import { SetiIcon } from "../../common/SetiIcon";
import { useChatStore } from "../../../stores/chatStore";
import { useWorkspaceStore } from "../../../stores/workspaceStore";
import { useFileEditorStore } from "../../../stores/fileEditorStore";
import { useTranslation } from "../../../i18n/useTranslation";
import {
  ContextMenu,
  useContextMenu,
  type ContextMenuItem,
} from "../../common/ContextMenu";
import { isGatewayLocal } from "../../../lib/config";
import type { TreeEntry } from "../../../stores/fileTree";
import { useSessionReadOnly } from "../../../lib/session-write-access";
import { isWorkspacePreviewable } from "../../editor/filePreview";

// Lazy-load Tauri dialog to avoid import error in browser dev mode
let _dialogModule: typeof import("@tauri-apps/plugin-dialog") | null = null;
async function getTauriDialog() {
  if (!_dialogModule) {
    _dialogModule = await import("@tauri-apps/plugin-dialog");
  }
  return _dialogModule;
}

interface FileTreeNodeProps {
  entry: TreeEntry;
  depth: number;
  agentId: string;
  /** Execution session this tree belongs to; `null` in the Chat-surface
   *  fallback view (no exec session). Gates "Add to Chat" only — the node's
   *  workspace comes from `workspaceId`, never from the session map. */
  sessionId: string | null;
  /** Workspace this node lives in (the tree's own workspace). */
  workspaceId: string;
  relPath: string;
  absPath: string;
  isExpanded: boolean;
  isLoading: boolean;
  isSelected: boolean;
  /** True when at least one open editor tab lives under this directory */
  hasOpenDescendant?: boolean;
  onToggle: (relPath: string) => void;
  onSelect: (entry: TreeEntry, relPath: string) => void;
  onDoubleClick?: (entry: TreeEntry, relPath: string) => void;
  onContextNewItem?: (type: "file" | "dir", parentPath: string) => void;
  onDelete?: (relPath: string, isDir: boolean) => void;
  onCopy?: (relPath: string, isDir: boolean) => void;
  onPaste?: (parentPath: string) => void;
  /**
   * Rename the entry to `newName` (basename only — the parent path is
   * taken from the entry's current location). Resolves to `true` on
   * success, `false` on failure (caller surfaces an error toast).
   */
  onRename?: (relPath: string, newName: string, isDir: boolean) => Promise<boolean>;
  /**
   * Right-click "Reveal in File Explorer" — opens the OS file manager
   * (Finder / Explorer / xdg-open) with the entry selected. Only
   * rendered when `isGatewayLocal()` is true; in remote mode the
   * corresponding menu item is hidden because revealing a file on
   * the Gateway host has no value to the user. The store still
   * re-checks `isGatewayLocal()` as defence in depth, so a stale or
   * manipulated menu cannot trigger a cross-host spawn.
   */
  onReveal?: (relPath: string) => void | Promise<void>;
  /** When this matches the node's `relPath`, the node swaps its name
   * span for an inline rename input. Owned by the parent so the same
   * machinery drives right-click "New File / Folder / Paste" (which
   * create the entry then enter rename) and right-click "Rename" on
   * an existing entry. */
  renameTarget?: string | null;
  /** Initial value seeded into the rename input when this node enters
   * rename mode. Updated only on transition into the rename state —
   * subsequent keystrokes live in local component state. */
  renameInitialValue?: string;
  /** Called when the user presses Esc, blurs without changes, or commits
   * an empty/no-op name. Parent should clear `renameTarget`. */
  onCancelRename?: (relPath: string) => void;
  /** Right-click "Rename" on this node — parent decides the target +
   * initial value (typically `entry.name`). */
  onRequestRename?: (relPath: string, initialName: string) => void;
  /* ── Pointer-based Drag & Drop ────────────────────────────────── */
  /** RelPath of the entry currently being dragged (or `null`). Set
   * by FileTree from the parent. When it matches this node's
   * `relPath`, the row renders at reduced opacity
   * (`.file-tree-row-drag-source`). */
  draggingRelPath?: string | null;
  /** RelPath of the directory the cursor is hovering. When it matches
   * this node AND the node is a directory, the row gets the accent
   * background (`.file-tree-row-drop-target`). */
  dropTarget?: string | null;
  /** pointerdown on a row - records the drag candidate. The parent's
   * global pointermove/pointerup listeners handle the actual drag. */
  onPointerDownTreeEntry?: (relPath: string, isDir: boolean, e: React.PointerEvent) => void;
  /** Virtualizer slot geometry for THIS row — the row div doubles as
   * the slot (see FileTree.tsx). `slotSize` is the exact float
   * (`fontSize × 16 × 0.857 × 1.9`), not a rounded integer. */
  slotSize: number;
  slotStart: number;
  slotIndex: number;
}

export const FileTreeNode = memo(function FileTreeNode({
  entry,
  depth,
  agentId,
  sessionId,
  workspaceId,
  relPath,
  absPath,
  isExpanded,
  isLoading,
  isSelected,
  hasOpenDescendant,
  onToggle,
  onSelect,
  onDoubleClick,
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
  slotSize,
  slotStart,
  slotIndex,
}: FileTreeNodeProps) {
  const isDir = entry.type === "directory";
  const fileIcon = isDir ? null : getFileIcon(entry.name);
  const { t } = useTranslation();
  const openPreview = useFileEditorStore((s) => s.openPreview);
  // Previewable covers images (jpg/png/gif/webp), SVG, Markdown and HTML.
  // Single source of truth lives in `isWorkspacePreviewable` so the
  // right-click "Preview" item, the search picker and the double-click
  // handler all agree on what counts as a previewable file.
  const isPreviewable = !isDir && isWorkspacePreviewable(entry.name);

  // Context menu state — viewport-aware positioning via shared hook
  // (see src/hooks/useContextMenuPosition) so the menu flips above the
  // cursor and stays inside the viewport when right-clicked near the
  // bottom/right edge.
  const ctxMenu = useContextMenu();

  // Inline rename state — controlled by the parent via `renameTarget`.
  // When `renameTarget === relPath` the name span is replaced by an
  // `<input>` seeded with `renameInitialValue`. Local `inputValue` owns
  // the keystroke buffer; `wasRenamingRef` makes sure we only reseed on
  // the false→true transition so the user can keep typing without
  // losing what they already entered.
  const isRenaming = renameTarget === relPath;
  const [inputValue, setInputValue] = useState("");
  const renameInputRef = useRef<HTMLInputElement | null>(null);
  const wasRenamingRef = useRef(false);

  // Reseed + focus + select-stem only on the false→true transition.
  // Dependency array intentionally omits `inputValue` — we don't want
  // to reset the user's typing on every parent re-render.
  useEffect(() => {
    if (isRenaming && !wasRenamingRef.current) {
      const seed = renameInitialValue || entry.name;
      setInputValue(seed);
      // Wait for the input to mount before focusing / selecting.
      const frame = requestAnimationFrame(() => {
        const input = renameInputRef.current;
        if (!input) return;
        input.focus();
        const dotIdx = seed.lastIndexOf(".");
        // Leading-dot filenames like ".gitignore" use the whole name as stem.
        if (dotIdx > 0) {
          input.setSelectionRange(0, dotIdx);
        } else {
          input.select();
        }
      });
      wasRenamingRef.current = true;
      return () => cancelAnimationFrame(frame);
    }
    if (!isRenaming) {
      wasRenamingRef.current = false;
    }
  }, [isRenaming, renameInitialValue, entry.name]);

  /** Right-click "Rename" on this entry — defer to parent so the rename
   * target lives in a single place alongside the "New File / Folder /
   * Paste" flows. */
  const beginRename = useCallback(() => {
    onRequestRename?.(relPath, entry.name);
  }, [onRequestRename, relPath, entry.name]);

  const handleCommitRename = useCallback(() => {
    if (!isRenaming) return; // already closed by parent
    const trimmed = inputValue.trim();
    // Empty / unchanged → just close (no server roundtrip).
    if (!trimmed || trimmed === entry.name) {
      onCancelRename?.(relPath);
      return;
    }
    if (!onRename) {
      onCancelRename?.(relPath);
      return;
    }
    // Fire-and-forget: the parent handles the async `rename_item` call
    // and closes the rename input via `onCancelRename` once it has a
    // result (success or toast-on-failure both close immediately).
    onRename(relPath, trimmed, isDir);
  }, [isRenaming, inputValue, entry.name, onRename, onCancelRename, relPath, isDir]);

  const handleCancelRename = useCallback(() => {
    if (!isRenaming) return;
    onCancelRename?.(relPath);
  }, [isRenaming, onCancelRename, relPath]);

  const handleRenameKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLInputElement>) => {
      if (e.key === "Enter") {
        e.preventDefault();
        handleCommitRename();
      } else if (e.key === "Escape") {
        e.preventDefault();
        handleCancelRename();
      }
    },
    [handleCommitRename, handleCancelRename],
  );

  const handleRenameBlur = useCallback(() => {
    handleCommitRename();
  }, [handleCommitRename]);

  const addAttachedContext = useChatStore((s) => s.addAttachedContext);
  // ADR-076: per-session, and this node renders inside the tree for one
  // specific session — so gate on that session, not the active one.
  const readOnlySession = useSessionReadOnly(agentId, sessionId);

  const handleClick = useCallback(() => {
    if (isDir) {
      // Directories: select (for toolbar "create in selected dir" target)
      // AND toggle expansion in one click. Matches VS Code behavior
      // where clicking a folder name selects it and toggles expansion
      // simultaneously — the chevron click is just a shortcut for the
      // same action.
      onSelect(entry, relPath);
      onToggle(relPath);
    } else {
      onSelect(entry, relPath);
    }
  }, [isDir, onToggle, onSelect, relPath, entry]);

  const handleDoubleClick = useCallback(() => {
    if (!isDir && onDoubleClick) {
      onDoubleClick(entry, relPath);
    }
  }, [isDir, onDoubleClick, entry, relPath]);

  const handleContextMenu = useCallback((e: React.MouseEvent) => {
    ctxMenu.openAt(e);
  }, [ctxMenu]);

  const handleNewFile = useCallback(() => {
    const parentPath = isDir ? relPath : relPath.substring(0, relPath.lastIndexOf("/"));
    onContextNewItem?.("file", parentPath);
  }, [isDir, relPath, onContextNewItem]);

  const handleNewFolder = useCallback(() => {
    const parentPath = isDir ? relPath : relPath.substring(0, relPath.lastIndexOf("/"));
    onContextNewItem?.("dir", parentPath);
  }, [isDir, relPath, onContextNewItem]);

  const handleAddToChat = useCallback(() => {
    if (!sessionId) return; // fallback view: no execution session to attach to
    addAttachedContext(agentId, sessionId, {
      id: `${agentId}:${relPath}`,
      type: isDir ? "directory" : "file",
      name: entry.name,
      absPath,
    });
  }, [agentId, sessionId, isDir, relPath, entry.name, absPath, addAttachedContext]);

  const handleDelete = useCallback(async () => {
    const label = isDir ? `directory "${entry.name}"` : `file "${entry.name}"`;
    let confirmed = false;
    try {
      const { ask } = await getTauriDialog();
      confirmed = await ask(`Delete ${label}?\n\nThis action cannot be undone.`, {
        title: "Confirm Delete",
        kind: "warning",
        okLabel: t("fileTree.deleteConfirmOk"),
        cancelLabel: t("fileTree.deleteConfirmCancel"),
      });
    } catch {
      // Fallback for non-Tauri environments (e.g. browser dev)
      confirmed = window.confirm(`Delete ${label}?\n\nThis action cannot be undone.`);
    }
    if (confirmed) {
      onDelete?.(relPath, isDir);
    }
  }, [isDir, relPath, entry.name, onDelete]);

  const handleCopy = useCallback(() => {
    onCopy?.(relPath, isDir);
  }, [isDir, relPath, onCopy]);

  const handlePaste = useCallback(() => {
    const parentPath = isDir ? relPath : relPath.substring(0, relPath.lastIndexOf("/"));
    onPaste?.(parentPath);
  }, [isDir, relPath, onPaste]);

  const handlePreview = useCallback(() => {
    void openPreview(agentId, workspaceId, relPath);
  }, [agentId, workspaceId, relPath, openPreview]);

  /** Right-click "Reveal in File Explorer" — local-mode only.
   * The corresponding menu item is hidden in remote mode (see the
   * menu JSX), so this handler should never fire then. We still
   * forward through `onReveal` rather than calling the store directly
   * so the parent's error-toast contract is preserved end-to-end. */
  const handleReveal = useCallback(() => {
    onReveal?.(relPath);
  }, [relPath, onReveal]);

  const handleTogglePromptFile = useCallback(() => {
    const state = useWorkspaceStore.getState();
    const workspace = state.workspaces.find((ws) => ws.id === workspaceId);
    const isActive = workspace?.prompt_file === entry.name;
    const newPromptFile = isActive ? null : entry.name;
    void state.setPromptFile(agentId, workspaceId, newPromptFile);
  }, [agentId, workspaceId, entry.name]);

  // Check if this file qualifies as a prompt file (CLAUDE.md / AGENTS.md)
  const isPromptFile = !isDir && /^(CLAUDE|AGENTS)\.md$/i.test(entry.name);
  const workspace = useWorkspaceStore((s) => s.workspaces.find((ws) => ws.id === workspaceId));
  const isActivePromptFile = workspace?.prompt_file === entry.name;

  // Memoised menu items. Rebuilt when the flags that gate item
  // visibility change (previewable / prompt-file / rename / reveal /
  // paste availability) OR when any handler identity changes (they
  // capture sessionId — e.g. handleAddToChat — so a stale closure here
  // would route "Add to Chat" to the previously active session). Keeps
  // the memoized FileTreeNode from churning the ContextMenu child on
  // unrelated re-renders.
  const ctxMenuItems = useMemo<ContextMenuItem[]>(() => {
    const items: ContextMenuItem[] = [];

    items.push({
      key: "add-to-chat",
      icon: <MessageSquarePlus size={14} />,
      label: t("workspace.contextMenu.addToChat"),
      // ADR-076: the attachment rides along with the next message, which a
      // viewer of someone else's session may not send. In the fallback view
      // (no execution session) there is nothing to attach to either.
      disabled: readOnlySession || !sessionId,
      onClick: handleAddToChat,
    });
    if (isPreviewable) {
      items.push({
        key: "preview",
        icon: <Eye size={14} />,
        label: t("workspace.contextMenu.preview"),
        onClick: handlePreview,
      });
    }
    if (isPromptFile) {
      items.push({
        key: "toggle-prompt-file",
        icon: isActivePromptFile ? (
          <Check size={14} style={{ color: "#22c55e" }} />
        ) : (
          <Code size={14} />
        ),
        label: isActivePromptFile ? t("workspace.contextMenu.cancelInjectContext") : t("workspace.contextMenu.injectContext"),
        onClick: handleTogglePromptFile,
      });
    }
    items.push({
      key: "new-file",
      icon: <FilePlus size={14} />,
      label: t("workspace.contextMenu.newFile"),
      dividerBefore: true,
      onClick: handleNewFile,
    });
    items.push({
      key: "new-folder",
      icon: <FolderPlus size={14} />,
      label: t("workspace.contextMenu.newFolder"),
      onClick: handleNewFolder,
    });
    items.push({
      key: "copy",
      icon: <Copy size={14} />,
      label: t("workspace.contextMenu.copy"),
      dividerBefore: true,
      onClick: handleCopy,
    });
    items.push({
      key: "paste",
      icon: <ClipboardPaste size={14} />,
      label: t("workspace.contextMenu.paste"),
      disabled: !useWorkspaceStore.getState().copiedEntry,
      onClick: handlePaste,
    });
    if (onRename) {
      items.push({
        key: "rename",
        icon: <Pencil size={14} />,
        label: t("workspace.contextMenu.rename"),
        onClick: beginRename,
      });
    }
    // "Reveal in File Explorer" — local-mode only.
    // In remote mode (Gateway on a different machine) opening the file
    // manager would reveal a folder the user can't see, so we hide the
    // menu item entirely. Matches VSCode's behaviour for remote
    // workspaces and keeps the menu clean. `isGatewayLocal()` is checked
    // again inside `revealItem` as defence in depth — see workspaceStore.ts.
    if (onReveal && isGatewayLocal()) {
      items.push({
        key: "reveal",
        icon: <ExternalLink size={14} />,
        label: t("workspace.contextMenu.reveal"),
        title: t("workspace.contextMenu.reveal"),
        onClick: handleReveal,
      });
    }
    items.push({
      key: "delete",
      icon: <Trash2 size={14} />,
      label: t("workspace.contextMenu.delete"),
      variant: "danger",
      onClick: handleDelete,
    });
    return items;
  }, [
    isPreviewable,
    isPromptFile,
    isActivePromptFile,
    onRename,
    onReveal,
    t,
    readOnlySession,
    handleAddToChat,
    handlePreview,
    handleTogglePromptFile,
    handleNewFile,
    handleNewFolder,
    handleCopy,
    handlePaste,
    beginRename,
    handleReveal,
    handleDelete,
  ]);

  // DnD visibility state — kept as locals because they're pure
  // derivations of parent-supplied props; recomputing inside JSX would
  // re-allocate className strings every render.
  const isDragSource = draggingRelPath === relPath;
  // Drop highlight: only directories can be drop targets, and the row
  // must currently be the active drop target per parent's state.
  // (Parent's `onDragOverTreeEntry` already rejected self/ancestor
  // drags and files, so the class application here is safe.)
  const isDropTarget = isDir && dropTarget === relPath;

  return (
    <>
      <div
        className={cn(
          "file-tree-row flex cursor-pointer items-center gap-1 py-[0.2em] pr-3 select-none",
          // Hover is the same accent tint every list row in the app uses;
          // the file tree lives on the same raised list surface (right-panel
          // / panel-block) as AgentList / ProjectSidebar / DocTreeSidebar,
          // so it takes the card token, not the inset one — otherwise the
          // cursor passing over a tree row reads as a different list family.
          !isSelected && listRowHover,
          // Selected highlight lives in globals.css (`.file-tree-row-selected`)
          // — see comment in styles/globals.css for why it can't be a Tailwind
          // utility on this row.
          isSelected && "file-tree-row-selected",
          isDragSource && "file-tree-row-drag-source",
          isDropTarget && "file-tree-row-drop-target",
        )}
        /* Row div doubles as the virtualizer slot (see FileTree.tsx for
         * why this is one div, not a wrapper + inner row). */
        style={{
          position: "absolute",
          top: 0,
          left: 0,
          minWidth: "100%",
          width: "fit-content",
          height: `${slotSize}px`,
          transform: `translateY(${slotStart}px)`,
          paddingLeft: `${depth * 16 + 8}px`,
          // No font-size here: `.file-tree-row` in globals.css owns the row
          // glyph size (12px at the default base) and is shared with
          // GitStatusPanel's rows, so the two lists cannot drift apart.
        }}
        /* DnD identity: `data-rel-path` is how the parent's global
         * pointermove/pointerup handlers find this row via
         * `elementFromPoint(...).closest('[data-rel-path]')`.
         * `onPointerDown` records the drag candidate; the parent
         * starts dragging after a 5px threshold. No HTML5 DnD. */
        data-rel-path={relPath}
        data-is-dir={String(isDir)}
        data-index={slotIndex}
        onPointerDown={(e) => {
          if (!onPointerDownTreeEntry) return;
          onPointerDownTreeEntry(relPath, isDir, e);
        }}
        onClick={handleClick}
        onDoubleClick={handleDoubleClick}
        onContextMenu={handleContextMenu}
        title={relPath}
      >
        {/* Icon — chevron for dirs, file-type for files; both occupy same 16px slot so names align */}
        <span className="flex shrink-0 items-center justify-center" style={{ height: "1.15em", width: "1.15em" }}>
          {isDir ? (
            <ChevronRight
              className={cn(
                "h-[0.8em] w-[0.8em] text-text-tertiary transition-transform duration-150",
                isExpanded && "rotate-90",
              )}
            />
          ) : fileIcon ? (
            <SetiIcon
              name={fileIcon.name}
              size={14}
            />
          ) : null}
        </span>

        {/* Name — no truncation; horizontal scrollbar on parent handles overflow.
            When `isRenaming`, swap the span for an inline input. */}
        {isRenaming ? (
          <input
            ref={renameInputRef}
            type="text"
            value={inputValue}
            onChange={(e) => setInputValue(e.target.value)}
            onKeyDown={handleRenameKeyDown}
            onBlur={handleRenameBlur}
            onClick={(e) => e.stopPropagation()}
            onDoubleClick={(e) => e.stopPropagation()}
            onContextMenu={(e) => e.stopPropagation()}
            className="min-w-0 flex-1 rounded-sm border border-[var(--color-accent)] bg-modal-surface px-1 text-text-secondary outline-none dark:bg-zinc-900 "
            // No font-size override: Preflight gives form controls
            // `font-size: 100%`, so the field takes the row's own size. It
            // replaces the label in place, and a 14px field inside a 12px row
            // would push the row past the virtualizer's slot height.
          />
        ) : (
          <span className="whitespace-nowrap text-text-tertiary ">{entry.name}</span>
        )}

        {/* Loading indicator for directories being fetched */}
        {isLoading && isDir && isExpanded && (
          <span className="ml-auto text-text-tertiary" style={{ fontSize: "calc(var(--ui-text-size) * 0.78)" }}>...</span>
        )}

        {/* Open-files dot indicator for directories (VS Code style) */}
        {isDir && hasOpenDescendant && (
          <span className="ml-auto h-1.5 w-1.5 shrink-0 rounded-full bg-[var(--color-accent)]" />
        )}
      </div>

      {/* Context menu — unified component. Renders to document.body to
          escape virtual-list transform containment (see ContextMenu.tsx). */}
      <ContextMenu
        isOpen={ctxMenu.isOpen}
        menuProps={ctxMenu.menuProps}
        items={ctxMenuItems}
        payload={undefined}
        selectionAtOpen={ctxMenu.selectionAtOpen}
        onClose={ctxMenu.close}
      />
    </>
  );
});