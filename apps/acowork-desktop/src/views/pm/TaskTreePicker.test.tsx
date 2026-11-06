/**
 * `TaskTreePicker` — 挑父任务 / 挑依赖的**选择形式**。
 *
 * 这里锁的三件事：
 * 1. 只有展开的枝才进 DOM（这是"任务多了会不会卡"的答案，不是样式偏好）；
 * 2. 父任务模式下自己的整棵子树不出现（含孙任务，不只是直接子任务）；
 * 3. 依赖模式下会成环的任务画出来但点不动，且原因写在行上，不藏在 tooltip。
 */
import { describe, it, expect, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { DependencyPicker, ParentTaskPicker, TaskTreePicker } from "./TaskTreePicker";
import { buildTaskTree } from "../../lib/taskTree";
import type { PmTaskResponse } from "../../lib/pm-types";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key, i18n: { language: "en" } }),
}));

function task(over: Partial<PmTaskResponse> & { id: string }): PmTaskResponse {
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
    created_at: "2026-10-01T00:00:00Z",
    updated_at: "2026-10-01T00:00:00Z",
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

/** 树：登录模块 → 接口重构 → 回调；支付模块（平级）；端到端测试依赖 回调。 */
function fixture(): PmTaskResponse[] {
  return [
    task({ id: "a", title: "登录模块" }),
    task({ id: "b", parent_id: "a", depth: 1, title: "接口重构" }),
    task({ id: "c", parent_id: "b", depth: 2, title: "回调" }),
    task({ id: "d", title: "支付模块" }),
    task({ id: "e", title: "端到端测试", depends_on: [{ task_id: "c", kind: "blocks" }] }),
  ];
}

/** 树里当前画出来的任务行（DOM 事实，不是模型事实）。 */
function renderedRowIds(): string[] {
  return Array.from(document.querySelectorAll("[data-testid='task-tree'] input")).map(
    (el) => (el.closest("label")?.textContent ?? "").trim(),
  );
}

describe("the tree renders only what is expanded", () => {
  it("starts with the roots and expands a branch on demand", () => {
    const tree = buildTaskTree(fixture());
    render(<TaskTreePicker tree={tree} mode="single" selected={[]} onSelect={() => {}} emptyLabel="none" />);
    // 根这一层：登录模块 / 支付模块 / 端到端测试 —— 孙任务一行都不在
    expect(renderedRowIds()).toEqual(["登录模块", "支付模块", "端到端测试"]);

    fireEvent.click(screen.getAllByLabelText("pm.task.expand")[0]);
    expect(renderedRowIds()).toEqual(["登录模块", "接口重构", "支付模块", "端到端测试"]);
    // 再下一层仍然是折叠的
    expect(renderedRowIds()).not.toContain("回调");
  });

  it("a collapsed branch contributes no DOM at all", () => {
    // 这条就是"两千个任务会不会挂两千个复选框"的答案：不会。
    const many = Array.from({ length: 200 }, (_, i) => task({ id: `root-${i}`, title: `根 ${i}` }));
    const children = Array.from({ length: 200 }, (_, i) =>
      task({ id: `kid-${i}`, parent_id: "root-0", depth: 1, title: `子 ${i}` }),
    );
    const tree = buildTaskTree([...many, ...children]);
    render(<TaskTreePicker tree={tree} mode="multi" selected={[]} onSelect={() => {}} emptyLabel="none" />);
    const boxes = document.querySelectorAll("[data-testid='task-tree'] input");
    // 200 个根可见；root-0 的 200 个子任务一个都不在 DOM 里
    expect(boxes).toHaveLength(200);
  });

  it("opens the branches that hold the current selection", () => {
    const tree = buildTaskTree(fixture());
    // 已选中的父任务是"回调"，它的祖先路径必须一开始就展开，否则选中项看不见
    render(
      <TaskTreePicker tree={tree} mode="single" selected={["c"]} onSelect={() => {}} emptyLabel="none" />,
    );
    expect(renderedRowIds()).toContain("回调");
    expect(screen.getByRole("radio", { name: "回调" })).toHaveProperty("checked", true);
  });

  it("falls back to an empty line instead of an empty box", () => {
    render(
      <TaskTreePicker tree={buildTaskTree([])} mode="single" selected={[]} onSelect={() => {}} emptyLabel="none" />,
    );
    expect(screen.getByText("none")).toBeTruthy();
  });
});

describe("父任务：自己的整棵子树不是候选", () => {
  it("hides the subtree, grandchild included — not just the direct children", () => {
    render(
      <ParentTaskPicker tasks={fixture()} selfId="a" value="" onChange={() => {}} />,
    );
    fireEvent.click(screen.getByText("pm.task.change"));
    const rows = renderedRowIds();
    // "登录模块"（自己）连同"接口重构""回调"整枝都不在候选里
    expect(rows).toEqual(["pm.task.noParent", "支付模块", "端到端测试"]);
    expect(renderedRowIds()).not.toContain("回调");
  });

  it("shows the current parent and closes the tree after picking one", () => {
    const onChange = vi.fn();
    render(<ParentTaskPicker tasks={fixture()} selfId="e" value="" onChange={onChange} />);
    expect(screen.getByTestId("parent-current")).toHaveProperty(
      "textContent",
      "pm.task.noParent",
    );
    fireEvent.click(screen.getByText("pm.task.change"));
    fireEvent.click(screen.getByRole("radio", { name: "支付模块" }));
    expect(onChange).toHaveBeenCalledWith("d");
    // 选完就收起：这一层的目的已经达到
    expect(screen.queryByTestId("task-tree")).toBeNull();
  });

  it("offers a way back to no parent at all", () => {
    const onChange = vi.fn();
    render(<ParentTaskPicker tasks={fixture()} selfId="e" value="d" onChange={onChange} />);
    expect(screen.getByTestId("parent-current")).toHaveProperty("textContent", "支付模块");
    fireEvent.click(screen.getByText("pm.task.change"));
    fireEvent.click(screen.getByRole("radio", { name: "pm.task.noParent" }));
    expect(onChange).toHaveBeenCalledWith("");
  });
});

describe("依赖：会成环的候选画出来但点不动", () => {
  it("disables the tasks that transitively depend on this one, with the reason on the row", () => {
    render(
      <DependencyPicker tasks={fixture()} selfId="c" selected={[]} onToggle={() => {}} />,
    );
    fireEvent.click(screen.getByText("+ pm.task.addDependency"));
    const cycleRow = screen.getByRole("checkbox", { name: /端到端测试/ }) as HTMLInputElement;
    expect(cycleRow.disabled).toBe(true);
    // 原因写在行上：悬停才看得见的解释等于没有解释
    expect(screen.getByText("pm.task.dependencyCycle")).toBeTruthy();
  });

  it("the task itself is not a candidate for itself", () => {
    render(
      <DependencyPicker tasks={fixture()} selfId="c" selected={[]} onToggle={() => {}} />,
    );
    fireEvent.click(screen.getByText("+ pm.task.addDependency"));
    expect(screen.queryByRole("checkbox", { name: "回调" })).toBeNull();
  });

  it("keeps the picked dependencies visible while the tree is closed", () => {
    const onToggle = vi.fn();
    render(
      <DependencyPicker tasks={fixture()} selfId="c" selected={["a"]} onToggle={onToggle} />,
    );
    // chips 是引用，不是候选：候选可以收起来，已选必须一直看得见
    const chips = screen.getByTestId("dependency-chips");
    expect(chips.textContent).toContain("登录模块");
    expect(screen.queryByTestId("task-tree")).toBeNull();
    fireEvent.click(screen.getByLabelText("common.remove 登录模块"));
    expect(onToggle).toHaveBeenCalledWith("a");
  });

  // 按钮不走手写样式：动作按钮用全局词表（btn-solid），chip 用 TagInput 那一套
  // 语法。理由见 styles/buttonVariants.test.ts —— 第三种色调只要在调用点悄悄长
  // 出来，肉眼看不出错，只有下一次并排摆放时才显得脏。
  it("wears the global button and chip grammar instead of hand-rolled styles", () => {
    render(<DependencyPicker tasks={fixture()} selfId="e" selected={["a"]} onToggle={() => {}} />);
    const add = screen.getByText("+ pm.task.addDependency");
    expect(add.className.split(/\s+/)).toContain("btn-solid");
    const chip = screen.getByTestId("dependency-chips").firstElementChild as HTMLElement;
    const chipTokens = (chip.className ?? "").split(/\s+/);
    expect(chipTokens).toContain("rounded");
    expect(chipTokens).toContain("bg-zinc-100");
    expect(chipTokens).toContain("text-10");
    // 删除控件不再是手写红字 ×，而是和 TagInput 一样的图标按钮
    expect(screen.getByLabelText("common.remove 登录模块")).toBeTruthy();
  });

  it("toggles a bare task on and off from the tree", () => {
    const onToggle = vi.fn();
    render(
      <DependencyPicker tasks={fixture()} selfId="e" selected={[]} onToggle={onToggle} />,
    );
    fireEvent.click(screen.getByText("+ pm.task.addDependency"));
    fireEvent.click(screen.getByRole("checkbox", { name: "支付模块" }));
    expect(onToggle).toHaveBeenCalledWith("d");
  });
});
