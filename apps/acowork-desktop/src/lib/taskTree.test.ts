/**
 * `lib/taskTree` — 挑父任务 / 挑依赖时的两条结构规则。
 *
 * 这里测的都是图上的事实，不碰 React：哪一块是"我的子树"、哪一块"依赖我因此
 * 会成环"。选择器只负责把这两个集合画成"不出现"和"出现但点不动"。
 */
import { describe, it, expect } from "vitest";
import {
  ancestorIds,
  buildTaskTree,
  dependentClosureIds,
  expandedForSelection,
  subtreeIds,
} from "./taskTree";
import type { PmTaskResponse } from "./pm-types";

let seq = 0;

function task(over: Partial<PmTaskResponse> & { id: string }): PmTaskResponse {
  seq += 1;
  return {
    project_id: "p-1",
    title: over.id,
    description: "",
    type: "task",
    priority: "normal",
    status: "pending",
    review_status: "none",
    assignee: null,
    creator: "u-1",
    created_at: `2026-10-${String(seq % 28 + 1).padStart(2, "0")}T00:00:00Z`,
    updated_at: `2026-10-${String(seq % 28 + 1).padStart(2, "0")}T00:00:00Z`,
    start_at: null,
    due_at: null,
    dispatched_at: null,
    dispatch_attempts: 0,
    dispatch_error: null,
    depends_on: [],
    attachments: [],
    result: null,
    is_blocked: false,
    blocked_by: [],
    depth: 0,
    parent_id: null,
    ...over,
  } as PmTaskResponse;
}

/** a → b → c（一条链）；d 独立；x 依赖 a 的两跳链。 */
function fixture(): PmTaskResponse[] {
  return [
    task({ id: "a", title: "登录模块" }),
    task({ id: "b", parent_id: "a", depth: 1, title: "接口重构" }),
    task({ id: "c", parent_id: "b", depth: 2, title: "回调" }),
    task({ id: "d", title: "支付模块" }),
    task({ id: "x", title: "端到端测试", depends_on: [{ task_id: "c", kind: "blocks" }] }),
    task({ id: "y", title: "回归", depends_on: [{ task_id: "x", kind: "relates" }] }),
  ];
}

describe("buildTaskTree", () => {
  it("groups children under their parent and keeps the input order", () => {
    const tree = buildTaskTree(fixture());
    expect(tree.roots.map((t) => t.id)).toEqual(["a", "d", "x", "y"]);
    expect(tree.childrenByParent.get("a")?.map((t) => t.id)).toEqual(["b"]);
    expect(tree.childrenByParent.get("b")?.map((t) => t.id)).toEqual(["c"]);
  });

  it("treats a task whose parent is outside the set as a root", () => {
    // 子集（一页搜索结果、一段筛选后的看板）不能让自己的任务凭空消失。
    const tree = buildTaskTree([task({ id: "c", parent_id: "b", depth: 2 })]);
    expect(tree.roots.map((t) => t.id)).toEqual(["c"]);
  });

  it("indexes the reverse dependency edges", () => {
    const tree = buildTaskTree(fixture());
    expect(tree.dependents.get("c")?.map((t) => t.id)).toEqual(["x"]);
    expect(tree.dependents.get("x")?.map((t) => t.id)).toEqual(["y"]);
    expect(tree.dependents.get("a")).toBeUndefined();
  });
});

describe("subtreeIds — a task cannot be parented under itself", () => {
  it("is the task plus every descendant, not just its direct children", () => {
    const tree = buildTaskTree(fixture());
    expect([...subtreeIds(tree, "a")].sort()).toEqual(["a", "b", "c"]);
    // 直接子任务不是全部：孙任务同样不能当父任务（服务端 cycle_detected）
    expect(subtreeIds(tree, "a").has("c")).toBe(true);
  });

  it("is just the task itself for a leaf", () => {
    const tree = buildTaskTree(fixture());
    expect([...subtreeIds(tree, "c")]).toEqual(["c"]);
  });

  it("is empty for an unknown id", () => {
    const tree = buildTaskTree(fixture());
    expect(subtreeIds(tree, "nope").size).toBe(0);
  });
});

describe("dependentClosureIds — a task cannot depend on something that waits for it", () => {
  it("is transitive: two hops away is still a cycle", () => {
    const tree = buildTaskTree(fixture());
    // c ← x ← y：让 c 依赖 y 会把 c→y→x→c 闭成环
    expect([...dependentClosureIds(tree, "c")].sort()).toEqual(["x", "y"]);
  });

  it("never contains the task itself", () => {
    const tree = buildTaskTree(fixture());
    expect(dependentClosureIds(tree, "x").has("x")).toBe(false);
  });

  it("ignores the edge kind — the server does not distinguish either", () => {
    const tree = buildTaskTree(fixture());
    // x 依赖 c 的边是 blocks，y 依赖 x 的边是 relates，两者都算
    expect(dependentClosureIds(tree, "x").has("y")).toBe(true);
  });

  it("is empty for a task nobody waits for", () => {
    const tree = buildTaskTree(fixture());
    expect(dependentClosureIds(tree, "d").size).toBe(0);
  });

  it("survives a cycle already in the data instead of looping forever", () => {
    const cyclic = [
      task({ id: "m", depends_on: [{ task_id: "n", kind: "blocks" }] }),
      task({ id: "n", depends_on: [{ task_id: "m", kind: "blocks" }] }),
    ];
    const tree = buildTaskTree(cyclic);
    expect([...dependentClosureIds(tree, "m")]).toEqual(["n"]);
  });
});

describe("ancestors / expansion", () => {
  it("walks root → parent, excluding the task itself", () => {
    const tree = buildTaskTree(fixture());
    expect(ancestorIds(tree, "c")).toEqual(["a", "b"]);
    expect(ancestorIds(tree, "a")).toEqual([]);
  });

  it("does not loop on a corrupt parent chain", () => {
    const broken = [
      task({ id: "p", parent_id: "q" }),
      task({ id: "q", parent_id: "p" }),
    ];
    const tree = buildTaskTree(broken);
    expect(ancestorIds(tree, "p").length).toBeLessThanOrEqual(2);
  });

  it("opens the branches that hold the selection, and only those", () => {
    const tree = buildTaskTree(fixture());
    expect([...expandedForSelection(tree, ["c"])].sort()).toEqual(["a", "b"]);
    expect(expandedForSelection(tree, []).size).toBe(0);
    // 已选项自己不需要展开（展开的是它的祖先）
    expect(expandedForSelection(tree, ["a"]).has("a")).toBe(false);
  });
});
