/**
 * taskDetailStore — the activity half of the task detail face.
 *
 * Pinned here (and nowhere else) because these are store facts:
 *
 * 1. Opening a row pulls the submission history next to the task itself, and a
 *    failure on that one request must not stop the dialog from opening — an
 *    older PM build has no `/activity` endpoint at all.
 * 2. `refresh()` keeps the previous array identity when the history did not
 *    change. The activity stream is append-only, so equal entry ids mean equal
 *    content, and a new identity on every refresh would re-mount rows the user
 *    is already reading (AGENTS.md「Identity-preserving store updates」).
 * 3. `addReply()` is the only writer of the local list: it appends what the
 *    server returned, and never appends on failure.
 */
import { describe, expect, it, vi, beforeEach } from "vitest";

const api = vi.hoisted(() => ({
  getTask: vi.fn(),
  listTaskAttachments: vi.fn(),
  listTaskActivity: vi.fn(),
  addTaskActivity: vi.fn(),
}));

vi.mock("../../lib/pm-api", () => api);
vi.mock("../../lib/logger", () => ({ log: { warn: vi.fn(), debug: vi.fn(), error: vi.fn() } }));

import { usePmTaskDetailStore } from "./taskDetailStore";
import type { PmTaskActivity, PmTaskResponse } from "../../lib/pm-types";

const row = {
  id: "t-1",
  project_id: "p-1",
  title: "T",
  created_at: "2026-10-30T08:00:00Z",
  updated_at: "2026-10-30T08:00:00Z",
} as unknown as PmTaskResponse;

const entry = (id: string): PmTaskActivity => ({
  id,
  kind: "submitted",
  actor: "422c3696-c762-4e19-b8fd-bdec5eb6b1bb",
  actor_kind: "agent",
  text: "done",
  attachment_ids: [],
  created_at: "2026-10-30T08:40:00Z",
});

beforeEach(() => {
  vi.clearAllMocks();
  usePmTaskDetailStore.getState().clear();
  api.getTask.mockResolvedValue(row);
  api.listTaskAttachments.mockResolvedValue([]);
  api.listTaskActivity.mockResolvedValue([entry("act-1")]);
});

describe("taskDetailStore activity", () => {
  it("loads history with the row", async () => {
    await usePmTaskDetailStore.getState().openTask("t-1");
    const s = usePmTaskDetailStore.getState();
    expect(s.detail?.id).toBe("t-1");
    expect(s.activity.map((a) => a.id)).toEqual(["act-1"]);
  });

  it("still opens the row when the history request fails", async () => {
    api.listTaskActivity.mockRejectedValue(new Error("404 not found"));
    await usePmTaskDetailStore.getState().openTask("t-1");
    const s = usePmTaskDetailStore.getState();
    expect(s.detail?.id).toBe("t-1");
    expect(s.activity).toEqual([]);
    expect(s.error).toBeNull();
  });

  it("keeps the array identity across a refresh that changed nothing", async () => {
    await usePmTaskDetailStore.getState().openTask("t-1");
    const first = usePmTaskDetailStore.getState().activity;
    api.listTaskActivity.mockResolvedValue([entry("act-1")]);
    await usePmTaskDetailStore.getState().refresh();
    expect(usePmTaskDetailStore.getState().activity).toBe(first);
  });

  it("swaps in the new list when an entry landed", async () => {
    await usePmTaskDetailStore.getState().openTask("t-1");
    api.listTaskActivity.mockResolvedValue([entry("act-1"), entry("act-2")]);
    await usePmTaskDetailStore.getState().refresh();
    expect(usePmTaskDetailStore.getState().activity.map((a) => a.id)).toEqual([
      "act-1",
      "act-2",
    ]);
  });

  it("appends the server entry on a reply", async () => {
    await usePmTaskDetailStore.getState().openTask("t-1");
    api.addTaskActivity.mockResolvedValue(entry("act-2"));
    const ok = await usePmTaskDetailStore.getState().addReply("  辛苦了  ");
    expect(ok).toBe(true);
    // 正文在服务端 trim，客户端发出去的就该是 trim 过的。
    expect(api.addTaskActivity).toHaveBeenCalledWith("t-1", "辛苦了");
    const s = usePmTaskDetailStore.getState();
    expect(s.activity.map((a) => a.id)).toEqual(["act-1", "act-2"]);
    expect(s.replying).toBe(false);
  });

  it("does not touch the list when the reply fails or is empty", async () => {
    await usePmTaskDetailStore.getState().openTask("t-1");
    expect(await usePmTaskDetailStore.getState().addReply("   ")).toBe(false);
    expect(api.addTaskActivity).not.toHaveBeenCalled();

    api.addTaskActivity.mockRejectedValue(new Error("500"));
    expect(await usePmTaskDetailStore.getState().addReply("再试一次")).toBe(false);
    const s = usePmTaskDetailStore.getState();
    expect(s.activity.map((a) => a.id)).toEqual(["act-1"]);
    expect(s.replying).toBe(false);
    expect(s.error).toBe("500");
  });

  it("a create-mode dialog owns no history", async () => {
    usePmTaskDetailStore.getState().openCreate({ projectId: "p-1" });
    const s = usePmTaskDetailStore.getState();
    expect(s.activity).toEqual([]);
    expect(api.listTaskActivity).not.toHaveBeenCalled();
  });
});
