/**
 * CommitPicker — small popover for selecting a git revision (commit or
 * Working Tree). Used by the diff banner buttons in FileEditorPanel to
 * change which revisions the side-by-side Diff diff compares, and by
 * the Git status bar's History button (repo-wide history).
 *
 * ADR-078 extension: previously the only option was HEAD vs working
 * tree. The banner now has two buttons ("base" / "compare"); clicking
 * either opens this picker so the user can pick any commit on the
 * file's history. The `compare` picker additionally offers "Working
 * Tree" (special ref = "") as a top option so the user can flip back.
 *
 * UX — unified across the three call sites:
 * - Single page (≤ 50 commits in the page): just the list, no chrome.
 * - Multiple pages (> 50 commits total): server-paginated with a
 *   search input + "Page X of Y" footer (matches the session list
 *   dropdown in SessionTabBar).
 * - Search is client-side on the current page (also matches session
 *   list) — typing filters the visible commits, leaving pagination
 *   intact so the user can flip through filtered pages.
 *
 * Position: anchored under `anchorEl` by default, fixed-width (320px),
 * capped height with internal scroll so long histories stay usable.
 * Callers can flip the placement with `placement="top"` when there
 * isn't enough room below the anchor (e.g. the Git status bar sits
 * near the bottom of the workspace panel and the expanded file list
 * below it is short — opening the dropdown downward would clip).
 */

import React, { useEffect, useMemo, useState } from "react";
import { ChevronLeft, ChevronRight, Search } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { useGitStore, type GitCommitDto, type GitLogPagination } from "../../stores/gitStore";
import { StyledInput } from "../common/StyledInput";
import { cn } from "../../lib/utils";

const PAGE_SIZE = 50;

interface CommitPickerProps {
    /** Anchor the popover positions itself under. Required while open. */
    anchorEl: HTMLElement | null;
    /** Active ref for this side — used to highlight the current selection. */
    currentRef: string;
    /** Working tree ref ("") — pass `true` for the compare-side picker,
     *  `false` for the base-side picker (which only accepts commits). */
    allowWorkingTree: boolean;
    /** Fetch path scope — null disables the fetch (caller is closing). */
    agentId: string;
    workspaceId: string;
    relPath: string;
    onSelect: (ref: string, label: string) => void;
    onClose: () => void;
    /** Vertical placement relative to the anchor. Default `"bottom"`
     *  (opens below the anchor, like a normal menu). Set to `"top"`
     *  when the caller knows there's not enough room below — e.g. the
     *  Git status bar's History button when the expanded file list is
     *  short, so the dropdown would otherwise clip off the workspace
     *  panel's bottom edge. */
    placement?: "top" | "bottom";
}

/** Working Tree is encoded as the empty string on the wire (see
 *  core/acowork-runtime/src/usecases/git_query_impl.rs `diff()`). The
 *  diff banner shows it as a literal "Working Tree" label. */
const WORKING_TREE_REF = "";
const WORKING_TREE_LABEL_KEY = "gitStatus.workingTreeLabel";

export function CommitPicker({
    anchorEl,
    currentRef,
    allowWorkingTree,
    agentId,
    workspaceId,
    relPath,
    onSelect,
    onClose,
    placement = "bottom",
}: CommitPickerProps) {
    const { t } = useTranslation();
    const fetchLog = useGitStore((s) => s.fetchLog);
    const [commits, setCommits] = useState<GitCommitDto[]>([]);
    const [pagination, setPagination] = useState<GitLogPagination | null>(null);
    const [loading2, setLoading2] = useState(false);
    // Search + page are reset by the path-mount effect (every reopen
    // starts on page 1, no search). Mid-session search flips page
    // back to 1 only if the current page would now point past the
    // end — otherwise the user keeps their position.
    const [page, setPage] = useState(1);
    const [search, setSearch] = useState("");

    // Fetch on mount (or when path / page changes). Re-fetches when the
    // user clicks Prev / Next; same path keeps the same cached view if
    // the caller re-renders without `page` changing. Search is applied
    // client-side on the returned page (matches the session list
    // dropdown), so we never need a server-side search round-trip.
    useEffect(() => {
        let cancelled = false;
        setLoading2(true);
        void fetchLog(agentId, workspaceId, relPath, PAGE_SIZE, (page - 1) * PAGE_SIZE)
            .then((log) => {
                if (cancelled) return;
                setCommits(log.commits);
                setPagination(log.pagination);
            })
            .catch((err) => {
                console.error("[CommitPicker] fetchLog failed:", err);
                if (!cancelled) {
                    setCommits([]);
                    setPagination(null);
                }
            })
            .finally(() => {
                if (!cancelled) setLoading2(false);
            });
        return () => {
            cancelled = true;
        };
    }, [agentId, workspaceId, relPath, page, fetchLog]);

    // Reset pagination state when the picker is closed (anchorEl=null)
    // so the next open starts on page 1 with no search leftover from a
    // previous session. Mirrors how the search input clears when you
    // reopen the session list dropdown.
    useEffect(() => {
        if (!anchorEl) {
            setPage(1);
            setSearch("");
        }
    }, [anchorEl]);

    // Client-side search filter on the current page. Same UX as the
    // session list (which also filters client-side on its 20-item page)
    // — trade-off: typing a term that matches a commit on a different
    // page won't surface it. We accept this for v1 because a server-
    // side search across all pages + pagination is a much larger
    // change, and the dominant use case ("pick a recent commit") is
    // covered by the filter on the most-recent page.
    const filteredCommits = useMemo(() => {
        const q = search.trim().toLowerCase();
        if (!q) return commits;
        return commits.filter(
            (c) =>
                c.subject.toLowerCase().includes(q) ||
                c.author.toLowerCase().includes(q) ||
                c.shortHash.toLowerCase().startsWith(q) ||
                c.hash.toLowerCase().startsWith(q),
        );
    }, [commits, search]);

    const totalPages = pagination?.totalPages ?? 1;
    const showChrome = totalPages > 1;

    // Close on Escape; close on outside click.
    useEffect(() => {
        if (!anchorEl) return;
        const onKey = (e: KeyboardEvent) => {
            if (e.key === "Escape") onClose();
        };
        const onDown = (e: MouseEvent) => {
            const popover = document.getElementById("commit-picker-popover");
            if (popover && popover.contains(e.target as Node)) return;
            if (anchorEl.contains(e.target as Node)) return;
            onClose();
        };
        document.addEventListener("keydown", onKey);
        document.addEventListener("mousedown", onDown);
        return () => {
            document.removeEventListener("keydown", onKey);
            document.removeEventListener("mousedown", onDown);
        };
    }, [anchorEl, onClose]);

    // Position relative to the anchor. Default (`bottom`) opens below
    // it; `top` anchors the popover's bottom edge just above the anchor.
    //
    // Height policy: the popover container has NO maxHeight — its three
    // vertical sections (search header / list / pagination footer) are
    // flex children; only the middle list section caps itself with
    // `max-h-72 overflow-y-auto`. Putting maxHeight on the container
    // itself is a footgun: with no `overflow:hidden`, the inline style
    // clips the *background* (bg-white) but the list still paints past
    // it, so the last few rows render on a transparent background.
    // See `commitPickerListMaxHeight` below for the top-placement
    // override that prevents the list from pushing the popover off
    // the top of the viewport.
    const containerStyle = useMemo<React.CSSProperties>(() => {
        if (!anchorEl) return { display: "none" };
        const r = anchorEl.getBoundingClientRect();
        return {
            position: "fixed",
            top: placement === "top" ? undefined : r.bottom + 4,
            // 4px gap above the anchor + 4px breathing room
            bottom:
                placement === "top"
                    ? window.innerHeight - r.top + 4
                    : undefined,
            left: Math.min(r.left, window.innerWidth - 340),
            width: 320,
            zIndex: 50,
            // Flex column so the three sections stack predictably and
            // the list section's `max-h-72` actually bounds the popover
            // (instead of the list overflowing past a clipped container
            // background — the original bug).
            display: "flex",
            flexDirection: "column",
        };
    }, [anchorEl, placement]);

    // When the popover opens UPWARD, the list's fixed 18rem cap could
    // push the chrome header above the viewport. Compute a viewport-
    // bounded max for the list section in that case.
    //
    // Layout: anchor sits at `r.top`; popover bottom is `4px` above
    // it (`window.innerHeight - r.top + 4` from `bottom:` above). The
    // search header and pagination footer are ~50px and ~40px tall
    // respectively, plus their borders. We leave 8px of breathing room
    // on top so the popover never butts the viewport edge.
    const commitPickerListMaxHeight = useMemo<number | undefined>(() => {
        if (placement !== "top" || !anchorEl) return undefined;
        const r = anchorEl.getBoundingClientRect();
        // Distance from the popover's top edge (at viewport y=0 plus
        // chrome/footer budget) to its bottom edge.
        const popoverBottom = window.innerHeight - r.top + 4;
        const chromeBudget = 8 /* top breathing room */ + 50 /* search */ + 40 /* footer */;
        return Math.max(120, popoverBottom - chromeBudget);
    }, [anchorEl, placement]);

    if (!anchorEl) return null;

    return (
        <div
            id="commit-picker-popover"
            role="listbox"
            aria-label={t("gitStatus.commitPickerLabel")}
            style={containerStyle}
            className="rounded-md border border-right-panel-border bg-surface shadow-lg"
        >
            {/* Search + header — only when the history has more than
             *  one page. Below that threshold the list fits in the
             *  dropdown's scroll area without chrome, matching the
             *  session list dropdown's "single page = no controls"
             *  pattern. */}
            {showChrome && (
                <div className="flex flex-col gap-1 border-b border-right-panel-border px-2 py-1.5 shrink-0">
                    <div className="relative">
                        <Search className="pointer-events-none absolute left-2 top-1/2 -translate-y-1/2 h-3 w-3 text-text-tertiary" />
                        <StyledInput
                            type="text"
                            value={search}
                            onChange={(e) => setSearch(e.target.value)}
                            placeholder={t("gitStatus.commitPickerSearchPlaceholder")}
                            aria-label={t("gitStatus.commitPickerSearchPlaceholder")}
                            className="pl-7"
                        />
                    </div>
                    {pagination && (
                        <div className="text-10 text-text-tertiary  px-1">
                            {(() => {
                                const start = (pagination.currentPage - 1) * pagination.pageSize + 1;
                                const end = Math.min(
                                    pagination.currentPage * pagination.pageSize,
                                    pagination.totalCount,
                                );
                                return t("gitStatus.commitPickerShowing", {
                                    start,
                                    end,
                                    total: pagination.totalCount,
                                });
                            })()}
                        </div>
                    )}
                </div>
            )}

            <div
                className="max-h-72 overflow-y-auto py-1 text-xs"
                style={
                    commitPickerListMaxHeight !== undefined
                        ? { maxHeight: commitPickerListMaxHeight }
                        : undefined
                }
            >
                {allowWorkingTree && (
                    <button
                        type="button"
                        role="option"
                        aria-selected={currentRef === WORKING_TREE_REF}
                        onClick={() =>
                            onSelect(WORKING_TREE_REF, t(WORKING_TREE_LABEL_KEY))
                        }
                        className={
                            "flex w-full items-center gap-2 px-3 py-1.5 text-left transition-colors " +
                            (currentRef === WORKING_TREE_REF
                                ? "bg-info-surface font-medium text-info"
                                : "text-text-secondary hover:bg-[var(--color-accent)]/10  dark:hover:bg-[var(--color-accent)]/15")
                        }
                    >
                        <span className="shrink-0 rounded bg-surface-strong px-1 py-px font-mono text-10 text-text-secondary ">
                            WT
                        </span>
                        <span className="truncate">{t(WORKING_TREE_LABEL_KEY)}</span>
                    </button>
                )}
                {loading2 && (
                    <div className="px-3 py-2 text-text-tertiary">
                        {t("gitStatus.loading")}
                    </div>
                )}
                {!loading2 && filteredCommits.length === 0 && (
                    <div className="px-3 py-2 text-text-tertiary">
                        {search.trim()
                            ? t("gitStatus.commitPickerNoMatches")
                            : t("gitStatus.noCommits")}
                    </div>
                )}
                {filteredCommits.map((c) => {
                    const selected = c.hash === currentRef;
                    return (
                        <button
                            key={c.hash}
                            type="button"
                            role="option"
                            aria-selected={selected}
                            title={`${c.hash}\n${c.author}  ${c.date}\n${c.subject}`}
                            onClick={() => onSelect(c.hash, c.shortHash)}
                            className={cn(
                                "flex w-full items-center gap-2 px-3 py-1.5 text-left transition-colors",
                                selected
                                    ? "bg-info-surface"
                                    : "hover:bg-[var(--color-accent)]/10 dark:hover:bg-[var(--color-accent)]/15",
                            )}
                        >
                            <span className="shrink-0 rounded bg-surface-strong px-1 py-px font-mono text-10 text-text-secondary ">
                                {c.shortHash}
                            </span>
                            <span className="min-w-0 flex-1 truncate text-text-secondary ">
                                {c.subject}
                            </span>
                            <span className="shrink-0 text-10 text-text-tertiary ">
                                {c.author}
                            </span>
                        </button>
                    );
                })}
            </div>

            {/* Pagination footer — only when more than one page.
             *  Same chrome as the session list dropdown so the two
             *  "list of items" surfaces in the app speak one visual
             *  language. */}
            {showChrome && pagination && (
                <div className="flex items-center justify-between border-t border-right-panel-border px-2 py-1.5 shrink-0">
                    <button
                        type="button"
                        onClick={() => setPage((p) => Math.max(1, p - 1))}
                        disabled={pagination.currentPage <= 1 || loading2}
                        aria-label={t("gitStatus.commitPickerPrevPage")}
                        className="inline-flex items-center rounded-md px-1.5 py-0.5 text-text-tertiary hover:bg-[var(--color-accent)]/10 disabled:opacity-30  dark:hover:bg-[var(--color-accent)]/15"
                    >
                        <ChevronLeft className="h-3.5 w-3.5" />
                    </button>
                    <span className="text-11 text-text-tertiary ">
                        {t("gitStatus.commitPickerPageOf", {
                            current: pagination.currentPage,
                            total: pagination.totalPages,
                        })}
                    </span>
                    <button
                        type="button"
                        onClick={() =>
                            setPage((p) => Math.min(pagination.totalPages, p + 1))
                        }
                        disabled={
                            pagination.currentPage >= pagination.totalPages || loading2
                        }
                        aria-label={t("gitStatus.commitPickerNextPage")}
                        className="inline-flex items-center rounded-md px-1.5 py-0.5 text-text-tertiary hover:bg-[var(--color-accent)]/10 disabled:opacity-30  dark:hover:bg-[var(--color-accent)]/15"
                    >
                        <ChevronRight className="h-3.5 w-3.5" />
                    </button>
                </div>
            )}
        </div>
    );
}