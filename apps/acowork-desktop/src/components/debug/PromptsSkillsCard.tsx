// Prompts & Skills card (Debug tab).
//
// Browses the two editable subtrees of the agent package (`prompts/` and
// `skills/`) through the restricted `__agent_package__` workspace API — the
// same tree / read / write plumbing the workspace Explorer uses, so a file
// opens in the Monaco file tab and Save goes back through the standard
// workspace file endpoint. The header's reload button pushes the on-disk
// content into the live agent (`POST /agents/{id}/prompts/reload`, which also
// rebuilds the skill registry).
//
// Named for what the panel tunes, not for where the bytes live: "package
// files" leaked the storage layout into the UI, while the operator's question
// is always "which prompt / which skill am I editing?". The `__agent_package__`
// workspace id stays in the code — that IS the storage fact this card reads.
//
// Card grammar: `ListBox` (raised box card) + one `ExpandableRow` header —
// identical to the Settings / Status-panel cards. The header row is the
// collapse toggle, the body sits on the `panel-inset` sub-surface below a
// hairline, and the file rows take the `inset` row fills so the cursor passing
// over them reads as one list family with every other expanded card body.
//
// Two layout rules specific to this card:
//   - Indent is 12px per level (capped at 3 levels) and the deepest level's
//     rows have no trailing `pr-3`, so the two subtree roots line up exactly
//     under each other instead of stair-stepping like a filesystem tree — this
//     is a two-section browse panel, not an Explorer.
//   - The body is capped at `max-h-60` (the same cap every other bounded
//     list in the shell uses) and scrolls on its own, so a deep `skills/`
//     tree can never push the debug controls off the tab.
import { useCallback, useEffect, useState } from "react";
import { FileText, Folder, FolderOpen, Loader2, RefreshCw } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { useAgentStore } from "../../stores/agentStore";
import { useDebugStore } from "../../stores/debugStore";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { fetchTreeNode, useFileTreeStore } from "../../stores/fileTree/fileTreeStore";
import type { TreeEntry } from "../../stores/fileTree/types";
import { getGatewayUrl } from "../../lib/config";
import { cn } from "../../lib/utils";
import { ListBox, ExpandableRow, ListRow, EmptyState } from "../common/list";

const PACKAGE_WORKSPACE = "__agent_package__";

// Per-level indent, capped at 3 levels — see the header note. Files take one
// extra step (16px, exactly the icon column) so their `FileText` glyph lands
// under their parent directory's name rather than under its folder icon —
// the same 14px-per-level + icon-column offset the Doc sidebar tree uses.
const DEPTH_INDENT = ["pl-3", "pl-6", "pl-9"] as const;
const FILE_INDENT_BUMP = "pl-7";

/** Translator passed down explicitly: FileNode recurses, so it has to stay a
 *  module-level component (a closure defined inside PromptsSkillsCard would remount
 *  every node on each render) yet still needs the locale-aware lookup for its
 *  empty-directory state. */
type Translate = ReturnType<typeof useTranslation>["t"];

function FileNode({
  agentId,
  entry,
  parentPath,
  depth,
  selectedPath,
  onSelect,
  onCollapse,
  t,
}: {
  agentId: string;
  entry: TreeEntry;
  parentPath: string;
  depth: number;
  selectedPath: string | null;
  onSelect: (relPath: string) => void;
  onCollapse: (dirPath: string) => void;
  t: Translate;
}) {
  const [expanded, setExpanded] = useState(false);
  const [state, setState] = useState<{ loading: boolean; entries: TreeEntry[] }>({ loading: false, entries: [] });
  const path = parentPath ? `${parentPath}/${entry.name}` : entry.name;
  const isDir = entry.type === "directory";
  const openFile = useFileEditorStore((s) => s.openFile);
  const isSelected = selectedPath === path;
  const indent = isDir
    ? DEPTH_INDENT[Math.min(depth, DEPTH_INDENT.length - 1)]
    : // One icon column deeper than the directory that contains it.
      cn(DEPTH_INDENT[Math.min(depth, DEPTH_INDENT.length - 1)], FILE_INDENT_BUMP);
  const toggle = useCallback(async () => {
    const willExpand = !expanded;
    if (willExpand && isDir && state.entries.length === 0) {
      setState({ loading: true, entries: [] });
      const node = await fetchTreeNode(agentId, PACKAGE_WORKSPACE, path);
      setState({
        loading: false,
        entries: node.kind === "ready" || node.kind === "stale" ? node.entries : [],
      });
    }
    setExpanded(willExpand);
    if (!willExpand) onCollapse(path);
  }, [agentId, expanded, isDir, onCollapse, path, state.entries.length]);

  // Click grammar mirrors the workspace Explorer file tree
  // (`components/workspace/FileTree/FileTreeNode.tsx`): directories toggle on
  // a single click, files SELECT on a single click and OPEN on a double click.
  // Single-click-to-open felt like the tree was skipping a step — and it is the
  // gesture the user already builds up on the workspace tree, which must stay
  // the benchmark for "what a file row does here".
  const handleClick = useCallback(() => {
    if (isDir) void toggle();
    else onSelect(path);
  }, [isDir, onSelect, path, toggle]);

  const handleDoubleClick = useCallback(() => {
    if (isDir) return;
    // Deliberately NOT the Explorer's `isWorkspacePreviewable` branch: every
    // file under `prompts/` and `skills/` is Markdown, which that rule routes
    // to preview mode — where Save is disabled (FileEditorPanel). This card
    // exists to edit and save these files, so they always open in Monaco.
    void openFile(agentId, PACKAGE_WORKSPACE, path);
  }, [agentId, isDir, openFile, path]);

  return (
    <div>
      <ListRow
        surface="inset"
        selected={isSelected}
        onClick={handleClick}
        onDoubleClick={handleDoubleClick}
        ariaLabel={path}
        className={`${indent} gap-1.5 py-1 text-11 ${isSelected ? "text-text" : "text-text-secondary"}`}
        leading={
          // Expansion is carried by the folder's own open/closed silhouette —
          // no chevron layered on top. Same grammar as the left-nav Doc tree
          // (`views/doc/DocTreeSidebar.tsx`): one icon per row, so the
          // leading slot is a single svg (Tailwind's preflight makes svg a
          // block element — siblings here would stack vertically, which is
          // how an earlier revision of this card rendered).
          state.loading ? (
            <Loader2 size={14} className="shrink-0 animate-spin text-text-tertiary" />
          ) : isDir ? (
            expanded ? (
              <FolderOpen size={14} className="shrink-0 text-text-tertiary" />
            ) : (
              <Folder size={14} className="shrink-0 text-text-tertiary" />
            )
          ) : (
            <FileText size={14} className="shrink-0 text-text-tertiary" />
          )
        }
      >
        <span className="truncate">{entry.name}</span>
      </ListRow>
      {expanded && isDir && (
        <>
          {state.loading ? (
            <div className="flex items-center gap-1.5 pl-6 py-1 text-10 text-text-tertiary">
              <Loader2 className="h-3 w-3 animate-spin" />
            </div>
          ) : state.entries.length === 0 ? (
            <EmptyState message={t("debugFiles.emptyDir")} className="pl-6" />
          ) : (
            state.entries.map((child) => (
              <FileNode
                key={`${path}/${child.name}`}
                agentId={agentId}
                entry={child}
                parentPath={path}
                depth={depth + 1}
                selectedPath={selectedPath}
                onSelect={onSelect}
                onCollapse={onCollapse}
                t={t}
              />
            ))
          )}
        </>
      )}
    </div>
  );
}

export function PromptsSkillsCard() {
  const { t } = useTranslation();
  const debugAgentId = useDebugStore((s) => s.debugAgentId);
  const selectedAgentId = useAgentStore((s) => s.selectedAgentId);
  const agentId = debugAgentId ?? selectedAgentId;
  const [open, setOpen] = useState(true);
  const [entries, setEntries] = useState<TreeEntry[]>([]);
  const [loading, setLoading] = useState(false);
  const [reloading, setReloading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Single-click selection target (Explorer file-tree grammar: click selects,
  // double click opens). Card-local: the workspace Explorer keeps its own in
  // FileTree, and there is no reason for the two trees to share one.
  const [selectedPath, setSelectedPath] = useState<string | null>(null);
  const invalidateTree = useFileTreeStore((s) => s.invalidate);

  // Switching agents invalidates the selection — the highlighted path belongs
  // to the previously browsed package.
  useEffect(() => {
    setSelectedPath(null);
  }, [agentId]);

  // Collapsing a subtree must clear the selection when the selected file lived
  // inside it, or the highlight survives on a row that is no longer on screen
  // and reappears stale the next time that directory is opened.
  const collapseUnder = useCallback((dirPath: string) => {
    setSelectedPath((current) =>
      current !== null && current.startsWith(`${dirPath}/`) ? null : current,
    );
  }, []);

  const refresh = useCallback(async () => {
    if (!agentId) {
      setEntries([]);
      return;
    }
    setLoading(true);
    setError(null);
    const node = await fetchTreeNode(agentId, PACKAGE_WORKSPACE, "");
    if (node.kind === "ready" || node.kind === "stale") setEntries(node.entries);
    else {
      setEntries([]);
      if (node.kind === "error") setError(t("debugFiles.loadError"));
    }
    setLoading(false);
  }, [agentId, t]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const reload = useCallback(async () => {
    if (!agentId) return;
    setReloading(true);
    setError(null);
    try {
      const response = await fetch(`${getGatewayUrl()}/api/agents/${agentId}/prompts/reload`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: "{}",
      });
      if (!response.ok) throw new Error(`${response.status} ${response.statusText}`);
      invalidateTree(agentId);
      await refresh();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setReloading(false);
    }
  }, [agentId, invalidateTree, refresh]);

  return (
    <ListBox dividers={false}>
      <ExpandableRow
        open={open}
        onToggle={() => setOpen((v) => !v)}
        title={t("debugFiles.title")}
        ariaLabel={t("debugFiles.title")}
        bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset"
        trailing={
          <button
            type="button"
            disabled={!agentId || reloading}
            onClick={(e) => {
              e.stopPropagation();
              void reload();
            }}
            aria-label={t("debugFiles.reload")}
            title={t("debugFiles.reload")}
            className="rounded p-1 text-text-tertiary transition-colors hover:bg-zinc-200 hover:text-zinc-700 disabled:opacity-50 disabled:hover:bg-transparent dark:hover:bg-zinc-700 dark:hover:text-zinc-200 dark:disabled:hover:bg-transparent"
          >
            <RefreshCw className={`h-3.5 w-3.5 ${reloading ? "animate-spin" : ""}`} />
          </button>
        }
      >
        <div className="max-h-60 overflow-y-auto py-1">
          {loading ? (
            <div className="flex items-center justify-center py-3 text-text-tertiary">
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
            </div>
          ) : error ? (
            <p className="px-3 py-2 text-10 text-red-500">{error}</p>
          ) : entries.length === 0 ? (
            <EmptyState message={t("debugFiles.empty")} />
          ) : (
            entries.map((entry) => (
              <FileNode
                key={entry.name}
                agentId={agentId ?? ""}
                entry={entry}
                parentPath=""
                depth={0}
                selectedPath={selectedPath}
                onSelect={setSelectedPath}
                onCollapse={collapseUnder}
                t={t}
              />
            ))
          )}
        </div>
      </ExpandableRow>
    </ListBox>
  );
}
