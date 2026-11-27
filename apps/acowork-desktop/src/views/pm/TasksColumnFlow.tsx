/**
 * TasksColumnFlow — 四张可折叠状态卡（PM 看板四列）的任务流，两个窄面板共用。
 *
 * 宿主：
 * - 项目群聊右侧的 `GroupTasksPanel`（`projectId` 作用域）
 * - bot 单聊右侧的任务 tab（`AgentTasksPanel`，`assignee` 作用域）
 *
 * 两个宿主只差「数据从哪来」，卡片外观 / 数量徽章 / 分页 / 展开收起全部在
 * 这里，所以两处界面是同一份代码，不是照着画的第二套：
 * - 默认全部折叠（只留列头），点列头展开；展开后各列按内容高度排、各自内部
 *   滚动（虚拟化 + 分页逻辑照旧，单页数量由宿主给 `pageSize`）；
 * - 拖拽在这里没有意义（窄面板里纵向拖不出去），所以 `DndContext` 不带传感器
 *   —— 卡片只读，点击打开唯一的任务弹框 `TaskEditDialog`。
 *
 * 数据读 `usePmBoardStore`：`ready` 由宿主判断自己的作用域是否已经落到 store
 * 里（换项目 / 换 agent 的那一帧不能把上一份数据先画出来）。
 */

import { useCallback, useMemo, useState } from "react";
import { DndContext } from "@dnd-kit/core";
import { OverlayScrollbar } from "../../components/common/OverlayScrollbar";
import { usePmBoardStore } from "../../stores/pm/boardStore";
import { usePmTaskDetailStore } from "../../stores/pm/taskDetailStore";
import { BOARD_COLUMNS } from "../../lib/pm-types";
import { KanbanColumn } from "./KanbanColumn";
import { TaskEditDialog } from "./TaskEditDialog";
import type { PmTaskResponse, TaskStatus } from "../../lib/pm-types";

interface TasksColumnFlowProps {
  /** store 里的任务是否已经属于本面板的作用域。false = 只画空列（不显示别处的任务）。 */
  ready: boolean;
  /** 单页显示的列根任务数（项目级显示设置；无项目时给默认值）。 */
  pageSize: number;
  /** 页码归属：换作用域时页码归零（与 `KanbanBoard` 同一条约定）。 */
  scopeKey: string;
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

/** 无传感器：这个流程里没有拖拽（见文件头）。 */
const NO_SENSORS: [] = [];

export function TasksColumnFlow({ ready, pageSize, scopeKey }: TasksColumnFlowProps) {
  const tasks = usePmBoardStore((s) => s.tasks);
  const columnRoots = usePmBoardStore((s) => s.columnRoots);

  const openTask = usePmTaskDetailStore((s) => s.openTask);
  const dialogOpen = usePmTaskDetailStore((s) => s.frames.length > 0);
  const clearDetail = usePmTaskDetailStore((s) => s.clear);

  // 折叠状态按列各记一份：点哪张卡片就展开哪张。
  const [collapsed, setCollapsed] = useState<Partial<Record<TaskStatus, boolean>>>(ALL_COLLAPSED);
  const toggle = useCallback((status: TaskStatus) => {
    setCollapsed((prev) => ({ ...prev, [status]: !prev[status] }));
  }, []);

  // 页码归流程持有（列内部重置会在数据变化后丢掉"翻到某页"的能力），
  // 换作用域时归零。
  const [pageState, setPageState] = useState<{
    scopeKey: string;
    pages: Partial<Record<TaskStatus, number>>;
  }>(() => ({ scopeKey, pages: EMPTY_PAGES }));
  const pages = pageState.scopeKey === scopeKey ? pageState.pages : EMPTY_PAGES;
  const handlePageChange = useCallback(
    (status: TaskStatus, page: number) => {
      setPageState((prev) => ({
        scopeKey,
        pages: { ...(prev.scopeKey === scopeKey ? prev.pages : EMPTY_PAGES), [status]: page },
      }));
    },
    [scopeKey],
  );

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
    <DndContext sensors={NO_SENSORS}>
      {/* 每张卡按内容高度排（`KanbanColumn` 里是 `flex-none`），所以几张卡
          同时展开可能比面板高 —— 这一层负责整个面板滚动，卡片内部的高度
          上限由各列自己管。 */}
      <OverlayScrollbar fill="flex" className="p-2">
        <div className="flex flex-col gap-1.5">
          {BOARD_COLUMNS.map((col) => (
            <KanbanColumn
              key={`${scopeKey}-${col.status}`}
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

      {/* 任务的唯一表面：卡片点开就是它在 PM 视图里的那个弹框 */}
      {dialogOpen && <TaskEditDialog onClose={clearDetail} />}
    </DndContext>
  );
}