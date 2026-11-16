/**
 * `ProjectsView` — first-load state machine.
 *
 * The defect being pinned: the first-load branch used to be
 * `projects.length === 0 && !loadingProjects`, which merged two very
 * different situations:
 *
 *   - **cold start** — the Gateway accepts `/api/pm/*` before the PM process
 *     has finished rebuilding its index, so the first request legitimately
 *     answers `503`. `loading` goes false and `projects` is empty.
 *   - **genuinely empty store**.
 *
 * Both rendered "No projects yet". During a cold start that reads as either
 * a freeze or as data loss — and the app had already been mistaken for
 * having lost every project once (see ADR-117 §2.1). A failed first load
 * must never claim the store is empty.
 *
 * The skeleton is also asserted to carry a *status line*: the original
 * placeholder blocks were `bg-surface` (#27272a) on `bg-page-bg` (#131316),
 * roughly 1.25:1 contrast in the dark theme, so the "loading animation" was
 * literally invisible and the whole view looked frozen.
 */

import { render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

// The view mounts with `loadProjects()`, so the API is the input under
// control here. `PmApiError` must be a real class export — the store does
// `instanceof pmApi.PmApiError` to keep the HTTP status.
class MockPmApiError extends Error {
  readonly status: number;
  readonly code: string;
  constructor(status: number) {
    super(`HTTP ${status}`);
    this.name = "PmApiError";
    this.status = status;
    this.code = "http_error";
  }
}

let listProjectsImpl: () => Promise<unknown>;

vi.mock("../lib/pm-api", () => ({
  PmApiError: MockPmApiError,
  listProjects: () => listProjectsImpl(),
  listProjectCounts: () => Promise.resolve([]),
  listProjectTasks: () => Promise.resolve([]),
}));

// Keys are asserted verbatim; the point is which branch rendered, not the copy.
vi.mock("../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

// Heavy children: they own their own data loading and are out of scope here.
vi.mock("./pm/ProjectSidebar", () => ({ ProjectSidebar: () => <div data-testid="sidebar" /> }));
vi.mock("./pm/ProjectBoard", () => ({
  ProjectBoard: ({ loading }: { loading?: boolean }) => (
    <div data-testid="board" data-loading={String(Boolean(loading))} />
  ),
}));
vi.mock("./pm/TaskEditDialog", () => ({ TaskEditDialog: () => null }));
vi.mock("./pm/ServiceOfflineBanner", () => ({ ServiceOfflineBanner: () => null }));

const { ProjectsView } = await import("./ProjectsView");
const { usePmProjectStore } = await import("../stores/pm/projectStore");
const { usePmBoardStore } = await import("../stores/pm/boardStore");

beforeEach(() => {
  listProjectsImpl = () => Promise.resolve([]);
  usePmProjectStore.setState({
    projects: [],
    selected: null,
    loading: false,
    error: null,
    errorStatus: null,
    updatedAt: null,
    counts: {},
  } as never);
  usePmBoardStore.setState({
    projectId: null,
    tasks: [],
    loading: false,
    error: null,
    updatedAt: null,
  } as never);
});

describe("ProjectsView first load", () => {
  it("keeps showing a status line while the first request is in flight", async () => {
    listProjectsImpl = () => new Promise(() => {});
    render(<ProjectsView />);

    expect(screen.getByRole("status").textContent).toBe("pm.loadingProjects");
    // Nothing has succeeded yet, so the empty state must not be reachable.
    expect(screen.queryByText("pm.emptyTitle")).toBeNull();
  });

  it("reports a booting service instead of an empty store when PM answers 503", async () => {
    listProjectsImpl = () => Promise.reject(new MockPmApiError(503));
    render(<ProjectsView />);

    await waitFor(() =>
      expect(screen.getByRole("status").textContent).toBe("pm.bootingTitle"),
    );
    // The regression guard: a cold start must never look like an empty store.
    expect(screen.queryByText("pm.emptyTitle")).toBeNull();
    // And it must offer a way out rather than waiting silently forever.
    expect(screen.getByText("common.retry")).toBeTruthy();
  });

  it("still reports a real error (4xx) as retryable boot text, not empty", async () => {
    listProjectsImpl = () => Promise.reject(new MockPmApiError(401));
    render(<ProjectsView />);

    await waitFor(() =>
      expect(screen.getByRole("status").textContent).toBe("pm.bootingTitle"),
    );
    expect(screen.queryByText("pm.emptyTitle")).toBeNull();
  });

  it("shows the empty state only after a successful load that returned nothing", async () => {
    listProjectsImpl = () => Promise.resolve([]);
    render(<ProjectsView />);

    await waitFor(() => expect(screen.getByText("pm.emptyTitle")).toBeTruthy());
    expect(screen.queryByRole("status")).toBeNull();
  });

  it("marks the board as loading when the selected project has not loaded yet", async () => {
    listProjectsImpl = () =>
      Promise.resolve([
        {
          id: "p-1",
          title: "P",
          description: "",
          status: "active",
          created_by: "human",
          created_at: "2026-11-16T00:00:00Z",
          updated_at: "2026-11-16T00:00:00Z",
          members: [],
        },
      ]);
    render(<ProjectsView />);

    // `loadTasks` is called with `silent: true`, so `boardLoading` stays false
    // and the board would render its "empty project" columns while 500 KB of
    // tasks were still on the wire. The view must derive loading from
    // "never loaded this project" as well.
    await waitFor(() => {
      const board = screen.getByTestId("board");
      expect(board.getAttribute("data-loading")).toBe("true");
    });
  });
});
