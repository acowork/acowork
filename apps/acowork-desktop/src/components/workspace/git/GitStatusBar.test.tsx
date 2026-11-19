/**
 * ADR-078 frontend tests — GitStatusBar.
 *
 * Collapsible version-control strip. Covers:
 *   1. Title derivation: default (pre-load), `branch · N changes`, notRepo,
 *      and branch-less (`N changes`) states.
 *   2. Loading affordance (Loader2) vs. manual-refresh (RefreshCw).
 *   3. Expand/collapse toggle → setExpanded with the correct flip.
 *   4. aria-expanded reflects state.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";

// ── Mocks ────────────────────────────────────────────────────────────────

const translations: Record<string, string> = {
  "gitStatusBar.title": "Git",
  "gitStatusBar.notRepo": "Not a Git repository",
  "gitStatusBar.changes": "changes",
  "gitStatusBar.history": "Show commit history",
  "gitStatusBar.refresh": "Refresh",
};

vi.mock("../../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string) => translations[key] ?? key,
  }),
}));

/** Stub CommitPicker so the history-dropdown tests can assert lifecycle
 *  (mount / unmount / onSelect) without depending on the real picker's
 *  fetchLog + DOM positioning logic. The picker is exercised end-to-end
 *  in `CommitPicker.test.tsx`. */
const pickerProps: Array<{
  anchorEl: HTMLElement | null;
  currentRef: string;
  allowWorkingTree: boolean;
  placement?: "top" | "bottom";
  onSelect: (ref: string, label: string) => void;
  onClose: () => void;
}> = [];
vi.mock("../../editor/CommitPicker", () => ({
  CommitPicker: (props: (typeof pickerProps)[number]) => {
    pickerProps.push(props);
    return (
      <div
        data-testid="commit-picker"
        data-allow-working-tree={props.allowWorkingTree}
        data-current-ref={props.currentRef}
        data-placement={props.placement}
      >
        <button
          data-testid="picker-select-commit"
          onClick={() => props.onSelect("abc1234", "abc1234")}
        >
          pick abc1234
        </button>
        <button
          data-testid="picker-select-worktree"
          onClick={() => props.onSelect("", "Working Tree")}
        >
          pick Working Tree
        </button>
        <button data-testid="picker-close" onClick={() => props.onClose()}>
          close
        </button>
      </div>
    );
  },
}));

/** Fake gitStore state fed through the selector pattern the component uses. */
const mocks = {
  expandedKey: null as string | null,
  viewingRev: {} as Record<string, string>,
  isExpanded: vi.fn(() => false),
  status: {} as Record<string, unknown>,
  setExpanded: vi.fn(),
  refresh: vi.fn(),
  setViewingRev: vi.fn(),
};

vi.mock("../../../stores/gitStore", () => ({
  gitGroupKey: (a: string, w: string) => `${a}\u0000${w}`,
  gitViewKey: (group: string, rev: string) => (rev ? `${group}|${rev}` : group),
  useGitStore: Object.assign(
    (selector: (s: Record<string, unknown>) => unknown) =>
      selector(mocks as unknown as Record<string, unknown>),
    { getState: () => mocks as unknown as Record<string, unknown> },
  ),
}));

// ── SUT ──────────────────────────────────────────────────────────────────

import { GitStatusBar } from "./GitStatusBar";

const KEY = "a1\u0000ws1";

function setEntry(entry: Record<string, unknown> | undefined) {
  mocks.status[KEY] = entry;
}

beforeEach(() => {
  mocks.expandedKey = null;
  mocks.isExpanded.mockReset().mockReturnValue(false);
  mocks.setExpanded.mockReset();
  mocks.refresh.mockReset();
  mocks.setViewingRev.mockReset();
  mocks.viewingRev = {};
  setEntry(undefined);
  pickerProps.length = 0;
});

describe("GitStatusBar", () => {
  it("shows the default title before data loads", () => {
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    expect(screen.getByText("Git")).toBeTruthy();
    expect(screen.getByTestId("git-status-bar").getAttribute("aria-label")).toBe(
      "Git",
    );
  });

  it("shows `branch · N changes` when a repo with changes is loaded", () => {
    setEntry({
      data: {
        isRepo: true,
        branch: "main",
        error: null,
        truncated: false,
        changes: [{}, {}],
      },
      loading: false,
    });
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    // The ref and its meta suffix are two spans, so assert on the
    // wrapper's textContent — that is what the user actually reads.
    expect(screen.getByTestId("git-status-bar").textContent)
      .toContain("main · 2 changes");
  });

  // Regression: the bar used to be `uppercase`, which mangled the branch
  // name (`feature/Foo_Bar` → `FEATURE/FOO_BAR`) and made it impossible
  // to eyeball-match against `git branch` output. The ref AND its meta
  // suffix must both stay normal case.
  it("renders the branch name and meta in original case (no uppercase)", () => {
    setEntry({
      data: {
        isRepo: true,
        branch: "feature/Foo_Bar",
        error: null,
        truncated: false,
        changes: [{}],
      },
      loading: false,
    });
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    const bar = screen.getByTestId("git-status-bar");
    expect(bar.textContent).toContain("feature/Foo_Bar · 1 changes");
    expect(bar.textContent).not.toMatch(/FEATURE|CHANGES/);
    // Nothing in the strip may re-introduce the uppercase transform.
    expect(bar.className).not.toContain("uppercase");
  });

  it("shows `N changes` when the repo has no branch name", () => {
    setEntry({
      data: { isRepo: true, branch: null, error: null, truncated: false, changes: [{}] },
      loading: false,
    });
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    expect(screen.getByText("1 changes")).toBeTruthy();
  });

  it("renders nothing when the workspace is not a git repository", () => {
    // A workspace with no repo has nothing for the strip to say: the
    // strip is not drawn at all, rather than drawn saying "Not a Git
    // repository" in the middle of the file tree's header stack.
    setEntry({
      data: { isRepo: false, branch: null, error: "not_a_repo", truncated: false, changes: [] },
      loading: false,
    });
    const { container } = render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    expect(container.querySelector("[data-testid='git-status-bar']")).toBeNull();
    expect(screen.queryByText("Not a Git repository")).toBeNull();
  });

  it("renders a spinner while loading (no refresh button)", () => {
    setEntry({ data: null, loading: true });
    const { container } = render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    expect(container.querySelector(".animate-spin")).toBeTruthy();
    // No manual-refresh / history affordance during an in-flight fetch.
    expect(screen.queryByTestId("git-status-bar-history")).toBeNull();
    expect(container.querySelector("svg.lucide-refresh-cw")).toBeNull();
  });

  it("clicking the bar expands a collapsed group", () => {
    mocks.isExpanded.mockReturnValue(false);
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    fireEvent.click(screen.getByTestId("git-status-bar"));
    expect(mocks.setExpanded).toHaveBeenCalledWith("a1", "ws1", true);
  });

  it("clicking the bar collapses an expanded group", () => {
    mocks.isExpanded.mockReturnValue(true);
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    fireEvent.click(screen.getByTestId("git-status-bar"));
    expect(mocks.setExpanded).toHaveBeenCalledWith("a1", "ws1", false);
    expect(
      screen.getByTestId("git-status-bar").getAttribute("aria-expanded"),
    ).toBe("true");
  });

  it("manual refresh button triggers a status refresh without toggling", () => {
    setEntry({
      data: { isRepo: true, branch: "main", error: null, truncated: false, changes: [] },
      loading: false,
    });
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    const bar = screen.getByTestId("git-status-bar");
    // RefreshCw renders inside the button; find it by class.
    const refreshIcon = bar.querySelector("svg.lucide-refresh-cw");
    expect(refreshIcon).toBeTruthy();
    fireEvent.click(refreshIcon!);
    expect(mocks.refresh).toHaveBeenCalledWith("a1", "ws1");
    // Toggle must NOT have fired (stopPropagation).
    expect(mocks.setExpanded).not.toHaveBeenCalled();
  });

  it("clears a stale expansion from another group on mount (releases old fs-watch)", () => {
    // Scenario: the bar mounted with the workspace panel collapsed / no file
    // open, but `expandedKey` still points at a DIFFERENT group from a
    // previous agent/workspace. The demand-driven fs-watch subscription
    // would keep watching that stale group (ADR-078 invariant 6) — the bar
    // must collapse it on mount.
    mocks.expandedKey = "b1\u0000ws9";
    mocks.isExpanded.mockReturnValue(false); // this group not expanded
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    expect(mocks.setExpanded).toHaveBeenCalledWith("a1", "ws1", false);
  });

  it("keeps an own-group expansion intact on mount", () => {
    mocks.expandedKey = "a1\u0000ws1";
    mocks.isExpanded.mockReturnValue(true);
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    expect(mocks.setExpanded).not.toHaveBeenCalled();
  });

  it("collapses on unmount when this group is expanded (panel no longer visible)", () => {
    mocks.isExpanded.mockReturnValue(true); // expanded while mounted
    const { unmount } = render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    expect(mocks.setExpanded).not.toHaveBeenCalled(); // no-op while mounted
    unmount();
    // Panel disappeared (active file closed / agent switched) → the
    // subscription must be released, not left watching in the background.
    expect(mocks.setExpanded).toHaveBeenCalledWith("a1", "ws1", false);
  });

  it("collapses a stale other-group expansion exactly once (setup), not again on unmount", () => {
    // A stale group from a previous agent/workspace must be collapsed on
    // mount (fs-watch released). Once cleared, unmount must NOT repeat it.
    mocks.expandedKey = "b1\u0000ws9";
    mocks.isExpanded.mockReturnValue(false);
    const { unmount } = render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    expect(mocks.setExpanded).toHaveBeenCalledTimes(1);
    expect(mocks.setExpanded).toHaveBeenCalledWith("a1", "ws1", false);
    unmount();
    expect(mocks.setExpanded).toHaveBeenCalledTimes(1); // no repeat
  });

  it("auto-refreshes status on mount so the banner shows branch info without a click", () => {
    // Bug fix: the collapsed banner previously stayed on the default "Git"
    // title until the user clicked to expand. FileTree.tsx already mounts
    // its fetchTree in the same shape — mirror it here so the banner
    // converges on the current branch immediately.
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    expect(mocks.refresh).toHaveBeenCalledTimes(1);
    expect(mocks.refresh).toHaveBeenCalledWith("a1", "ws1");
  });

  it("auto-refreshes status when the workspace switches", () => {
    // Workspace switch (selected workspace changes inside the same agent)
    // must re-fetch git status for the new group so the banner doesn't
    // keep showing the previous workspace's branch.
    const { rerender } = render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    expect(mocks.refresh).toHaveBeenCalledTimes(1);
    expect(mocks.refresh).toHaveBeenLastCalledWith("a1", "ws1");

    rerender(<GitStatusBar agentId="a1" workspaceId="ws2" />);
    expect(mocks.refresh).toHaveBeenCalledTimes(2);
    expect(mocks.refresh).toHaveBeenLastCalledWith("a1", "ws2");
  });

  it("auto-refreshes status when the agent switches", () => {
    // Agent switch (the entire WorkspaceExplorer stays mounted, only the
    // (agent, workspace) group changes) must re-fetch for the new group.
    const { rerender } = render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    expect(mocks.refresh).toHaveBeenLastCalledWith("a1", "ws1");

    rerender(<GitStatusBar agentId="a2" workspaceId="ws1" />);
    expect(mocks.refresh).toHaveBeenCalledTimes(2);
    expect(mocks.refresh).toHaveBeenLastCalledWith("a2", "ws1");
  });

  it("does not re-fetch when only an unrelated re-render leaves props stable", () => {
    // The refresh selector returns a stable zustand action — re-rendering
    // with the same props must NOT cause a duplicate fetch (inflight dedup
    // would already swallow it, but skipping the call is cheaper).
    const { rerender } = render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    expect(mocks.refresh).toHaveBeenCalledTimes(1);
    rerender(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    expect(mocks.refresh).toHaveBeenCalledTimes(1);
  });

  // ── History dropdown (ADR-XXX) ────────────────────────────────────────

  it("renders a History button next to Refresh when not loading", () => {
    setEntry({
      data: { isRepo: true, branch: "main", error: null, truncated: false, changes: [] },
      loading: false,
    });
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    expect(screen.getByTestId("git-status-bar-history")).toBeTruthy();
    expect(screen.getByLabelText("Show commit history")).toBeTruthy();
    expect(screen.getByLabelText("Refresh")).toBeTruthy();
    // Picker must NOT mount until the user clicks the History button.
    expect(pickerProps).toHaveLength(0);
  });

  it("opens the commit picker on history click and passes allowWorkingTree + currentRef", () => {
    setEntry({
      data: { isRepo: true, branch: "main", error: null, truncated: false, changes: [] },
      loading: false,
    });
    mocks.viewingRev = { "a1\u0000ws1": "deadbeef" };
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    fireEvent.click(screen.getByTestId("git-status-bar-history"));
    expect(pickerProps).toHaveLength(1);
    expect(pickerProps[0].allowWorkingTree).toBe(true);
    expect(pickerProps[0].currentRef).toBe("deadbeef");
    // Repo-wide history (no file scope) — the picker passes "" so the
    // fetchLog endpoint skips the path param.
    expect(pickerProps[0].anchorEl).toBe(screen.getByTestId("git-status-bar-history"));
  });

  it("commits the picked hash to setViewingRev and closes the popover", () => {
    setEntry({
      data: { isRepo: true, branch: "main", error: null, truncated: false, changes: [] },
      loading: false,
    });
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    fireEvent.click(screen.getByTestId("git-status-bar-history"));
    // Pick from the (mocked) picker list.
    fireEvent.click(screen.getByTestId("picker-select-commit"));
    expect(mocks.setViewingRev).toHaveBeenCalledWith("a1", "ws1", "abc1234");
    // Popover should unmount — no CommitPicker rendered anymore.
    expect(screen.queryByTestId("commit-picker")).toBeNull();
  });

  it("commits the empty ref when the user picks Local Working Tree", () => {
    setEntry({
      data: { isRepo: true, branch: "main", error: null, truncated: false, changes: [] },
      loading: false,
    });
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    fireEvent.click(screen.getByTestId("git-status-bar-history"));
    fireEvent.click(screen.getByTestId("picker-select-worktree"));
    // The store action is the source of truth for clearing the view.
    expect(mocks.setViewingRev).toHaveBeenCalledWith("a1", "ws1", "");
  });

  it("toggles the popover closed on a second click of the History button", () => {
    setEntry({
      data: { isRepo: true, branch: "main", error: null, truncated: false, changes: [] },
      loading: false,
    });
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    const btn = screen.getByTestId("git-status-bar-history");
    fireEvent.click(btn);
    expect(pickerProps).toHaveLength(1);
    // StopPropagation inside the History span must NOT have toggled the
    // bar's expand/collapse state.
    expect(mocks.setExpanded).not.toHaveBeenCalled();
    fireEvent.click(btn);
    expect(screen.queryByTestId("commit-picker")).toBeNull();
  });

  it("renders the branch header (still the same `branch · N changes`) when viewing a commit", () => {
    // The backend rewrites `branch` to "<short_sha> <subject>" for any
    // rev-view fetch (see git_query_impl::status_for_commit). The bar's
    // title doesn't care whether the label is a branch or a commit — it
    // just renders `branch · N changes` for both, so existing tests stay
    // green and the user sees the commit they picked. The entry is
    // stored under the view-key (groupKey|rev) — not the bare groupKey —
    // because fetchStatus writes there; the bar reads from the same
    // slot.
    mocks.viewingRev = { "a1\u0000ws1": "abc1234full" };
    mocks.status["a1\u0000ws1|abc1234full"] = {
      data: {
        isRepo: true,
        branch: "abc1234 add feature",
        error: null,
        truncated: false,
        changes: [{}, {}],
        rev: "abc1234full",
      },
      loading: false,
    };
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    expect(screen.getByTestId("git-status-bar").textContent)
      .toContain("abc1234 add feature · 2 changes");
  });

  // ── History dropdown placement ──────────────────────────────────────

  // The Git status bar lives at the bottom of the workspace panel
  // (WorkspaceExplorer.tsx L933 — "lives at the bottom"); above the
  // bar is the FileTree, below it is either the file list (when
  // expanded) or the panel's bottom edge / chat input area (when
  // collapsed). Either way there is no room to open the ~288px
  // CommitPicker downward without clipping off-screen or being
  // covered by the file list — anchor it ABOVE the bar in BOTH
  // states.

  it("opens the history popover downward when the panel is collapsed", () => {
    // The strip now sits in the panel's HEADER stack, so the space
    // below it is the search row and the file tree — the dropdown
    // opens downward in every state.
    mocks.isExpanded.mockReturnValue(false);
    setEntry({
      data: { isRepo: true, branch: "main", error: null, truncated: false, changes: [] },
      loading: false,
    });
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    fireEvent.click(screen.getByTestId("git-status-bar-history"));
    expect(pickerProps).toHaveLength(1);
    expect(pickerProps[0].placement).toBe("bottom");
  });

  it("opens the history popover downward when the expanded file list has 6+ rows", () => {
    // A long file list pushes the banner well above the panel bottom
    // — there's room to open downward without clipping off-screen,
    // and upward would cover the workspace FileTree.
    mocks.isExpanded.mockReturnValue(true);
    setEntry({
      data: {
        isRepo: true,
        branch: "main",
        error: null,
        truncated: false,
        changes: [{}, {}, {}, {}, {}, {}],
      },
      loading: false,
    });
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    fireEvent.click(screen.getByTestId("git-status-bar-history"));
    expect(pickerProps[0].placement).toBe("bottom");
  });

  it("opens the history popover downward when the expanded file list is short (<6 rows)", () => {
    // The strip moved into the panel's HEADER stack (between the
    // workspace selector and the file search), so the space below it is
    // the search row and the file tree — the dropdown opens downward
    // regardless of how long the change list is. It used to sit at the
    // panel's bottom edge and flip upward when the panel was short.
    mocks.isExpanded.mockReturnValue(true);
    setEntry({
      data: {
        isRepo: true,
        branch: "main",
        error: null,
        truncated: false,
        changes: [{}],
      },
      loading: false,
    });
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    fireEvent.click(screen.getByTestId("git-status-bar-history"));
    expect(pickerProps[0].placement).toBe("bottom");
  });

  it("opens the history popover downward at the 5-row boundary", () => {
    mocks.isExpanded.mockReturnValue(true);
    setEntry({
      data: {
        isRepo: true,
        branch: "main",
        error: null,
        truncated: false,
        changes: [{}, {}, {}, {}, {}],
      },
      loading: false,
    });
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    fireEvent.click(screen.getByTestId("git-status-bar-history"));
    expect(pickerProps[0].placement).toBe("bottom");
  });

  it("opens the history popover downward when collapsed even with 6+ rows", () => {
    mocks.isExpanded.mockReturnValue(false);
    setEntry({
      data: {
        isRepo: true,
        branch: "main",
        error: null,
        truncated: false,
        changes: [{}, {}, {}, {}, {}, {}],
      },
      loading: false,
    });
    render(<GitStatusBar agentId="a1" workspaceId="ws1" />);
    fireEvent.click(screen.getByTestId("git-status-bar-history"));
    expect(pickerProps[0].placement).toBe("bottom");
  });
});
