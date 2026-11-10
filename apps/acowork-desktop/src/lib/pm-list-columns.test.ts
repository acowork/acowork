/**
 * pm-list-columns — 列表视图的派生链（筛选 → 排序 → 树扁平化 → 分组 → 分页）。
 *
 * 这些函数是纯的，所以分页 / 树 / 分组的正确性在这里钉死，视图层只负责渲染。
 * 重点是分页的三条不变量：
 *   1. **子树不被页边界劈开**（父任务在上一页、子任务在下一页 = 缩进失真）；
 *   2. 一棵超预算的子树独占一页（否则翻页会死循环）；
 *   3. 页码越界被 clamp（数据变少时不白屏）。
 */
import { describe, expect, it } from "vitest";
import {
  DEFAULT_PM_LIST_COLUMNS,
  EMPTY_PM_LIST_FILTERS,
  NO_PM_LIST_SORT,
  UNASSIGNED,
  filterPmListTasks,
  flattenPmListRows,
  groupPmListRows,
  hasActivePmListFilters,
  movePmListColumn,
  nextPmListSort,
  paginatePmListGroups,
  sortPmListTasks,
  type PmListRow,
  type PmListSort,
  type PmListValueCtx,
} from "./pm-list-columns";
import type { PmTaskResponse, TaskStatus } from "./pm-types";

const NO_CTX: PmListValueCtx = {
  parentTitle: () => "",
  childCount: () => 0,
  childDone: () => 0,
};

const LABEL = () => "";

function task(
  id: string,
  overrides: Partial<PmTaskResponse> = {},
): PmTaskResponse {
  return {
    id,
    project_id: "p-1",
    title: `Task ${id}`,
    description: "",
    type: "task",
    status: "pending",
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

/** `n` root tasks, each with `kids` children. */
function forest(roots: number, kids = 0): PmTaskResponse[] {
  const out: PmTaskResponse[] = [];
  for (let r = 1; r <= roots; r += 1) {
    out.push(task(`r${r}`));
    for (let k = 1; k <= kids; k += 1) {
      out.push(task(`r${r}c${k}`, { parent_id: `r${r}`, depth: 1 }));
    }
  }
  return out;
}

const ids = (rows: PmListRow[]) => rows.map((r) => r.task.id);
/** Flat list (grouping off) as the pager sees it. */
const toGroups = (tasks: PmTaskResponse[]) =>
  groupPmListRows(flattenPmListRows(tasks, NO_PM_LIST_SORT, NO_CTX, new Set()), "none");
const flatIds = (groups: { rows: PmListRow[] }[]) => groups.flatMap((g) => ids(g.rows));

describe("filterPmListTasks", () => {
  it("matches title, id, description and the assignee display name", () => {
    const tasks = [
      task("t-1", { title: "Ship the relay", assignee: "i-1" }),
      task("t-2", { description: "relay internals" }),
      task("t-3", { title: "Unrelated" }),
    ];
    expect(
      filterPmListTasks(tasks, { ...EMPTY_PM_LIST_FILTERS, query: "ship" }, LABEL).map(
        (t) => t.id,
      ),
    ).toEqual(["t-1"]);
    expect(filterPmListTasks(tasks, { ...EMPTY_PM_LIST_FILTERS, query: "relay" }, LABEL).length).toBe(2);
    // 负责人显示名由调用方注入：解析出来才允许命中。
    expect(
      filterPmListTasks(tasks, { ...EMPTY_PM_LIST_FILTERS, query: "kevin" }, (id) =>
        id === "i-1" ? "Kevin" : "",
      ).map((t) => t.id),
    ).toEqual(["t-1"]);
  });

  it("treats UNASSIGNED as a filterable bucket", () => {
    const tasks = [task("t-1", { assignee: "i-1" }), task("t-2")];
    expect(
      filterPmListTasks(tasks, { ...EMPTY_PM_LIST_FILTERS, assignee: [UNASSIGNED] }, LABEL).map(
        (t) => t.id,
      ),
    ).toEqual(["t-2"]);
  });

  it("reports active filters so the toolbar can show a clear button", () => {
    expect(hasActivePmListFilters(EMPTY_PM_LIST_FILTERS)).toBe(false);
    expect(hasActivePmListFilters({ ...EMPTY_PM_LIST_FILTERS, query: "  " })).toBe(false);
    expect(hasActivePmListFilters({ ...EMPTY_PM_LIST_FILTERS, status: ["done"] })).toBe(true);
  });
});

describe("sortPmListTasks", () => {
  it("cycles ascending → descending → off, restarting on a new column", () => {
    let s: PmListSort = NO_PM_LIST_SORT;
    s = nextPmListSort(s, "priority");
    expect(s).toEqual({ key: "priority", dir: 1 });
    s = nextPmListSort(s, "priority");
    expect(s.dir).toBe(-1);
    s = nextPmListSort(s, "priority");
    expect(s).toEqual(NO_PM_LIST_SORT);
    expect(nextPmListSort({ key: "status", dir: -1 }, "due_at")).toEqual({
      key: "due_at",
      dir: 1,
    });
  });

  it("orders priority by weight, not by the raw string", () => {
    const tasks = [
      task("t-low", { priority: "low" }),
      task("t-urgent", { priority: "urgent" }),
      task("t-normal", { priority: "normal" }),
    ];
    expect(
      sortPmListTasks(tasks, { key: "priority", dir: 1 }, NO_CTX).map((t) => t.id),
    ).toEqual(["t-urgent", "t-normal", "t-low"]);
    expect(
      sortPmListTasks(tasks, { key: "priority", dir: -1 }, NO_CTX).map((t) => t.id),
    ).toEqual(["t-low", "t-normal", "t-urgent"]);
  });

  it("is stable for equal values (server order survives)", () => {
    const tasks = [task("a", { status: "done" }), task("b", { status: "done" }), task("c")];
    expect(
      sortPmListTasks(tasks, { key: "status", dir: 1 }, NO_CTX).map((t) => t.id),
    ).toEqual(["c", "a", "b"]);
  });

  it("sorts missing dates last in ascending order", () => {
    const tasks = [task("no-date"), task("due", { due_at: "2026-01-05T00:00:00Z" })];
    expect(
      sortPmListTasks(tasks, { key: "due_at", dir: 1 }, NO_CTX).map((t) => t.id),
    ).toEqual(["due", "no-date"]);
  });
});

describe("flattenPmListRows", () => {
  it("keeps a subtree contiguous, children right after the parent", () => {
    const rows = flattenPmListRows(forest(2, 2), NO_PM_LIST_SORT, NO_CTX, new Set());
    expect(ids(rows)).toEqual(["r1", "r1c1", "r1c2", "r2", "r2c1", "r2c2"]);
    expect(rows.map((r) => r.depth)).toEqual([0, 1, 1, 0, 1, 1]);
  });

  it("promotes orphans when the parent is filtered out", () => {
    const tasks = [task("r1"), task("r1c1", { parent_id: "r1", depth: 1 })];
    const onlyChild = tasks.filter((t) => t.id !== "r1");
    const rows = flattenPmListRows(onlyChild, NO_PM_LIST_SORT, NO_CTX, new Set());
    expect(ids(rows)).toEqual(["r1c1"]);
    expect(rows[0].depth).toBe(0);
  });

  it("trims collapsed children out of the row set, keeping the parent", () => {
    // 折叠改变行集 = 改变页脚计数与分页装箱（一棵折叠的子树只占 1 行）。
    // 单元数（根任务数）不变，所以分页器不会因为你折叠了一下就多出一页。
    const rows = flattenPmListRows(forest(1, 2), NO_PM_LIST_SORT, NO_CTX, new Set(["r1"]));
    expect(ids(rows)).toEqual(["r1"]);
    const open = toGroups(forest(1, 2));
    const closed = groupPmListRows(rows, "none");
    expect(paginatePmListGroups(open, 2, 0).unitCount).toBe(
      paginatePmListGroups(closed, 2, 0).unitCount,
    );
  });

  it("survives cyclic parent_id garbage", () => {
    const tasks = [
      task("a", { parent_id: "b" }),
      task("b", { parent_id: "a" }),
    ];
    const rows = flattenPmListRows(tasks, NO_PM_LIST_SORT, NO_CTX, new Set());
    expect(new Set(ids(rows)).size).toBe(rows.length);
  });
});

describe("groupPmListRows", () => {
  it("orders groups by data semantics (urgent first, unassigned last)", () => {
    const rows = flattenPmListRows(
      [
        task("t-1", { priority: "low" }),
        task("t-2", { priority: "urgent" }),
        task("t-3", { assignee: "i-1" }),
        task("t-4"),
      ],
      NO_PM_LIST_SORT,
      NO_CTX,
      new Set(),
    );
    expect(groupPmListRows(rows, "priority").map((g) => g.id)).toEqual([
      "urgent",
      "normal",
      "low",
    ]);
    expect(groupPmListRows(rows, "assignee").map((g) => g.id)).toEqual(["i-1", UNASSIGNED]);
  });

  it("returns one anonymous group when grouping is off", () => {
    const rows = flattenPmListRows([task("t-1")], NO_PM_LIST_SORT, NO_CTX, new Set());
    const groups = groupPmListRows(rows, "none");
    expect(groups).toHaveLength(1);
    expect(groups[0].id).toBe("");
  });
});

describe("movePmListColumn", () => {
  it("never moves the title column out of position 0", () => {
    expect(movePmListColumn(DEFAULT_PM_LIST_COLUMNS, "title", 1)).toEqual(
      DEFAULT_PM_LIST_COLUMNS,
    );
    expect(movePmListColumn(["title", "status", "priority"], "status", -1)).toEqual([
      "title",
      "status",
      "priority",
    ]);
  });

  it("swaps neighbours without mutating the input", () => {
    const before = ["title", "status", "priority"] as const;
    const next = movePmListColumn(before, "priority", -1);
    expect(next).toEqual(["title", "priority", "status"]);
    expect(before).toEqual(["title", "status", "priority"]);
  });
});

describe("paginatePmListGroups", () => {
  it("returns everything on one page when the budget covers the list", () => {
    const page = paginatePmListGroups(toGroups(forest(3)), 50, 0);
    expect(page.pageCount).toBe(1);
    expect(page.rowCount).toBe(3);
    expect(page.startRow).toBe(1);
    expect(page.endRow).toBe(3);
  });

  it("never splits a subtree across pages", () => {
    // 4 个根任务 × 2 个子任务 = 12 行；每页 5 行。
    const groups = toGroups(forest(4, 2));
    const p0 = paginatePmListGroups(groups, 5, 0);
    // 一个子树 3 行：装 1 棵 = 3 行，装 2 棵 = 6 行 > 5 → 第一页只有 3 行。
    expect(flatIds(p0.groups)).toEqual(["r1", "r1c1", "r1c2"]);
    const p1 = paginatePmListGroups(groups, 5, 1);
    expect(flatIds(p1.groups)).toEqual(["r2", "r2c1", "r2c2"]);
    // 每个子树完整出现在某一页，且只出现一次。
    const all = [p0, p1, paginatePmListGroups(groups, 5, 2), paginatePmListGroups(groups, 5, 3)];
    const seen = all.flatMap((p) => flatIds(p.groups));
    expect(seen).toHaveLength(12);
    expect(new Set(seen).size).toBe(12);
  });

  it("gives an oversized subtree its own page instead of looping forever", () => {
    const groups = toGroups(forest(3, 9)); // 每棵 10 行，预算 5
    const page = paginatePmListGroups(groups, 5, 0);
    expect(page.pageCount).toBe(3);
    expect(page.rowCount).toBe(10);
    expect(page.endRow).toBe(10);
  });

  it("reports cumulative row numbers, not pageIndex × pageSize", () => {
    const groups = toGroups(forest(4, 2));
    const p1 = paginatePmListGroups(groups, 5, 1);
    expect(p1.startRow).toBe(4);
    expect(p1.endRow).toBe(6);
  });

  it("clamps an out-of-range page when the result set shrinks", () => {
    const groups = toGroups(forest(2));
    expect(paginatePmListGroups(groups, 1, 99).pageIndex).toBe(1);
    expect(paginatePmListGroups(groups, 1, -3).pageIndex).toBe(0);
    expect(paginatePmListGroups(groups, 1, Number.NaN).pageIndex).toBe(0);
  });

  it("keeps group order and drops groups with no rows on the page", () => {
    const tasks = [
      task("a", { status: "done" as TaskStatus }),
      task("b", { status: "done" as TaskStatus }),
      task("c", { status: "pending" as TaskStatus }),
    ];
    const groups = groupPmListRows(
      flattenPmListRows(tasks, NO_PM_LIST_SORT, NO_CTX, new Set()),
      "status",
    );
    // 组顺序：pending 在 done 之前（STATUS_ORDER），每页 1 行。
    const p0 = paginatePmListGroups(groups, 1, 0);
    expect(p0.groups.map((g) => g.id)).toEqual(["pending"]);
    const p1 = paginatePmListGroups(groups, 1, 1);
    expect(p1.groups.map((g) => g.id)).toEqual(["done"]);
    expect(p1.groups[0].rows).toHaveLength(1);
  });

  it("treats a non-positive budget as no pagination", () => {
    const groups = toGroups(forest(5));
    const page = paginatePmListGroups(groups, 0, 2);
    expect(page.pageCount).toBe(1);
    expect(page.rowCount).toBe(5);
  });

  it("handles the empty project without a 0-page division", () => {
    const page = paginatePmListGroups([], 50, 0);
    expect(page.pageCount).toBe(1);
    expect(page.rowCount).toBe(0);
    expect(page.startRow).toBe(0);
    expect(page.totalRows).toBe(0);
  });

  it("counts subtrees, not rows, as units", () => {
    const page = paginatePmListGroups(toGroups(forest(3, 2)), 50, 0);
    expect(page.unitCount).toBe(3);
    expect(page.totalRows).toBe(9);
  });
});
