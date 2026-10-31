/**
 * PM 看板显示偏好 — 单页任务数解析与分页区间。
 *
 * 这些断言钉住的是「脏 metadata 不能让看板崩」：旧项目没有 boardPageSize，
 * 手改 / 别的客户端写进来的值可能是 0、负数、字符串、超大数。读取路径
 * 必须每次都回退到默认 20 / 收敛到合法区间，写入路径必须保留兄弟字段。
 */
import { describe, expect, it } from "vitest";
import {
  BOARD_PAGE_SIZE_OPTIONS,
  DEFAULT_BOARD_PAGE_SIZE,
  MAX_BOARD_PAGE_SIZE,
  MIN_BOARD_PAGE_SIZE,
  boardPageBounds,
  boardPageForTask,
  boardPageSlice,
  clampBoardPageSize,
  parseBoardPageSize,
  withBoardPageSize,
} from "./pm-board-settings";

describe("parseBoardPageSize", () => {
  it("falls back to 20 when the project has no preference", () => {
    expect(parseBoardPageSize({})).toBe(DEFAULT_BOARD_PAGE_SIZE);
    expect(parseBoardPageSize(null)).toBe(DEFAULT_BOARD_PAGE_SIZE);
    expect(parseBoardPageSize(undefined)).toBe(DEFAULT_BOARD_PAGE_SIZE);
    expect(DEFAULT_BOARD_PAGE_SIZE).toBe(20);
  });

  it("ignores non-numeric / non-finite values", () => {
    expect(parseBoardPageSize({ boardPageSize: "50" })).toBe(DEFAULT_BOARD_PAGE_SIZE);
    expect(parseBoardPageSize({ boardPageSize: Number.NaN })).toBe(DEFAULT_BOARD_PAGE_SIZE);
    expect(parseBoardPageSize({ boardPageSize: Number.POSITIVE_INFINITY })).toBe(
      DEFAULT_BOARD_PAGE_SIZE,
    );
    expect(parseBoardPageSize({ boardPageSize: true })).toBe(DEFAULT_BOARD_PAGE_SIZE);
  });

  it("clamps values outside the supported window", () => {
    expect(parseBoardPageSize({ boardPageSize: 0 })).toBe(MIN_BOARD_PAGE_SIZE);
    expect(parseBoardPageSize({ boardPageSize: -12 })).toBe(MIN_BOARD_PAGE_SIZE);
    expect(parseBoardPageSize({ boardPageSize: 9999 })).toBe(MAX_BOARD_PAGE_SIZE);
    expect(parseBoardPageSize({ boardPageSize: 50 })).toBe(50);
  });

  it("offers the default as one of the selectable options", () => {
    expect(BOARD_PAGE_SIZE_OPTIONS).toContain(DEFAULT_BOARD_PAGE_SIZE);
  });
});

describe("withBoardPageSize", () => {
  it("merges sibling metadata instead of replacing it", () => {
    // The backend replaces `metadata` wholesale — a bare { boardPageSize }
    // write would wipe the project icon.
    const icon = { kind: "builtin", id: "icon-07" };
    expect(withBoardPageSize({ icon, other: 1 }, 30)).toEqual({
      icon,
      other: 1,
      boardPageSize: 30,
    });
    expect(withBoardPageSize(null, 30)).toEqual({ boardPageSize: 30 });
  });

  it("stores the clamped value, never a raw out-of-range number", () => {
    expect(withBoardPageSize({}, 1).boardPageSize).toBe(MIN_BOARD_PAGE_SIZE);
    expect(withBoardPageSize({}, 10_000).boardPageSize).toBe(MAX_BOARD_PAGE_SIZE);
  });
});

describe("boardPageBounds", () => {
  it("reports the 1-indexed window of the current page", () => {
    expect(boardPageBounds(45, 20, 1)).toEqual({
      page: 1,
      totalPages: 3,
      start: 1,
      end: 20,
    });
    expect(boardPageBounds(45, 20, 2)).toEqual({
      page: 2,
      totalPages: 3,
      start: 21,
      end: 40,
    });
    expect(boardPageBounds(45, 20, 3)).toEqual({
      page: 3,
      totalPages: 3,
      start: 41,
      end: 45,
    });
  });

  it("collapses the page when tasks disappear (delete / drag / page-size change)", () => {
    // Page 3 of 45 → 8 tasks left: the requested page no longer exists.
    expect(boardPageBounds(8, 20, 3).page).toBe(1);
    expect(boardPageBounds(0, 20, 5)).toEqual({
      page: 1,
      totalPages: 1,
      start: 0,
      end: 0,
    });
  });

  it("never divides by a zero / negative page size", () => {
    const bounds = boardPageBounds(10, 0, 1);
    expect(bounds.totalPages).toBe(Math.ceil(10 / MIN_BOARD_PAGE_SIZE));
    expect(clampBoardPageSize(0)).toBe(MIN_BOARD_PAGE_SIZE);
  });
});

describe("boardPageSlice", () => {
  const rows = Array.from({ length: 25 }, (_, i) => i);

  it("slices the requested page and clamps the page first", () => {
    expect(boardPageSlice(rows, 20, 1)).toEqual(rows.slice(0, 20));
    expect(boardPageSlice(rows, 20, 2)).toEqual(rows.slice(20, 25));
    // Out-of-range page clamps to the last real page.
    expect(boardPageSlice(rows, 20, 9)).toEqual(rows.slice(20, 25));
  });

  it("returns everything when the page size covers the list", () => {
    expect(boardPageSlice(rows, 100, 1)).toEqual(rows);
  });
});

describe("boardPageForTask", () => {
  const ref = (id: string, status: string, parentId: string | null) => ({
    id,
    status,
    parent_id: parentId,
  });

  it("lands on the page that holds the task's column root", () => {
    // 60 roots, page 20 → root #44 sits on page 3.
    const roots = Array.from({ length: 60 }, (_, i) => ref(`r-${i}`, "pending", null));
    expect(boardPageForTask(roots, roots, "r-0", "pending", 20)).toBe(1);
    expect(boardPageForTask(roots, roots, "r-20", "pending", 20)).toBe(2);
    expect(boardPageForTask(roots, roots, "r-44", "pending", 20)).toBe(3);
  });

  it("follows a subtask up to its root (children travel with the parent)", () => {
    const roots = Array.from({ length: 30 }, (_, i) => ref(`r-${i}`, "pending", null));
    const child = ref("c-1", "pending", "r-25");
    const grandchild = ref("g-1", "pending", "c-1");
    const tasks = [...roots, child, grandchild];
    expect(boardPageForTask(tasks, roots, "c-1", "pending", 20)).toBe(2);
    expect(boardPageForTask(tasks, roots, "g-1", "pending", 20)).toBe(2);
  });

  it("treats a task whose parent stayed in another column as a root", () => {
    const roots = Array.from({ length: 25 }, (_, i) => ref(`r-${i}`, "done", null));
    const moved = ref("m-1", "done", "r-0");
    // r-0 is still in `pending`, so m-1 is a root of the `done` column.
    const tasks = [ref("r-0", "pending", null), ...roots.slice(1), moved];
    const doneRoots = [...roots.slice(1), moved];
    expect(boardPageForTask(tasks, doneRoots, "m-1", "done", 20)).toBe(2);
  });

  it("returns null when the task is not in that column", () => {
    const roots = [ref("r-0", "pending", null)];
    expect(boardPageForTask(roots, roots, "r-0", "done", 20)).toBeNull();
    expect(boardPageForTask(roots, roots, "missing", "pending", 20)).toBeNull();
    expect(boardPageForTask(roots, [], "r-0", "pending", 20)).toBeNull();
  });
});
