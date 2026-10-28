/**
 * The close path, end to end.
 *
 * `TaskEditDialog.test.tsx` mocks the task-detail store, so it can only prove
 * that the dialog *asks* to be closed. This file uses the REAL store, because
 * "close" is a store fact: `ProjectsView` renders the box iff
 * `usePmTaskDetailStore(s => s.mode)` is truthy, and `onClose` is
 * `clearDetail()` → `clear()` → `mode = null`. If that chain is broken, the
 * box stays on screen no matter how correct the dialog looks.
 *
 * The shape under test is the one the user reported: 新建任务 → 填内容 →
 * 保存 → 弹框必须自己消失。
 */
import { describe, expect, it, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor, act } from "@testing-library/react";
import { usePmTaskDetailStore } from "../../stores/pm/taskDetailStore";

const mocks = vi.hoisted(() => ({
  save: vi.fn(async () => ({ ok: true, taskId: "t-new" })),
  moveTask: vi.fn(async () => true),
  reviewTask: vi.fn(async () => true),
}));

vi.mock("../../lib/pm-api", () => ({
  attachmentUrl: (id: string) => `/api/pm/attachments/${id}`,
  deleteTask: vi.fn(async () => ({})),
  getTask: vi.fn(async () => ({
    id: "t-1",
    project_id: "p-1",
    title: "已有任务",
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
  })),
  listTaskAttachments: vi.fn(async () => []),
}));

vi.mock("../../stores/pm/boardStore", () => ({
  usePmBoardStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({ tasks: [], removeTask: vi.fn(), moveTask: mocks.moveTask, reviewTask: mocks.reviewTask }),
}));

vi.mock("../../components/common/ToastProvider", () => ({ showToast: vi.fn() }));

vi.mock("./pmMembers", () => ({
  useActorLabel: () => null,
}));

vi.mock("./SubtaskTree", () => ({ SubtaskTree: () => null }));

// The form is a seam here: what is under test is the dialog ↔ store contract,
// not the field set (that is `useTaskForm`'s own test file).
vi.mock("./useTaskForm", () => ({
  useTaskForm: () => ({
    values: {
      title: "新任务",
      type: "task",
      priority: "normal",
      description: "",
      assignee: "",
      dueAt: "",
      startAt: "",
      parentId: "",
      dependsOn: [],
    },
    setTitle: vi.fn(),
    setType: vi.fn(),
    setPriority: vi.fn(),
    setDescription: vi.fn(),
    setAssignee: vi.fn(),
    setDueAt: vi.fn(),
    setStartAt: vi.fn(),
    setParentId: vi.fn(),
    toggleDependency: vi.fn(),
    assigneeOptions: [],
    parentOptions: [],
    dependencyOptions: [],
    hasMembers: false,
    saving: false,
    dirty: false,
    reset: vi.fn(),
    save: mocks.save,
  }),
}));

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

import { TaskEditDialog } from "./TaskEditDialog";

/** Exactly what ProjectsView passes: the box's close IS the store's clear. */
function closeLikeProjectsView() {
  usePmTaskDetailStore.getState().clear();
}

beforeEach(() => {
  mocks.save.mockClear();
  mocks.save.mockResolvedValue({ ok: true, taskId: "t-new" });
  usePmTaskDetailStore.getState().clear();
});

describe("saving closes the box (real store)", () => {
  it("a new task: saving clears the store, so the box unmounts", async () => {
    act(() => {
      usePmTaskDetailStore.getState().openCreate({ projectId: "p-1" });
    });
    render(<TaskEditDialog onClose={closeLikeProjectsView} />);
    expect(usePmTaskDetailStore.getState().mode).toBe("create");

    fireEvent.click(screen.getByText("pm.task.save"));

    await waitFor(() => expect(mocks.save).toHaveBeenCalled());
    // The box is gone: the store says there is no mode to render.
    await waitFor(() => expect(usePmTaskDetailStore.getState().mode).toBeNull());
  });

  it("an edit: saving clears the store too", async () => {
    act(() => {
      void usePmTaskDetailStore.getState().openTask("t-1");
    });
    await waitFor(() => expect(usePmTaskDetailStore.getState().mode).toBe("edit"));
    render(<TaskEditDialog onClose={closeLikeProjectsView} />);

    fireEvent.click(screen.getByText("pm.task.save"));

    await waitFor(() => expect(mocks.save).toHaveBeenCalled());
    await waitFor(() => expect(usePmTaskDetailStore.getState().mode).toBeNull());
  });

  it("a failed save keeps the box (and the draft) open", async () => {
    mocks.save.mockResolvedValue({ ok: false, reason: "failed", message: "boom" });
    act(() => {
      usePmTaskDetailStore.getState().openCreate({ projectId: "p-1" });
    });
    render(<TaskEditDialog onClose={closeLikeProjectsView} />);

    fireEvent.click(screen.getByText("pm.task.save"));

    await waitFor(() => expect(mocks.save).toHaveBeenCalled());
    expect(usePmTaskDetailStore.getState().mode).toBe("create");
  });
});
