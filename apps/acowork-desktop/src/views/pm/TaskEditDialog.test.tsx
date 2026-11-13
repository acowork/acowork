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
import type { PmAttachmentMeta, PmTaskActivity, PmTaskResponse } from "../../lib/pm-types";

const mocks = vi.hoisted(() => ({
  mode: "edit" as "edit" | "create" | null,
  detail: null as PmTaskResponse | null,
  attachments: [] as PmAttachmentMeta[],
  activity: [] as PmTaskActivity[],
  uploading: false,
  replying: false,
  error: null as string | null,
  projectId: null as string | null,
  createParentId: null as string | null,
  returnTo: null as string | null,
  tasks: [] as PmTaskResponse[],
  /** A second (stacked) frame — the child box on top of the parent. */
  child: null as null | { mode: "edit" | "create"; detail: PmTaskResponse | null },
  dirty: false,
  saving: false,
  hasMembers: true,
  values: {} as Record<string, unknown>,
  openTask: vi.fn(async () => {}),
  openCreate: vi.fn(),
  pushTask: vi.fn(),
  pushCreate: vi.fn(),
  pop: vi.fn(),
  setMode: vi.fn(),
  refresh: vi.fn(async () => {}),
  uploadAttachment: vi.fn(async () => ({ id: "att-1" }) as unknown as PmAttachmentMeta),
  deleteAttachment: vi.fn(async () => true),
  addReply: vi.fn(async () => true),
  removeTask: vi.fn(),
  moveTask: vi.fn(async () => true),
  reviewTask: vi.fn(async () => true),
  deleteTask: vi.fn(async () => ({})),
  save: vi.fn(async () => ({ ok: true, taskId: "t-new" })),
  reset: vi.fn(),
  setStartAt: vi.fn(),
  setDueAt: vi.fn(),
}));

/**
 * The store is a STACK: `frames[]` holds the open boxes (bottom → top) and
 * `rows[key]` holds each box's row data. The mock mirrors that shape — one
 * frame by default, two when `mocks.child` is set (a subtask box on top of
 * its parent).
 */
function detailState() {
  const parent = {
    key: "tf1",
    mode: mocks.mode,
    taskId: mocks.detail?.id ?? null,
    projectId: mocks.projectId ?? mocks.detail?.project_id ?? null,
    createParentId: mocks.createParentId,
  };
  const child = mocks.child
    ? {
        key: "tf2",
        mode: mocks.child.mode,
        taskId: mocks.child.detail?.id ?? null,
        projectId: mocks.projectId ?? mocks.child.detail?.project_id ?? null,
        createParentId: null,
      }
    : null;
  return {
    frames: child ? [parent, child] : [parent],
    rows: {
      tf1: {
        detail: mocks.detail,
        attachments: mocks.attachments,
        activity: mocks.activity,
        uploading: mocks.uploading,
        replying: mocks.replying,
        loading: false,
        error: mocks.error,
      },
      ...(child
        ? {
            tf2: {
              detail: child.detail,
              attachments: [],
              activity: [],
              uploading: false,
              replying: false,
              loading: false,
              error: null,
            },
          }
        : {}),
    },
    addReply: mocks.addReply,
    refresh: mocks.refresh,
    uploadAttachment: mocks.uploadAttachment,
    deleteAttachment: mocks.deleteAttachment,
    openTask: mocks.openTask,
    openCreate: mocks.openCreate,
    pushTask: mocks.pushTask,
    pushCreate: mocks.pushCreate,
    pop: mocks.pop,
    setMode: mocks.setMode,
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

// 提交历史区有自己的测试（TaskActivity.test.tsx）；在对话框测试里它是一个
// 带标记的接缝 —— 这里要证明的是"有落盘的行才渲染它"，不是它内部长什么样。
vi.mock("./TaskActivity", () => ({
  TaskActivitySection: ({ activity }: { activity: PmTaskActivity[] }) => (
    <div data-testid="activity-section" data-count={activity.length} />
  ),
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
    setDueAt: mocks.setDueAt,
    setStartAt: mocks.setStartAt,
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
  localDateTimeToIso: (v: string) => v,
  isoToLocalDateTime: (v?: string | null) => v ?? "",
}));

vi.mock("../../i18n/useTranslation", () => ({
  // The hook's real shape: `DateTimePicker` reads the language for its weekday
  // initials, so a `{ t }`-only mock is a lie that only shows up at runtime.
  useTranslation: () => ({ t: (key: string) => key, i18n: { language: "en" } }),
}));

import { TaskEditDialog } from "./TaskEditDialog";
import type { TaskEditTab } from "./taskEditTabs";

/** Click a tab and prove it opened — every tab's content is one click in. */
function openTab(tab: TaskEditTab) {
  const button = screen.getByTestId(`task-tab-${tab}`);
  fireEvent.click(button);
  expect(button.getAttribute("aria-selected")).toBe("true");
}

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
  mocks.activity = [];
  mocks.uploading = false;
  mocks.replying = false;
  mocks.error = null;
  mocks.projectId = null;
  mocks.createParentId = null;
  mocks.returnTo = null;
  mocks.child = null;
  mocks.tasks = [];
  mocks.dirty = false;
  mocks.saving = false;
  mocks.hasMembers = true;
  mocks.values = {};
  mocks.openTask.mockClear();
  mocks.openCreate.mockClear();
  mocks.pushTask.mockClear();
  mocks.pushCreate.mockClear();
  mocks.pop.mockClear();
  mocks.setMode.mockClear();
  mocks.uploadAttachment.mockClear();
  mocks.deleteAttachment.mockClear();
  mocks.removeTask.mockClear();
  mocks.deleteTask.mockClear();
  // Re-seed the save seam: a test that swaps it for a failing one must not
  // leak that failure into the next test.
  mocks.save = vi.fn(async () => ({ ok: true, taskId: "t-new" }));
  mocks.reset.mockClear();
  mocks.setStartAt.mockClear();
  mocks.setDueAt.mockClear();
});

// 删除按钮现在有可见文字（安静的行内红字），所以按无障碍名取，而不是 aria-label。
const deleteButton = () => screen.getByRole("button", { name: "pm.task.delete" });
const queryDeleteButton = () => screen.queryByRole("button", { name: "pm.task.delete" });

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

  // 信息量太大堆一屏是这次要修的问题（缺陷 t-f23fd817）。标签栏是 GlobalSearchDialog
  // 那套外壳：标题行下面一条固定高的分隔行，正文滚动。
  it("carries a tab strip under the title", () => {
    const { container } = render(<TaskEditDialog onClose={() => {}} />);
    const strip = screen.getByRole("tablist");
    const header = container.querySelector('[role="dialog"] > header');
    // Directly under the header, and never part of the scrolling body.
    expect(header?.nextElementSibling).toBe(strip);
    expect(strip.className).toContain("border-b");
    expect(strip.className).toContain("min-h-[var(--ui-dialog-zone-h)]");
    // A separate zone below it carries the open tab; the strip never scrolls
    // with the body.
    const body = strip.nextElementSibling as HTMLElement | null;
    expect(body).not.toBeNull();
    expect(body!.contains(strip)).toBe(false);
    expect(body!.textContent).toContain("pm.task.title");
  });

  it("opens on 基本信息 and only 基本信息", () => {
    render(<TaskEditDialog onClose={() => {}} />);
    expect(screen.getByTestId("task-tab-basic").getAttribute("aria-selected")).toBe("true");
    expect(screen.getByText("pm.task.title")).toBeTruthy();
    // The other tabs' bodies are not merely hidden — they are not rendered.
    expect(screen.queryByTestId("activity-section")).toBeNull();
    expect(screen.queryByTestId("subtask-tree")).toBeNull();
  });

  it("a persisted row reaches all four tabs, in order", () => {
    render(<TaskEditDialog onClose={() => {}} />);
    const labels = screen.getAllByRole("tab").map((el) => el.textContent);
    expect(labels).toEqual([
      "pm.task.tabs.basic",
      "pm.task.tabs.related",
      "pm.task.tabs.files",
      "pm.task.tabs.submissions",
    ]);
  });

  it("a new task only gets the two tabs it can fill", () => {
    // 文件上传 posts to /tasks/{tid}/attachments and 提交记录 reads
    // /tasks/{tid}/activity — no id, no tab.
    mocks.mode = "create";
    mocks.detail = null;
    render(<TaskEditDialog onClose={() => {}} />);
    const labels = screen.getAllByRole("tab").map((el) => el.textContent);
    expect(labels).toEqual(["pm.task.tabs.basic", "pm.task.tabs.related"]);
  });

  // 一行要么排满，要么它就是半行空档。类型 / 优先级 / 状态 一行，指派给 + 两个
  // 日期 一行 —— 3 + 3，两行都排到最后一列。
  it("type / priority / status fill one row", () => {
    render(<TaskEditDialog onClose={() => {}} />);
    const cells = Array.from(screen.getByTestId("row-what").children);
    const labels = cells.map((c) => (c.querySelector("span")?.textContent ?? "").trim());
    expect(labels).toEqual(["pm.task.type", "pm.task.priority", "pm.task.status"]);
  });

  it("assignee and the two dates fill one row to its last column", () => {
    render(<TaskEditDialog onClose={() => {}} />);
    const cells = Array.from(screen.getByTestId("row-when").children);
    const labels = cells.map((c) => (c.querySelector("span")?.textContent ?? "").trim());
    expect(labels).toEqual(["pm.task.assignee", "pm.task.startDate", "pm.task.dueDate"]);
  });

  // 原生 `<input type="date">` 的弹层是 WebView2 自己画的（不在页面 DOM 里），
  // 箭头尺寸和底部按钮的底色都改不动 —— 那正是"看着业余"的根因。所以日期字段
  // 必须是组件库的 DateTimePicker，且它写回表单的值仍是本地墙钟
  // `YYYY-MM-DDTHH:mm`。
  it("the date fields are the design-system picker, not a native date input", () => {
    mocks.values = { startAt: "2026-11-06T08:30" };
    const { container } = render(<TaskEditDialog onClose={() => {}} />);
    expect(container.querySelector('input[type="date"]')).toBeNull();
    const triggers = screen.getAllByTestId("date-picker-trigger");
    expect(triggers).toHaveLength(2);
    expect(triggers[0].textContent).toContain("2026-11-06");
    expect(triggers[0].textContent).toContain("08:30");
    // 空字段读占位符，不再是一个空白原生控件
    expect(triggers[1].textContent).toContain("datePicker.placeholder");
  });

  it("picking a day writes the local wall clock back into the form", () => {
    mocks.values = { startAt: "2026-11-06T08:30" };
    render(<TaskEditDialog onClose={() => {}} />);
    fireEvent.click(screen.getAllByTestId("date-picker-trigger")[0]);
    fireEvent.click(screen.getByRole("button", { name: "2026-11-20" }));
    expect(mocks.setStartAt).toHaveBeenCalledWith("2026-11-20T08:30");
    expect(mocks.setDueAt).not.toHaveBeenCalled();
  });

  it("dismissing the calendar with Escape leaves the task form open", () => {
    // 对话框自己在 window 上监听 Escape 来关闭整张表单；日历必须吞掉这个键 ——
    // 关掉日历不等于丢弃草稿。
    mocks.values = { startAt: "2026-11-06T08:30" };
    const onClose = vi.fn();
    render(<TaskEditDialog onClose={onClose} />);
    fireEvent.click(screen.getAllByTestId("date-picker-trigger")[0]);
    fireEvent.keyDown(screen.getByRole("button", { name: "2026-11-06" }), { key: "Escape" });
    expect(screen.queryByTestId("date-picker-popup")).toBeNull();
    expect(onClose).not.toHaveBeenCalled();
  });

  it("the title spans the full width", () => {
    const { container } = render(<TaskEditDialog onClose={() => {}} />);
    const cell = Array.from(container.querySelectorAll("label")).find((el) =>
      (el.querySelector("span")?.textContent ?? "").trim().startsWith("pm.task.title"),
    );
    // Its wrapper is the plain column, not one of the multi-column grids.
    const column = cell?.parentElement?.className ?? "";
    expect(column).toContain("flex-col");
    expect(column).not.toContain("grid-cols");
  });
});

describe("the box is three zones — identity, fields, commit", () => {
  // 标题栏 = 对话框的身份（标题 + 关闭）；正文 = 这一行的字段（状态也是"这条
  // 任务现在是什么"，所以它跟类型 / 优先级 同一行）；底栏 = 表单提交，删除停在
  // 最左角，离 保存 最远。
  it("the footer parks 删除 at the far left and the form's commit at the right", () => {
    render(<TaskEditDialog onClose={() => {}} />);
    const footer = screen.getByRole("dialog").querySelector("footer");
    expect(footer).toBeTruthy();
    const labels = Array.from(footer!.querySelectorAll("button")).map((b) =>
      (b.textContent ?? "").trim(),
    );
    expect(labels).toEqual([
      "pm.task.delete",
      "pm.review.openExec",
      "common.cancel",
      "pm.task.save",
    ]);
    // 一块 flex-1 撑开中间：危险动作 / 查看执行 与提交路径不相邻
    expect(footer!.querySelector(".flex-1")).toBeTruthy();
  });

  it("view-run is absent when the row never ran", () => {
    // pending = 还没开工，背后没有执行 session；摆一颗点了只会报"未找到"的
    // 按钮就是把失败做成入口。判断与看板卡片共用 `canViewExecSession`。
    mocks.detail = row({ status: "pending" });
    render(<TaskEditDialog onClose={() => {}} />);
    expect(screen.queryByRole("button", { name: /pm\.review\.openExec/ })).toBeNull();
  });

  it("the header carries only the identity — title and close", () => {
    render(<TaskEditDialog onClose={() => {}} />);
    const header = screen.getByRole("dialog").querySelector("header") as HTMLElement;
    const labels = Array.from(header.querySelectorAll("button")).map(
      (b) => b.getAttribute("aria-label") ?? (b.textContent ?? "").trim(),
    );
    expect(labels).toEqual(["common.close"]);
    // 标题栏不再挂任何行级动作
    expect(header.contains(screen.getByLabelText("pm.task.status"))).toBe(false);
    expect(header.contains(deleteButton())).toBe(false);
  });

  it("状态 is a field of 基本信息, not chrome on every tab", () => {
    render(<TaskEditDialog onClose={() => {}} />);
    const status = screen.getByLabelText("pm.task.status");
    // 它坐在 类型 / 优先级 那一行的第三格
    expect(screen.getByTestId("row-what").contains(status)).toBe(true);
    expect(screen.getByTestId("task-audit").contains(status)).toBe(false);
    // 换到别的标签页就没有（那些页讲的是内容，不是这一行的属性）
    openTab("files");
    expect(screen.queryByLabelText("pm.task.moveTo")).toBeNull();
  });

  it("a new task has neither — there is no row to move or delete", () => {
    mocks.mode = "create";
    mocks.detail = null;
    render(<TaskEditDialog onClose={() => {}} />);
    expect(screen.queryByLabelText("pm.task.moveTo")).toBeNull();
    expect(queryDeleteButton()).toBeNull();
    // 少一格就少一列：两格仍要排满整行，不留半行空档
    const cells = Array.from(screen.getByTestId("row-what").children);
    expect(cells.map((c) => (c.querySelector("span")?.textContent ?? "").trim())).toEqual([
      "pm.task.type",
      "pm.task.priority",
    ]);
  });

  it("the audit line prints only the facts you cannot edit", () => {
    // 类型 / 优先级 的正下方就是它们的可编辑下拉框，状态的可编辑控件在头部。
    // 再只读印一遍，屏幕上就有了两个"当前值"。
    render(<TaskEditDialog onClose={() => {}} />);
    const audit = screen.getByTestId("task-audit");
    expect(audit.textContent).toContain("pm.task.createdBy");
    expect(audit.textContent).toContain("pm.task.updated");
    expect(audit.textContent).not.toContain("pm.board.");
  });

  it("the date fields sit in a full row instead of half a row each", () => {
    render(<TaskEditDialog onClose={() => {}} />);
    const [start, due] = screen.getAllByTestId("date-picker-trigger");
    // 每个日期只坐一行的 1/3：靠列结构变窄，不靠 `w-40` 把半栏填成半栏空。
    expect(start.parentElement?.parentElement?.className).toContain("grid-cols-3");
    expect(due.parentElement?.parentElement).toBe(start.parentElement?.parentElement);
    for (const trigger of [start, due]) {
      const tokens = trigger.className.split(/\s+/);
      expect(tokens).toContain("w-full");
      expect(tokens).not.toContain("w-40");
    }
  });

  it("描述 absorbs the height the fixed box has left over", () => {
    // 标题行 → 三下拉 → 两日期 之后，固定 560px 的盒子里还剩 ≈30px（编辑）/
    // ≈75px（创建）的死空间。jsdom 没有排版，所以这条契约就是类名本身。
    render(<TaskEditDialog onClose={() => {}} />);
    const textarea = document.querySelector("textarea") as HTMLTextAreaElement;
    expect(textarea).toBeTruthy();
    expect(textarea.className).toContain("flex-1");
    expect(textarea.className).toContain("min-h-[7.5rem]");
    const field = textarea.closest("label") as HTMLElement;
    expect(field.className).toContain("flex-1");
    expect(field.className).toContain("flex-col");
  });
});

describe("the three carried-over capabilities", () => {
  // 提交历史（t-80cf07ec）：Agent 交完活，任务面上必须读得到他提交了什么、
  // 人类回了什么。这一条只证明"对话框把 store 里的历史挂上了这个面"，
  // 行内容本身在 TaskActivity.test.tsx。
  it("a persisted row shows the submission timeline with the stored history", () => {
    mocks.activity = [
      { id: "act-1", kind: "submitted", actor: "a", actor_kind: "agent", text: "done", attachment_ids: [], created_at: "2026-10-30T08:40:00Z" },
      { id: "act-2", kind: "comment", actor: "human", actor_kind: "user", text: "ok", attachment_ids: [], created_at: "2026-10-30T08:41:00Z" },
    ] as PmTaskActivity[];
    render(<TaskEditDialog onClose={() => {}} />);
    openTab("submissions");
    const section = screen.getByTestId("activity-section");
    expect(section.getAttribute("data-count")).toBe("2");
  });

  it("a persisted row offers attachments, subtasks and delete", () => {
    render(<TaskEditDialog onClose={() => {}} />);
    // Each tab owns one kind of content — 文件上传 does not carry the timeline.
    openTab("files");
    expect(screen.queryByTestId("activity-section")).toBeNull();
    expect(screen.getByText("pm.task.uploadAttachment")).toBeTruthy();
    openTab("related");
    expect(screen.getByTestId("subtask-tree")).toBeTruthy();
    expect(screen.getByText("+ pm.task.addSubtask")).toBeTruthy();
    // Delete is footer chrome again: outside the tab body, so it stays
    // reachable on every tab.
    expect(deleteButton()).toBeTruthy();
  });

  it("create mode offers none of them — there is no task id to attach to", () => {
    mocks.mode = "create";
    mocks.detail = null;
    render(<TaskEditDialog onClose={() => {}} />);
    expect(screen.queryByText("pm.task.uploadAttachment")).toBeNull();
    expect(screen.queryByTestId("activity-section")).toBeNull();
    expect(screen.queryByTestId("subtask-tree")).toBeNull();
    expect(queryDeleteButton()).toBeNull();
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

  it("a new task speaks about no row — not even a cached one", () => {
    // Same trap on the tabs: the store keeps the last detail, so "detail exists"
    // is not "this form is about that row". A create form must not print the
    // previous row's audit line or its children.
    mocks.mode = "create";
    render(<TaskEditDialog onClose={() => {}} />);
    expect(screen.queryByText("pm.task.createdBy")).toBeNull();
    openTab("related");
    expect(screen.queryByTestId("subtask-tree")).toBeNull();
    expect(screen.getByText("pm.task.subtasksNeedRow")).toBeTruthy();
    // The parent / dependency pickers are create-time fields, so they stay.
    expect(screen.getByText("pm.task.parentTask")).toBeTruthy();
  });

  it("uploading hands the picked file to the store", async () => {
    render(<TaskEditDialog onClose={() => {}} />);
    openTab("files");
    const input = screen.getByLabelText("pm.task.uploadAttachment") as HTMLInputElement;
    const file = new File(["data"], "shot.png", { type: "image/png" });
    fireEvent.change(input, { target: { files: [file] } });
    await waitFor(() => expect(mocks.uploadAttachment).toHaveBeenCalledWith(file));
  });

  it("deleting an attachment asks first", () => {
    mocks.attachments = [att()];
    render(<TaskEditDialog onClose={() => {}} />);
    openTab("files");
    fireEvent.click(screen.getAllByLabelText("pm.task.deleteAttachment")[0]);
    expect(mocks.deleteAttachment).not.toHaveBeenCalled();
    fireEvent.click(screen.getByText("pm.task.deleteAttachmentTitle"));
    expect(mocks.deleteAttachment).not.toHaveBeenCalled();
    fireEvent.click(screen.getByText("common.delete"));
    expect(mocks.deleteAttachment).toHaveBeenCalledWith("att-1");
  });

  // 关联任务的两个候选集都是一棵树里的一块，不是一袋条目：换成树下钻之后，
  // 这一页不该再出现"把整个项目拍平"的控件（下拉 / 一次性挂满的复选列表）。
  it("offers a tree to drill into, not a flat dropdown or checkbox wall", () => {
    render(<TaskEditDialog onClose={() => {}} />);
    openTab("related");
    expect(screen.queryByRole("combobox")).toBeNull();
    expect(screen.getByTestId("parent-current")).toBeTruthy();
    expect(screen.getByText("pm.task.change")).toBeTruthy();
    expect(screen.getByText("+ pm.task.addDependency")).toBeTruthy();
    // 没选依赖时没有 chips —— 引用区只在有引用时存在
    expect(screen.queryByTestId("dependency-chips")).toBeNull();
    // 点开才长出树
    expect(screen.queryByTestId("task-tree")).toBeNull();
    fireEvent.click(screen.getByText("pm.task.change"));
    expect(screen.getByTestId("task-tree")).toBeTruthy();
  });

  it("添加子任务 opens a create form ON TOP of this one", () => {
    render(<TaskEditDialog onClose={() => {}} />);
    openTab("related");
    fireEvent.click(screen.getByText("+ pm.task.addSubtask"));
    expect(mocks.pushCreate).toHaveBeenCalledWith({
      projectId: "p-1",
      parentId: "t-1",
    });
  });

  it("clicking a child row opens ANOTHER box on top of this one", () => {
    render(<TaskEditDialog onClose={() => {}} />);
    openTab("related");
    fireEvent.click(screen.getByText("child-row"));
    expect(mocks.pushTask).toHaveBeenCalledWith("t-child");
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
    fireEvent.click(deleteButton());
    fireEvent.click(screen.getByText("common.delete"));
    await waitFor(() => expect(mocks.removeTask).toHaveBeenCalledWith("t-1"));
    expect(mocks.openTask).toHaveBeenCalledWith("t-root");
    expect(onClose).not.toHaveBeenCalled();
  });

  it("deleting a top-level task closes the box", async () => {
    const onClose = vi.fn();
    render(<TaskEditDialog onClose={onClose} />);
    fireEvent.click(deleteButton());
    fireEvent.click(screen.getByText("common.delete"));
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
  });

  it("the delete goes to the server, cascade included", async () => {
    render(<TaskEditDialog onClose={() => {}} />);
    fireEvent.click(deleteButton());
    fireEvent.click(screen.getByText("common.delete"));
    await waitFor(() => expect(mocks.deleteTask).toHaveBeenCalledWith("t-1", { cascade: true }));
  });

  it("a failed delete leaves the row on the board", async () => {
    mocks.deleteTask = vi.fn(async () => {
      throw new Error("409");
    });
    render(<TaskEditDialog onClose={() => {}} />);
    fireEvent.click(deleteButton());
    fireEvent.click(screen.getByText("common.delete"));
    await waitFor(() => expect(mocks.deleteTask).toHaveBeenCalled());
    // removeTask is the local mirror of a server delete — it must not run first.
    expect(mocks.removeTask).not.toHaveBeenCalled();
  });

  it("delete is a solid red button with white text, not an outline", () => {
    // 它紧挨着 取消 / 保存，所以必须靠"重量"而不是颜色微差来区分：纯色红底 +
    // 白字（与任务卡审核条的 拒绝 同一档）。描边+浅底会被读成第三个同等重量
    // 的中性动作，那是最糟的误读。
    render(<TaskEditDialog onClose={() => {}} />);
    const button = deleteButton();
    expect(button.tagName).toBe("BUTTON");
    expect(button.textContent).toContain("pm.task.delete");
    const tokens = button.className.split(/\s+/);
    expect(tokens).toContain("bg-red-600");
    expect(tokens).toContain("text-white");
    expect(tokens).not.toContain("border-red-200");
    expect(tokens).not.toContain("bg-red-50");
    // 位置：底栏最左，且与 保存 之间隔着那块 flex-1
    const footer = screen.getByRole("dialog").querySelector("footer") as HTMLElement;
    expect(footer.contains(button)).toBe(true);
    expect(footer.firstElementChild).toBe(button);
  });

  it("delete needs confirming — one click does not remove a row", () => {
    render(<TaskEditDialog onClose={() => {}} />);
    fireEvent.click(deleteButton());
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

  // 用户报的 bug：点 取消 → 弹出放弃确认 → 点「放弃修改」没反应，只有点
  // 「取消」才关得掉。根因是确认回调又走了一次带 dirty 守卫的导航，守卫把
  // 刚关掉的确认框重新装回去了。
  it("discarding actually leaves — the confirm must not re-arm itself", () => {
    mocks.dirty = true;
    const onClose = vi.fn();
    render(<TaskEditDialog onClose={onClose} />);
    fireEvent.click(screen.getByText("common.cancel"));
    fireEvent.click(screen.getByText("pm.task.discardChangesConfirm"));

    expect(mocks.reset).toHaveBeenCalledTimes(1);
    expect(onClose).toHaveBeenCalledTimes(1);
    // …and the confirm is gone, not re-opened over the (now discarded) form.
    expect(screen.queryByText("pm.task.discardChangesTitle")).toBeNull();
  });

  it("discarding from ESC or the backdrop leaves too", () => {
    mocks.dirty = true;
    const onClose = vi.fn();
    const { container } = render(<TaskEditDialog onClose={onClose} />);
    fireEvent.click(container.querySelector(".absolute.inset-0")!);
    fireEvent.click(screen.getByText("pm.task.discardChangesConfirm"));
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(screen.queryByText("pm.task.discardChangesTitle")).toBeNull();
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
    openTab("related");
    fireEvent.click(screen.getByText("child-row"));
    expect(mocks.pushTask).not.toHaveBeenCalled();
    fireEvent.click(screen.getByText("pm.task.discardChangesConfirm"));
    expect(mocks.pushTask).toHaveBeenCalledWith("t-child");
  });

  it("leaving a clean form clears the draft — it must not resurrect on re-entry", () => {
    render(<TaskEditDialog onClose={() => {}} />);
    fireEvent.click(screen.getByText("common.cancel"));
    expect(mocks.reset).toHaveBeenCalledTimes(1);
  });

  it("cancelling a box that sits ON TOP pops it — the parent stays open", () => {
    // A subtask box is a second layer of the stack: cancelling it must land
    // the reader back on the parent, not close the whole thing.
    mocks.child = { mode: "create", detail: null };
    const onClose = vi.fn();
    render(<TaskEditDialog onClose={onClose} />);
    // Two boxes are on screen; the child's cancel is the second one.
    expect(screen.getAllByRole("dialog")).toHaveLength(2);
    fireEvent.click(screen.getAllByText("common.cancel")[1]);
    expect(mocks.pop).toHaveBeenCalled();
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
    expect(screen.getByLabelText("pm.task.status")).toBeTruthy();
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
