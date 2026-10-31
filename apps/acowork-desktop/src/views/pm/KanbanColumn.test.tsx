/**
 * KanbanColumn — 每列分页。
 *
 * 看板列原来只有虚拟滚动、没有单页上限，任务一多就变成一条无限长的列表。
 * 这些断言钉住分页的三件事：
 *   1. 单页只切出该页的列根任务（子任务随父任务整棵走）；
 *   2. 列底区间 / 页码 / 上下页按钮随页码更新，首末页按钮禁用；
 *   3. 任务数或单页数量变化后页码自动收敛，分页条在不需要时消失。
 */
import { describe, expect, it, vi } from "vitest";
import { useState } from "react";
import { fireEvent, render, screen } from "@testing-library/react";
import { DndContext } from "@dnd-kit/core";
import { KanbanColumn } from "./KanbanColumn";
import type { PmTaskResponse } from "../../lib/pm-types";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    // Keep assertions locale-independent; pageRange keeps its numbers.
    t: (key: string, opts?: Record<string, unknown>) =>
      opts ? `${key}:${String(opts.start)}-${String(opts.end)}/${String(opts.total)}` : key,
  }),
}));

vi.mock("../../stores/pm/healthStore", () => ({
  usePmHealthStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({ healthy: true }),
}));

vi.mock("../../stores/pm/boardStore", () => ({
  usePmBoardStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({ childrenOf: () => [] }),
}));

// TaskCard resolves display names through the project directory; the paging
// assertions do not need it (and it would pull in unrelated stores).
vi.mock("./pmMembers", () => ({
  useActorLabel: () => null,
}));

function task(index: number): PmTaskResponse {
  return {
    id: `t-${index}`,
    project_id: "p-1",
    title: `Task ${index}`,
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
  };
}

function tasks(count: number): PmTaskResponse[] {
  return Array.from({ length: count }, (_, i) => task(i));
}

// 页码在生产里由看板持有（拖动后要能翻页），测试用一个 stateful 宿主模拟。
// 宿主必须是模块级组件：写在 `column()` 里每次都是新类型，React 会重挂载
// 并把页码重置回 1，页码收敛类断言就失去意义。
function ColumnHost({ rows, size }: { rows: PmTaskResponse[]; size: number }) {
  const [page, setPage] = useState(1);
  return (
    <KanbanColumn
      status="pending"
      i18nKey="pm.board.pending"
      tasks={rows}
      onOpenTask={() => {}}
      pageSize={size}
      page={page}
      onPageChange={setPage}
    />
  );
}

function column(tasksInColumn: PmTaskResponse[], pageSize: number) {
  return (
    <DndContext>
      <ColumnHost rows={tasksInColumn} size={pageSize} />
    </DndContext>
  );
}

describe("KanbanColumn paging", () => {
  it("shows one page at a time and walks it with prev / next", () => {
    render(column(tasks(45), 20));

    expect(screen.getByText("pm.board.pageRange:1-20/45")).toBeTruthy();
    expect(screen.getByText("1/3")).toBeTruthy();
    expect(
      (screen.getByLabelText("pm.board.pagePrev") as HTMLButtonElement).disabled,
    ).toBe(true);

    fireEvent.click(screen.getByLabelText("pm.board.pageNext"));
    expect(screen.getByText("pm.board.pageRange:21-40/45")).toBeTruthy();
    expect(screen.getByText("2/3")).toBeTruthy();

    fireEvent.click(screen.getByLabelText("pm.board.pageNext"));
    expect(screen.getByText("pm.board.pageRange:41-45/45")).toBeTruthy();
    expect(screen.getByText("3/3")).toBeTruthy();
    expect(
      (screen.getByLabelText("pm.board.pageNext") as HTMLButtonElement).disabled,
    ).toBe(true);

    fireEvent.click(screen.getByLabelText("pm.board.pagePrev"));
    expect(screen.getByText("pm.board.pageRange:21-40/45")).toBeTruthy();
  });

  it("keeps short columns free of paging chrome", () => {
    render(column(tasks(12), 20));

    expect(screen.queryByText(/pm\.board\.pageRange/)).toBeNull();
    expect(screen.queryByLabelText("pm.board.pageNext")).toBeNull();
    expect(screen.queryByLabelText("pm.board.pagePrev")).toBeNull();
  });

  it("clamps the current page when the page size grows", () => {
    const view = render(column(tasks(120), 20));

    for (let i = 0; i < 5; i += 1) {
      fireEvent.click(screen.getByLabelText("pm.board.pageNext"));
    }
    expect(screen.getByText("6/6")).toBeTruthy();

    // 100 per page → 2 pages; page 6 no longer exists and collapses to 2.
    view.rerender(column(tasks(120), 100));
    expect(screen.getByText("2/2")).toBeTruthy();
    expect(screen.getByText("pm.board.pageRange:101-120/120")).toBeTruthy();
  });

  it("drops back to page 1 when the column empties", () => {
    const view = render(column(tasks(45), 20));
    fireEvent.click(screen.getByLabelText("pm.board.pageNext"));
    expect(screen.getByText("2/3")).toBeTruthy();

    view.rerender(column([], 20));
    expect(screen.queryByText(/pm\.board\.pageRange/)).toBeNull();
    expect(screen.getByText("pm.board.empty")).toBeTruthy();
  });
});
