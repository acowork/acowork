/**
 * TaskFieldPicker — 列配置弹层。
 *
 * 钉住四件事：
 *   1. 「已显示」按 columns 数组顺序渲染，顺序就是表格列序（所以排序必须
 *      真的回传新数组）；
 *   2. 「任务」列既勾不掉也挪不动（`title` 不可隐藏是产品规则，不是 UI 巧合）；
 *   3. 勾选即增删列，回传的是新数组而不是原地修改；
 *   4. 搜索同时过滤两段，重置按钮只上抛意图（默认列由调用方决定）。
 */
import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { TaskFieldPicker } from "./TaskFieldPicker";
import { DEFAULT_PM_LIST_COLUMNS } from "../../lib/pm-list-columns";
import type { PmListColumnKey } from "../../lib/pm-list-columns";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

const ANCHOR = document.createElement("button");
const COLUMNS: PmListColumnKey[] = ["title", "status", "priority"];

function open(
  props: Partial<{
    columns: PmListColumnKey[];
    onColumnsChange: (next: PmListColumnKey[]) => void;
    onReset: () => void;
  }> = {},
) {
  const onColumnsChange = props.onColumnsChange ?? vi.fn();
  const onReset = props.onReset ?? vi.fn();
  const onClose = vi.fn();
  render(
    <TaskFieldPicker
      anchor={ANCHOR}
      columns={props.columns ?? COLUMNS}
      onColumnsChange={onColumnsChange}
      onReset={onReset}
      onClose={onClose}
    />,
  );
  return { onColumnsChange, onReset, onClose };
}

const click = (label: string) => fireEvent.click(screen.getByText(label));

beforeEach(() => {
  vi.clearAllMocks();
});

describe("shown columns", () => {
  it("lists them in array order", () => {
    open();
    const labels = ["pm.list.col.title", "pm.list.col.status", "pm.list.col.priority"].map(
      (label) => screen.getByText(label),
    );
    // DOM 顺序 = 列顺序：断言"谁在前"而不是"谁存在"。
    const all = [...document.querySelectorAll("button")];
    const positions = labels.map((el) => all.indexOf(el.closest("button") ?? el));
    expect(positions[0]).toBeLessThan(positions[1]);
    expect(positions[1]).toBeLessThan(positions[2]);
  });

  it("cannot hide or move the task column", () => {
    const { onColumnsChange } = open();
    // 弹层条目用 aria-disabled（保留 hover 与 title，让"为什么不能隐藏"看得见），
    // 所以断言的是"点了不发生任何列变更"，不是原生 disabled。
    const title = screen.getByText("pm.list.col.title").closest("button") as HTMLButtonElement;
    expect(title.getAttribute("aria-disabled")).toBe("true");
    fireEvent.click(title);
    expect(onColumnsChange).not.toHaveBeenCalled();
    // 三个条目各有一个上移按钮：第 0 个属于 title，必须禁用。
    const movesUp = screen.getAllByLabelText("pm.list.fields.moveUp") as HTMLButtonElement[];
    expect(movesUp[0].disabled).toBe(true);
  });

  it("unchecking a shown column drops it and keeps title", () => {
    const { onColumnsChange } = open();
    click("pm.list.col.status");
    expect(onColumnsChange).toHaveBeenCalledWith(["title", "priority"]);
  });

  it("moves a column down by swapping with its neighbour", () => {
    const { onColumnsChange } = open();
    const movesDown = screen.getAllByLabelText("pm.list.fields.moveDown");
    // 第 0 个是 title（禁用），第 1 个是 status → 下移 = 与 priority 换位。
    fireEvent.click(movesDown[1]);
    expect(onColumnsChange).toHaveBeenCalledWith(["title", "priority", "status"]);
  });

  it("never lets a column move up past the task column", () => {
    const { onColumnsChange } = open();
    const movesUp = screen.getAllByLabelText("pm.list.fields.moveUp");
    // status 是第 1 位，上面就是 title：按钮禁用，硬点也不该回传。
    expect(movesUp[1].hasAttribute("disabled")).toBe(true);
    fireEvent.click(movesUp[2]);
    expect(onColumnsChange).toHaveBeenCalledWith(["title", "priority", "status"]);
  });
});

describe("addable columns", () => {
  it("appends a field when checked", () => {
    const { onColumnsChange } = open();
    click("pm.list.col.reviewStatus");
    expect(onColumnsChange).toHaveBeenCalledWith([...COLUMNS, "review_status"]);
  });

  it("does not offer what is already shown", () => {
    open();
    // status 已在「已显示」里，整段只应出现一次。
    expect(screen.getAllByText("pm.list.col.status")).toHaveLength(1);
  });
});

describe("search", () => {
  it("filters both sections", () => {
    open();
    fireEvent.change(screen.getByLabelText("pm.list.fields.searchPlaceholder"), {
      target: { value: "review" },
    });
    expect(screen.queryByText("pm.list.col.priority")).toBeNull();
    expect(screen.getByText("pm.list.col.reviewStatus")).toBeTruthy();
  });

  it("says nothing matched instead of showing an empty popover", () => {
    open();
    fireEvent.change(screen.getByLabelText("pm.list.fields.searchPlaceholder"), {
      target: { value: "zzz-no-such-field" },
    });
    expect(screen.getAllByText("pm.list.noMatch").length).toBeGreaterThan(0);
  });
});

describe("reset", () => {
  it("uphands the intent without touching columns itself", () => {
    const { onReset, onColumnsChange } = open();
    click("pm.list.fields.reset");
    expect(onReset).toHaveBeenCalledTimes(1);
    expect(onColumnsChange).not.toHaveBeenCalled();
  });

  it("the default set is what the caller resets to", () => {
    // 视图层的"重置"必须回到注册表声明的 5 个核心列，不是硬编码副本。
    expect(DEFAULT_PM_LIST_COLUMNS).toEqual(["title", "status", "priority", "assignee", "due_at"]);
  });
});
