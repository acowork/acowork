/**
 * `projectStore.refreshCounts` — the sidebar badge tally.
 *
 * Contract being locked down here:
 *
 *   1. **One call, not N+1.** The previous implementation called
 *      `listProjectTasks(pid)` once per project and counted client-side.
 *      On a real store that shipped 83 task entities / 503 KB for the
 *      largest project, and cost ~160 server-side file opens per render
 *      (list + per-task blocked_by), just to display two integers.
 *      `listProjectTasks` must therefore never be touched by this path.
 *
 *   2. **Absence means zero.** The endpoint omits projects that hold no
 *      tasks, so the store has to seed zeros for every known project or the
 *      badge renders `undefined`.
 *
 *   3. **A failed tally never blanks the board.** Throwing here would clear
 *      the sidebar; the seeded zeros stay.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";

const mockListProjectCounts = vi.fn<() => Promise<unknown>>();
const mockListProjectTasks = vi.fn<(pid: string) => Promise<unknown>>();

vi.mock("../../lib/pm-api", () => ({
  listProjectCounts: () => mockListProjectCounts(),
  listProjectTasks: (pid: string) => mockListProjectTasks(pid),
  listProjects: () => Promise.resolve([]),
}));
vi.mock("../../lib/logger", () => ({
  log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
}));

const { usePmProjectStore } = await import("./projectStore");

function project(id: string, title: string) {
  return {
    id,
    title,
    description: "",
    status: "active" as const,
    created_by: "human",
    created_at: "2026-11-16T00:00:00Z",
    updated_at: "2026-11-16T00:00:00Z",
    members: [],
  };
}

beforeEach(() => {
  mockListProjectCounts.mockReset();
  mockListProjectTasks.mockReset();
  usePmProjectStore.setState({
    projects: [project("p-1", "One"), project("p-2", "Two")],
    selected: null,
    counts: {},
  } as never);
});

describe("projectStore.refreshCounts", () => {
  it("takes the tallies from a single counts call and never lists tasks", async () => {
    mockListProjectCounts.mockResolvedValue([{ project_id: "p-1", total: 8, submitted: 2 }]);

    await usePmProjectStore.getState().refreshCounts();

    expect(usePmProjectStore.getState().counts).toEqual({
      "p-1": { total: 8, submitted: 2 },
      // Absent from the response → seeded zero, not `undefined`.
      "p-2": { total: 0, submitted: 0 },
    });
    expect(mockListProjectCounts).toHaveBeenCalledTimes(1);
    expect(mockListProjectTasks).not.toHaveBeenCalled();
  });

  it("keeps the seeded zeros when the tally request fails", async () => {
    mockListProjectCounts.mockRejectedValue(new Error("pm offline"));
    usePmProjectStore.setState({ counts: { "p-1": { total: 99, submitted: 9 } } } as never);

    await expect(usePmProjectStore.getState().refreshCounts()).resolves.toBeUndefined();

    expect(usePmProjectStore.getState().counts).toEqual({
      "p-1": { total: 0, submitted: 0 },
      "p-2": { total: 0, submitted: 0 },
    });
  });
});
