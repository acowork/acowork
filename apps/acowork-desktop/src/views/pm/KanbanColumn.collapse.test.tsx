/**
 * KanbanColumn — 可折叠形态（群聊右侧任务面板用的那一版）。
 *
 * 钉住三件事：
 *   1. 折叠时只有列头：卡片、分页条都不渲染（面板窄，折叠就是为了省高度）；
 *   2. 列头就是这个开关（`aria-expanded`），点一下上报 onToggleCollapsed；
 *   3. 展开后卡片与分页条回来。不传 onToggleCollapsed 的看板列没有这个按钮
 *      —— 那条路径由 `KanbanColumn.test.tsx` 覆盖。
 */
import { describe, expect, it, vi } from "vitest";
import { useState } from "react";
import { fireEvent, render, screen } from "@testing-library/react";
import { DndContext } from "@dnd-kit/core";
import { KanbanColumn } from "./KanbanColumn";
import type { PmTaskResponse } from "../../lib/pm-types";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
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

function FoldHost({
  rows,
  initial,
  variant,
}: {
  rows: PmTaskResponse[];
  initial: boolean;
  variant?: "pane" | "card";
}) {
  const [collapsed, setCollapsed] = useState(initial);
  return (
    <KanbanColumn
      status="pending"
      i18nKey="pm.board.pending"
      tasks={rows}
      onOpenTask={() => {}}
      pageSize={20}
      page={1}
      onPageChange={() => {}}
      variant={variant}
      collapsed={collapsed}
      onToggleCollapsed={() => setCollapsed((v) => !v)}
    />
  );
}

/** 群聊任务面板就是这条路径：有 `DndContext` 但**没有传感器**（窄面板里
 *  纵向拖拽没有意义），列必须能正常渲染、卡片只读可点。下面每个用例都走
 *  这条 read-only 路径，所以它顺带钉住"无传感器也能挂载"。 */
const NO_SENSORS: [] = [];

const rows = [0, 1, 2].map(task);
const fold = (initial: boolean) => (
  <DndContext sensors={NO_SENSORS}>
    <FoldHost rows={rows} initial={initial} />
  </DndContext>
);

describe("KanbanColumn collapsed", () => {
  it("renders the head only: count badge, no cards, no pager", () => {
    render(fold(true));

    const head = screen.getByRole("button", { name: /pm\.board\.pending/ });
    expect(head.getAttribute("aria-expanded")).toBe("false");
    expect(head.textContent).toContain("3");
    expect(screen.queryByText("Task 0")).toBeNull();
    expect(screen.queryByText(/pm\.board\.pageRange/)).toBeNull();
  });

  it("expands from the head click and pages again", () => {
    render(fold(true));

    fireEvent.click(screen.getByRole("button", { name: /pm\.board\.pending/ }));

    expect(screen.getByText("Task 0")).toBeTruthy();
    expect(
      screen.getByRole("button", { name: /pm\.board\.pending/ }).getAttribute("aria-expanded"),
    ).toBe("true");
    // 3 条 < 单页 20 → 没有分页条；有分页时按钮与展开状态互不干扰，由
    // KanbanColumn.test.tsx 的翻页断言守住。
    expect(screen.queryByLabelText("pm.board.pageNext")).toBeNull();
  });

  // 两个视觉回归（用户实测报过）：
  //   1. 折叠时列头的下沿发丝线和卡片外框撞在同一条线上，看起来像加粗；
  //   2. 正文高度上限：内容少按内容高度，超过上限才在卡内滚动。
  it("drops the head hairline while folded and caps the body height when open", () => {
    const { container } = render(fold(true));
    const head = () =>
      screen.getByRole("button", { name: /pm\.board\.pending/ }).closest("header")!;

    expect(head().className).not.toContain("border-b");

    fireEvent.click(screen.getByRole("button", { name: /pm\.board\.pending/ }));
    expect(head().className).toContain("border-b");
    // 滚动区带上限 → 内容不到上限时按内容高度，超过才滚
    expect(container.querySelector(".overlay-scrollbar")?.getAttribute("style")).toContain(
      "max-height: 60vh",
    );
  });

  it("card variant (settings-card grammar) folds and unfolds the same way", () => {
    render(
      <DndContext sensors={NO_SENSORS}>
        <FoldHost rows={rows} initial={true} variant="card" />
      </DndContext>,
    );
    expect(screen.queryByText("Task 0")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: /pm\.board\.pending/ }));
    expect(screen.getByText("Task 0")).toBeTruthy();
  });

  it("keeps the pager while expanded when a page overflows", () => {
    render(
      <DndContext sensors={NO_SENSORS}>
        <FoldHost rows={Array.from({ length: 45 }, (_, i) => task(i))} initial={false} />
      </DndContext>,
    );

    expect(screen.getByText("pm.board.pageRange:1-20/45")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /pm\.board\.pending/ }));
    expect(screen.queryByText(/pm\.board\.pageRange/)).toBeNull();
    expect(screen.queryByText("Task 0")).toBeNull();
  });
});