/**
 * `boardStore.loadAssigned` — 单聊右侧任务面板的 assignee 作用域。
 *
 * 这个 store 同时装两种作用域（某个项目 / 某个 agent 的任务），所以这里要钉
 * 住的是「换作用域时不会把上一份数据先画出来」，以及跨项目的那份任务确实
 * 落在同一个 `tasks` 里（卡片上的乐观更新靠它，不能用第二份数据源）。
 */

import { describe, it, expect, vi, beforeEach } from "vitest";
import type { PmTaskResponse } from "../../lib/pm-types";

const mockListAssigned = vi.fn<(assignee: string) => Promise<PmTaskResponse[]>>();
const mockListProject = vi.fn<(pid: string) => Promise<PmTaskResponse[]>>();

vi.mock("../../lib/pm-api", () => ({
  listAssignedTasks: (assignee: string) => mockListAssigned(assignee),
  listProjectTasks: (pid: string) => mockListProject(pid),
  reviewTask: () => Promise.resolve({}),
  updateTask: () => Promise.resolve({}),
}));
vi.mock("../../components/common/ToastProvider", () => ({ showToast: () => {} }));
vi.mock("../../lib/logger", () => ({
  log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
}));

const { assigneeScope, projectScope, sameScope, usePmBoardStore } = await import("./boardStore");

function task(over: Partial<PmTaskResponse>): PmTaskResponse {
  return {
    id: "t-1",
    project_id: "p-1",
    title: "T",
    description: "",
    type: "task",
    status: "pending",
    review_status: "not_required",
    priority: "normal",
    assignee: null,
    due_at: null,
    depends_on: [],
    attachments: [],
    result: null,
    created_by: "human",
    created_at: "2026-01-01T00:00:00Z",
    updated_at: "2026-01-01T00:00:00Z",
    claimed_at: null,
    submitted_at: null,
    is_blocked: false,
    blocked_by: [],
    depth: 0,
    parent_id: null,
    ...over,
  };
}

beforeEach(() => {
  mockListAssigned.mockReset();
  mockListProject.mockReset();
  usePmBoardStore.getState().clear();
});

describe("sameScope", () => {
  it("按值比较作用域，kind 不同不算同一份", () => {
    expect(sameScope(assigneeScope("x"), assigneeScope("x"))).toBe(true);
    expect(sameScope(assigneeScope("x"), projectScope("x"))).toBe(false);
    expect(sameScope(assigneeScope("x"), null)).toBe(false);
    expect(sameScope(null, null)).toBe(true);
  });
});

describe("loadAssigned", () => {
  it("把跨项目的任务装进同一个 tasks，作用域标成 assignee", async () => {
    mockListAssigned.mockResolvedValue([
      task({ id: "t-1", project_id: "p-1", assignee: "agent-1" }),
      task({ id: "t-2", project_id: "p-2", assignee: "agent-1" }),
    ]);

    await usePmBoardStore.getState().loadAssigned("agent-1");

    const s = usePmBoardStore.getState();
    expect(mockListAssigned).toHaveBeenCalledWith("agent-1");
    expect(s.scope).toEqual(assigneeScope("agent-1"));
    // 两个项目来的任务都在同一列根集合里 —— 卡片渲染 / 乐观更新共用这一份。
    expect(s.columnRoots("pending").map((t) => t.id)).toEqual(["t-1", "t-2"]);
    // 同一 assignee 的重复挂载走 10s 缓存，不重复请求。
    await usePmBoardStore.getState().loadAssigned("agent-1");
    expect(mockListAssigned).toHaveBeenCalledTimes(1);
  });

  it("换 agent 的那一帧先清空，不显示上一个 bot 的卡片", async () => {
    mockListAssigned.mockResolvedValueOnce([task({ id: "t-1", assignee: "agent-1" })]);
    await usePmBoardStore.getState().loadAssigned("agent-1");
    expect(usePmBoardStore.getState().tasks).toHaveLength(1);

    // 第二个 agent 的请求悬着不落 —— 此刻 store 里必须是空的。
    let release!: (tasks: PmTaskResponse[]) => void;
    mockListAssigned.mockReturnValueOnce(new Promise((res) => (release = res)));
    const pending = usePmBoardStore.getState().loadAssigned("agent-2");

    expect(usePmBoardStore.getState().scope).toEqual(assigneeScope("agent-2"));
    expect(usePmBoardStore.getState().tasks).toEqual([]);
    expect(usePmBoardStore.getState().columnRoots("pending")).toEqual([]);

    release([task({ id: "t-9", assignee: "agent-2" })]);
    await pending;
    expect(usePmBoardStore.getState().tasks.map((t) => t.id)).toEqual(["t-9"]);
  });

  it("迟到的响应不属于当前作用域就丢掉", async () => {
    // agent-1 的请求悬着，随后换成 agent-2 并落定。
    let releaseStale!: (tasks: PmTaskResponse[]) => void;
    mockListAssigned.mockReturnValueOnce(new Promise((res) => (releaseStale = res)));
    const stale = usePmBoardStore.getState().loadAssigned("agent-1");

    mockListAssigned.mockResolvedValueOnce([task({ id: "t-2", assignee: "agent-2" })]);
    await usePmBoardStore.getState().loadAssigned("agent-2");
    expect(usePmBoardStore.getState().tasks.map((t) => t.id)).toEqual(["t-2"]);

    releaseStale([task({ id: "t-1", assignee: "agent-1" })]);
    await stale;
    // agent-1 的响应晚到，但它已经不是当前作用域了。
    expect(usePmBoardStore.getState().tasks.map((t) => t.id)).toEqual(["t-2"]);
    expect(usePmBoardStore.getState().scope).toEqual(assigneeScope("agent-2"));
  });

  it("切回项目作用域后 scope 换成项目", async () => {
    mockListAssigned.mockResolvedValue([task({ id: "t-1", assignee: "agent-1" })]);
    mockListProject.mockResolvedValue([task({ id: "t-2", project_id: "p-1" })]);

    await usePmBoardStore.getState().loadAssigned("agent-1");
    await usePmBoardStore.getState().loadTasks("p-1");

    const s = usePmBoardStore.getState();
    expect(s.scope).toEqual(projectScope("p-1"));
    expect(s.tasks.map((t) => t.id)).toEqual(["t-2"]);
  });
});