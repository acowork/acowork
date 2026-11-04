/**
 * The task surface's tab model, as a pure rule.
 *
 * Which tabs exist is a server fact (`/tasks/{tid}/attachments`,
 * `/tasks/{tid}/activity`), so it is testable without rendering the dialog —
 * same split as `visibleTabOrder` in GlobalSearchDialog. The dialog's own test
 * file proves the strip renders; this one proves the policy.
 */
import { describe, expect, it } from "vitest";
import {
  resolveTaskTab,
  TASK_EDIT_TAB_I18N,
  TASK_EDIT_TAB_ORDER,
  visibleTaskTabs,
  type TaskEditTab,
} from "./taskEditTabs";

describe("visibleTaskTabs", () => {
  it("a persisted row reaches all four tabs, in canonical order", () => {
    expect(visibleTaskTabs(true)).toEqual(["basic", "related", "files", "submissions"]);
  });

  it("a new task drops the two server-backed tabs", () => {
    // 文件上传 posts to /tasks/{tid}/attachments, 提交记录 reads
    // /tasks/{tid}/activity — without an id both are dead ends.
    expect(visibleTaskTabs(false)).toEqual(["basic", "related"]);
  });

  it("always keeps 基本信息 — a tab strip with nothing reachable is not a strip", () => {
    for (const hasRow of [true, false]) {
      expect(visibleTaskTabs(hasRow)).toContain("basic");
      expect(visibleTaskTabs(hasRow).length).toBeGreaterThan(0);
    }
  });

  it("never invents a tab outside the canonical order", () => {
    const visible = visibleTaskTabs(false);
    const canonical = TASK_EDIT_TAB_ORDER.filter((tab) => visible.includes(tab));
    expect(visible).toEqual(canonical);
  });
});

describe("resolveTaskTab", () => {
  it("keeps a tab that is still reachable", () => {
    const tabs = visibleTaskTabs(true);
    expect(resolveTaskTab("submissions", tabs)).toBe("submissions");
  });

  it("falls back to 基本信息 when the strip changed underneath it", () => {
    // e.g. the row was closed and the same surface re-opened as 新建任务.
    expect(resolveTaskTab("files", visibleTaskTabs(false))).toBe("basic");
    expect(resolveTaskTab("submissions", visibleTaskTabs(false))).toBe("basic");
  });

  it("never falls back to a tab that does not exist", () => {
    for (const tab of TASK_EDIT_TAB_ORDER) {
      expect(TASK_EDIT_TAB_ORDER).toContain(resolveTaskTab(tab, []));
    }
  });
});

describe("TASK_EDIT_TAB_I18N", () => {
  it("labels every tab — a missing entry would paint the key path", () => {
    const mapped = Object.keys(TASK_EDIT_TAB_I18N).sort();
    expect(mapped).toEqual([...TASK_EDIT_TAB_ORDER].sort());
    for (const tab of TASK_EDIT_TAB_ORDER) {
      expect(TASK_EDIT_TAB_I18N[tab as TaskEditTab]).toMatch(/^pm\.task\.tabs\./);
    }
  });
});
