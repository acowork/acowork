/**
 * GitStatusPanel — ADR-078 decision 6. Flat (non-tree) list of uncommitted
 * changes below GitStatusBar. Row styling mirrors the file-tree rows
 * (FileTreeNode.tsx). Right-click menu: Show Diff / Show Log / Open in
 * editor / Revert (destructive, confirm-guarded — discards uncommitted
 * changes back to HEAD; untracked files are deleted). Deleted rows
 * redirect to Show Diff (decision 6).
 */

import { useEffect, useMemo, useState } from "react";
import {
  AlertCircle,
  ArrowRightLeft,
  ChevronLeft,
  ChevronRight,
  FileMinus,
  FilePlus,
  GitMerge,
  Lock,
  Pencil,
} from "lucide-react";
import {
  type GitChangeDto,
  type GitDiffResponse,
  type GitLogResponse,
  gitGroupKey,
  gitViewKey,
  useGitStore,
} from "../../../stores/gitStore";
import { useFileEditorStore } from "../../../stores/fileEditorStore";
import { languageForPath } from "../../../lib/monacoLanguage";
import { useTranslation } from "../../../i18n/useTranslation";
import { useContextMenu, ContextMenu } from "../../common/ContextMenu";
import { ConfirmDialog } from "../../common/ConfirmDialog";
import { OverlayScrollbar } from "../../common/OverlayScrollbar";
import { showToast } from "../../common/ToastProvider";
import { cn } from "../../../lib/utils";
import { listRowHover } from "../../../lib/ui-styles";

// Pagination: a workspace with a stray `node_modules/` or `target/`
// under version control can balloon the changed-file list to 5000+
// rows, which freezes the panel before the user even sees it. 100
// rows/page matches the git-domain LOG_STEP=50 in GitVirtualNav (file
// rows are denser, single-line, so we double it).  Sized to leave the
// scrolled viewport filled without becoming a wall of text.
const PAGE_SIZE = 100;

// Stable empty array so `changes` reference doesn't churn when the
// store is still loading (avoiding an extra render loop on first mount).
const EMPTY_CHANGES: GitChangeDto[] = [];

/** Inline-friendly sibling of `BUTTON_CLASS` from GitVirtualNav.tsx —
 *  same icon size + zinc-100/border treatment, minus the overlay
 *  decorations (absolute, shadow, animate-in) which would look out
 *  of place inside the flat file-list footer. */
const PAGINATION_BUTTON_CLASS =
  "inline-flex items-center justify-center rounded-full border border-zinc-200 bg-zinc-100 p-1 text-text-tertiary transition-colors hover:bg-zinc-200 disabled:cursor-not-allowed disabled:opacity-30 dark:border-zinc-600 dark:bg-zinc-700  dark:hover:bg-zinc-600";

interface GitStatusPanelProps {
  agentId: string;
  workspaceId: string;
}

function statusMeta(c: GitChangeDto): { icon: React.ReactNode; color: string; labelKey: string } {
  // Unmerged / conflicted paths take priority — never rendered as "clean"
  // (ADR-078 invariant 5; runtime maps porcelain `UU`/`AU`/`AA`/... to
  // `conflicted` on both columns).
  if (c.index === "conflicted" || c.worktree === "conflicted") {
    return {
      icon: <GitMerge size={13} />,
      color: "text-red-500 dark:text-red-400",
      labelKey: "gitStatus.conflicted",
    };
  }
  if (c.index === "renamed" || c.index === "added") {
    return {
      icon: c.index === "renamed" ? <ArrowRightLeft size={13} /> : <FilePlus size={13} />,
      color: c.staged ? "text-emerald-600 dark:text-emerald-400" : "text-text-tertiary ",
      labelKey: c.index === "renamed" ? "gitStatus.renamed" : "gitStatus.added",
    };
  }
  if (c.index === "deleted" || c.worktree === "deleted") {
    return {
      icon: <FileMinus size={13} />,
      color: "text-red-500 dark:text-red-400",
      labelKey: "gitStatus.deleted",
    };
  }
  if (c.worktree === "untracked") {
    return {
      icon: <FilePlus size={13} />,
      color: "text-sky-500 dark:text-sky-400",
      labelKey: "gitStatus.untracked",
    };
  }
  return {
    icon: <Pencil size={13} />,
    color: "text-amber-500 dark:text-amber-400",
    labelKey: "gitStatus.modified",
  };
}

export function GitStatusPanel({ agentId, workspaceId }: GitStatusPanelProps) {
  const { t } = useTranslation();
  const groupKey = gitGroupKey(agentId, workspaceId);
  // Subscribe to the rev the group is currently viewing so a click in the
  // bar's history dropdown swaps the panel's source data on the next
  // render. Reading via the same `gitViewKey` the store writes keeps
  // the panel, the cache, and the bar title coherent.
  const viewingRev = useGitStore((s) => s.viewingRev[groupKey] ?? "");
  const entry = useGitStore((s) => s.status[gitViewKey(groupKey, viewingRev)]);
  const menu = useContextMenu<GitChangeDto>();

  const data = entry?.data;
  const loading = entry?.loading ?? false;
  const error = entry?.error ?? null;
  const errorStatus = entry?.errorStatus ?? null;
  const changes = data?.changes ?? EMPTY_CHANGES;

  // ── Pagination (ADR-078 decision 6 follow-up) ───────────────────
  // Clamp the page index whenever the underlying changes array
  // changes (new fetch, switching rev via the history dropdown).
  // `changes` reference is stable across re-renders of the same
  // snapshot because `EMPTY_CHANGES` is a module-level const, so the
  // effect fires on real data swaps only.
  const totalPages = Math.max(1, Math.ceil(changes.length / PAGE_SIZE));
  const [pageIndex, setPageIndex] = useState(0);
  useEffect(() => {
    // `changes.length` shift (ref change with same length is a no-op)
    if (pageIndex > totalPages - 1) {
      setPageIndex(Math.max(0, totalPages - 1));
    } else if (pageIndex < 0) {
      setPageIndex(0);
    }
  }, [changes, totalPages, pageIndex]);
  const pagedChanges = useMemo(
    () => changes.slice(pageIndex * PAGE_SIZE, (pageIndex + 1) * PAGE_SIZE),
    [changes, pageIndex],
  );

  const openDiff = useMemo(
    () => async (c: GitChangeDto) => {
      const editor = useFileEditorStore.getState();
      try {
        // When the panel is showing files in commit X, click-row opens
        // the diff for X vs X^ (git's first-parent shorthand; root
        // commits have no parent and the backend will surface a 400,
        // which the user sees as a toast). Otherwise the default is
        // HEAD vs working tree.
        //
        // The backend then PROMOTES the base from `X^` (first-parent)
        // to the file-history predecessor of X on `c.path`, so the diff
        // banner label matches the row above X in the banner's
        // `CommitPicker` (which lists `git log -- <path>`, also file
        // history). This is the same semantic IDE / GitKraken use by
        // default ("what did this file look like right before this
        // commit?"). First-parent is only retained when `X` introduced
        // `c.path` (no file-history predecessor exists).
        const baseRef = viewingRev ? `${viewingRev}^` : "HEAD";
        const headRef = viewingRev;
        const diff: GitDiffResponse = await useGitStore
          .getState()
          .fetchDiff(agentId, workspaceId, c.path, baseRef, headRef);
        editor.openVirtualFile({
          agentId,
          workspaceId,
          kind: "diff",
          relPath: c.path,
          content: diff.modified,
          original: diff.original,
          gitDiffKind: diff.kind,
          language: languageForPath(c.path),
          // Store the server-canonicalised commit SHAs as the OpenFile's
          // refs. The diff-side banner `slice(0, 7)` relies on these
          // being already-canonical SHAs — the backend does the
          // git-rev-resolution + file-history promotion so the client
          // never has to reason about git semantics (ADR-009 v2:
          // gateway / desktop / runtime split).
          // Working-tree variant returns `headRev = null` so the existing
          // `!diffHeadRef → "Working Tree"` rendering still works.
          diffBaseRef: diff.baseRev,
          diffHeadRef: diff.headRev ?? "",
        });
      } catch (e) {
        console.error("[GitStatusPanel] fetchDiff failed:", e);
      }
    },
    [agentId, workspaceId, viewingRev],
  );

  const openLog = useMemo(
    () => async (c: GitChangeDto) => {
      const editor = useFileEditorStore.getState();
      try {
        const INITIAL_LIMIT = 50;
        const log: GitLogResponse = await useGitStore
          .getState()
          .fetchLog(agentId, workspaceId, c.path, INITIAL_LIMIT);
        const text = log.commits
          .map(
            (cm) =>
              `${cm.shortHash}  ${cm.author}  ${cm.date}\n    ${cm.subject}`,
          )
          .join("\n\n");
        // Seed pagination from the ACTUAL fetch — not the requested
        // limit. If the file has < INITIAL_LIMIT commits we must mark
        // `reachedEnd` so the Next button disables itself on first
        // open (otherwise the user clicks Next, fetch returns the
        // same tail, and Prev would no longer be able to restore the
        // display since the old cache would be overwritten with the
        // empty result).
        editor.openVirtualFile({
          agentId,
          workspaceId,
          kind: "log",
          relPath: c.path,
          content: text || t("gitStatus.noCommits"),
          language: "plaintext",
          loadedCommits: log.commits,
          displayedLimit: log.commits.length,
          reachedEnd: log.commits.length < INITIAL_LIMIT,
        });
      } catch (e) {
        console.error("[GitStatusPanel] fetchLog failed:", e);
      }
    },
    [agentId, workspaceId, t],
  );

  const menuItems = useMemo(
    () => [
      {
        key: "diff",
        label: t("gitStatus.showDiff"),
        onClick: ({ payload }: { payload: GitChangeDto | undefined }) => {
          if (payload) void openDiff(payload);
        },
      },
      {
        key: "log",
        label: t("gitStatus.showLog"),
        onClick: ({ payload }: { payload: GitChangeDto | undefined }) => {
          if (payload) void openLog(payload);
        },
      },
      {
        key: "open",
        label: t("gitStatus.openInEditor"),
        onClick: ({ payload }: { payload: GitChangeDto | undefined }) => {
          if (payload && payload.worktree !== "deleted") {
            void useFileEditorStore
              .getState()
              .openFile(agentId, workspaceId, payload.path);
          } else if (payload) {
            // Deleted rows redirect to Show Diff (ADR-078 decision 6).
            void openDiff(payload);
          }
        },
      },
      // Reverting "files in commit X" makes no sense — the rows are
      // historical, not uncommitted changes. Only offer it on the
      // working-tree view (`viewingRev === ""`).
      ...(viewingRev
        ? []
        : [
            {
              key: "revert",
              label: t("gitStatus.revert"),
              onClick: ({ payload }: { payload: GitChangeDto | undefined }) => {
                if (payload) setRevertTarget(payload);
              },
            },
          ]),
    ],
    [t, openDiff, openLog, agentId, workspaceId, viewingRev],
  );

  /** Pending revert target (null = dialog closed). */
  const [revertTarget, setRevertTarget] = useState<GitChangeDto | null>(null);
  const handleRevert = useMemo(
    () => async () => {
      const c = revertTarget;
      if (!c) return;
      setRevertTarget(null);
      try {
        await useGitStore
          .getState()
          .revertFile(agentId, workspaceId, c.path, c.oldPath);
        // Discarding changes rewrites the working tree — refresh the
        // panel's current view so the row disappears.
        await useGitStore.getState().refresh(agentId, workspaceId);
        showToast({ type: "success", message: t("gitStatus.revertSuccess", { path: c.path }) });
      } catch (e) {
        console.error("[GitStatusPanel] revert failed:", e);
        showToast({
          type: "error",
          message: t("gitStatus.revertError", {
            detail: e instanceof Error ? e.message : String(e),
          }),
        });
      }
    },
    [agentId, workspaceId, revertTarget, t],
  );

  const body = (() => {
    if (loading && !data) {
      return (
        <div className="flex h-full items-center justify-center text-xs text-text-tertiary">
          {t("gitStatus.loading")}
        </div>
      );
    }
    if (error && !data) {
      // A 403 is an access state, not a failure — render it in the same
      // neutral tone as "not a repo" / "clean", so a view-only guest sees
      // information rather than a red alarm they cannot act on.
      const denied = errorStatus === 403;
      return (
        <div
          className={`flex h-full items-center justify-center gap-1.5 text-xs ${
            denied ? "text-text-tertiary" : "text-red-500"
          }`}
        >
          {denied ? <Lock size={13} /> : <AlertCircle size={13} />} {error}
        </div>
      );
    }
    if (!data) return null;
    if (!data.isRepo) {
      return (
        <div className="flex h-full items-center justify-center text-xs text-text-tertiary">
          {data.error === "git_unavailable"
            ? t("gitStatus.gitUnavailable")
            : t("gitStatus.notRepo")}
        </div>
      );
    }
    if (changes.length === 0) {
      return (
        <div className="flex h-full items-center justify-center text-xs text-text-tertiary">
          {t("gitStatus.clean")}
        </div>
      );
    }
    return (
      <>
      {/* Overflow moved to the OverlayScrollbar viewport: long file paths
          exceed the panel's width, so the x-axis still needs to scroll —
          `showXThumb` + `overflow-x-auto` is the same pairing FileTree uses
          (the native bar cannot be re-enabled on one axis only). */}
      <OverlayScrollbar showXThumb className="overflow-x-auto py-0.5">
        <ul>
        {pagedChanges.map((c) => {
          const meta = statusMeta(c);
          return (
            <li
              key={c.path}
              onContextMenu={(e) => menu.openAt(e, c)}
              className={cn(
                // Font size comes from `.file-tree-row` in globals.css, the
                // same rule FileTreeNode's rows carry — both lists sit
                // stacked in one panel, so they scale from one declaration
                // (12px at the default base, i.e. the app's `text-xs` step).
                // Keep `py-[0.2em]` in sync with the tree row: the em padding
                // is part of the height the tree's virtualizer derives.
                // `min-w-max` keeps long paths from collapsing into the
                // row width — combined with the scroller's overflow-auto
                // it surfaces a horizontal scrollbar instead of `…`.
                "file-tree-row flex min-w-max cursor-pointer items-center gap-1.5 py-[0.2em] pr-3 pl-4 select-none",
                listRowHover,
              )}
              onDoubleClick={() => {
                // Double-click opens the file (or its diff if unavailable on
                // disk). Unifies the gesture with the workspace working-tree
                // list, which uses single-click to select and double-click
                // to open (FileTreeNode.tsx).
                if (viewingRev || c.worktree === "deleted") void openDiff(c);
                else
                  void useFileEditorStore
                    .getState()
                    .openFile(agentId, workspaceId, c.path);
              }}
              data-testid="git-status-row"
            >
              <span className={cn("shrink-0", meta.color)} title={t(meta.labelKey)}>
                {meta.icon}
              </span>
              {/* whitespace-nowrap (was truncate): long paths must stay on
                  one line and overflow the row, not be cut to `…`. The
                  scroller's overflow-auto + the row's `min-w-max` then
                  surface a horizontal scrollbar so the user can read the
                  full path. */}
              <span className="flex-1 whitespace-nowrap text-text-tertiary ">
                {c.path}
              </span>
              {c.oldPath && (
                // `text-11`, not `text-10`: these meta bits resolve against the
                // row's own 12px font-size (em-based tokens), so text-10 here
                // would land at 8.6px — under the app's 9px floor. text-11
                // renders 9.4px, i.e. the same visual step as before.
                <span className="whitespace-nowrap text-11 text-text-tertiary ">
                  ← {c.oldPath}
                </span>
              )}
              {c.staged && (
                <span
                  className="shrink-0 rounded bg-emerald-100 px-1 text-11 text-emerald-700 dark:bg-emerald-900/40 dark:text-emerald-300"
                  title={t("gitStatus.staged")}
                >
                  {t("gitStatus.staged")}
                </span>
              )}
            </li>
          );
        })}
        </ul>
      </OverlayScrollbar>
      {totalPages > 1 && (
        <div
          data-testid="git-status-pagination"
          className="flex shrink-0 items-center justify-between gap-2 border-t border-border-divider px-2 py-1"
        >
          <span className="text-10 tabular-nums text-text-tertiary ">
            {pageIndex * PAGE_SIZE + 1}–{Math.min((pageIndex + 1) * PAGE_SIZE, changes.length)} / {changes.length}
          </span>
          <div className="flex items-center gap-1">
            <button
              type="button"
              onClick={() => setPageIndex((p) => Math.max(0, p - 1))}
              disabled={pageIndex === 0}
              aria-label={t("gitStatus.prevPage")}
              title={t("gitStatus.prevPage")}
              className={PAGINATION_BUTTON_CLASS}
            >
              <ChevronLeft className="h-3.5 w-3.5" />
            </button>
            <button
              type="button"
              onClick={() => setPageIndex((p) => Math.min(totalPages - 1, p + 1))}
              disabled={pageIndex >= totalPages - 1}
              aria-label={t("gitStatus.nextPage")}
              title={t("gitStatus.nextPage")}
              className={PAGINATION_BUTTON_CLASS}
            >
              <ChevronRight className="h-3.5 w-3.5" />
            </button>
          </div>
        </div>
      )}
    </>
    );
  })();

  return (
    <div
      // bg-right-panel — match WorkspaceExplorer/FileTree so the
      // expanded git list blends into the right panel instead of
      // showing as a darker "page-bg" slab. ADR-078 decision 6.
      className="flex min-h-0 flex-1 flex-col overflow-hidden border-b border-right-panel-border bg-right-panel"
      data-testid="git-status-panel"
    >
      {body}
      <ContextMenu
        isOpen={menu.isOpen}
        menuProps={menu.menuProps}
        items={menuItems}
        payload={menu.payload}
        selectionAtOpen={menu.selectionAtOpen}
        onClose={menu.close}
        compact
      />
      {/* Destructive discard of uncommitted changes — explicit consent
          before the backend runs `git restore` (untracked → delete). */}
      <ConfirmDialog
        open={revertTarget !== null}
        title={t("gitStatus.revertConfirmTitle")}
        message={t("gitStatus.revertConfirmMessage", {
          path: revertTarget?.path ?? "",
        })}
        confirmLabel={t("gitStatus.revert")}
        destructive
        onConfirm={handleRevert}
        onCancel={() => setRevertTarget(null)}
      />
      {/* manual refresh affordance is on the bar; keep the panel focused */}
      <span className="sr-only">{loading ? t("gitStatus.loading") : ""}</span>
    </div>
  );
}
