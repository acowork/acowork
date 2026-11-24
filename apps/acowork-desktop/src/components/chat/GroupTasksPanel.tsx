/**
 * GroupTasksPanel — 项目群聊右侧槽位里的任务面板。
 *
 * 四个折叠卡片 = PM 看板的四列（`KanbanColumn` + `TaskCard`），所以卡片外观、
 * 数量徽章、空态、分页条与看板是同一份代码，不是照着画的第二套：
 * - 面板窄、纵向排列，四列默认全部折叠（只留列头），点列头展开；
 * - 展开后各列均分面板高度并各自内部滚动（虚拟化 + 分页逻辑照旧，
 *   单页数量仍取项目 `metadata.boardPageSize`）；
 * - 拖拽在这里没有意义（40px 宽的面板里纵向拖不出去），所以 `DndContext`
 *   不带传感器 —— 卡片只读，点击打开唯一的任务弹框 `TaskEditDialog`。
 *
 * 数据与其他 PM 表面共用 `usePmBoardStore`：群背后的项目 id 从
 * `useProjectGroupStore` 拿，进面板时 `loadTasks(projectId)`（带 10s 缓存）。
 */

import { useCallback, useEffect, useMemo, useState } from "react";
import { DndContext } from "@dnd-kit/core";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import { CAPSULE_PANE_CN } from "../common/capsule";
import { OverlayScrollbar } from "../common/OverlayScrollbar";
import { useProjectGroupStore } from "../../stores/projectGroupStore";
import { usePmProjectStore } from "../../stores/pm/projectStore";
import { usePmBoardStore } from "../../stores/pm/boardStore";
import { usePmTaskDetailStore } from "../../stores/pm/taskDetailStore";
import { KanbanColumn } from "../../views/pm/KanbanColumn";
import { TaskEditDialog } from "../../views/pm/TaskEditDialog";
import { BOARD_COLUMNS } from "../../lib/pm-types";
import { parseBoardPageSize } from "../../lib/pm-board-settings";
import type { PmTaskResponse, TaskStatus } from "../../lib/pm-types";

interface GroupTasksPanelProps {
  width: number;
  onResizeStart?: (e: React.MouseEvent) => void;
  /** Rendered inside the hover drawer: the drawer owns the geometry (see
   *  `GroupMembersPanel` for the same contract). */
  overlay?: boolean;
}

/** 默认全折叠：面板的常规形态是四个列头一行一个，点开才占篇幅。 */
const ALL_COLLAPSED: Partial<Record<TaskStatus, boolean>> = {
  pending: true,
  in_progress: true,
  submitted: true,
  done: true,
};

/** 页码表为空时的稳定引用（selector 契约 / memo 依赖都吃这一套）。 */
const EMPTY_PAGES: Partial<Record<TaskStatus, number>> = {};

/** 空任务列：稳定引用，别让 `?? []` 每帧产生新数组。 */
const EMPTY_TASKS: PmTaskResponse[] = [];

/** 无传感器：这个面板里没有拖拽（见文件头）。 */
const NO_SENSORS: [] = [];

export function GroupTasksPanel({ width, onResizeStart, overlay = false }: GroupTasksPanelProps) {
  const { t } = useTranslation();
  const projectId = useProjectGroupStore(
    (s) => s.groups.find((g) => g.session_id === s.activeGroupId)?.project_id,
  );
  const pageSize = usePmProjectStore(
    (s) => parseBoardPageSize(s.projects.find((p) => p.id === projectId)?.metadata),
  );
  const loadProjects = usePmProjectStore((s) => s.loadProjects);

  const tasks = usePmBoardStore((s) => s.tasks);
  const boardProjectId = usePmBoardStore((s) => s.projectId);
  const columnRoots = usePmBoardStore((s) => s.columnRoots);
  const loadTasks = usePmBoardStore((s) => s.loadTasks);

  const openTask = usePmTaskDetailStore((s) => s.openTask);
  const dialogOpen = usePmTaskDetailStore((s) => s.frames.length > 0);
  const clearDetail = usePmTaskDetailStore((s) => s.clear);

  // 群聊视图不加载 PM 项目列表，这里补一次（30s 缓存，重复挂载不折腾）。
  // 列表只用来读每页数量，读不到就按默认值走。
  useEffect(() => {
    if (projectId) void loadProjects({ silent: true });
  }, [projectId, loadProjects]);

  useEffect(() => {
    if (projectId) void loadTasks(projectId);
  }, [projectId, loadTasks]);

  // 折叠状态按列各记一份：点哪张卡片就展开哪张。
  const [collapsed, setCollapsed] = useState<Partial<Record<TaskStatus, boolean>>>(ALL_COLLAPSED);
  const toggle = useCallback((status: TaskStatus) => {
    setCollapsed((prev) => ({ ...prev, [status]: !prev[status] }));
  }, []);

  // 页码归面板持有（列内部重置会在数据变化后丢掉"翻到某页"的能力），
  // 切项目时按 projectId 归零 —— 与 `KanbanBoard` 同一条约定。
  const [pageState, setPageState] = useState<{
    projectId: string | undefined;
    pages: Partial<Record<TaskStatus, number>>;
  }>(() => ({ projectId, pages: EMPTY_PAGES }));
  const pages = pageState.projectId === projectId ? pageState.pages : EMPTY_PAGES;
  const handlePageChange = useCallback(
    (status: TaskStatus, page: number) => {
      setPageState((prev) => ({
        projectId,
        pages: { ...(prev.projectId === projectId ? prev.pages : EMPTY_PAGES), [status]: page },
      }));
    },
    [projectId],
  );

  // 只看本项目的数据：store 里可能还留着上一个项目（PM 视图刚看过别的项目）。
  // `tasks[0]` 那一半判断挡的是 loadTasks 已把 projectId 改成本项目、列表还没
  // 回来的那一帧 —— 只比 projectId 会把上一个项目的任务先画出来。
  const ready =
    boardProjectId !== null &&
    boardProjectId === projectId &&
    (tasks.length === 0 || tasks[0].project_id === projectId);
  const rootsByStatus = useMemo(() => {
    const map = new Map<TaskStatus, PmTaskResponse[]>();
    for (const col of BOARD_COLUMNS) map.set(col.status, ready ? columnRoots(col.status) : EMPTY_TASKS);
    return map;
    // `tasks` 不是本函数的入参，但 `columnRoots` 读的就是 store 里那一份
    // tasks —— 少了它，任务更新后这里永远不重算。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ready, tasks, columnRoots]);

  const handleOpenTask = useCallback(
    (taskId: string) => {
      void openTask(taskId);
    },
    [openTask],
  );

  return (
    <div
      className={cn("relative flex shrink-0 flex-col", overlay ? "h-full w-full" : "ml-0.5")}
      style={overlay ? undefined : { width }}
    >
      {!overlay && onResizeStart && (
        <div
          className="group absolute bottom-0 -left-[3px] top-0 z-10 w-1 cursor-col-resize"
          onMouseDown={onResizeStart}
        >
          <div className="absolute inset-y-0 left-0 w-1 rounded-full transition-colors group-hover:bg-[var(--color-accent)]/30 group-active:bg-[var(--color-accent)]/60" />
        </div>
      )}

      <div className={cn(CAPSULE_PANE_CN, "flex-1 bg-right-panel")}>
        {/* 面板头 —— 与群成员面板 / agent 右面板同一个 pane 头语法 */}
        <div className="flex min-h-[var(--ui-list-header-h)] shrink-0 items-center border-b border-border-divider px-[var(--tab-px)] text-[length:var(--tab-font-size)] font-medium leading-[var(--tab-line-height)] text-text-tertiary">
          <span className="min-w-0 flex-1 truncate">{t("groupTasks.title")}</span>
        </div>

        <DndContext sensors={NO_SENSORS}>
          {/* 每张卡按内容高度排（`KanbanColumn` 里是 `flex-none`），所以几张卡
              同时展开可能比面板高 —— 这一层负责整个面板滚动，卡片内部的高度
              上限由各列自己管。 */}
          <OverlayScrollbar fill="flex" className="p-2">
            <div className="flex flex-col gap-1.5">
              {BOARD_COLUMNS.map((col) => (
                <KanbanColumn
                  key={`${projectId}-${col.status}`}
                  // 设置卡片语法：标题条 panel-block + 正文 panel-inset（PM 看板
                  // 的列是一整片面板色，在 320px 宽的右侧面板里和面板撞色）。
                  variant="card"
                  status={col.status}
                  i18nKey={col.i18nKey}
                  tasks={rootsByStatus.get(col.status) ?? EMPTY_TASKS}
                  onOpenTask={handleOpenTask}
                  pageSize={pageSize}
                  page={pages[col.status] ?? 1}
                  onPageChange={(next) => handlePageChange(col.status, next)}
                  collapsed={collapsed[col.status] ?? false}
                  onToggleCollapsed={() => toggle(col.status)}
                />
              ))}
            </div>
          </OverlayScrollbar>
        </DndContext>
      </div>

      {/* 任务的唯一表面：卡片点开就是它在 PM 视图里的那个弹框 */}
      {dialogOpen && <TaskEditDialog onClose={clearDetail} />}
    </div>
  );
}