/**
 * pm-view-settings — 项目 metadata 里的视图偏好（视图 / 显示列 / 每页行数）。
 *
 * 钉住三条契约：
 *   1. **读**：缺失 / 脏值一律回退默认，绝不抛错（旧项目零迁移，手改过的
 *      JSON 也不能白屏）；
 *   2. **写**：`with*` 必须先展开旧 metadata 再覆盖单键 —— 后端 PATCH 是
 *      整体替换，漏一个展开就会把图标、`boardPageSize` 等兄弟字段清空；
 *   3. 列数组的归一化读写同规则（未知 key 丢弃、`title` 留在第 0 位、去重），
 *      否则内存态和 metadata 会长期漂移。
 */
import { describe, expect, it } from "vitest";
import {
  DEFAULT_PM_LIST_PAGE_SIZE,
  DEFAULT_PM_VIEW_MODE,
  MAX_PM_LIST_PAGE_SIZE,
  MIN_PM_LIST_PAGE_SIZE,
  PM_LIST_COLUMNS_METADATA_KEY,
  PM_LIST_PAGE_SIZE_METADATA_KEY,
  PM_VIEW_METADATA_KEY,
  parsePmListColumns,
  parsePmListPageSize,
  parsePmView,
  withPmListColumns,
  withPmListPageSize,
  withPmView,
} from "./pm-view-settings";

/** The documented default set — duplicated on purpose so a registry edit that
 *  changes the defaults has to be a deliberate, reviewed decision. */
const DEFAULT_PM_LIST_COLUMNS_FALLBACK = ["title", "status", "priority", "assignee", "due_at"];

describe("parsePmView", () => {
  it("falls back to the board view when unset or dirty", () => {
    expect(parsePmView(undefined)).toBe(DEFAULT_PM_VIEW_MODE);
    expect(parsePmView(null)).toBe("board");
    expect(parsePmView({})).toBe("board");
    expect(parsePmView({ pmView: "table" })).toBe("board");
    expect(parsePmView({ pmView: 42 })).toBe("board");
    expect(parsePmView({ pmView: "list" })).toBe("list");
  });
});

describe("withPmView", () => {
  it("merges instead of replacing metadata", () => {
    const metadata = { icon: "📁", boardPageSize: 30 };
    expect(withPmView(metadata, "list")).toEqual({
      icon: "📁",
      boardPageSize: 30,
      [PM_VIEW_METADATA_KEY]: "list",
    });
  });

  it("tolerates null metadata", () => {
    expect(withPmView(null, "board")).toEqual({ [PM_VIEW_METADATA_KEY]: "board" });
  });
});

describe("parsePmListColumns", () => {
  it("falls back to the 5 default columns when missing or not an array", () => {
    expect(parsePmListColumns(undefined)).toEqual(DEFAULT_PM_LIST_COLUMNS_FALLBACK);
    expect(parsePmListColumns({ pmListColumns: "status" })).toEqual(
      DEFAULT_PM_LIST_COLUMNS_FALLBACK,
    );
  });

  it("drops unknown keys and duplicates, keeps title first", () => {
    expect(
      parsePmListColumns({
        pmListColumns: ["status", "nope", "title", "priority", "status"],
      }),
    ).toEqual(["title", "status", "priority"]);
  });

  it("re-adds title when a hand-edited array hides it", () => {
    expect(parsePmListColumns({ pmListColumns: ["status", "priority"] })).toEqual([
      "title",
      "status",
      "priority",
    ]);
  });
});

describe("withPmListColumns", () => {
  it("normalizes on write so memory and metadata cannot drift", () => {
    const metadata = { icon: "📁" };
    const next = withPmListColumns(metadata, ["status", "bogus", "title"]);
    expect(next[PM_LIST_COLUMNS_METADATA_KEY]).toEqual(["title", "status"]);
    expect(next.icon).toBe("📁");
  });
});

describe("parsePmListPageSize / withPmListPageSize", () => {
  it("defaults to 50 rows and clamps into the legal range", () => {
    expect(parsePmListPageSize({})).toBe(DEFAULT_PM_LIST_PAGE_SIZE);
    expect(parsePmListPageSize({ pmListPageSize: "50" })).toBe(DEFAULT_PM_LIST_PAGE_SIZE);
    expect(parsePmListPageSize({ pmListPageSize: 1 })).toBe(MIN_PM_LIST_PAGE_SIZE);
    expect(parsePmListPageSize({ pmListPageSize: 99_999 })).toBe(MAX_PM_LIST_PAGE_SIZE);
    expect(parsePmListPageSize({ pmListPageSize: 100 })).toBe(100);
  });

  it("clamps on write and preserves sibling metadata", () => {
    const next = withPmListPageSize({ icon: "📁", pmView: "list" }, 7);
    expect(next[PM_LIST_PAGE_SIZE_METADATA_KEY]).toBe(MIN_PM_LIST_PAGE_SIZE);
    expect(next.icon).toBe("📁");
    expect(next.pmView).toBe("list");
  });
});
