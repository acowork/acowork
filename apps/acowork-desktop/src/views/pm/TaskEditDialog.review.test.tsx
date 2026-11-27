/**
 * 审核条带 —— 弹框里的裁决入口。
 *
 * 看板卡片一直有「✓ 批准 / ✗ 拒绝」，弹框里却只有一个藏在「状态」下拉里的伪
 * 选项（而且只对 pending + review_status=pending 出现）。读材料的地方（尤其
 * 「提交记录」页）必须能就地裁决，否则审批要退回列表去点卡片。
 *
 * 这里钉三件事：
 *  1. 待审核的行（submitted，或 pending + review_status=pending）在**任意**标签
 *     下都有这两颗按钮；批准/拒绝调的是 `boardStore.reviewTask`，与卡片同一入口。
 *  2. 拒绝走 RejectDialog，填的理由作为 `_comment` 一起传下去。
 *  3. 不在审核态（in_progress / 已批准过的 pending / 新建表单）就没有条带。
 */
import { describe, expect, it, vi, beforeEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import type { PmTaskResponse } from "../../lib/pm-types";

const mocks = vi.hoisted(() => ({
  detail: null as PmTaskResponse | null,
  mode: "edit" as "edit" | "create" | null,
  refresh: vi.fn(async () => {}),
  reviewTask: vi.fn(async () => true),
}));

vi.mock("../../stores/pm/taskDetailStore", () => ({
  usePmTaskDetailStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({
      frames: [
        { key: "tf1", mode: mocks.mode, taskId: mocks.detail?.id ?? null, projectId: "p-1", createParentId: null },
      ],
      rows: {
        tf1: {
          detail: mocks.detail,
          attachments: [],
          activity: [],
          uploading: false,
          replying: false,
          loading: false,
          error: null,
        },
      },
      refresh: mocks.refresh,
      addReply: vi.fn(),
      uploadAttachment: vi.fn(),
      deleteAttachment: vi.fn(),
      openTask: vi.fn(),
      openCreate: vi.fn(),
      pushTask: vi.fn(),
      pushCreate: vi.fn(),
      pop: vi.fn(),
      setMode: vi.fn(),
      refreshBelowTop: vi.fn(),
      clear: vi.fn(),
    }),
}));

vi.mock("../../stores/pm/boardStore", () => ({
  usePmBoardStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({ tasks: [], removeTask: vi.fn(), moveTask: vi.fn(), reviewTask: mocks.reviewTask }),
}));

vi.mock("./SubtaskTree", () => ({ SubtaskTree: () => <div data-testid="subtask-tree" /> }));
vi.mock("./TaskActivity", () => ({ TaskActivitySection: () => <div data-testid="activity-section" /> }));
vi.mock("../../lib/pm-api", () => ({
  attachmentUrl: (id: string) => `/api/pm/attachments/${id}`,
  deleteTask: vi.fn(),
  retryTaskDispatch: vi.fn(),
}));
vi.mock("../../components/common/ToastProvider", () => ({ showToast: vi.fn() }));
vi.mock("./pmMembers", () => ({
  useActorLabel: (id: string | null) => (id === "u-1" ? "张三" : null),
}));
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
    hasMembers: true,
    saving: false,
    dirty: false,
    reset: vi.fn(),
    save: vi.fn(async () => ({ ok: true, taskId: "t-1" })),
  }),
  localDateTimeToIso: (v: string) => v,
  isoToLocalDateTime: (v?: string | null) => v ?? "",
}));
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key, i18n: { language: "en" } }),
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
    status: "submitted",
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
    review_status: "pending",
    created_at: "2026-10-01T02:00:00Z",
    updated_at: "2026-10-02T02:00:00Z",
    ...over,
  } as PmTaskResponse;
}

const approveButton = () => screen.getByRole("button", { name: /pm\.review\.approve/ });
const rejectButton = () => screen.getByRole("button", { name: /pm\.review\.reject/ });

beforeEach(() => {
  mocks.mode = "edit";
  mocks.detail = row();
  mocks.refresh.mockClear();
  mocks.reviewTask.mockClear();
  mocks.reviewTask = vi.fn(async () => true);
});

describe("审核条带", () => {
  it("submitted 行：批准 → reviewTask(id, true)，然后重读该行", async () => {
    render(<TaskEditDialog onClose={() => {}} />);
    fireEvent.click(approveButton());
    await waitFor(() => expect(mocks.reviewTask).toHaveBeenCalledWith("t-1", true, undefined));
    await waitFor(() => expect(mocks.refresh).toHaveBeenCalled());
  });

  it("拒绝走 RejectDialog，理由作为 _comment 传下去", async () => {
    render(<TaskEditDialog onClose={() => {}} />);
    fireEvent.click(rejectButton());
    fireEvent.change(screen.getByPlaceholderText("pm.reject.reasonPlaceholder"), {
      target: { value: "验收不过" },
    });
    fireEvent.click(screen.getByRole("button", { name: "pm.reject.confirm" }));
    await waitFor(() =>
      expect(mocks.reviewTask).toHaveBeenCalledWith("t-1", false, "验收不过"),
    );
  });

  it("pending + review_status=pending 也算待审（Agent 新建等批准开工）", () => {
    mocks.detail = row({ status: "pending", review_status: "pending" });
    render(<TaskEditDialog onClose={() => {}} />);
    expect(approveButton()).toBeTruthy();
  });

  it("裁决入口只在「基本信息」—— 元信息条的右端，读完这一页的字段再决定", () => {
    render(<TaskEditDialog onClose={() => {}} />);
    // 与创建人 / 时间同一行：它是这条审计信息行的动作，不是单独的条带。
    const audit = screen.getByTestId("task-audit");
    expect(audit.contains(approveButton())).toBe(true);
    expect(audit.contains(rejectButton())).toBe(true);
    // 别的标签页不重复摆同一颗按钮（一个动作一个位置）。
    fireEvent.click(screen.getByTestId("task-tab-submissions"));
    expect(screen.queryByRole("button", { name: /pm\.review\.approve/ })).toBeNull();
  });

  it("不需要审核的行没有这两颗按钮", () => {
    mocks.detail = row({ status: "in_progress", review_status: "not_required" });
    render(<TaskEditDialog onClose={() => {}} />);
    expect(screen.queryByRole("button", { name: /pm\.review\.approve/ })).toBeNull();
  });

  it("新建表单没有审核条带", () => {
    mocks.mode = "create";
    mocks.detail = null;
    render(<TaskEditDialog onClose={() => {}} />);
    expect(screen.queryByRole("button", { name: /pm\.review\.approve/ })).toBeNull();
  });
});