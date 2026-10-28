/**
 * `TaskStatusMove` — 弹框底部的状态流转控件。
 *
 * 要钉住的是三件事：
 *
 * 1. 只列服务端会接受的流转（`validate_transition`，
 *    `core/acowork-pm/src/store/tree.rs:570`）。客户端多列一个选项 = 给用户
 *    一个必然 409 的按钮。
 * 2. 审批闸门：Agent 新建、人类未批准的任务（pending + review_status=pending）
 *    不能进 in_progress（服务端 `ensure_creation_approved`），所以选项里
 *    没有 in_progress，取而代之的是"批准"。
 * 3. 写的是服务端那一列，不是表单字段 —— 它走 `boardStore.moveTask`，
 *    不经过 useTaskForm 的保存路径。
 */
import { describe, expect, it, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import type { PmTaskResponse, TaskStatus } from "../../lib/pm-types";

const mocks = vi.hoisted(() => ({
  moveTask: vi.fn(async () => true),
  reviewTask: vi.fn(async () => true),
  toast: vi.fn(),
}));

vi.mock("../../stores/pm/boardStore", () => ({
  usePmBoardStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({ moveTask: mocks.moveTask, reviewTask: mocks.reviewTask }),
}));

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

vi.mock("../../components/common/ToastProvider", () => ({
  showToast: (t: unknown) => mocks.toast(t),
}));

import { TaskStatusMove, STATUS_TRANSITIONS } from "./TaskStatusMove";

function row(over: Partial<PmTaskResponse> = {}): PmTaskResponse {
  return {
    id: "t-1",
    project_id: "p-1",
    title: "任务",
    description: "",
    type: "task",
    priority: "normal",
    status: "pending",
    review_status: "not_required",
    assignee: null,
    created_by: "u-1",
    parent_id: null,
    depth: 0,
    is_blocked: false,
    depends_on: [],
    start_at: null,
    due_at: null,
    dispatched_at: null,
    dispatch_attempts: 0,
    dispatch_error: null,
    created_at: "2026-10-01T02:00:00Z",
    updated_at: "2026-10-02T02:00:00Z",
    ...over,
  } as PmTaskResponse;
}

/** The control is a native <select>; its options are the legal targets. */
function options(): string[] {
  return Array.from(
    screen.getByLabelText("pm.task.moveTo").querySelectorAll("option"),
  ).map((o) => (o as HTMLOptionElement).value);
}

beforeEach(() => {
  mocks.moveTask.mockClear();
  mocks.reviewTask.mockClear();
  mocks.toast.mockClear();
  mocks.moveTask.mockResolvedValue(true);
  mocks.reviewTask.mockResolvedValue(true);
});

describe("the legal targets", () => {
  it("are the ones the server accepts", () => {
    render(<TaskStatusMove task={row()} onMoved={() => {}} />);
    // pending → in_progress / rejected / cancelled (server: validate_transition)
    expect(options()).toEqual(["", "in_progress", "rejected", "cancelled"]);
  });

  it("cover the columns a drag cannot reach (cancel / roll back)", () => {
    render(<TaskStatusMove task={row({ status: "in_progress" })} onMoved={() => {}} />);
    expect(options()).toEqual(["", "pending", "submitted", "cancelled"]);
  });

  it("vanish on a terminal status — nothing to move to", () => {
    const { container } = render(
      <TaskStatusMove task={row({ status: "cancelled" })} onMoved={() => {}} />,
    );
    expect(container.querySelector("select")).toBeNull();
  });

  it("mirror the server table", () => {
    // The client table is a mirror of `validate_transition`
    // (core/acowork-pm/src/store/tree.rs:570) — keep both in sight.
    expect(STATUS_TRANSITIONS.pending).toEqual(["in_progress", "rejected", "cancelled"]);
    expect(STATUS_TRANSITIONS.in_progress).toEqual(["pending", "submitted", "cancelled"]);
    expect(STATUS_TRANSITIONS.submitted).toEqual(["done", "rejected"]);
    expect(STATUS_TRANSITIONS.done).toEqual(["in_progress"]);
    expect(STATUS_TRANSITIONS.rejected).toEqual(["pending", "in_progress", "cancelled"]);
    expect(STATUS_TRANSITIONS.cancelled).toEqual([]);
  });
});

describe("the approval gate", () => {
  it("offers 批准 instead of in_progress for an unapproved agent task", () => {
    render(
      <TaskStatusMove
        task={row({ status: "pending", review_status: "pending" })}
        onMoved={() => {}}
      />,
    );
    // in_progress would 409 (ensure_creation_approved) — never offer it.
    expect(options()).toEqual(["", "__approve__", "rejected", "cancelled"]);
  });

  it("approving goes through reviewTask, not a status write", async () => {
    const onMoved = vi.fn();
    render(
      <TaskStatusMove
        task={row({ status: "pending", review_status: "pending" })}
        onMoved={onMoved}
      />,
    );
    fireEvent.change(screen.getByLabelText("pm.task.moveTo"), {
      target: { value: "__approve__" },
    });
    await waitFor(() => expect(mocks.reviewTask).toHaveBeenCalledWith("t-1", true));
    expect(mocks.moveTask).not.toHaveBeenCalled();
    await waitFor(() => expect(onMoved).toHaveBeenCalled());
  });
});

describe("moving", () => {
  it("writes the new status and re-reads the row", async () => {
    const onMoved = vi.fn();
    render(<TaskStatusMove task={row()} onMoved={onMoved} />);
    fireEvent.change(screen.getByLabelText("pm.task.moveTo"), {
      target: { value: "in_progress" },
    });
    await waitFor(() => expect(mocks.moveTask).toHaveBeenCalledWith("t-1", "in_progress"));
    await waitFor(() => expect(onMoved).toHaveBeenCalled());
    expect(mocks.toast).toHaveBeenCalled();
  });

  it("a rejected move leaves the row alone and says nothing", async () => {
    mocks.moveTask.mockResolvedValue(false);
    const onMoved = vi.fn();
    render(<TaskStatusMove task={row()} onMoved={onMoved} />);
    fireEvent.change(screen.getByLabelText("pm.task.moveTo"), {
      target: { value: "in_progress" },
    });
    await waitFor(() => expect(mocks.moveTask).toHaveBeenCalled());
    expect(onMoved).not.toHaveBeenCalled();
    expect(mocks.toast).not.toHaveBeenCalled();
  });

  it("re-picking the current status does nothing", async () => {
    render(<TaskStatusMove task={row()} onMoved={() => {}} />);
    fireEvent.change(screen.getByLabelText("pm.task.moveTo"), { target: { value: "" } });
    await waitFor(() => expect(mocks.moveTask).not.toHaveBeenCalled());
  });
});
