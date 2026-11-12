/**
 * TaskCard — 两条硬约束。
 *
 * 1. 卡片是两段：标题带 + `border-t` 发丝线 + `bg-panel-inset` 正文带（与
 *    右侧设置面板的 ListBox 卡片同一语法）。合成一整块平铺色块就又是另一种
 *    卡片了。
 * 2. 源卡片绝不贴 dnd-kit 的 active transform。看板用 `<DragOverlay>` 画跟随
 *    指针的那一份，而 `ActiveDraggableContext` 的 transform 带 scaleX/scaleY
 *    （dnd-kit core `adjustScale` = overRect / activeNodeRect）：指针还在源卡片
 *    上时比值恰为 1（看起来正常），一进目标列就按整列 rect 放大数倍，再被列的
 *    `overflow-hidden` 裁掉。这条断言钉的是「不贴回去」。
 */
import { describe, expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { render, screen } from "@testing-library/react";
import { DndContext } from "@dnd-kit/core";
import { TaskCard } from "./TaskCard";
import type { PmTaskResponse } from "../../lib/pm-types";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

vi.mock("../../stores/pm/healthStore", () => ({
  usePmHealthStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({ healthy: true }),
}));

vi.mock("../../stores/pm/boardStore", () => ({
  usePmBoardStore: Object.assign(
    (selector: (s: Record<string, unknown>) => unknown) => selector({ childrenOf: () => [] }),
    { getState: () => ({ childrenOf: () => [] }) },
  ),
}));

vi.mock("./pmMembers", () => ({
  useActorLabel: () => null,
}));

const __dirname = dirname(fileURLToPath(import.meta.url));

function task(over: Partial<PmTaskResponse> = {}): PmTaskResponse {
  return {
    id: "t-1",
    project_id: "p-1",
    title: "Task 1",
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
    ...over,
  };
}

function renderCard(over: Partial<PmTaskResponse> = {}) {
  return render(
    <DndContext>
      <TaskCard task={task(over)} onOpenTask={() => {}} />
    </DndContext>,
  );
}

describe("TaskCard card grammar", () => {
  it("splits title band from an inset body band", () => {
    renderCard();
    const card = screen.getByLabelText("Task 1");

    // 外壳 = ListBox box：圆角卡片 + 外发丝线 + 裁掉子块自己的圆角
    expect(card.className).toContain("overflow-hidden");
    expect(card.className).toContain("bg-panel-block");

    // 两段：标题带、正文带（待审核任务才有第三段，这里没有）
    expect(card.children).toHaveLength(2);
    const body = card.children[1] as HTMLElement;
    expect(body.className).toContain("border-t");
    expect(body.className).toContain("bg-panel-inset");
  });

  it("adds the review band as a third separated segment", () => {
    renderCard({ status: "submitted" });
    const card = screen.getByLabelText("Task 1");
    expect(card.children).toHaveLength(3);
    const review = card.children[2] as HTMLElement;
    expect(review.className).toContain("border-t");
    expect(review.className).toContain("bg-panel-inset");
  });
});

describe("TaskCard 查看执行", () => {
  // 这颗按钮的关键就是"哪些状态有东西可看"：三种跑过的状态都出，没跑过的
  // 状态不出（否则点了只会弹"未找到执行会话"）。
  const find = () => screen.queryByRole("button", { name: /pm\.review\.openExec/ });

  it.each(["in_progress", "submitted", "done"] as const)("shows on %s", (status) => {
    renderCard({ status });
    expect(find()).not.toBeNull();
  });

  it.each(["pending", "rejected", "cancelled"] as const)("hides on %s", (status) => {
    renderCard({ status });
    expect(find()).toBeNull();
  });
});

describe("TaskCard drag source", () => {
  it("never mirrors the active draggable transform (scale would blow the card up)", () => {
    const source = readFileSync(resolve(__dirname, "TaskCard.tsx"), "utf8");
    // 跟随指针的那一份由 KanbanBoard 的 <DragOverlay> 负责。
    expect(source).not.toContain("CSS.Transform.toString");
    expect(source).not.toContain("CSS.Translate.toString");
  });

  it("renders the source card with no inline transform", () => {
    renderCard();
    const card = screen.getByLabelText("Task 1");
    expect(card.getAttribute("style")).toBeNull();
  });
});
