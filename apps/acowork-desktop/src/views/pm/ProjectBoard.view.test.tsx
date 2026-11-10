/**
 * ProjectBoard — 视图路由与项目偏好的读写。
 *
 * 这里验的是"接线"，不是两种视图的内容（内容在各自的测试里）：
 *   1. 默认（metadata 没写过 `pmView`）必须还是看板 —— 打开项目的行为不因
 *      本次改动而变（验收标准第 1 条）；
 *   2. 写偏好时**必须合并兄弟字段**（图标 / `boardPageSize`），后端 PATCH 是
 *      整体替换，漏合并就是数据丢失；
 *   3. 切换是乐观的：写请求还在飞的时候列表就已经出来了，PM 服务不可用
 *      （离线）时照样能切 —— 列表是纯读视图；
 *   4. 写失败只 toast + 撤销镜像，界面回到 metadata 的真值。
 */
import React from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { ProjectBoard } from "./ProjectBoard";
import type { PmListColumnKey } from "../../lib/pm-list-columns";
import type { PmProject } from "../../lib/pm-types";

const state = vi.hoisted(() => ({
  healthy: true as boolean | null,
  updateProjectMeta: vi.fn(),
}));

/** The mocked list view records its props here so a test can fire its callbacks. */
const probe = vi.hoisted(() => ({
  columns: [] as string[],
  pageSize: 0,
  onColumnsChange: (_: string[]) => {},
  onPageSizeChange: (_: number) => {},
}));

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
vi.mock("../../components/common/ToastProvider", () => ({ showToast: vi.fn() }));
vi.mock("../../stores/pm/projectStore", () => ({
  usePmProjectStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({ updateProjectMeta: state.updateProjectMeta }),
}));
vi.mock("../../stores/pm/boardStore", () => ({
  usePmBoardStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({ tasks: [], loading: false }),
}));
vi.mock("../../stores/pm/healthStore", () => ({
  usePmHealthStore: (selector: (s: Record<string, unknown>) => unknown) =>
    selector({ healthy: state.healthy }),
}));
vi.mock("../../components/common/ProjectAvatar", () => ({
  ProjectAvatar: () => null,
}));
// The two task surfaces have their own tests; here they are wiring probes.
vi.mock("./KanbanBoard", () => ({
  KanbanBoard: (props: { pageSize: number }) => (
    <div data-testid="kanban" data-page-size={props.pageSize} />
  ),
}));
vi.mock("./TaskListView", () => ({
  TaskListView: (props: {
    columns: string[];
    pageSize: number;
    onColumnsChange: (next: string[]) => void;
    onPageSizeChange: (size: number) => void;
  }) => {
    probe.columns = props.columns;
    probe.pageSize = props.pageSize;
    probe.onColumnsChange = props.onColumnsChange;
    probe.onPageSizeChange = props.onPageSizeChange;
    return <div data-testid="list" data-columns={props.columns.join(",")} data-page-size={props.pageSize} />;
  },
}));

function project(metadata: Record<string, unknown> | null, id = "p-1"): PmProject {
  return {
    id,
    title: "Demo",
    description: "",
    status: "active",
    metadata,
    created_at: "2026-10-01T00:00:00Z",
    updated_at: "2026-10-01T00:00:00Z",
  };
}

/** The metadata patch the last `updateProjectMeta` call sent. */
function lastPatch(): Record<string, unknown> {
  const calls = state.updateProjectMeta.mock.calls as unknown as [string, { metadata: Record<string, unknown> }][];
  return calls[calls.length - 1][1].metadata;
}

beforeEach(() => {
  state.healthy = true;
  state.updateProjectMeta = vi.fn().mockResolvedValue(true);
  vi.clearAllMocks();
});

describe("routing", () => {
  it("shows the board when the project never chose a view", () => {
    render(<ProjectBoard project={project(null)} />);
    expect(screen.getByTestId("kanban")).toBeTruthy();
    expect(screen.queryByTestId("list")).toBeNull();
  });

  it("shows the list when metadata says so, with the stored columns", () => {
    render(<ProjectBoard project={project({ pmView: "list", pmListColumns: ["title", "status", "dispatch"], icon: "📁" })} />);
    expect(screen.getByTestId("list").getAttribute("data-columns")).toBe("title,status,dispatch");
    // 未写过的偏好走默认值：每页 50 行。
    expect(screen.getByTestId("list").getAttribute("data-page-size")).toBe("50");
  });

  it("hands the board its own per-column page size, untouched by the list setting", () => {
    render(<ProjectBoard project={project({ boardPageSize: 7 })} />);
    expect(screen.getByTestId("kanban").getAttribute("data-page-size")).toBe("7");
  });

  it("ignores a dirty pmView instead of rendering nothing", () => {
    render(<ProjectBoard project={project({ pmView: "table" })} />);
    expect(screen.getByTestId("kanban")).toBeTruthy();
  });

  it("keeps the empty-project pane when nothing is selected", () => {
    render(<ProjectBoard project={null} />);
    expect(screen.queryByTestId("kanban")).toBeNull();
    expect(screen.queryByTestId("list")).toBeNull();
  });
});

describe("view switch", () => {
  it("switches optimistically and merges metadata instead of replacing it", async () => {
    let resolveWrite: (ok: boolean) => void = () => {};
    state.updateProjectMeta = vi.fn().mockReturnValue(new Promise<boolean>((r) => (resolveWrite = r)));
    render(<ProjectBoard project={project({ icon: "📁", boardPageSize: 30 })} />);

    fireEvent.click(screen.getByText("pm.view.list"));
    // 请求还在飞，列表已经在屏幕上了。
    expect(screen.getByTestId("list")).toBeTruthy();
    expect(state.updateProjectMeta).toHaveBeenCalledTimes(1);

    const patch = lastPatch();
    resolveWrite(true);
    await waitFor(() => expect(screen.getByTestId("list")).toBeTruthy());
    expect(patch).toEqual({ icon: "📁", boardPageSize: 30, pmView: "list" });
  });

  it("falls back to the stored view when the write fails", async () => {
    state.updateProjectMeta = vi.fn().mockResolvedValue(false);
    render(<ProjectBoard project={project({ pmView: "board" })} />);

    fireEvent.click(screen.getByText("pm.view.list"));
    expect(screen.getByTestId("list")).toBeTruthy();
    await waitFor(() => expect(screen.queryByTestId("list")).toBeNull());
    expect(screen.getByTestId("kanban")).toBeTruthy();
  });

  it("stays clickable when the PM service is offline", async () => {
    state.healthy = false;
    state.updateProjectMeta = vi.fn().mockResolvedValue(false);
    render(<ProjectBoard project={project(null)} />);

    const switcher = screen.getByText("pm.view.list").closest("button") as HTMLButtonElement;
    expect(switcher.disabled).toBe(false);
    fireEvent.click(switcher);
    expect(screen.getByTestId("list")).toBeTruthy();
    await waitFor(() => expect(screen.getByTestId("kanban")).toBeTruthy());
  });

  it("does not write anything when the user picks the view already shown", () => {
    render(<ProjectBoard project={project({ pmView: "list" })} />);
    fireEvent.click(screen.getByText("pm.view.list"));
    expect(state.updateProjectMeta).not.toHaveBeenCalled();
  });

  it("drops the in-flight mirror when the project changes", async () => {
    // A 的切换请求还在飞，用户已经点到 B：B 从没选过视图，那一帧就必须是看板。
    // 镜像曾经不带项目身份（靠 useEffect 事后清），这一用例会看到 A 的列表。
    let resolveWrite: (ok: boolean) => void = () => {};
    state.updateProjectMeta = vi.fn().mockReturnValue(new Promise<boolean>((r) => (resolveWrite = r)));
    const { rerender } = render(<ProjectBoard project={project(null, "p-a")} />);
    fireEvent.click(screen.getByText("pm.view.list"));
    expect(screen.getByTestId("list")).toBeTruthy();

    rerender(<ProjectBoard project={project(null, "p-b")} />);
    expect(screen.queryByTestId("list")).toBeNull();
    expect(screen.getByTestId("kanban")).toBeTruthy();

    // 迟到回来的那次写入也不能把 B 翻成列表。
    resolveWrite(true);
    await waitFor(() => expect(screen.getByTestId("kanban")).toBeTruthy());
    expect(screen.queryByTestId("list")).toBeNull();
  });
});

describe("list preferences", () => {
  it("persists a column change with the sibling metadata intact", async () => {
    render(<ProjectBoard project={project({ pmView: "list", icon: "📁" })} />);
    await act(async () => {
      probe.onColumnsChange(["title", "review_status", "title"]);
    });
    // 写入前就归一化：重复 key 去掉、title 留在第 0 位、图标不丢。
    expect(lastPatch()).toEqual({
      pmView: "list",
      icon: "📁",
      pmListColumns: ["title", "review_status"],
    });
  });

  it("persists the page size and clamps a bogus value", async () => {
    render(<ProjectBoard project={project({ pmView: "list", pmListPageSize: 50 })} />);
    expect(probe.pageSize).toBe(50);
    await act(async () => {
      probe.onPageSizeChange(100);
    });
    expect(lastPatch().pmListPageSize).toBe(100);
    await act(async () => {
      probe.onPageSizeChange(3);
    });
    expect(lastPatch().pmListPageSize).toBe(10);
  });

  it("shows the new columns immediately, before the write lands", async () => {
    let resolveWrite: (ok: boolean) => void = () => {};
    state.updateProjectMeta = vi.fn().mockReturnValue(new Promise<boolean>((r) => (resolveWrite = r)));
    render(<ProjectBoard project={project({ pmView: "list" })} />);
    await act(async () => {
      probe.onColumnsChange(["title", "description"] as PmListColumnKey[]);
    });
    expect(screen.getByTestId("list").getAttribute("data-columns")).toBe("title,description");
    resolveWrite(true);
    await waitFor(() => expect(screen.getByTestId("list")).toBeTruthy());
  });
});
