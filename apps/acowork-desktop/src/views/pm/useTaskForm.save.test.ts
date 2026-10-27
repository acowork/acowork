/**
 * Save-path contract (the P0 fix).
 *
 * The old dialog believed `CreateTask` had no `assignee` / `due_at`, so it
 * created the row and then issued a second `updateTask` for those three fields.
 * The server has always accepted them (`CreateTask` in
 * `core/acowork-pm/src/types.rs`), so the extra call bought a round-trip and a
 * failure window: when it failed, the toast said "save failed" while an
 * unassigned row stayed on the board.
 *
 * These pin "one call on create" and, to keep the guard honest, "the one second
 * call that IS required" — a parent change, which the server only accepts
 * through the explicit reparent endpoint.
 *
 * Separate file because `./pmMembers` is mocked here (the save path needs a
 * stable member list, not the real resolver) and `useTaskForm.test.ts` pins the
 * real `memberOptions` mapping against ADR-073. Vitest mocks are file-scoped.
 */
import { act, renderHook, waitFor } from "@testing-library/react";
import { localDateFieldToIso, useTaskForm } from "./useTaskForm";
import { createTask, reparentTask, updateTask } from "../../lib/pm-api";

vi.mock("../../lib/pm-api", () => ({
  createTask: vi.fn(async () => ({ id: "t-new" })),
  updateTask: vi.fn(async () => ({ id: "t-1" })),
  reparentTask: vi.fn(async () => ({})),
}));

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

const stores = vi.hoisted(() => ({
  members: [{ instance_id: "u-1", kind: "user" as const, added_at: "2026-10-08T07:22:18Z" }],
  reload: vi.fn(async () => {}),
}));

vi.mock("../../stores/agentStore", () => ({
  useAgentStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({ agents: {}, fetchAgents: vi.fn(async () => {}) }),
}));

vi.mock("../../stores/pm/boardStore", () => ({
  usePmBoardStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({ tasks: [], reload: stores.reload }),
}));

vi.mock("../../stores/pm/projectStore", () => ({
  usePmProjectStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({ projects: [{ id: "p-1", members: stores.members }] }),
}));

vi.mock("../../stores/userProfileStore", () => ({
  useUserProfileStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({ profile: { displayName: "admin" } }),
}));

vi.mock("./pmMembers", () => ({
  memberOptions: () => [{ value: "u-1", label: "admin" }],
  useSelfAccount: () => ({ user_id: "u-1", username: "admin", display_name: "admin" }),
  usePmDirectoryUsers: () => [],
}));

const row = (over: Record<string, unknown> = {}) =>
  ({
    id: "t-1",
    project_id: "p-1",
    title: "旧标题",
    description: "",
    type: "task",
    status: "pending",
    review_status: "approved",
    priority: "normal",
    assignee: null,
    due_at: null,
    start_at: null,
    dispatched_at: null,
    dispatch_attempts: 0,
    depends_on: [],
    attachments: [],
    result: null,
    created_by: "u-1",
    created_at: "2026-10-05T02:35:00Z",
    updated_at: "2026-10-05T02:35:00Z",
    claimed_at: null,
    submitted_at: null,
    is_blocked: false,
    blocked_by: [],
    depth: 0,
    parent_id: null,
    ...over,
  }) as never;

describe("useTaskForm save path", () => {
  beforeEach(() => {
    vi.mocked(createTask).mockClear();
    vi.mocked(updateTask).mockClear();
    vi.mocked(reparentTask).mockClear();
  });

  it("creates in one call carrying assignee and both dates", async () => {
    const { result } = renderHook(() => useTaskForm({ projectId: "p-1" }));

    act(() => result.current.setTitle("新任务"));
    act(() => result.current.setAssignee("u-1"));
    act(() => result.current.setDueAt("2026-10-30"));
    act(() => result.current.setStartAt("2026-10-20"));

    let outcome: Awaited<ReturnType<typeof result.current.save>> | undefined;
    await act(async () => {
      outcome = await result.current.save();
    });

    expect(outcome?.ok).toBe(true);
    expect(createTask).toHaveBeenCalledTimes(1);
    expect(createTask).toHaveBeenCalledWith("p-1", {
      title: "新任务",
      description: undefined,
      type: "task",
      priority: "normal",
      assignee: "u-1",
      due_at: localDateFieldToIso("2026-10-30"),
      start_at: localDateFieldToIso("2026-10-20"),
      parent_task_id: null,
      depends_on: [],
    });
    // The regression: no follow-up PATCH.
    expect(updateTask).not.toHaveBeenCalled();
    expect(stores.reload).toHaveBeenCalledTimes(1);
  });

  it("reset puts the row back — a discarded draft must not survive into the next edit", () => {
    // The form stays mounted across view <-> edit, so "cancel" that only flips
    // the mode would show the abandoned values again on the next 编辑.
    const initial = row({ title: "Persisted title" });
    const { result } = renderHook(() => useTaskForm({ projectId: "p-1", initial }));
    act(() => result.current.setTitle("Typed then abandoned"));
    expect(result.current.dirty).toBe(true);

    act(() => result.current.reset());
    expect(result.current.values.title).toBe("Persisted title");
    expect(result.current.dirty).toBe(false);
  });

  it("edits with one update, and reparents only when the parent moved", async () => {
    const initial = row({ parent_id: "t-old" });
    const { result } = renderHook(() => useTaskForm({ projectId: "p-1", initial }));
    act(() => result.current.setTitle("改过的标题"));
    await act(async () => {
      await result.current.save();
    });
    expect(updateTask).toHaveBeenCalledTimes(1);
    expect(updateTask).toHaveBeenCalledWith(
      "t-1",
      expect.objectContaining({ title: "改过的标题", assignee: null }),
    );
    expect(reparentTask).not.toHaveBeenCalled();
    expect(createTask).not.toHaveBeenCalled();

    act(() => result.current.setParentId("t-other"));
    await act(async () => {
      await result.current.save();
    });
    expect(reparentTask).toHaveBeenCalledWith("t-1", "t-other");
  });

  it("refuses an empty title without touching the server", async () => {
    const { result } = renderHook(() => useTaskForm({ projectId: "p-1" }));
    let outcome: Awaited<ReturnType<typeof result.current.save>> | undefined;
    await act(async () => {
      outcome = await result.current.save();
    });
    expect(outcome).toEqual({ ok: false, reason: "title_required" });
    expect(createTask).not.toHaveBeenCalled();
    expect(updateTask).not.toHaveBeenCalled();
  });

  it("reports a server failure instead of leaving a half-saved row unspoken", async () => {
    vi.mocked(createTask).mockRejectedValueOnce(new Error("boom"));
    const { result } = renderHook(() => useTaskForm({ projectId: "p-1" }));
    act(() => result.current.setTitle("x"));
    let outcome: Awaited<ReturnType<typeof result.current.save>> | undefined;
    await act(async () => {
      outcome = await result.current.save();
    });
    expect(outcome).toEqual({ ok: false, reason: "failed", message: "boom" });
    await waitFor(() => expect(result.current.saving).toBe(false));
  });

  it("tracks dirty against the row it was opened with", async () => {
    const initial = row();
    const { result } = renderHook(() => useTaskForm({ projectId: "p-1", initial }));
    expect(result.current.dirty).toBe(false);
    act(() => result.current.setPriority("urgent"));
    expect(result.current.dirty).toBe(true);
    act(() => result.current.setPriority("normal"));
    expect(result.current.dirty).toBe(false);
  });
});
