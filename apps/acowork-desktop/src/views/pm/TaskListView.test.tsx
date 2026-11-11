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
import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { TaskListView } from "./TaskListView";
import { showToast } from "../../components/common/ToastProvider";
import { CAPSULE_PANE_CN } from "../../components/common/capsule";
import { createTask, deleteTask, updateTask } from "../../lib/pm-api";
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
  removeTask: vi.fn(),
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
      removeTask: state.removeTask,
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
  deleteTask: vi.fn(),
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

/** The confirm button inside the delete confirmation dialog (role=alertdialog). */
const confirmDeleteButton = () => {
  const dialog = document.querySelector('[role="alertdialog"]');
  const btn = dialog?.querySelector("button.btn-accent") as HTMLButtonElement | null;
  if (!btn) throw new Error("no confirm dialog");
  return btn;
};

const input = (container: HTMLElement, ariaLabel: string) =>
  container.querySelector(`input[aria-label="${ariaLabel}"]`) as HTMLInputElement;

beforeEach(() => {
  state.tasks = [];
  state.loading = false;
  state.healthy = true;
  // 批量用例会把 moveTask 换成带成败编排的实现；每个用例都从默认桩重新开始，
  // 否则实现会漏给后面的用例（`clearAllMocks` 只清调用记录，不清实现）。
  state.moveTask = vi.fn();
  vi.clearAllMocks();
});

describe("capsule shell", () => {
  // 列表和看板要像同一套 UI：看板的每一列是一枚大胶囊（CAPSULE_PANE_CN +
  // bg-right-panel），列表整块也必须是 —— 头部工具栏、尾部页码、中间行。
  // 改动前它是一整片 bg-page-bg，左右撑满、和窗口边缘没有边界。
  it("wraps toolbar + rows + pager in the shared capsule pane", () => {
    state.tasks = [task("t-1")];
    const { container } = view();
    const shell = container.firstElementChild as HTMLElement;
    for (const cn of CAPSULE_PANE_CN.split(" ")) {
      expect(shell.className, `shell missing "${cn}"`).toContain(cn);
    }
    expect(shell.className).toContain("bg-right-panel");
    expect(shell.className).not.toContain("bg-page-bg");
    // 头部/尾部仍在胶囊内部（同一个 firstElementChild 的后代里）
    expect(shell.querySelector("table")).not.toBeNull();
  });
});

describe("collapse control", () => {
  // 折叠开关是 + / −，不是 10px 的 ▾：小字号三角在 34px 行里只剩一个小点，
  // 看不出「能不能点」。父行是可点按钮（− 展开 / + 收起），叶子行是淡化的
  // − 占位（装饰，不进 Tab 序），缩进列宽因此恒定。
  it("gives a parent row a clickable +/− toggle and a leaf row a muted −", () => {
    state.tasks = [task("t-1"), task("t-2", { parent_id: "t-1", depth: 1 })];
    const { container } = view();
    expect(renderedIds(container)).toEqual(["t-1", "t-2"]);

    const collapse = container.querySelector(
      'button[aria-label="pm.task.collapse"]',
    ) as HTMLButtonElement | null;
    expect(collapse, "parent row has no collapse toggle").not.toBeNull();
    expect(collapse!.getAttribute("aria-expanded")).toBe("true");
    expect(collapse!.querySelector("svg")).not.toBeNull();

    fireEvent.click(collapse!);
    expect(renderedIds(container)).toEqual(["t-1"]);
    const expand = container.querySelector(
      'button[aria-label="pm.task.expand"]',
    ) as HTMLButtonElement | null;
    expect(expand?.getAttribute("aria-expanded")).toBe("false");

    // 叶子行（t-2 收起后不在 DOM 里了，用 t-3 验）没有可点的折叠按钮
    state.tasks = [task("t-3")];
    const leafOnly = view();
    expect(leafOnly.container.querySelector('button[aria-label^="pm.task."]')).toBeNull();
  });
});

describe("table shape", () => {
  // 表头少一列、数据行里却还留着一个占位 <td>（或反过来），`table-layout: fixed`
  // 就塌了：浏览器给多出的单元格造匿名列，剩余宽度不再分给 `title` 列，标题被压成
  // 内容宽、右侧空一大片。所以钉住"每行格子数 = 表头格子数"。
  it("keeps every data row's cell count equal to the header's", () => {
    state.tasks = [task("t-1"), task("t-2", { parent_id: "t-1", depth: 1 })];
    const { container } = view();
    const headCells = container.querySelectorAll("thead tr:first-child > *").length;
    const rows = [...container.querySelectorAll("tbody tr[data-task-id]")];
    expect(rows.length).toBe(2);
    for (const tr of rows) {
      expect(tr.children.length, `row ${tr.getAttribute("data-task-id")}`).toBe(headCells);
    }
  });
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

describe("column header: sort vs resize", () => {
  /** 拖宽手柄后浏览器会补一个 `click`，派发点在 mousedown/mouseup 的共同祖先。 */
  const dragThenClickHeader = (container: HTMLElement, column: string) => {
    const th = container.querySelector(`[data-column="${column}"]`) as HTMLElement;
    const handle = container.querySelector(`[data-resize-column="${column}"]`) as HTMLElement;
    fireEvent.mouseDown(handle, { clientX: 100 });
    fireEvent.mouseMove(document, { clientX: 160 });
    fireEvent.mouseUp(document, { clientX: 160 });
    fireEvent.click(th);
    return th;
  };

  it("does not sort when a column width drag ends inside the header", () => {
    state.tasks = [task("t-1"), task("t-2"), task("t-3")];
    const { container } = view();
    const th = dragThenClickHeader(container, "status");
    // 调列宽只是调列宽：排序状态必须还是 none（曾经会顺带排一次序）。
    expect(th.getAttribute("aria-sort")).toBe("none");
  });

  it("sorts when the header label itself is clicked", () => {
    state.tasks = [task("t-1"), task("t-2"), task("t-3")];
    const { container } = view();
    const th = container.querySelector(`[data-column="status"]`) as HTMLElement;
    fireEvent.click(within(th).getByText("pm.list.col.status"));
    expect(th.getAttribute("aria-sort")).toBe("ascending");
    fireEvent.click(within(th).getByText("pm.list.col.status"));
    expect(th.getAttribute("aria-sort")).toBe("descending");
  });
});

describe("bulk edit failure accounting", () => {
  /** 勾选全部 → 批量条 → 状态 → done。 */
  async function bulkStatus(container: HTMLElement) {
    fireEvent.click(input(container, "pm.list.selectAll"));
    const bar = container.querySelector('[aria-label="pm.list.bulkTitle"]') as HTMLElement;
    fireEvent.click(within(bar).getByText("pm.list.col.status"));
    fireEvent.click(await screen.findByText("pm.board.done"));
    await act(async () => {});
  }

  it("reports partial failure truthfully and keeps the failed rows selected", async () => {
    state.tasks = [task("t-1"), task("t-2"), task("t-3")];
    // 第二条改状态失败：moveTask 自己回滚并返回 false。
    state.moveTask = vi.fn(async (id: string) => id !== "t-2");
    const { container } = view();
    await bulkStatus(container);

    expect(showToast).toHaveBeenCalledWith({
      type: "warning",
      message: "pm.list.bulkPartial",
    });
    // 关键：不能再出现那条谎报的"已更新 3 个任务"。
    expect(showToast.mock.calls.some((c) => (c[0] as { type: string }).type === "success")).toBe(
      false,
    );
    const boxes = [...container.querySelectorAll('input[aria-label="pm.list.selectRow"]')] as (
      | HTMLInputElement
      | null
    )[];
    expect(boxes.map((b) => b?.checked)).toEqual([false, true, false]);
  });

  it("reports success only when every row went through", async () => {
    state.tasks = [task("t-1"), task("t-2")];
    state.moveTask = vi.fn(async () => true);
    const { container } = view();
    await bulkStatus(container);

    expect(showToast).toHaveBeenCalledWith({
      type: "success",
      message: "pm.list.bulkDone",
    });
    expect(input(container, "pm.list.selectAll").checked).toBe(false);
  });
});

describe("row-level inline field editing", () => {
  it("changes the priority from the row's own priority cell", async () => {
    state.tasks = [task("t-1")];
    (updateTask as unknown as ReturnType<typeof vi.fn>).mockResolvedValue({
      ...task("t-1"),
      priority: "urgent",
    });
    const { container } = view();
    // 优先级格（徽章）就在这一行里：点它弹菜单，不该触发打开任务详情。
    const row = container.querySelector('[data-task-id="t-1"]') as HTMLElement;
    const badge = within(row).getByText("pm.priority.normal");
    fireEvent.click(badge);
    fireEvent.click(await screen.findByText("pm.priority.urgent"));
    await act(async () => {});
    expect(updateTask).toHaveBeenCalledWith("t-1", { priority: "urgent" });
  });

  it("changes the assignee from the row's own assignee cell", async () => {
    state.tasks = [task("t-1")];
    (updateTask as unknown as ReturnType<typeof vi.fn>).mockResolvedValue(task("t-1"));
    const { container } = view();
    const row = container.querySelector('[data-task-id="t-1"]') as HTMLElement;
    // 未指派也是一个可点入口（点了才能指派出去）。
    fireEvent.click(within(row).getByText("pm.task.unassigned"));
    // 弹层里的「未指派」项：选它等于清空负责人。
    const unassignedItem = (await screen.findAllByText("pm.task.unassigned")).find(
      (el) => el.closest(".context-menu") !== null,
    );
    expect(unassignedItem).toBeTruthy();
    fireEvent.click(unassignedItem!);
    await act(async () => {});
    expect(updateTask).toHaveBeenCalledWith("t-1", { assignee: null });
  });
});

describe("bulk delete", () => {
  it("asks for confirmation before deleting the selected rows", async () => {
    state.tasks = [task("t-1"), task("t-2")];
    (deleteTask as unknown as ReturnType<typeof vi.fn>).mockResolvedValue(undefined);
    const { container } = view();
    fireEvent.click(input(container, "pm.list.selectAll"));
    const bar = container.querySelector('[aria-label="pm.list.bulkTitle"]') as HTMLElement;
    fireEvent.click(button(bar, "pm.list.bulkDelete")!);
    // 确认之前不能删。
    expect(await screen.findByText("pm.list.bulkDeleteTitle")).toBeTruthy();
    expect(deleteTask).not.toHaveBeenCalled();

    fireEvent.click(confirmDeleteButton());
    await act(async () => {});
    expect(deleteTask).toHaveBeenCalledTimes(2);
  });

  it("removes the deleted rows from the store", async () => {
    state.tasks = [task("t-1"), task("t-2")];
    (deleteTask as unknown as ReturnType<typeof vi.fn>).mockResolvedValue(undefined);
    const { container } = view();
    fireEvent.click(input(container, "pm.list.selectAll"));
    const bar = container.querySelector('[aria-label="pm.list.bulkTitle"]') as HTMLElement;
    fireEvent.click(button(bar, "pm.list.bulkDelete")!);
    fireEvent.click(confirmDeleteButton());
    await act(async () => {});
    expect(state.removeTask).toHaveBeenCalledWith("t-1");
    expect(state.removeTask).toHaveBeenCalledWith("t-2");
  });

  it("also removes the subtasks the server cascades away", async () => {
    // 服务端 `delete_task` 连子树一起删（P1 无视 cascade 标志），
    // 本地也必须把子孙摘掉，否则留下服务端已不存在的行。
    state.tasks = [
      task("r1"),
      task("r1c1", { parent_id: "r1", depth: 1 }),
      task("r1c2", { parent_id: "r1", depth: 1 }),
      task("r2"),
    ];
    (deleteTask as unknown as ReturnType<typeof vi.fn>).mockResolvedValue(undefined);
    const { container } = view();
    fireEvent.click(input(container, "pm.list.selectAll"));
    const bar = container.querySelector('[aria-label="pm.list.bulkTitle"]') as HTMLElement;
    fireEvent.click(button(bar, "pm.list.bulkDelete")!);
    fireEvent.click(confirmDeleteButton());
    await act(async () => {});
    const removed = (state.removeTask as unknown as ReturnType<typeof vi.fn>).mock.calls.map(
      (c) => c[0],
    );
    expect(removed).toContain("r1");
    expect(removed).toContain("r1c1");
    expect(removed).toContain("r1c2");
    expect(removed).toContain("r2");
  });

  it("keeps the failed rows selected and reports the real numbers", async () => {
    state.tasks = [task("t-1"), task("t-2")];
    (deleteTask as unknown as ReturnType<typeof vi.fn>).mockImplementation(async (id: string) => {
      if (id === "t-2") throw new Error("boom");
    });
    const { container } = view();
    fireEvent.click(input(container, "pm.list.selectAll"));
    const bar = container.querySelector('[aria-label="pm.list.bulkTitle"]') as HTMLElement;
    fireEvent.click(button(bar, "pm.list.bulkDelete")!);
    fireEvent.click(confirmDeleteButton());
    await act(async () => {});
    expect(showToast).toHaveBeenCalledWith({
      type: "warning",
      message: "pm.list.bulkDeletePartial",
    });
    // 失败的那条保持选中，用户可以原地重试。
    const boxes = [...container.querySelectorAll('input[aria-label="pm.list.selectRow"]')] as (
      | HTMLInputElement
      | null
    )[];
    expect(boxes.map((b) => b?.checked)).toEqual([false, true]);
  });
});

describe("anchored popover dismissal", () => {
  const openColumnMenu = (container: HTMLElement) => {
    fireEvent.click(button(container, "pm.list.columnMenu")!);
    return screen.queryByText("pm.list.sortAsc");
  };

  it("closes on Escape", () => {
    state.tasks = [task("t-1")];
    const { container } = view();
    expect(openColumnMenu(container)).toBeTruthy();
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByText("pm.list.sortAsc")).toBeNull();
  });

  it("closes on a mousedown outside the panel and the anchor", () => {
    state.tasks = [task("t-1")];
    const { container } = view();
    expect(openColumnMenu(container)).toBeTruthy();
    fireEvent.mouseDown(document.body);
    expect(screen.queryByText("pm.list.sortAsc")).toBeNull();
  });
});

