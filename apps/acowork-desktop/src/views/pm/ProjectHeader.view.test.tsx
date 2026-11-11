/**
 * ProjectHeader — 视图切换控件的可用性契约。
 *
 * 这是"离线不禁用视图切换"（决策 1）的落点：列表视图是纯读的，PM 服务不可用
 * 时看数据反而更有用，所以 segmented 必须保持可点；真正该置灰的是**写**入口
 * （新建任务）。切换本身只上抛意图，落盘与回滚在 `ProjectBoard`（那边的
 * `ProjectBoard.view.test.tsx` 覆盖了乐观写与失败回退）。
 */
import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { ProjectHeader } from "./ProjectHeader";
import type { PmProject } from "../../lib/pm-types";

const state = vi.hoisted(() => ({ healthy: true as boolean | null }));

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
vi.mock("../../stores/pm/projectStore", () => ({
  usePmProjectStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({ updateProjectMeta: vi.fn() }),
}));
vi.mock("../../stores/pm/boardStore", () => ({
  usePmBoardStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({ tasks: [] }),
}));
vi.mock("../../stores/pm/healthStore", () => ({
  usePmHealthStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({ healthy: state.healthy }),
}));
vi.mock("../../components/common/ProjectAvatar", () => ({
  ProjectAvatar: () => null,
}));

const PROJECT: PmProject = {
  id: "p-1",
  title: "Demo",
  description: "",
  status: "active",
  metadata: null,
  created_at: "2026-10-01T00:00:00Z",
  updated_at: "2026-10-01T00:00:00Z",
};

function header(onViewChange = vi.fn()) {
  render(
    <ProjectHeader project={PROJECT} onNewTask={() => {}} view="board" onViewChange={onViewChange} />,
  );
  return onViewChange;
}

const buttonFor = (label: string) =>
  screen.getByText(label).closest("button") as HTMLButtonElement;

/** The create button renders as "+ pm.newTask" (two text nodes), so match on
 *  the accessible name instead of a text substring. */
const newTaskButton = () =>
  screen.getByRole("button", { name: "+ pm.newTask" }) as HTMLButtonElement;

beforeEach(() => {
  state.healthy = true;
});

describe("project note", () => {
  // 备注原来是"整块可点"：一行字连同它周围的空白全是点击区，误触一下就滚进
  // 编辑态，退出还得先点里面再点外面。入口收成行尾的铅笔，正文只读。
  it("opens the note editor from the pencil, not from the note text", () => {
    header();
    fireEvent.click(screen.getByText("pm.noDescription"));
    expect(screen.queryByRole("textbox", { name: "pm.projectDescEdit" })).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "pm.projectDescEdit" }));
    expect(screen.getByRole("textbox", { name: "pm.projectDescEdit" })).toBeTruthy();
  });
});

describe("view switch", () => {
  it("marks the active view for assistive tech", () => {
    header();
    expect(buttonFor("pm.view.board").getAttribute("aria-pressed")).toBe("true");
    expect(buttonFor("pm.view.list").getAttribute("aria-pressed")).toBe("false");
  });

  it("uphands the picked view without writing anything itself", () => {
    const onViewChange = header();
    fireEvent.click(buttonFor("pm.view.list"));
    expect(onViewChange).toHaveBeenCalledWith("list");
  });

  it("stays enabled when the PM service is offline", () => {
    state.healthy = false;
    const onViewChange = header();
    expect(buttonFor("pm.view.list").disabled).toBe(false);
    fireEvent.click(buttonFor("pm.view.list"));
    expect(onViewChange).toHaveBeenCalledWith("list");
  });

  it("still disables the write entry point next to it", () => {
    state.healthy = false;
    header();
    expect(newTaskButton().disabled).toBe(true);
  });

  it("is enabled again when the service is healthy", () => {
    header();
    expect(newTaskButton().disabled).toBe(false);
  });
});
