/**
 * GitStatusBar — ADR-078 decision 6. Collapsible version-control strip at
 * the bottom of the FileEditorPanel. Visual spec: min-h-[2.5rem] (40px,
 * matches the workspace-selector toolbar row and the file-search row
 * above), text at the file-tree row step (`--ui-font-size` × 0.857 = 12px at
 * the default base — the size of the rows it labels), no bold, normal case,
 * zinc-400/500, border-y, hover tint, ChevronRight rotation.
 *
 * Not bold, not uppercase: a bold uppercase strip read as a foreign,
 * heavier control family next to the panel's other two strips (workspace
 * selector, file search), which are plain normal case — and uppercasing
 * also mangled the branch name, which must match `git branch` output byte
 * for byte.
 *
 * Right-side controls: `History` (clock icon) opens a CommitPicker
 * dropdown letting the user view files from any commit on the
 * repository's history, with `Local Working Tree` pinned at the top;
 * `Refresh` reloads the CURRENTLY-VIEWED view (working tree or the
 * selected commit's file list).
 */

import { useEffect, useState } from "react";
import { ChevronRight, Clock, GitBranch, Loader2, RefreshCw } from "lucide-react";
import { gitGroupKey, gitViewKey, useGitStore } from "../../../stores/gitStore";
import { useTranslation } from "../../../i18n/useTranslation";
import { cn } from "../../../lib/utils";
import { CommitPicker } from "../../editor/CommitPicker";
import { Tooltip } from "../../common/Tooltip";

interface GitStatusBarProps {
  agentId: string;
  workspaceId: string;
}

export function GitStatusBar({ agentId, workspaceId }: GitStatusBarProps) {
  const { t } = useTranslation();
  const isExpanded = useGitStore((s) => s.isExpanded(agentId, workspaceId));
  const groupKey = gitGroupKey(agentId, workspaceId);
  // Always read the view-slot the panel is currently rendering so the
  // bar title + History button reflect the active view (working tree
  // by default, or the commit the user picked from the dropdown).
  const viewingRev = useGitStore((s) => s.viewingRev[groupKey] ?? "");
  const entry = useGitStore((s) => s.status[gitViewKey(groupKey, viewingRev)]);
  const setExpanded = useGitStore((s) => s.setExpanded);
  const refresh = useGitStore((s) => s.refresh);
  const setViewingRev = useGitStore((s) => s.setViewingRev);

  // CommitPicker anchor state. `null` means closed; toggling clicks on
  // the history button flip it. The same picker host is reused for both
  // open/close so its mount effect (fetchLog) only runs once per open.
  const [historyAnchor, setHistoryAnchor] = useState<HTMLElement | null>(null);

  // ADR-078 decision 6 / invariant 6 ("subscription == visibility"): the
  // expanded (subscribed) group must match the group this bar renders.
  //   - Setup: a stale expansion from a previous agent/workspace (panel
  //     switched groups without collapsing) is cleared so the demand-driven
  //     fs-watch subscription (workspaceFsWatch deriveWatchGroups) is
  //     released instead of watching the old group in the background.
  //   - Cleanup: when this bar unmounts (right panel collapsed / switched
  //     to another tab) or the (agent, workspace) group changes, collapse
  //     this group if expanded — the panel is no longer visible.
  useEffect(() => {
    const s = useGitStore.getState();
    if (s.expandedKey && !s.isExpanded(agentId, workspaceId)) {
      s.setExpanded(agentId, workspaceId, false);
    }
    return () => {
      const cur = useGitStore.getState();
      if (cur.isExpanded(agentId, workspaceId)) {
        cur.setExpanded(agentId, workspaceId, false);
      }
    };
  }, [agentId, workspaceId]);

  // Auto-refresh status on mount and whenever the (agent, workspace) group
  // changes (workspace switch, agent restart while the workspace panel is
  // mounted). Mirrors FileTree.tsx's mount-time `fetchTree(agentId,
  // workspaceId, "")` effect so the banner converges without a click —
  // without this, the collapsed bar would stay on the default "Git" title
  // until the user expands it (the expand path already calls refresh, but
  // a collapsed banner should still show the current branch). Inflight
  // dedup in `fetchStatus` keeps a double-fetch from clicking the refresh
  // icon or expanding the bar from issuing a second HTTP request.
  useEffect(() => {
    void refresh(agentId, workspaceId);
  }, [agentId, workspaceId, refresh]);

  const data = entry?.data;
  const loading = entry?.loading ?? false;
  const isRepo = data?.isRepo ?? true; // optimistically interactive pre-load
  const branch = data?.branch;
  const changes = data?.changes.length ?? 0;
  // True only when the bar is showing a specific commit's file list
  // (i.e. the user picked a non-empty rev). The "files in commit X"
  // copy replaces the "branch · N changes" copy so the bar reads as
  // "you're viewing X's file list, not your worktree".
  const viewingCommit = viewingRev !== "";

  // Direction of the history dropdown. The strip now lives in the panel's
  // HEADER stack (between the workspace selector and the file search), so
  // the space below it is the search row and the file tree — the dropdown
  // always opens DOWNWARD. (It used to sit at the panel's bottom edge and
  // flipped upward when the panel was short; that geometry is gone with
  // the move.)
  const historyPlacement: "top" | "bottom" = "bottom";

  // Branch name is an IDENTIFIER, not a label: the whole bar is
  // `uppercase` to match the NodeGroupHeader spec, but rendering
  // `feature/Foo_Bar` as `FEATURE/FOO_BAR` makes it impossible to
  // eyeball-match against `git branch` output (and defeats the
  // case-sensitive distinction some branch names rely on). So the
  // ref keeps its own casing via `normal-case`; the surrounding
  // copy ("· 3 changes", the not-a-repo / no-data strings) still
  // picks up the bar's uppercase.
  const ref = viewingCommit ? (branch ?? viewingRev) : branch;
  const meta = `${changes} ${t("gitStatusBar.changes")}`;
  const title = !data
    ? t("gitStatusBar.title")
    : !isRepo
      ? t("gitStatusBar.notRepo")
      : ref
        ? `${ref} · ${meta}`
        : meta;

  // A workspace that is not a git repository has nothing for this strip to
  // say, and a strip that reads "Not a Git repository" is noise where the
  // file tree is the subject — so the strip is NOT rendered at all. The
  // check waits for the data: before the first fetch lands there is no
  // verdict, and rendering the bar optimistically would flash a strip
  // that a moment later disappears.
  if (data && !data.isRepo) return null;

  return (
    // The wrapper div uses `display: contents` so it doesn't break the
    // bar's flex layout (the parent panel positions this element as if
    // the button were its direct child). The wrapper exists only to
    // give the CommitPicker popover a sibling outside the bar's own
    // <button> — nesting <button> inside <button> is invalid HTML
    // (React warns) AND a click on any picker item would otherwise
    // bubble up and toggle the bar's expand state.
    <div className="contents">
      <button
        type="button"
        onClick={() => setExpanded(agentId, workspaceId, !isExpanded)}
        aria-expanded={isExpanded}
        aria-label={title}
        title={title}
        data-testid="git-status-bar"
        className={cn(
          // min-h-[2.5rem] — matches the workspace-selector row (toolbarButton
          // py-1.5 + text-xs ≈ 40px) and the file-search row (WorkspaceExplorer
          // L833, `min-h-[2.5rem]`) above, so the three right-panel banners
          // read as a single height family. Was `h-6` (24px) — visually
          // ~16px shorter than the other two banners.
          // `pl-3.5` / `pr-2.5`: the workspace-selector row above is
          // `px-1.5` + the trigger's `px-2`, so ITS folder icon starts
          // 14px from the panel edge and its icon cluster ends 10px from
          // it. This strip uses the same two numbers, so the git icon
          // lines up under the folder icon and the three icons on the
          // right line up with the three above them.
          "flex min-h-[2.5rem] w-full shrink-0 items-center gap-1 pl-3.5 pr-2.5 text-left",
          // Sized to the rows it labels: `.file-tree-row` in globals.css sets
          // the tree and the status list below to `--ui-text-size` (12px at
          // the default base), so the banner takes the same step rather than
          // its own. Was `text-10` (10px) — visibly smaller than both lists
          // it sits against.
          // No `font-medium`: a bold strip read as heavier than the file
          // names it labels.
          "text-[length:var(--ui-text-size)] tracking-wide",
          "text-text-tertiary hover:text-text-secondary",
          "transition-colors duration-150",
          // `border-b` only, not `border-y`: the strip sits directly under the
          // workspace-selector row, which already draws its own `border-b`.
          // A top border here doubled the divider into two hairlines.
          // `border-border-divider`, the SAME token the workspace-selector row and
          // the file-search row draw: this strip is a header strip in their
          // stack, and a hairline at 40% opacity read as a fainter divider
          // than its two neighbours.
          "border-b border-border-divider",
          // Subtle elevation when expanded. Collapsed = transparent so the
          // banner blends into `bg-right-panel` (the workspace surface);
          // expanded = `bg-surface-subtle` / `` so the banner
          // reads as the active "section" header above the file list
          // (the list itself stays on the workspace surface — no change
          // there — so the band of contrast sits on the banner only).
          // Same elevation pair as FileEditorPanel's diff header
          // (FileEditorPanel.tsx L1531), keeping strip elevations
          // consistent across panels.
          isExpanded && "bg-surface-subtle",
        )}
      >
      {/* The git icon takes the folder icon's slot (14px, `gap-1`), so the
          ref below it starts where the workspace name starts. */}
      <GitBranch className="h-3.5 w-3.5 shrink-0" />
      {/* No `uppercase` here: the bar's siblings in this panel (the
          workspace-selector row and the file-search row, both
          `toolbarButton` / `text-xs`) are normal case, so uppercasing
          this one strip made it read as a different control family
          rather than part of the panel. It also destroyed the ref —
          `feature/Foo_Bar` → `FEATURE/FOO_BAR` can't be eyeball-matched
          against `git branch` output. `truncate` keeps long refs from
          widening the strip. */}
      <span className="flex-1 truncate">
        {ref ? (
          <>
            <span>{ref}</span>
            <span> · {meta}</span>
          </>
        ) : (
          title
        )}
      </span>
      {loading ? (
        /* `ml-auto` so the spinner sits at the SAME right edge the icon
           cluster takes when the status is loaded — otherwise the strip's
           right-hand content jumps when the fetch lands. */
        <Loader2 className="ml-auto h-3 w-3 shrink-0 animate-spin" />
      ) : (
        /* `ml-auto`: the icon cluster is pushed hard right, so its RIGHT
           edge lands where the workspace-selector row's three-icon
           cluster ends. The two strips then read as one aligned column.
           THREE icons here (history, refresh, chevron) against the three
           above (new file, new folder, refresh) — one icon per column. */
        <div className="ml-auto flex items-center gap-1 shrink-0">
          {/* History picker button. Rendered as a <span role="button">
              because it sits inside the bar's own <button> — nested
              <button> is invalid HTML and React will warn. The class
              string + Tooltip wrap mirror SessionTabBar's session
              history button so the two toolbar strips read as a family.
              Active state (history dropdown open OR a commit is being
              viewed) shows the accent treatment, also matching the
              session history button. */}
          <div className="relative inline-flex">
            <Tooltip content={t("gitStatusBar.history")} variant="plain">
              <span
                role="button"
                tabIndex={0}
                aria-label={t("gitStatusBar.history")}
                data-testid="git-status-bar-history"
                onClick={(e) => {
                  e.stopPropagation();
                  setHistoryAnchor((cur) => (cur ? null : e.currentTarget));
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    e.stopPropagation();
                    setHistoryAnchor((cur) => (cur ? null : e.currentTarget));
                  }
                }}
                className={cn(
                  "inline-flex items-center justify-center rounded h-6 w-6 transition-colors",
                  historyAnchor || viewingCommit
                    ? "text-[var(--color-accent)] bg-surface-hover"
                    : "text-text-tertiary hover:bg-[var(--color-accent)]/10 hover:text-text-tertiary dark:hover:bg-[var(--color-accent)]/15",
                )}
              >
                <Clock className="h-3.5 w-3.5" />
              </span>
            </Tooltip>
          </div>
          <div className="relative inline-flex">
            <Tooltip content={t("gitStatusBar.refresh")} variant="plain">
              <span
                role="button"
                tabIndex={0}
                aria-label={t("gitStatusBar.refresh")}
                data-testid="git-status-bar-refresh"
                onClick={(e) => {
                  e.stopPropagation();
                  void refresh(agentId, workspaceId);
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    e.stopPropagation();
                    void refresh(agentId, workspaceId);
                  }
                }}
                className="inline-flex items-center justify-center rounded h-6 w-6 transition-colors text-text-tertiary hover:bg-[var(--color-accent)]/10 hover:text-text-tertiary dark:hover:bg-[var(--color-accent)]/15"
              >
                <RefreshCw className="h-3.5 w-3.5" />
              </span>
            </Tooltip>
          </div>
          {/* The expand chevron — the LAST of the three icons on the
              right, where the workspace-selector row's third icon sits.
              It used to open the strip from the LEFT edge; there it
              pushed the ref and the git icon out of alignment with the
              folder icon and the workspace name above. */}
          {/* The chevron wears the SAME 24x24 box as the two icon buttons
              beside it and as the three buttons in the workspace-selector
              row above (`h-6 w-6`, `gap-1`). A bare 14px glyph would make
              this cluster one box narrower than the row above, so the three
              icons would sit in three DIFFERENT columns — the alignment
              the strip is after. */}
          <span className="inline-flex h-6 w-6 items-center justify-center">
            <ChevronRight
              data-testid="git-status-bar-chevron"
              className={cn(
                "h-3.5 w-3.5 shrink-0 text-text-tertiary transition-transform duration-150",
                isExpanded && "rotate-90",
              )}
            />
          </span>
        </div>
      )}
      </button>
      {historyAnchor && (
        <CommitPicker
          // Pinned at top of the dropdown: "Local Working Tree". The
          // picker passes "" as the special working-tree ref, which
          // the store normalises to the bare group key.
          anchorEl={historyAnchor}
          currentRef={viewingRev}
          allowWorkingTree
          agentId={agentId}
          workspaceId={workspaceId}
          // Empty path → repo-wide history (the fetchLog endpoint
          // skips the path param when it's empty).
          relPath=""
          // Flip the popover above the bar when the expanded file list
          // is short (see `historyPlacement` derivation above) so the
          // dropdown doesn't clip off the workspace panel's bottom.
          placement={historyPlacement}
          onSelect={(rev) => {
            setViewingRev(agentId, workspaceId, rev);
            setHistoryAnchor(null);
          }}
          onClose={() => setHistoryAnchor(null)}
        />
      )}
    </div>
  );
}
