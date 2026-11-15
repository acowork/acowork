/**
 * ADR-078 frontend tests — GitStatusPanel.
 *
 * Flat list of uncommitted changes with a right-click menu. Covers:
 *   1. Row rendering: path, status icon by state, staged badge, oldPath.
 *   2. Double-click a normal row → openFile in the editor.
 *   3. Double-click a deleted row → redirect to Show Diff (ADR-078 decision 6).
 *   4. Right-click menu → Show Diff / Show Log / Open in editor actions.
 *   5. Empty states: clean, notRepo, gitUnavailable, loading, error.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, within } from "@testing-library/react";

// ── Mocks ────────────────────────────────────────────────────────────────

const translations: Record<string, string> = {
  "gitStatus.showDiff": "Show Diff",
  "gitStatus.showLog": "Show Log",
  "gitStatus.openInEditor": "Open in Editor",
  "gitStatus.revert": "Revert",
  "gitStatus.revertConfirmTitle": "Revert changes?",
  "gitStatus.loading": "Loading…",
  "gitStatus.notRepo": "Not a Git repository",
  "gitStatus.gitUnavailable": "Git unavailable",
  "gitStatus.clean": "No changes",
  "gitStatus.staged": "staged",
  "gitStatus.renamed": "renamed",
  "gitStatus.added": "added",
  "gitStatus.conflicted": "conflicted",
  "gitStatus.deleted": "deleted",
  "gitStatus.untracked": "untracked",
  "gitStatus.modified": "modified",
  "gitStatus.noCommits": "No commits yet",
};

vi.mock("../../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string) => translations[key] ?? key,
  }),
}));

/** Mutable gitStore mock — both the selector form and getState() form. */
const gitStoreMocks = {
  status: {} as Record<string, unknown>,
  viewingRev: {} as Record<string, string>,
  fetchDiff: vi.fn(),
  fetchLog: vi.fn(),
  revertFile: vi.fn(),
  refresh: vi.fn(),
};

vi.mock("../../../stores/gitStore", () => ({
  gitGroupKey: (a: string, w: string) => `${a}\u0000${w}`,
  gitViewKey: (group: string, rev: string) => (rev ? `${group}|${rev}` : group),
  useGitStore: Object.assign(
    (selector: (s: typeof gitStoreMocks) => unknown) => selector(gitStoreMocks),
    { getState: () => gitStoreMocks },
  ),
}));

const fileEditorMocks = {
  openFile: vi.fn(),
  openVirtualFile: vi.fn(),
};

vi.mock("../../../stores/fileEditorStore", () => ({
  useFileEditorStore: { getState: () => fileEditorMocks },
}));

// ── SUT ──────────────────────────────────────────────────────────────────

import { GitStatusPanel } from "./GitStatusPanel";
import type { GitStatusResponse } from "../../../stores/gitStore";

const KEY = "a1\u0000ws1";

function setEntry(entry: Record<string, unknown> | undefined) {
  gitStoreMocks.status[KEY] = entry;
}

/** Mirror of `setEntry` that writes to the view-keyed slot the bar's
 *  history dropdown fills. Mirrors production: `fetchStatus(..., rev)`
 *  stores under `gitViewKey(group, rev)`. */
function setViewEntry(rev: string, entry: Record<string, unknown> | undefined) {
  gitStoreMocks.status[`${KEY}|${rev}`] = entry;
}

function okResponse(body: Record<string, unknown>) {
  return Promise.resolve(body);
}

beforeEach(() => {
  gitStoreMocks.viewingRev = {};
  for (const k of Object.keys(gitStoreMocks.status)) delete gitStoreMocks.status[k];
  setEntry(undefined);
  gitStoreMocks.fetchDiff.mockReset();
  gitStoreMocks.fetchLog.mockReset();
  gitStoreMocks.revertFile.mockReset();
  gitStoreMocks.refresh.mockReset();
  fileEditorMocks.openFile.mockReset();
  fileEditorMocks.openVirtualFile.mockReset();
});

describe("GitStatusPanel", () => {
  it("renders change rows with path and staged badge", () => {
    setEntry({
      data: {
        isRepo: true,
        branch: "main",
        error: null,
        truncated: false,
        changes: [
          {
            path: "src/a.ts",
            oldPath: null,
            index: "modified",
            worktree: "modified",
            staged: true,
          },
        ],
      },
      loading: false,
    });
    render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    expect(screen.getByText("src/a.ts")).toBeTruthy();
    expect(screen.getByText("staged")).toBeTruthy();
  });

  it("shows oldPath for renames", () => {
    setEntry({
      data: {
        isRepo: true,
        branch: "main",
        error: null,
        truncated: false,
        changes: [
          {
            path: "src/b.ts",
            oldPath: "src/a.ts",
            index: "renamed",
            worktree: "modified",
            staged: true,
          },
        ],
      },
      loading: false,
    });
    render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    expect(screen.getByText("← src/a.ts")).toBeTruthy();
  });

  it("double-clicking a normal row opens the file in the editor", () => {
    setEntry({
      data: {
        isRepo: true,
        branch: "main",
        error: null,
        truncated: false,
        changes: [
          {
            path: "src/a.ts",
            oldPath: null,
            index: "modified",
            worktree: "modified",
            staged: false,
          },
        ],
      },
      loading: false,
    });
    render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    fireEvent.doubleClick(screen.getByTestId("git-status-row"));
    expect(fileEditorMocks.openFile).toHaveBeenCalledWith("a1", "ws1", "src/a.ts");
  });

  it("double-clicking a deleted row redirects to Show Diff (decision 6)", async () => {
    setEntry({
      data: {
        isRepo: true,
        branch: "main",
        error: null,
        truncated: false,
        changes: [
          {
            path: "gone.ts",
            oldPath: null,
            index: "deleted",
            worktree: "deleted",
            staged: false,
          },
        ],
      },
      loading: false,
    });
    gitStoreMocks.fetchDiff.mockReturnValue(
      okResponse({ kind: "deleted", original: "HEAD content", modified: "" }),
    );
    render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    fireEvent.doubleClick(screen.getByTestId("git-status-row"));

    await vi.waitFor(() => {
      expect(gitStoreMocks.fetchDiff).toHaveBeenCalledWith("a1", "ws1", "gone.ts", "HEAD", "");
    });
    await vi.waitFor(() => {
      expect(fileEditorMocks.openVirtualFile).toHaveBeenCalledWith(
        expect.objectContaining({
          agentId: "a1",
          workspaceId: "ws1",
          kind: "diff",
          relPath: "gone.ts",
          content: "",
          original: "HEAD content",
          gitDiffKind: "deleted",
        }),
      );
    });
    // Deleted rows must NOT try to open the (missing) file.
    expect(fileEditorMocks.openFile).not.toHaveBeenCalled();
  });

  it("right-click opens the context menu with Show Diff / Show Log / Open", async () => {
    setEntry({
      data: {
        isRepo: true,
        branch: "main",
        error: null,
        truncated: false,
        changes: [
          {
            path: "src/a.ts",
            oldPath: null,
            index: "modified",
            worktree: "modified",
            staged: false,
          },
        ],
      },
      loading: false,
    });
    gitStoreMocks.fetchDiff.mockReturnValue(
      okResponse({ kind: "modified", original: "old", modified: "new" }),
    );
    gitStoreMocks.fetchLog.mockReturnValue(
      okResponse({
        commits: [
          {
            hash: "abc123",
            shortHash: "abc123",
            author: "me",
            date: "2025-01-01",
            subject: "init",
          },
        ],
      }),
    );

    render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    fireEvent.contextMenu(screen.getByTestId("git-status-row"), {
      clientX: 10,
      clientY: 20,
    });

    expect(screen.getByText("Show Diff")).toBeTruthy();
    expect(screen.getByText("Show Log")).toBeTruthy();
    expect(screen.getByText("Open in Editor")).toBeTruthy();

    // Show Diff action → fetchDiff + openVirtualFile(kind: diff).
    fireEvent.click(screen.getByText("Show Diff"));
    await vi.waitFor(() => {
      expect(gitStoreMocks.fetchDiff).toHaveBeenCalledWith("a1", "ws1", "src/a.ts", "HEAD", "");
    });
    await vi.waitFor(() => {
      expect(fileEditorMocks.openVirtualFile).toHaveBeenCalledWith(
        expect.objectContaining({
          agentId: "a1",
          workspaceId: "ws1",
          kind: "diff",
          relPath: "src/a.ts",
          gitDiffKind: "modified",
        }),
      );
    });
  });

  it("right-click Revert discards changes after confirmation and refreshes", async () => {
    setEntry({
      data: {
        isRepo: true,
        branch: "main",
        error: null,
        truncated: false,
        changes: [
          {
            path: "src/a.ts",
            oldPath: null,
            index: "modified",
            worktree: "modified",
            staged: false,
          },
        ],
      },
      loading: false,
    });
    gitStoreMocks.revertFile.mockReturnValue(
      okResponse({ path: "src/a.ts", oldPath: null }),
    );
    gitStoreMocks.refresh.mockReturnValue(Promise.resolve());

    render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    fireEvent.contextMenu(screen.getByTestId("git-status-row"), {
      clientX: 10,
      clientY: 20,
    });
    expect(screen.getByText("Revert")).toBeTruthy();

    // Menu item → destructive confirm dialog (not yet reverted).
    fireEvent.click(screen.getByText("Revert"));
    const dialog = screen.getByRole("alertdialog");
    expect(dialog.textContent).toContain("Revert changes?");
    expect(gitStoreMocks.revertFile).not.toHaveBeenCalled();

    // Confirm → revertFile + working-tree refresh.
    fireEvent.click(within(dialog).getByText("Revert"));
    await vi.waitFor(() => {
      expect(gitStoreMocks.revertFile).toHaveBeenCalledWith("a1", "ws1", "src/a.ts", null);
    });
    await vi.waitFor(() => {
      expect(gitStoreMocks.refresh).toHaveBeenCalledWith("a1", "ws1");
    });
  });

  it("right-click Show Log opens a log virtual tab", async () => {
    setEntry({
      data: {
        isRepo: true,
        branch: "main",
        error: null,
        truncated: false,
        changes: [
          {
            path: "src/a.ts",
            oldPath: null,
            index: "modified",
            worktree: "modified",
            staged: false,
          },
        ],
      },
      loading: false,
    });
    gitStoreMocks.fetchLog.mockReturnValue(
      okResponse({
        commits: [
          {
            hash: "abc123",
            shortHash: "abc123",
            author: "me",
            date: "2025-01-01",
            subject: "init",
          },
        ],
      }),
    );

    render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    fireEvent.contextMenu(screen.getByTestId("git-status-row"), {
      clientX: 10,
      clientY: 20,
    });
    fireEvent.click(screen.getByText("Show Log"));

    await vi.waitFor(() => {
      expect(gitStoreMocks.fetchLog).toHaveBeenCalledWith("a1", "ws1", "src/a.ts", 50);
    });
    await vi.waitFor(() => {
      expect(fileEditorMocks.openVirtualFile).toHaveBeenCalledTimes(1);
    });
    const call = fileEditorMocks.openVirtualFile.mock.calls[0][0];
    expect(call.kind).toBe("log");
    expect(call.relPath).toBe("src/a.ts");
    expect(call.language).toBe("plaintext");
    expect(call.content).toContain("abc123");
    expect(call.content).toContain("init");
  });

  it("renders the clean empty state", () => {
    setEntry({
      data: { isRepo: true, branch: "main", error: null, truncated: false, changes: [] },
      loading: false,
    });
    render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    expect(screen.getByText("No changes")).toBeTruthy();
  });

  it("renders notRepo / gitUnavailable states", () => {
    setEntry({
      data: { isRepo: false, branch: null, error: "not_a_repo", truncated: false, changes: [] },
      loading: false,
    });
    const { unmount } = render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    expect(screen.getByText("Not a Git repository")).toBeTruthy();
    unmount();

    setEntry({
      data: { isRepo: false, branch: null, error: "git_unavailable", truncated: false, changes: [] },
      loading: false,
    });
    render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    expect(screen.getByText("Git unavailable")).toBeTruthy();
  });

  it("renders loading and error states", () => {
    setEntry({ data: null, loading: true, error: null });
    const { unmount } = render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    expect(screen.getAllByText("Loading…").length).toBeGreaterThan(0);
    unmount();

    setEntry({ data: null, loading: false, error: "boom 500" });
    render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    expect(screen.getByText("boom 500")).toBeTruthy();
  });

  // A view-only guest gets 403 on the git endpoints. Red alarm styling
  // reads as "something broke / retry" — but there is nothing they can
  // do; it must sit in the same neutral tone as "not a repo".
  it("renders a 403 denial neutrally, not as a red error", () => {
    const { container, unmount } = render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    unmount();

    setEntry({
      data: null,
      loading: false,
      error: "This action requires use access",
      errorStatus: 403,
    });
    const { container: deniedBox } = render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    expect(screen.getByText("This action requires use access")).toBeTruthy();
    expect(deniedBox.innerHTML).not.toContain("text-danger-icon");
    expect(deniedBox.innerHTML).toContain("text-text-tertiary");
  });

  it("keeps a real failure red", () => {
    setEntry({ data: null, loading: false, error: "boom 500", errorStatus: 500 });
    const { container } = render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    expect(container.innerHTML).toContain("text-danger-icon");
  });

  it("maps status states to icons (untracked / deleted / staged renamed / conflicted)", () => {
    const changes: GitStatusResponse["changes"] = [
      { path: "u.txt", oldPath: null, index: "unmodified", worktree: "untracked", staged: false },
      { path: "d.txt", oldPath: null, index: "deleted", worktree: "deleted", staged: false },
      { path: "r.txt", oldPath: "old.txt", index: "renamed", worktree: "modified", staged: true },
      // Unmerged paths arrive as conflicted on both columns — must render as
      // a conflict icon, never as clean/modified (ADR-078 invariant 5).
      { path: "c.txt", oldPath: null, index: "conflicted", worktree: "conflicted", staged: false },
    ];
    setEntry({
      data: { isRepo: true, branch: "main", error: null, truncated: false, changes },
      loading: false,
    });
    const { container } = render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    expect(container.querySelector(".lucide-file-plus")).toBeTruthy();
    expect(container.querySelector(".lucide-file-minus")).toBeTruthy();
    expect(container.querySelector(".lucide-arrow-right-left")).toBeTruthy();
    expect(container.querySelector(".lucide-git-merge")).toBeTruthy();
    // Conflicted rows must not show a staged badge.
    const cRow = screen.getByText("c.txt").closest("li");
    expect(cRow?.textContent).not.toContain("staged");
  });

  // ── History dropdown → files-in-commit (ADR-XXX) ─────────────────────

  it("renders files in the viewed commit when viewingRev is set", () => {
    // After the user picks commit X from the bar's history dropdown,
    // `viewingRev` is set and `status[groupKey|X]` carries that
    // commit's file list. The panel must read from the view-keyed slot
    // (NOT the bare group key) so a commit-view never accidentally
    // overlays the working-tree list.
    gitStoreMocks.viewingRev = { [KEY]: "abc1234" };
    setViewEntry("abc1234", {
      data: {
        isRepo: true,
        branch: "abc1234 feat",
        error: null,
        truncated: false,
        changes: [{ path: "a.txt", index: "modified", worktree: "modified", staged: true }],
        rev: "abc1234",
      },
      loading: false,
    });
    render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    // Commit-view files render through the same row template as the
    // working-tree list — the difference is purely which status slot
    // the panel reads. (The branch label is the bar's concern, not
    // the panel's — see GitStatusBar.test.tsx.)
    expect(screen.getByText("a.txt")).toBeTruthy();
  });

  it("opens the diff for the viewed commit when double-clicking a row", () => {
    // Double-click-row in commit view defaults to "open the diff for the
    // viewed commit". The client only:
    //   1. Calls `fetchDiff(<viewingRev>^, <viewingRev>, path)` —
    //      sending git shorthand on the wire (it's a request param,
    //      not stored data).
    //   2. Stores the server's `baseRev` / `headRev` into the OpenFile
    //      verbatim — banner labels and diff body are both sourced
    //      from the backend.
    //
    // The backend owns all git semantics (ADR-009 v2 split): it
    // resolves `<sha>^` to the parent SHA AND promotes the base to
    // the file-history predecessor of `head` on `path` so the banner
    // label matches the row above `head` in the diff banner's
    // `CommitPicker` (which lists `git log -- <path>`). Frontend tests
    // therefore assert the round-trip (request shorthand → stored
    // server SHAs), not git semantics themselves.
    gitStoreMocks.viewingRev = { [KEY]: "abc1234" };
    setViewEntry("abc1234", {
      data: {
        isRepo: true,
        branch: "abc1234 feat",
        error: null,
        truncated: false,
        changes: [{ path: "a.txt", index: "modified", worktree: "modified", staged: true }],
        rev: "abc1234",
      },
      loading: false,
    });
    gitStoreMocks.fetchDiff.mockReturnValue(
      okResponse({
        kind: "modified",
        original: "old",
        modified: "new",
        // The backend promotes base from `abc1234^` (first-parent) to
        // the file-history predecessor on `a.txt`; we mock whatever
        // SHAs the server would have returned. The client contract is
        // "store verbatim, no client-side git reasoning".
        baseRev: "0123456789abcdef0123456789abcdef01234567",
        headRev: "abc1234567890abcdef0123456789abcdef012345",
      }) as never,
    );
    render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    fireEvent.doubleClick(screen.getByText("a.txt"));
    // Wait for the async openDiff to settle.
    return vi.waitFor(() => {
      expect(gitStoreMocks.fetchDiff).toHaveBeenCalledWith(
        "a1",
        "ws1",
        "a.txt",
        "abc1234^", // request uses git shorthand (network protocol)
        "abc1234",
      );
      // OpenFile stores the server's canonical SHAs (the bug fix from
      // round 1: client never has to `slice(0, 7)` a `<sha>^` string).
      expect(fileEditorMocks.openVirtualFile).toHaveBeenCalledWith(
        expect.objectContaining({
          diffBaseRef: "0123456789abcdef0123456789abcdef01234567",
          diffHeadRef: "abc1234567890abcdef0123456789abcdef012345",
        }),
      );
      // The two stored refs MUST differ in their first 7 chars —
      // that's the regression we're guarding.
      const call = fileEditorMocks.openVirtualFile.mock.calls[0][0];
      expect(call.diffBaseRef.slice(0, 7)).not.toBe(
        call.diffHeadRef.slice(0, 7),
      );
    });
  });

  it("stores headRef as empty string when server returns null headRev", () => {
    // Worktree variant: `headRev` is null → stored as "" so the
    // existing `!diffHeadRef → "Working Tree"` rendering contract
    // continues to work without UI changes. We trigger openDiff via a
    // deleted row (decision 6) since plain worktree rows open in the
    // editor instead — openDiff is the wrong code path to exercise here.
    setEntry({
      data: {
        isRepo: true,
        branch: "main",
        error: null,
        truncated: false,
        changes: [{ path: "a.txt", index: "deleted", worktree: "deleted", staged: true }],
        rev: "",
      },
      loading: false,
    });
    gitStoreMocks.fetchDiff.mockReturnValue(
      okResponse({
        kind: "deleted",
        original: "old",
        modified: "",
        baseRev: "0123456789abcdef0123456789abcdef01234567",
        headRev: null,
      }) as never,
    );
    render(<GitStatusPanel agentId="a1" workspaceId="ws1" />);
    fireEvent.doubleClick(screen.getByText("a.txt"));
    return vi.waitFor(() => {
      expect(fileEditorMocks.openVirtualFile).toHaveBeenCalledWith(
        expect.objectContaining({
          diffBaseRef: "0123456789abcdef0123456789abcdef01234567",
          diffHeadRef: "",
        }),
      );
    });
  });
});
