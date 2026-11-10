/**
 * TaskListView — 列表视图的行渲染与分页。
 *
 * 派生链的算术在 `lib/pm-list-columns.test.ts` 里钉住了；这里只验视图层独有的
 * 四件事：
 *   1. 分页确实只挂载本页的行（不是"渲染了再藏起来"）；
 *   2. 空项目 / 筛选无命中 / 加载中是三种不同的画面（must-have 边界）；
 *   3. 离线只杀写入口（状态胶囊、行内新建），读与视图操作照常；
 *   4. 点击行为接到既有回调（打开详情、改每页行数）。
 *
 * 本仓库不引 `@testing-library/jest-dom`，所以断言写成 DOM 事实：
 * `el.disabled`、`queryByText(...) === null`。
 */
import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { TaskListView } from "./TaskListView";
import { createTask } from "../../lib/pm-api";
import { DEFAULT_PM_LIST_COLUMNS } from "../../lib/pm-list-columns";
import type { PmProject, PmTaskResponse, TaskStatus } from "../../lib/pm-types";

/** Mutable fixtures the store mocks read at render time (mocks are hoisted). */
const state = vi.hoisted(() => ({
  tasks: [] as unknown[],
  loading: false,
  healthy: true as boolean | null,
  moveTask: vi.fn(),
  reviewTask: vi.fn(),
  upsertTask: vi.fn(),
}));

vi.mock("../../i18n/useTranslation", () => ({
  // Assertions are locale-independent: `t` echoes the key (params dropped).
  useTranslation: () => ({ t: (key: string) => key }),
}));
vi.mock("../../components/common/ToastProvider", () => ({ showToast: vi.fn() }));
vi.mock("../../stores/pm/boardStore", () => ({
  usePmBoardStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({
      tasks: state.tasks,
      loading: state.loading,
      moveTask: state.moveTask,
      reviewTask: state.reviewTask,
      upsertTask: state.upsertTask,
    }),
}));
vi.mock("../../stores/pm/healthStore", () => ({
  usePmHealthStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({ healthy: state.healthy }),
}));
vi.mock("../../stores/agentStore", () => ({
  useAgentStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({ agents: [] }),
}));
vi.mock("../../stores/userProfileStore", () => ({
  useUserProfileStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({ profile: null }),
}));
// Directory joins are covered in pmMembers.test.ts; here they only add noise.
vi.mock("./pmMembers", () => ({
  resolveActorLabel: (_m: unknown, id: string | null) => id ?? "",
  resolveMembers: () => [],
  usePmDirectoryUsers: () => [],
  useSelfAccount: () => null,
  withOwner: () => [],
  MemberAvatar: () => null,
}));
vi.mock("../../lib/pm-api", () => ({
  createTask: vi.fn(),
  updateTask: vi.fn(),
}));

const PROJECT: PmProject = {
  id: "p-1",
  title: "Demo",
  description: "",
  status: "active",
  metadata: null,
  created_at: "2026-10-01T00:00:00Z",
  updated_at: "2026-10-01T00:00:00Z",
};

function task(id: string, overrides: Partial<PmTaskResponse> = {}): PmTaskResponse {
  return {
    id,
    project_id: "p-1",
    title: `Title ${id}`,
    description: "",
    type: "task",
    status: "pending" as TaskStatus,
    review_status: "not_required",
    priority: "normal",
    assignee: null,
    due_at: null,
    start_at: null,
    dispatched_at: null,
    dispatch_attempts: 0,
    depends_on: [],
    attachments: [],
    result: null,
    created_by: "human",
    created_at: "2026-10-01T00:00:00Z",
    updated_at: "2026-10-01T00:00:00Z",
    claimed_at: null,
    submitted_at: null,
    is_blocked: false,
    blocked_by: [],
    depth: 0,
    parent_id: null,
    ...overrides,
  };
}

function view(props: { pageSize?: number; onPageSizeChange?: (n: number) => void } = {}) {
  const onOpenTask = vi.fn();
  const onPageSizeChange = props.onPageSizeChange ?? vi.fn();
  const utils = render(
    <TaskListView
      project={PROJECT}
      columns={DEFAULT_PM_LIST_COLUMNS.slice()}
      onColumnsChange={() => {}}
      pageSize={props.pageSize ?? 50}
      onPageSizeChange={onPageSizeChange}
      onOpenTask={onOpenTask}
    />,
  );
  return { ...utils, onOpenTask, onPageSizeChange };
}

/** Rendered task ids, in DOM order. */
const renderedIds = (container: HTMLElement) =>
  [...container.querySelectorAll("[data-task-id]")].map((el) => el.getAttribute("data-task-id"));

const button = (container: HTMLElement, ariaLabel: string) =>
  container.querySelector(`[aria-label="${ariaLabel}"]`) as HTMLButtonElement | null;

const input = (container: HTMLElement, ariaLabel: string) =>
  container.querySelector(`input[aria-label="${ariaLabel}"]`) as HTMLInputElement;

beforeEach(() => {
  state.tasks = [];
  state.loading = false;
  state.healthy = true;
  vi.clearAllMocks();
});

describe("row rendering", () => {
  it("renders one row per task", () => {
    state.tasks = [task("t-1"), task("t-2"), task("t-3")];
    const { container } = view();
    expect(renderedIds(container)).toEqual(["t-1", "t-2", "t-3"]);
  });

  it("opens the task when a row is clicked", () => {
    state.tasks = [task("t-1")];
    const { container, onOpenTask } = view();
    fireEvent.click(container.querySelector('[data-task-id="t-1"]')!);
    expect(onOpenTask).toHaveBeenCalledWith("t-1");
  });

  it("keeps a parent and its children on the same page", () => {
    // 3 棵子树 × 3 行；预算 5 行 → 第一页只装得下第一棵（第二棵要 6 行会劈开）。
    state.tasks = [
      task("r1"),
      task("r1c1", { parent_id: "r1", depth: 1 }),
      task("r1c2", { parent_id: "r1", depth: 1 }),
      task("r2"),
      task("r2c1", { parent_id: "r2", depth: 1 }),
      task("r2c2", { parent_id: "r2", depth: 1 }),
      task("r3"),
      task("r3c1", { parent_id: "r3", depth: 1 }),
      task("r3c2", { parent_id: "r3", depth: 1 }),
    ];
    const { container } = view({ pageSize: 5 });
    expect(renderedIds(container)).toEqual(["r1", "r1c1", "r1c2"]);
    fireEvent.click(button(container, "pm.board.pageNext")!);
    expect(renderedIds(container)).toEqual(["r2", "r2c1", "r2c2"]);
  });
});

describe("pagination", () => {
  it("shows only the current page and walks it with prev / next", () => {
    state.tasks = ["t-1", "t-2", "t-3", "t-4", "t-5"].map((id) => task(id));
    const { container } = view({ pageSize: 2 });
    expect(renderedIds(container)).toEqual(["t-1", "t-2"]);
    // 首页的"上一页"禁用，末页的"下一页"禁用。
    expect(button(container, "pm.board.pagePrev")!.disabled).toBe(true);
    fireEvent.click(button(container, "pm.board.pageNext")!);
    expect(renderedIds(container)).toEqual(["t-3", "t-4"]);
    fireEvent.click(button(container, "pm.board.pageNext")!);
    expect(renderedIds(container)).toEqual(["t-5"]);
    expect(button(container, "pm.board.pageNext")!.disabled).toBe(true);
    fireEvent.click(button(container, "pm.board.pagePrev")!);
    expect(renderedIds(container)).toEqual(["t-3", "t-4"]);
  });

  it("reports the row range and hides the pager controls on a single page", () => {
    state.tasks = [task("t-1"), task("t-2")];
    const { container } = view({ pageSize: 50 });
    expect(screen.getByText("pm.list.page.range").textContent).toBe("pm.list.page.range");
    expect(button(container, "pm.board.pageNext")).toBeNull();
  });

  it("hands the page-size choice up so it can be persisted per project", () => {
    state.tasks = [task("t-1")];
    const { container, onPageSizeChange } = view({ pageSize: 50 });
    const select = container.querySelector(
      `select[aria-label="pm.list.page.size"]`,
    ) as HTMLSelectElement;
    fireEvent.change(select, { target: { value: "100" } });
    expect(onPageSizeChange).toHaveBeenCalledWith(100);
  });

  it("goes back to page one when the result set changes", () => {
    state.tasks = ["t-1", "t-2", "t-3", "t-4"].map((id) => task(id));
    const { container } = view({ pageSize: 2 });
    fireEvent.click(button(container, "pm.board.pageNext")!);
    expect(renderedIds(container)).toEqual(["t-3", "t-4"]);
    // 搜索改变了结果集：停在第 2 页看到的会是不相干的一批。
    fireEvent.change(input(container, "pm.list.searchPlaceholder"), {
      target: { value: "Title t-4" },
    });
    expect(renderedIds(container)).toEqual(["t-4"]);
  });
});

describe("empty, filtered-out and loading", () => {
  it("says the project has no tasks when there are none at all", () => {
    state.tasks = [];
    view();
    expect(screen.queryByText("pm.list.empty")).not.toBeNull();
    expect(screen.queryByText("pm.list.noMatch")).toBeNull();
  });

  it("offers a way out when filters match nothing", () => {
    state.tasks = [task("t-1")];
    const { container } = view();
    fireEvent.change(input(container, "pm.list.searchPlaceholder"), {
      target: { value: "no-such-text" },
    });
    expect(screen.queryByText("pm.list.noMatch")).not.toBeNull();
    // 工具栏和空态行都给同一个出口，取哪个都行。
    fireEvent.click(screen.getAllByText("pm.list.clearFilters")[0]);
    expect(renderedIds(container)).toEqual(["t-1"]);
  });

  it("shows skeleton rows instead of an empty table while loading", () => {
    state.tasks = [];
    state.loading = true;
    view();
    expect(screen.queryAllByLabelText("loading").length).toBeGreaterThan(0);
    expect(screen.queryByText("pm.list.empty")).toBeNull();
  });
});

describe("inline create", () => {
  it("jumps to the last page so the created row is on screen", async () => {
    // 默认序下新任务排在最后：不翻页的话用户建完看不见自己刚建的东西。
    state.tasks = ["t-1", "t-2", "t-3", "t-4"].map((id) => task(id));
    state.upsertTask = vi.fn((created: PmTaskResponse) => {
      state.tasks = [...state.tasks, created];
    });
    vi.mocked(createTask).mockResolvedValue(task("t-5", { title: "Brand new" }) as never);

    const element = (
      <TaskListView
        project={PROJECT}
        columns={DEFAULT_PM_LIST_COLUMNS.slice()}
        onColumnsChange={() => {}}
        pageSize={2}
        onPageSizeChange={() => {}}
        onOpenTask={() => {}}
      />
    );
    const { container, rerender } = render(element);
    fireEvent.click(button(container, "pm.board.pageNext")!);
    expect(renderedIds(container)).toEqual(["t-3", "t-4"]);

    fireEvent.click(screen.getByText("pm.newTask").closest("button")!);
    const row = screen.getByPlaceholderText("pm.list.newRowPlaceholder");
    fireEvent.change(row, { target: { value: "Brand new" } });
    await act(async () => {
      fireEvent.keyDown(row, { key: "Enter" });
    });
    rerender(element);
    expect(renderedIds(container)).toEqual(["t-5"]);
  });
});

describe("offline", () => {
  it("disables the write entry points but keeps the list readable", () => {
    state.tasks = [task("t-1")];
    state.healthy = false;
    const { container } = view();
    expect(renderedIds(container)).toEqual(["t-1"]);
    // 状态胶囊是写入口 → 禁用；行内新建同样是写入口 → 禁用。
    const pill = screen.getByText("pm.board.pending").closest("button") as HTMLButtonElement;
    expect(pill.disabled).toBe(true);
    const create = screen.getByText("pm.newTask").closest("button") as HTMLButtonElement;
    expect(create.disabled).toBe(true);
  });

  it("leaves the read-only controls alone when the service is down", () => {
    state.tasks = [task("t-1")];
    state.healthy = false;
    const { container } = view({ pageSize: 50 });
    const select = container.querySelector(
      `select[aria-label="pm.list.page.size"]`,
    ) as HTMLSelectElement;
    expect(select.disabled).toBe(false);
    expect(input(container, "pm.list.searchPlaceholder").disabled).toBe(false);
  });
});
