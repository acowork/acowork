/**
 * TaskEditDialog — the single task surface (create + edit + attachments +
 * subtasks + delete).
 *
 * What this file has to pin, in priority order:
 *
 * 1. The box did not get bigger or jumpy. The retired detail dialog was a
 *    five-tab shell whose height followed its content; this one is a fixed
 *    640px-wide box with a fixed height and a scrolling body, so jsdom class
 *    assertions ARE the spec (same approach as capsule.test.tsx).
 * 2. The three carried-over capabilities need a persisted row. Attachments
 *    upload to `POST /tasks/{tid}/attachments`, so in create mode they must not
 *    be offered — and saving a new task must submit first and close the box
 *    only after the server confirms the row.
 * 3. Where the reader lands after save / delete / navigate-to-child, and that
 *    a failed save keeps the box (and the draft) open.
 * 4. One dirty-form guard for every way out (cancel, ESC, backdrop, ×,
 *    navigating to a child), which the old form never had.
 */
import { describe, expect, it, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import type { PmAttachmentMeta, PmTaskResponse } from "../../lib/pm-types";

const mocks = vi.hoisted(() => ({
  mode: "edit" as "edit" | "create" | null,
  detail: null as PmTaskResponse | null,
  attachments: [] as PmAttachmentMeta[],
  uploading: false,
  error: null as string | null,
  projectId: null as string | null,
  createParentId: null as string | null,
  returnTo: null as string | null,
  tasks: [] as PmTaskResponse[],
  dirty: false,
  saving: false,
  hasMembers: true,
  values: {} as Record<string, unknown>,
  openTask: vi.fn(async () => {}),
  openCreate: vi.fn(),
  setMode: vi.fn(),
  refresh: vi.fn(async () => {}),
  uploadAttachment: vi.fn(async () => ({ id: "att-1" }) as unknown as PmAttachmentMeta),
  deleteAttachment: vi.fn(async () => true),
  removeTask: vi.fn(),
  moveTask: vi.fn(async () => true),
  reviewTask: vi.fn(async () => true),
  deleteTask: vi.fn(async () => ({})),
  save: vi.fn(async () => ({ ok: true, taskId: "t-new" })),
  reset: vi.fn(),
}));

function detailState() {
  return {
    taskId: mocks.detail?.id ?? null,
    detail: mocks.detail,
    attachments: mocks.attachments,
    uploading: mocks.uploading,
    loading: false,
    error: mocks.error,
    mode: mocks.mode,
    projectId: mocks.projectId ?? mocks.detail?.project_id ?? null,
    createParentId: mocks.createParentId,
    returnTo: mocks.returnTo,
    openTask: mocks.openTask,
    openCreate: mocks.openCreate,
    setMode: mocks.setMode,
    refresh: mocks.refresh,
    uploadAttachment: mocks.uploadAttachment,
    deleteAttachment: mocks.deleteAttachment,
    updateDetail: vi.fn(),
    close: vi.fn(),
    clear: vi.fn(),
  };
}

vi.mock("../../stores/pm/taskDetailStore", () => ({
  usePmTaskDetailStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector(detailState()),
}));

vi.mock("../../stores/pm/boardStore", () => ({
  usePmBoardStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({
      tasks: mocks.tasks,
      removeTask: mocks.removeTask,
      moveTask: mocks.moveTask,
      reviewTask: mocks.reviewTask,
    }),
}));

// The tree is covered by its own tests; here it is a seam with one clickable
// child, which is all the dialog's navigation guard needs.
vi.mock("./SubtaskTree", () => ({
  SubtaskTree: ({ rootId, onSelect }: { rootId: string; onSelect: (id: string) => void }) => (
    <div data-testid="subtask-tree" data-root={rootId}>
      <button type="button" onClick={() => onSelect("t-child")}>
        child-row
      </button>
    </div>
  ),
}));

vi.mock("../../lib/pm-api", () => ({
  attachmentUrl: (id: string) => `/api/pm/attachments/${id}`,
  deleteTask: (...args: unknown[]) => mocks.deleteTask(...args),
}));

vi.mock("../../components/common/ToastProvider", () => ({ showToast: vi.fn() }));

// Names, not uuids — the audit line is the place a raw id used to leak out.
vi.mock("./pmMembers", () => ({
  useActorLabel: (id: string | null) => (id === "u-1" ? "张三" : null),
}));

// The form is covered by `useTaskForm.test.ts` / `useTaskForm.save.test.ts`;
// here it is a seam so the dialog's navigation is tested without the member /
// agent / project graph behind it.
vi.mock("./useTaskForm", () => ({
  useTaskForm: () => ({
    values: {
      title: "Row title",
      type: "task",
      priority: "normal",
      description: "",
      assignee: "",
      dueAt: "",
      startAt: "",
      parentId: "",
      dependsOn: [],
      ...mocks.values,
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
    hasMembers: mocks.hasMembers,
    saving: mocks.saving,
    dirty: mocks.dirty,
    reset: mocks.reset,
    save: mocks.save,
  }),
  localDateFieldToIso: (v: string) => v,
  isoToLocalDate: (v?: string | null) => v ?? "",
}));

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

import { TaskEditDialog } from "./TaskEditDialog";

function row(over: Partial<PmTaskResponse> = {}): PmTaskResponse {
  return {
    id: "t-1",
    project_id: "p-1",
    title: "Persisted row",
    description: "",
    type: "task",
    priority: "normal",
    status: "in_progress",
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
    review_status: "approved",
    created_at: "2026-10-01T02:00:00Z",
    updated_at: "2026-10-02T02:00:00Z",
    ...over,
  } as PmTaskResponse;
}

const att = (over: Partial<PmAttachmentMeta> = {}): PmAttachmentMeta =>
  ({ id: "att-1", filename: "shot.png", kind: "image", size: 2048, ...over }) as PmAttachmentMeta;

beforeEach(() => {
  mocks.mode = "edit";
  mocks.detail = row();
  mocks.attachments = [];
  mocks.uploading = false;
  mocks.error = null;
  mocks.projectId = null;
  mocks.createParentId = null;
  mocks.returnTo = null;
  mocks.tasks = [];
  mocks.dirty = false;
  mocks.saving = false;
  mocks.hasMembers = true;
  mocks.values = {};
  mocks.openTask.mockClear();
  mocks.openCreate.mockClear();
  mocks.setMode.mockClear();
  mocks.uploadAttachment.mockClear();
  mocks.deleteAttachment.mockClear();
  mocks.removeTask.mockClear();
  mocks.deleteTask.mockClear();
  // Re-seed the save seam: a test that swaps it for a failing one must not
  // leak that failure into the next test.
  mocks.save = vi.fn(async () => ({ ok: true, taskId: "t-new" }));
  mocks.reset.mockClear();
});

describe("chrome — the box the user asked to keep", () => {
  it("is a fixed-height box, so nothing resizes it when sections appear", () => {
    const { container } = render(<TaskEditDialog onClose={() => {}} />);
    const cls = container.querySelector('[role="dialog"]')?.className ?? "";
    expect(cls).toContain("h-[560px]");
    expect(cls).toContain("max-h-[86vh]");
    // No max-h-only panel: content-driven height is what made the old shell jump.
    expect(cls).not.toMatch(/max-h-\[70vh\]/);
  });

  it("keeps the retired form's width and centre placement", () => {
    const { container } = render(<TaskEditDialog onClose={() => {}} />);
    const panel = container.querySelector('[role="dialog"]');
    expect(panel?.className).toContain("max-w-[640px]");
    expect(panel?.className).toContain("relative z-10");
    expect(panel?.parentElement?.className).toContain("items-center");
  });

  it("has no tab strip at all", () => {
    render(<TaskEditDialog onClose={() => {}} />);
    expect(screen.queryByRole("tablist")).toBeNull();
  });

  it("type / priority / assignee share one row", () => {
    const { container } = render(<TaskEditDialog onClose={() => {}} />);
    const cells = Array.from(container.querySelector(".grid.grid-cols-3")?.children ?? []);
    expect(cells).toHaveLength(3);
    const first = cells.map((c) => (c.querySelector("span")?.textContent ?? "").trim());
    expect(first).toEqual(["pm.task.type", "pm.task.priority", "pm.task.assignee"]);
  });

  it("the two dates share one row", () => {
    const { container } = render(<TaskEditDialog onClose={() => {}} />);
    const cells = Array.from(container.querySelector(".grid.grid-cols-2")?.children ?? []);
    expect(cells).toHaveLength(2);
    const first = cells.map((c) => (c.querySelector("span")?.textContent ?? "").trim());
    expect(first).toEqual(["pm.task.startDate", "pm.task.dueDate"]);
  });

  it("the title spans the full width", () => {
    const { container } = render(<TaskEditDialog onClose={() => {}} />);
    const cell = Array.from(container.querySelectorAll("label")).find((el) =>
      (el.querySelector("span")?.textContent ?? "").trim().startsWith("pm.task.title"),
    );
    // Its wrapper is the plain stack, not one of the multi-column grids.
    expect(cell?.parentElement?.className).toContain("space-y-3");
  });
});

describe("the three carried-over capabilities", () => {
  it("a persisted row offers attachments, subtasks and delete", () => {
    render(<TaskEditDialog onClose={() => {}} />);
    expect(screen.getByText("pm.task.attachments")).toBeTruthy();
    expect(screen.getByText("pm.task.uploadAttachment")).toBeTruthy();
    expect(screen.getByTestId("subtask-tree")).toBeTruthy();
    expect(screen.getByText("pm.task.delete")).toBeTruthy();
  });

  it("create mode offers none of them — there is no task id to attach to", () => {
    mocks.mode = "create";
    mocks.detail = null;
    render(<TaskEditDialog onClose={() => {}} />);
    expect(screen.queryByText("pm.task.uploadAttachment")).toBeNull();
    expect(screen.queryByTestId("subtask-tree")).toBeNull();
    expect(screen.queryByText("pm.task.delete")).toBeNull();
    // but the form itself is the same form
    expect(screen.getByText("pm.task.title")).toBeTruthy();
  });

  it("the create form keeps a cached row's fields out of the subtask section", () => {
    // 新建任务 pressed while a row is still loaded: gating on the MODE is what
    // hides the sections, not an empty cache.
    mocks.mode = "create";
    render(<TaskEditDialog onClose={() => {}} />);
    expect(screen.queryByTestId("subtask-tree")).toBeNull();
    expect(screen.queryByText("pm.task.uploadAttachment")).toBeNull();
  });

  it("uploading hands the picked file to the store", async () => {
    render(<TaskEditDialog onClose={() => {}} />);
    const input = screen.getByLabelText("pm.task.uploadAttachment") as HTMLInputElement;
    const file = new File(["data"], "shot.png", { type: "image/png" });
    fireEvent.change(input, { target: { files: [file] } });
    await waitFor(() => expect(mocks.uploadAttachment).toHaveBeenCalledWith(file));
  });

  it("deleting an attachment asks first", () => {
    mocks.attachments = [att()];
    render(<TaskEditDialog onClose={() => {}} />);
    fireEvent.click(screen.getAllByLabelText("pm.task.deleteAttachment")[0]);
    expect(mocks.deleteAttachment).not.toHaveBeenCalled();
    fireEvent.click(screen.getByText("pm.task.deleteAttachmentTitle"));
    expect(mocks.deleteAttachment).not.toHaveBeenCalled();
    fireEvent.click(screen.getByText("common.delete"));
    expect(mocks.deleteAttachment).toHaveBeenCalledWith("att-1");
  });

  it("添加子任务 opens a create form that knows where it came from", () => {
    render(<TaskEditDialog onClose={() => {}} />);
    fireEvent.click(screen.getByText("+ pm.task.addSubtask"));
    expect(mocks.openCreate).toHaveBeenCalledWith({
      projectId: "p-1",
      parentId: "t-1",
      returnTo: "t-1",
    });
  });

  it("clicking a child row re-opens this box on that row", () => {
    render(<TaskEditDialog onClose={() => {}} />);
    fireEvent.click(screen.getByText("child-row"));
    expect(mocks.openTask).toHaveBeenCalledWith("t-child");
  });
});

describe("where saving and deleting land", () => {
  it("saving a new task closes the box — submit first, close after", async () => {
    mocks.mode = "create";
    mocks.detail = null;
    const onClose = vi.fn();
    render(<TaskEditDialog onClose={onClose} />);
    fireEvent.click(screen.getByText("pm.task.save"));
    // The request goes out first; the box closes only once the server says so.
    await waitFor(() => expect(mocks.save).toHaveBeenCalled());
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
  });

  it("a failed save keeps the box open (the draft is still there)", async () => {
    mocks.mode = "create";
    mocks.detail = null;
    mocks.save = vi.fn(async () => ({ ok: false, reason: "failed", message: "boom" }));
    const onClose = vi.fn();
    render(<TaskEditDialog onClose={onClose} />);
    fireEvent.click(screen.getByText("pm.task.save"));
    await waitFor(() => expect(mocks.save).toHaveBeenCalled());
    expect(onClose).not.toHaveBeenCalled();
  });

  it("a subtask save closes the box too — one rule for every shape", async () => {
    mocks.mode = "create";
    mocks.detail = null;
    mocks.returnTo = "t-parent";
    const onClose = vi.fn();
    render(<TaskEditDialog onClose={onClose} />);
    fireEvent.click(screen.getByText("pm.task.save"));
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
  });

  it("saving an edit closes the box as well", async () => {
    mocks.save = vi.fn(async () => ({ ok: true, taskId: "t-1" }));
    const onClose = vi.fn();
    render(<TaskEditDialog onClose={onClose} />);
    fireEvent.click(screen.getByText("pm.task.save"));
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
  });

  it("deleting a child returns to its parent instead of closing", async () => {
    mocks.detail = row({ parent_id: "t-root" });
    const onClose = vi.fn();
    render(<TaskEditDialog onClose={onClose} />);
    fireEvent.click(screen.getByText("pm.task.delete"));
    fireEvent.click(screen.getByText("common.delete"));
    await waitFor(() => expect(mocks.removeTask).toHaveBeenCalledWith("t-1"));
    expect(mocks.openTask).toHaveBeenCalledWith("t-root");
    expect(onClose).not.toHaveBeenCalled();
  });

  it("deleting a top-level task closes the box", async () => {
    const onClose = vi.fn();
    render(<TaskEditDialog onClose={onClose} />);
    fireEvent.click(screen.getByText("pm.task.delete"));
    fireEvent.click(screen.getByText("common.delete"));
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
  });

  it("the delete goes to the server, cascade included", async () => {
    render(<TaskEditDialog onClose={() => {}} />);
    fireEvent.click(screen.getByText("pm.task.delete"));
    fireEvent.click(screen.getByText("common.delete"));
    await waitFor(() => expect(mocks.deleteTask).toHaveBeenCalledWith("t-1", { cascade: true }));
  });

  it("a failed delete leaves the row on the board", async () => {
    mocks.deleteTask = vi.fn(async () => {
      throw new Error("409");
    });
    render(<TaskEditDialog onClose={() => {}} />);
    fireEvent.click(screen.getByText("pm.task.delete"));
    fireEvent.click(screen.getByText("common.delete"));
    await waitFor(() => expect(mocks.deleteTask).toHaveBeenCalled());
    // removeTask is the local mirror of a server delete — it must not run first.
    expect(mocks.removeTask).not.toHaveBeenCalled();
  });

  it("delete is the visibly dangerous button, like the memory panel's", () => {
    // A bare red text link next to two solid buttons reads as a third neutral
    // action. The memory panel's Delete Node (MemoryNodeDetail.tsx) is the
    // reference: bordered, tinted, iconed.
    render(<TaskEditDialog onClose={() => {}} />);
    const cls = screen.getByText("pm.task.delete").closest("button")?.className ?? "";
    expect(cls).toContain("border-red-200");
    expect(cls).toContain("bg-red-50");
    expect(cls).toContain("text-red-700");
    expect(cls).toContain("hover:bg-red-100");
  });

  it("delete needs confirming — one click does not remove a row", () => {
    render(<TaskEditDialog onClose={() => {}} />);
    fireEvent.click(screen.getByText("pm.task.delete"));
    expect(mocks.removeTask).not.toHaveBeenCalled();
    expect(screen.getByText("pm.task.deleteTaskTitle")).toBeTruthy();
  });
});

describe("leaving a dirty form", () => {
  it("cancel asks instead of eating the draft", () => {
    mocks.dirty = true;
    const onClose = vi.fn();
    render(<TaskEditDialog onClose={onClose} />);
    fireEvent.click(screen.getByText("common.cancel"));
    expect(onClose).not.toHaveBeenCalled();
    expect(screen.getByText("pm.task.discardChangesTitle")).toBeTruthy();
  });

  it("ESC goes through the guard rather than closing over the draft", () => {
    mocks.dirty = true;
    render(<TaskEditDialog onClose={() => {}} />);
    fireEvent.keyDown(window, { key: "Escape" });
    expect(screen.getByText("pm.task.discardChangesTitle")).toBeTruthy();
  });

  it("so does the backdrop", () => {
    mocks.dirty = true;
    const { container } = render(<TaskEditDialog onClose={() => {}} />);
    fireEvent.click(container.querySelector(".absolute.inset-0")!);
    expect(screen.getByText("pm.task.discardChangesTitle")).toBeTruthy();
  });

  it("a clean backdrop click closes straight away", () => {
    const onClose = vi.fn();
    const { container } = render(<TaskEditDialog onClose={onClose} />);
    fireEvent.click(container.querySelector(".absolute.inset-0")!);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("navigating to a child asks too, and discarding runs the navigation", () => {
    mocks.dirty = true;
    render(<TaskEditDialog onClose={() => {}} />);
    fireEvent.click(screen.getByText("child-row"));
    expect(mocks.openTask).not.toHaveBeenCalled();
    fireEvent.click(screen.getByText("pm.task.discardChangesConfirm"));
    expect(mocks.openTask).toHaveBeenCalledWith("t-child");
  });

  it("leaving a clean form clears the draft — it must not resurrect on re-entry", () => {
    render(<TaskEditDialog onClose={() => {}} />);
    fireEvent.click(screen.getByText("common.cancel"));
    expect(mocks.reset).toHaveBeenCalledTimes(1);
  });

  it("cancelling a subtask form returns to the parent it was opened from", () => {
    mocks.mode = "create";
    mocks.detail = null;
    mocks.returnTo = "t-root";
    const onClose = vi.fn();
    render(<TaskEditDialog onClose={onClose} />);
    fireEvent.click(screen.getByText("common.cancel"));
    expect(mocks.openTask).toHaveBeenCalledWith("t-root");
    expect(onClose).not.toHaveBeenCalled();
  });

  it("a top-level form with nothing to return to closes", () => {
    mocks.mode = "create";
    mocks.detail = null;
    const onClose = vi.fn();
    render(<TaskEditDialog onClose={onClose} />);
    fireEvent.click(screen.getByText("common.cancel"));
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});

describe("audit line", () => {
  it("prints the creator's name, not the uuid", () => {
    render(<TaskEditDialog onClose={() => {}} />);
    expect(screen.getByText(/张三/)).toBeTruthy();
    expect(screen.queryByText("u-1")).toBeNull();
  });

  it("carries a status-transition control — the board drag is not the only way", () => {
    render(<TaskEditDialog onClose={() => {}} />);
    // The footer door to `boardStore.moveTask`: not every view lets the user
    // drag a card, and dragging is a precise act.
    expect(screen.getByLabelText("pm.task.moveTo")).toBeTruthy();
  });

  it("has no status control on a task that does not exist yet", () => {
    mocks.mode = "create";
    mocks.detail = null;
    render(<TaskEditDialog onClose={() => {}} />);
    expect(screen.queryByLabelText("pm.task.moveTo")).toBeNull();
  });

  it("shows a failed dispatch while the row is still undispatched", () => {
    mocks.detail = row({ status: "pending", dispatch_error: "no runtime", dispatch_attempts: 2 });
    render(<TaskEditDialog onClose={() => {}} />);
    expect(screen.getByText(/pm.task.dispatchFailed/)).toBeTruthy();
  });

  it("says 'abandoned' once attempts reach the bound", () => {
    mocks.detail = row({ status: "pending", dispatch_error: "no runtime", dispatch_attempts: 5 });
    render(<TaskEditDialog onClose={() => {}} />);
    expect(screen.getByText(/pm.task.dispatchAbandoned/)).toBeTruthy();
  });

  it("stays silent after a successful dispatch", () => {
    mocks.detail = row({ status: "pending", dispatch_error: "no runtime", dispatched_at: "2026-10-03T02:00:00Z" });
    render(<TaskEditDialog onClose={() => {}} />);
    expect(screen.queryByText(/pm.task.dispatchFailed/)).toBeNull();
  });

  it("does not appear on a new task", () => {
    mocks.mode = "create";
    mocks.detail = null;
    render(<TaskEditDialog onClose={() => {}} />);
    expect(screen.queryByText("pm.task.createdBy")).toBeNull();
  });
});
