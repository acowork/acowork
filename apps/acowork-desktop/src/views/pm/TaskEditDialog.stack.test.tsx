/**
 * 任务弹框的**栈**：点子任务是在父任务弹框**上面**再开一个，关掉子任务回到
 * 父任务（父任务的弹框不卸载，草稿还在）。
 *
 * 用真实的 store 写这一条 —— 栈本身就是 store 的事实：框的数量 = `frames`
 * 的长度，而 `ProjectsView` 正是按 `frames.length` 决定画不画弹框的。
 * 旧的实现只有一格，点子任务等于把父任务整个替换掉，× 一关全没了（用户报
 * 的那个 bug）。
 */
import { describe, expect, it, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor, act } from "@testing-library/react";
import { usePmTaskDetailStore } from "../../stores/pm/taskDetailStore";

const mocks = vi.hoisted(() => ({
  moveTask: vi.fn(async () => true),
  reviewTask: vi.fn(async () => true),
}));

vi.mock("../../lib/pm-api", () => ({
  attachmentUrl: (id: string) => `/api/pm/attachments/${id}`,
  deleteTask: vi.fn(async () => ({})),
  getTask: vi.fn(async (id: string) => ({
    id,
    project_id: "p-1",
    title: id === "t-child" ? "子任务" : "父任务",
    description: "",
    type: "task",
    priority: "normal",
    status: "pending",
    review_status: "not_required",
    assignee: null,
    created_by: "u-1",
    parent_id: id === "t-child" ? "t-parent" : null,
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
  listTaskActivity: vi.fn(async () => []),
  addTaskActivity: vi.fn(async () => ({ id: "act-1" })),
}));

vi.mock("../../stores/pm/boardStore", () => ({
  usePmBoardStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({ tasks: [], removeTask: vi.fn(), moveTask: mocks.moveTask, reviewTask: mocks.reviewTask }),
}));

vi.mock("../../components/common/ToastProvider", () => ({ showToast: vi.fn() }));

vi.mock("./pmMembers", () => ({ useActorLabel: () => null }));

// 子任务树在这里是一个接缝：它只负责把"点了子任务"这件事说出来。
vi.mock("./SubtaskTree", () => ({
  SubtaskTree: ({ onSelect }: { onSelect: (id: string) => void }) => (
    <button type="button" onClick={() => onSelect("t-child")}>
      child-row
    </button>
  ),
}));

vi.mock("./useTaskForm", () => ({
  useTaskForm: () => ({
    values: {
      title: "标题",
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
    save: vi.fn(async () => ({ ok: true, taskId: "t-new" })),
  }),
}));

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key, i18n: { language: "en" } }),
}));

import { TaskEditDialog } from "./TaskEditDialog";

/** 子任务树住在「关联任务」标签页里。 */
function openRelatedTab() {
  fireEvent.click(screen.getByTestId("task-tab-related"));
}

/** ProjectsView 的关闭动作：清掉整个栈。 */
function closeLikeProjectsView() {
  usePmTaskDetailStore.getState().clear();
}

beforeEach(() => {
  usePmTaskDetailStore.getState().clear();
});

describe("点子任务 = 在父任务上面再开一个框", () => {
  it("子任务弹框盖在父任务弹框上面，父任务没有卸载", async () => {
    act(() => {
      void usePmTaskDetailStore.getState().openTask("t-parent");
    });
    await waitFor(() => expect(usePmTaskDetailStore.getState().frames).toHaveLength(1));

    render(<TaskEditDialog onClose={closeLikeProjectsView} />);
    expect(screen.getAllByRole("dialog")).toHaveLength(1);

    // 点子任务 → 栈上多一层，屏幕上多一个框（父任务那个还在）。
    openRelatedTab();
    fireEvent.click(screen.getByText("child-row"));
    await waitFor(() => expect(usePmTaskDetailStore.getState().frames).toHaveLength(2));
    expect(screen.getAllByRole("dialog")).toHaveLength(2);

    // × 关掉上面那层 → 回到父任务，父任务的框还在（没被一起关掉）。
    const closes = screen.getAllByLabelText("common.close");
    fireEvent.click(closes[closes.length - 1]);
    await waitFor(() => expect(usePmTaskDetailStore.getState().frames).toHaveLength(1));
    expect(screen.getAllByRole("dialog")).toHaveLength(1);
  });

  it("× 关掉子任务时，父任务那层的数据没有被覆盖", async () => {
    act(() => {
      void usePmTaskDetailStore.getState().openTask("t-parent");
    });
    await waitFor(() => expect(usePmTaskDetailStore.getState().frames).toHaveLength(1));
    const parentKey = usePmTaskDetailStore.getState().frames[0].key;
    render(<TaskEditDialog onClose={closeLikeProjectsView} />);

    openRelatedTab();
    fireEvent.click(screen.getByText("child-row"));
    await waitFor(() => expect(usePmTaskDetailStore.getState().frames).toHaveLength(2));

    const s = usePmTaskDetailStore.getState();
    expect(s.rows[parentKey]?.detail?.title).toBe("父任务");
    expect(s.rows[s.frames[1].key]?.detail?.title).toBe("子任务");
  });
});
