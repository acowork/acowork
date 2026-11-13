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

  // ── 层级：确认框必须压在自己的弹框之上 ──────────────────────────
  //
  // 本弹框自己会弹 ConfirmDialog（删除 / 放弃修改），那是个 `fixed inset-0
  // z-50` 的浮层。弹框层如果也在 z-50 之上，子任务层（depth=1）的确认框
  // 就被自己的弹框盖住 —— 用户点了「删除」什么也看不见。
  it("确认框压在自己的弹框之上：弹框层低于 ConfirmDialog 的 z-50", async () => {
    act(() => {
      void usePmTaskDetailStore.getState().openTask("t-parent");
    });
    await waitFor(() => expect(usePmTaskDetailStore.getState().frames).toHaveLength(1));
    render(<TaskEditDialog onClose={closeLikeProjectsView} />);

    openRelatedTab();
    fireEvent.click(screen.getByText("child-row"));
    await waitFor(() => expect(usePmTaskDetailStore.getState().frames).toHaveLength(2));

    // 两层都在场，且层号递增（上层盖下层）。
    const layers = screen.getAllByRole("dialog").map((el) => {
      const layer = el.parentElement as HTMLElement;
      return Number(layer.style.zIndex);
    });
    expect(layers).toHaveLength(2);
    expect(layers[1]).toBeGreaterThan(layers[0]);
    // 关键：都在 ConfirmDialog（z-50）之下 —— 确认框浮在弹框之上。
    expect(Math.max(...layers)).toBeLessThan(50);

    // 只有栈顶是模态 + 可聚焦：下面那层露着一条边，但它不该抢焦点，
    // 否则 Tab 键能绕进被盖住的父任务表单里。
    const panes = screen.getAllByRole("dialog");
    expect(panes[0].getAttribute("aria-modal")).toBe("false");
    expect(panes[1].getAttribute("aria-modal")).toBe("true");
    expect(panes[0].hasAttribute("inert")).toBe(true);
    expect(panes[1].hasAttribute("inert")).toBe(false);
  });

  // ── 确认框只属于栈顶 ───────────────────────────────────────────
  //
  // ConfirmDialog 是 fixed + z-50，浮在所有层之上，遮罩拦不住它。父层那一
  // 份如果也渲染，用户以为在删子任务，实际触发的是**父任务**的删除确认。
  it("只有栈顶那层能弹确认框：父层那份不出现", async () => {
    act(() => {
      void usePmTaskDetailStore.getState().openTask("t-parent");
    });
    await waitFor(() => expect(usePmTaskDetailStore.getState().frames).toHaveLength(1));
    render(<TaskEditDialog onClose={closeLikeProjectsView} />);

    openRelatedTab();
    fireEvent.click(screen.getByText("child-row"));
    await waitFor(() => expect(usePmTaskDetailStore.getState().frames).toHaveLength(2));

    // 两层都有「删除」按钮。点**父层**那个（DOM 顺序在前 = 栈底）。
    const deletes = screen.getAllByText("pm.task.delete");
    expect(deletes.length).toBe(2);
    fireEvent.click(deletes[0]);
    // 父层不是栈顶 → 不弹确认框。
    expect(screen.queryByText("pm.task.deleteTaskTitle")).toBeNull();

    // 点栈顶那个 → 弹出**子任务**的确认框。
    fireEvent.click(deletes[deletes.length - 1]);
    expect(screen.getByText("pm.task.deleteTaskTitle")).not.toBeNull();
  });

  // ── 父层数据跟着子层的变化更新 ─────────────────────────────────
  //
  // 子任务保存后 pop，父层露出来 —— 它的行数据可能已经过时。单层时代没有
  // 这个问题（保存即关窗），栈化之后必须显式补一次刷新。
  it("子任务保存后，露出来的父层被刷新", async () => {
    act(() => {
      void usePmTaskDetailStore.getState().openTask("t-parent");
    });
    await waitFor(() => expect(usePmTaskDetailStore.getState().frames).toHaveLength(1));
    render(<TaskEditDialog onClose={closeLikeProjectsView} />);

    openRelatedTab();
    fireEvent.click(screen.getByText("child-row"));
    await waitFor(() => expect(usePmTaskDetailStore.getState().frames).toHaveLength(2));

    const parentKey = usePmTaskDetailStore.getState().frames[0].key;
    const before = usePmTaskDetailStore.getState().rows[parentKey]?.detail;

    // 保存子任务 → 这一层关闭，父层重新露出来。
    const saves = screen.getAllByText("pm.task.save");
    fireEvent.click(saves[saves.length - 1]);
    await waitFor(() => expect(usePmTaskDetailStore.getState().frames).toHaveLength(1));

    // 父层的行被重新拉过（detail 是新对象，不是原来那个）。
    const after = usePmTaskDetailStore.getState().rows[parentKey]?.detail;
    expect(after).not.toBe(before);
  });
});
