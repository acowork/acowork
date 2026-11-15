/**
 * GitVirtualNav — floating overlay for ADR-078 virtual diff / log
 * tabs in FileEditorPanel.
 *
 * Mounted inside the editor area only when the active tab has
 * `kind === "diff" | "log"`. Renders two-to-three side-by-side
 * buttons at the top-right corner (`top-10 right-4`), stacked
 * horizontally via a `flex gap-2` container.
 *
 * Why top-10 (40px) and not top-3 (12px): the floating overlay is
 * positioned relative to the same `relative` div that holds the diff
 * banner ("HEAD | src/foo.ts" / "Working Tree" row) at the top of
 * the editor area. With `top-3` the buttons sat at y=12..40px which
 * lands the upper half of each button on top of the banner — visually
 * it looked like the buttons were clipping the banner. 40px clears
 * the ~25-30px-tall banner (text-11 + py-1 + border-b) with a
 * ~10-15px visual gap so the banner reads as a separate strip.
 *
 *   - **diff tabs** — three buttons, left-to-right: a diff-mode toggle
 *     (`<Columns2>` / `<Rows2>`) for switching between inline and
 *     side-by-side (ADR-078 extension; state on OpenFile.diffInlineMode,
 *     `true` = inline / default), then `↑ Previous change` /
 *     `↓ Next change` wired to Monaco's
 *     `IStandaloneDiffEditor.goToDiff("previous" | "next")`. On mount
 *     we call `revealFirstDiff()` so the user lands on the first
 *     hunk immediately. Buttons disable at the file's first / last
 *     hunk — no cycling (Monaco's goToDiff otherwise wraps around and
 *     the user can't tell head from tail).
 *
 *   - **log tabs** — `↑ Previous commits` / `↓ Next commits` for
 *     BIDIRECTIONAL pagination over a local cache:
 *       - `loadedCommits` (OpenFile) is the ever-growing server fetch.
 *       - `displayedLimit` (OpenFile) is the window size we render.
 *       - `reachedEnd` (OpenFile) flips true the moment a fetch returns
 *         fewer commits than asked for; Next then disables itself
 *         WITHOUT overwriting the cache. So clicking Next at the end
 *         no longer nukes the display into "no commits yet" — Prev
 *         still restores the previously shown window.
 */

import { useCallback, useEffect, useState } from "react";
import type { editor } from "monaco-editor";
import { ChevronDown, ChevronUp, Columns2, Rows2 } from "lucide-react";
import { useGitStore, type GitCommitDto } from "../../stores/gitStore";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { useTranslation } from "../../i18n/useTranslation";
import type { OpenFile } from "../../stores/fileEditorStore";

interface GitVirtualNavProps {
    /** The active virtual tab — must have `kind === "diff" | "log"`. */
    file: OpenFile;
    /**
     * Diff editor instance — REQUIRED for diff tabs (the Up/Down
     * buttons dispatch `goToDiff` on it). Pass `null` for log tabs
     * or while the DiffEditor is still mounting.
     */
    diffEditor: editor.IStandaloneDiffEditor | null;
}

/** Hard cap that mirrors the Runtime's LOG_MAX_LIMIT in
 *  core/acowork-runtime/src/usecases/git_query_impl.rs. Don't raise
 *  without raising the backend first. */
const LOG_MAX_LIMIT = 200;
/** Initial load size + step. Matches the limit GitStatusPanel.openLog
 *  passes to its first fetchLog, so the seed and the step agree. */
const LOG_STEP = 50;

const BUTTON_CLASS =
    "rounded-full bg-surface-hover border border-border-outer shadow-md p-1.5 opacity-90 hover:opacity-100 focus-visible:opacity-100 hover:bg-[var(--color-accent)]/10 dark:hover:bg-[var(--color-accent)]/15 transition-all animate-in fade-in zoom-in disabled:opacity-20 disabled:cursor-not-allowed";

const CONTAINER_CLASS =
    "absolute top-10 right-4 z-10 flex items-center gap-2";

interface HunkLike {
    modifiedStartLineNumber: number;
}

/** Pure helper — exported for tests. "Are we at the first / last
 *  hunk given the current cursor line?" Returns
 *  `{ atFirst, atLast, hasHunks }`. An empty list yields all-false. */
export function computeHunkBounds(
    hunks: readonly HunkLike[],
    cursorLine: number,
): { atFirst: boolean; atLast: boolean; hasHunks: boolean } {
    if (hunks.length === 0) {
        return { atFirst: false, atLast: false, hasHunks: false };
    }
    const first = hunks[0].modifiedStartLineNumber;
    const last = hunks[hunks.length - 1].modifiedStartLineNumber;
    return {
        atFirst: cursorLine <= first,
        atLast: cursorLine >= last,
        hasHunks: true,
    };
}

export function GitVirtualNav({ file, diffEditor }: GitVirtualNavProps) {
    const { t } = useTranslation();

    const fetchLog = useGitStore((s) => s.fetchLog);
    const setVirtualFileContent = useFileEditorStore(
        (s) => s.setVirtualFileContent,
    );
    const setDiffInlineMode = useFileEditorStore(
        (s) => s.setDiffInlineMode,
    );

    const isDiff = file.kind === "diff";
    const isLog = file.kind === "log";

    // Busy only blocks log navigation (Prev/Next commit). Diff's
    // hunk jumps are synchronous Monaco calls — no debounce needed.
    const [logBusy, setLogBusy] = useState(false);

    // ── Diff: cursor + hunk tracking ────────────────────────────────
    // Monaco computes the diff asynchronously after mount, so
    // `getLineChanges()` may return null briefly. We subscribe to
    // `onDidUpdateDiff` to re-read once it's ready, and to
    // `onDidChangeCursorPosition` to keep `cursorLine` in lockstep
    // with Monaco's cursor (so boundary detection stays accurate
    // even when the user clicks around inside the diff).
    const [cursorLine, setCursorLine] = useState(1);
    const [hunks, setHunks] = useState<readonly HunkLike[]>([]);

    useEffect(() => {
        // Defensive null-check — Monaco's DiffEditor exposes
        // `getModifiedEditor` only after mount, so a partial stub
        // (or a re-render mid-cleanup) can yield a half-initialised
        // object. Skip the subscription rather than crashing.
        if (!diffEditor?.getModifiedEditor) return;
        const modified = diffEditor.getModifiedEditor();
        if (!modified) return;

        const refresh = () => {
            setCursorLine(modified.getPosition()?.lineNumber ?? 1);
            setHunks(diffEditor.getLineChanges() ?? []);
        };

        refresh();

        const subs: { dispose(): void }[] = [];
        const upd = diffEditor.onDidUpdateDiff?.(refresh);
        if (upd) subs.push(upd);
        const cur = modified.onDidChangeCursorPosition?.((e) => {
            setCursorLine(e.position.lineNumber);
        });
        if (cur) subs.push(cur);

        return () => {
            for (const s of subs) s.dispose();
        };
    }, [diffEditor]);

    // On mount: jump to the first hunk so the user lands somewhere
    // meaningful instead of at line 1 (which is often an unchanged
    // import / copyright header). Monaco's `revealFirstDiff` waits
    // internally for the diff computation to finish, so it's safe to
    // call immediately after mount.
    useEffect(() => {
        diffEditor?.revealFirstDiff?.();
    }, [diffEditor]);

    // ── Log: bidirectional pagination ───────────────────────────────
    const loadedCommits: GitCommitDto[] = file.loadedCommits ?? [];
    const displayedLimit = file.displayedLimit ?? LOG_STEP;
    const reachedEnd = file.reachedEnd ?? false;

    const canPrev = isLog && !logBusy && displayedLimit > LOG_STEP;
    const canNext =
        isLog &&
        !logBusy &&
        displayedLimit < LOG_MAX_LIMIT &&
        !reachedEnd;

    const onPrevCommits = useCallback(() => {
        if (!canPrev) return;
        const newLimit = Math.max(displayedLimit - LOG_STEP, LOG_STEP);
        setVirtualFileContent(
            file.id,
            formatLogText(loadedCommits.slice(0, newLimit), t),
            { displayedLimit: newLimit },
        );
    }, [canPrev, displayedLimit, file.id, loadedCommits, setVirtualFileContent, t]);

    const onNextCommits = useCallback(async () => {
        if (!canNext) return;
        const newLimit = Math.min(displayedLimit + LOG_STEP, LOG_MAX_LIMIT);
        if (newLimit > loadedCommits.length) {
            // Window grew past the cache — fetch more from server.
            setLogBusy(true);
            try {
                const log = await fetchLog(
                    file.agentId,
                    file.workspaceId,
                    file.relPath,
                    newLimit,
                );
                if (log.commits.length === 0) {
                    // Server returned nothing — git refused or the
                    // path no longer has history. Don't nuke the
                    // display; just mark reachedEnd so Next disables.
                    setVirtualFileContent(
                        file.id,
                        formatLogText(loadedCommits.slice(0, displayedLimit), t),
                        { reachedEnd: true },
                    );
                    return;
                }
                if (log.commits.length < newLimit) {
                    // Server returned fewer than asked — we've reached
                    // the end. Extend cache to whatever it gave us but
                    // mark reachedEnd so Next disables.
                    setVirtualFileContent(
                        file.id,
                        formatLogText(log.commits, t),
                        {
                            loadedCommits: log.commits,
                            displayedLimit: log.commits.length,
                            reachedEnd: true,
                        },
                    );
                    return;
                }
                // Full response — extend cache and window normally.
                setVirtualFileContent(
                    file.id,
                    formatLogText(log.commits, t),
                    {
                        loadedCommits: log.commits,
                        displayedLimit: newLimit,
                    },
                );
            } catch (err) {
                console.error("[GitVirtualNav] nextCommits failed:", err);
            } finally {
                setLogBusy(false);
            }
        } else {
            // Already cached — just expand the window from local data.
            setVirtualFileContent(
                file.id,
                formatLogText(loadedCommits.slice(0, newLimit), t),
                { displayedLimit: newLimit },
            );
        }
    }, [
        canNext,
        displayedLimit,
        fetchLog,
        file.agentId,
        file.id,
        file.relPath,
        file.workspaceId,
        loadedCommits,
        setVirtualFileContent,
        t,
    ]);

    // ── Diff: click handlers ─────────────────────────────────────────
    const onJumpPrev = useCallback(() => {
        if (!diffEditor) return;
        diffEditor.goToDiff("previous");
    }, [diffEditor]);
    const onJumpNext = useCallback(() => {
        if (!diffEditor) return;
        diffEditor.goToDiff("next");
    }, [diffEditor]);
    // Flip between inline (default — single pane +/- colour blocks) and
    // side-by-side (two-pane) layouts. The store mutation cascades to
    // FileEditorPanel, which passes `!diffInlineMode` as Monaco's
    // `renderSideBySide` option — @monaco-editor/react's options-watcher
    // calls editor.updateOptions for us, so no direct Monaco call needed
    // here. Log tabs ignore this (setDiffInlineMode is a no-op for them).
    const onToggleDiffMode = useCallback(() => {
        if (!isDiff) return;
        setDiffInlineMode(file.id, !file.diffInlineMode);
    }, [isDiff, file.id, file.diffInlineMode, setDiffInlineMode]);

    // ── Render ──────────────────────────────────────────────────────
    const showContainer = isDiff ? !!diffEditor : isLog;
    if (!showContainer) return null;

    // Diff: button disabled iff cursor is at the corresponding edge.
    // Log: button disabled per canPrev / canNext (which already fold in
    //      reachedEnd + bounds).
    let prevDisabled = false;
    let nextDisabled = false;
    if (isDiff) {
        const { atFirst, atLast } = computeHunkBounds(hunks, cursorLine);
        prevDisabled = atFirst;
        nextDisabled = atLast;
    } else {
        prevDisabled = !canPrev;
        nextDisabled = !canNext;
    }

    return (
        <div className={CONTAINER_CLASS}>
            {/* View-mode toggle — diff only. Shows the icon of the
                mode you'd switch INTO (current state is encoded in
                the aria-label). Log tabs skip this button entirely
                (the icon would be misleading there). */}
            {isDiff && (
                <button
                    type="button"
                    onClick={onToggleDiffMode}
                    data-testid="git-diff-mode-toggle"
                    aria-label={
                        file.diffInlineMode
                            ? t("gitStatus.switchToSideBySide")
                            : t("gitStatus.switchToInline")
                    }
                    title={
                        file.diffInlineMode
                            ? t("gitStatus.switchToSideBySide")
                            : t("gitStatus.switchToInline")
                    }
                    className={BUTTON_CLASS}
                >
                    {file.diffInlineMode ? (
                        <Columns2 className="h-4 w-4 text-text-tertiary " />
                    ) : (
                        <Rows2 className="h-4 w-4 text-text-tertiary " />
                    )}
                </button>
            )}
            <button
                type="button"
                onClick={isDiff ? onJumpPrev : onPrevCommits}
                disabled={prevDisabled}
                aria-label={
                    isDiff
                        ? t("gitStatus.prevChange")
                        : t("gitStatus.prevCommits")
                }
                title={
                    isDiff
                        ? t("gitStatus.prevChange")
                        : t("gitStatus.prevCommits")
                }
                className={BUTTON_CLASS}
            >
                <ChevronUp className="h-4 w-4 text-text-tertiary " />
            </button>
            <button
                type="button"
                onClick={isDiff ? onJumpNext : onNextCommits}
                disabled={nextDisabled}
                aria-label={
                    isDiff
                        ? t("gitStatus.nextChange")
                        : t("gitStatus.nextCommits")
                }
                title={
                    isDiff
                        ? t("gitStatus.nextChange")
                        : t("gitStatus.nextCommits")
                }
                className={BUTTON_CLASS}
            >
                <ChevronDown className="h-4 w-4 text-text-tertiary " />
            </button>
        </div>
    );
}

/** Render the commit list — duplicated from GitStatusPanel.openLog
 *  (5 lines) to avoid a round-trip through a shared helper just to
 *  keep the rendering in one place. */
function formatLogText(
    commits: GitCommitDto[],
    t: (key: string) => string,
): string {
    return (
        commits
            .map(
                (cm) =>
                    `${cm.shortHash}  ${cm.author}  ${cm.date}\n    ${cm.subject}`,
            )
            .join("\n\n") || t("gitStatus.noCommits")
    );
}