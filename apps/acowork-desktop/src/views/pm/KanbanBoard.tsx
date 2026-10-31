/**
 * KanbanBoard — 看板主体（T2-3 + T2-8）。
 *
 * 对齐 UX 设计 §3.3：
 * - DndContext 包裹 4 列（pending/in_progress/submitted/done）
 * - PointerSensor 拖动：列间流转 → boardStore.moveTask（乐观更新）
 * - KeyboardSensor 键盘拖动：←/→ 跨列、↑/↓ 列内（自定义坐标策略）
 * - 同列落点去重：避免无意义 PATCH
 * - DragOverlay 跟随 + 高亮目标列
 * - 分页：每列独立页码由看板持有（列自己重置会丢页），拖放落定后
 *   自动翻到卡片所在页 —— 否则卡片会掉进看不见的那一页
 */

import { useCallback, useMemo, useState } from "react";
import {
  DndContext,
  DragOverlay,
  KeyboardSensor,
  PointerSensor,
  closestCorners,
  useSensor,
  useSensors,
  type DragEndEvent,
  type DragStartEvent,
  type DragOverEvent,
  type KeyboardCoordinateGetter,
} from "@dnd-kit/core";
import { usePmBoardStore } from "../../stores/pm/boardStore";
import { usePmHealthStore } from "../../stores/pm/healthStore";
import { useTranslation } from "../../i18n/useTranslation";
import { KanbanColumn } from "./KanbanColumn";
import { TaskCard } from "./TaskCard";
import { BOARD_COLUMNS } from "../../lib/pm-types";
import { boardPageForTask } from "../../lib/pm-board-settings";
import type { Coordinates } from "@dnd-kit/utilities";
import type { PmTaskResponse, TaskStatus } from "../../lib/pm-types";

interface KanbanBoardProps {
  projectId: string;
  /** 单页显示的列根任务数（项目 metadata.boardPageSize） */
  pageSize: number;
  onOpenTask: (taskId: string) => void;
}

/** 键盘坐标步进：水平约一列宽、垂直约一张卡高 */
const KEYBOARD_STEP_X = 280;
const KEYBOARD_STEP_Y = 96;

/** 空页码表：模块级常量，保证「全在第 1 页」时引用稳定 */
const EMPTY_PAGES: Partial<Record<TaskStatus, number>> = {};

/** 自定义键盘坐标策略：←/→ 跨列、↑/↓ 列内 */
const boardKeyboardCoordinates: KeyboardCoordinateGetter = (event, { currentCoordinates }) => {
  let deltaX = 0;
  let deltaY = 0;
  switch (event.code) {
    case "ArrowRight":
      deltaX = KEYBOARD_STEP_X;
      break;
    case "ArrowLeft":
      deltaX = -KEYBOARD_STEP_X;
      break;
    case "ArrowUp":
      deltaY = -KEYBOARD_STEP_Y;
      break;
    case "ArrowDown":
      deltaY = KEYBOARD_STEP_Y;
      break;
    default:
      return undefined;
  }
  const next: Coordinates = {
    x: currentCoordinates.x + deltaX,
    y: currentCoordinates.y + deltaY,
  };
  return next;
};

export function KanbanBoard({ projectId, pageSize, onOpenTask }: KanbanBoardProps) {
  const { t } = useTranslation();
  const columnRoots = usePmBoardStore((s) => s.columnRoots);
  // ⚠️ 不能在 selector 里建新对象 — React 18 + zustand v5 的
  // useSyncExternalStore 用 Object.is 比较 snapshot，每次返回新 Map
  // 会触发 "getSnapshot should be cached" + 无限重渲染
  // （修 T2 死循环 bug：原写法让 KanbanBoard 抛 Maximum update depth）。
  const tasks = usePmBoardStore((s) => s.tasks);
  const tasksById = useMemo(() => {
    const byId = new Map<string, PmTaskResponse>();
    for (const task of tasks) byId.set(task.id, task);
    return byId;
  }, [tasks]);
  const healthy = usePmHealthStore((s) => s.healthy);
  const offline = healthy === false;

  const [activeId, setActiveId] = useState<string | null>(null);
  const [overColumn, setOverColumn] = useState<TaskStatus | null>(null);

  // 每列独立页码（1-indexed）。页码归看板持有：列内部重置会在拖放落定后
  // 丢掉「翻到卡片所在页」的能力。切项目时按 projectId 归零。
  const [pageState, setPageState] = useState<{
    projectId: string;
    pages: Partial<Record<TaskStatus, number>>;
  }>(() => ({ projectId, pages: EMPTY_PAGES }));
  const pages = pageState.projectId === projectId ? pageState.pages : EMPTY_PAGES;

  const handlePageChange = useCallback(
    (status: TaskStatus, page: number) => {
      setPageState((prev) => {
        const base = prev.projectId === projectId ? prev.pages : EMPTY_PAGES;
        if (base[status] === page) return prev; // 同引用 → 不惊动订阅者
        return { projectId, pages: { ...base, [status]: page } };
      });
    },
    [projectId],
  );

  /**
   * 翻到某任务所在页。页码由 [`boardPageForTask`] 算（子任务跟随父任务），
   * 任务还没落定到目标列时不做任何事。
   */
  const revealTask = useCallback(
    (taskId: string, status: TaskStatus) => {
      const store = usePmBoardStore.getState();
      const page = boardPageForTask(
        store.tasks,
        store.columnRoots(status),
        taskId,
        status,
        pageSize,
      );
      if (page !== null) handlePageChange(status, page);
    },
    [handlePageChange, pageSize],
  );

  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 6 } }),
    useSensor(KeyboardSensor, { coordinateGetter: boardKeyboardCoordinates }),
  );

  const activeTask = useMemo(
    () => (activeId ? tasksById.get(activeId) ?? null : null),
    [activeId, tasksById],
  );

  /** 从 over 对象推断目标列或目标任务 */
  const overInfo = useCallback(
    (over: { data: { current: unknown } } | null):
      | { kind: "column"; status: TaskStatus }
      | { kind: "task"; taskId: string; status: TaskStatus }
      | null => {
      if (!over) return null;
      const data = over.data.current as
        | { type?: string; status?: TaskStatus; taskId?: string }
        | undefined;
      if (data?.type === "column" && data.status) return { kind: "column", status: data.status };
      if (data?.type === "task" && data.taskId && data.status) {
        return { kind: "task", taskId: data.taskId, status: data.status };
      }
      return null;
    },
    [],
  );

  /** 循环防护：target 是否为 task 自身或其后代 */
  const isSelfOrDescendant = useCallback(
    (taskId: string, targetId: string): boolean => {
      if (taskId === targetId) return true;
      const byId = tasksById;
      const visited = new Set<string>();
      const stack = [targetId];
      while (stack.length > 0) {
        const cur = stack.pop()!;
        if (visited.has(cur)) continue;
        visited.add(cur);
        const task = byId.get(cur);
        if (!task) continue;
        if (task.parent_id === taskId) return true;
        if (task.parent_id) stack.push(task.parent_id);
      }
      return false;
    },
    [tasksById],
  );

  const handleDragStart = (e: DragStartEvent) => {
    if (offline) return;
    setActiveId(String(e.active.id));
  };

  const handleDragOver = (e: DragOverEvent) => {
    const info = overInfo(e.over);
    setOverColumn(info?.kind === "column" ? info.status : info?.status ?? null);
  };

  const handleDragEnd = (e: DragEndEvent) => {
    const { active, over } = e;
    setActiveId(null);
    setOverColumn(null);
    const info = overInfo(over);
    if (!info) return;
    const taskId = String(active.id);
    const store = usePmBoardStore.getState();

    if (info.kind === "task") {
      // Reparent：拖到某张卡片上 → 成为其子任务（同时流转到该列）
      if (isSelfOrDescendant(taskId, info.taskId)) return; // 循环防护
      const prev = tasksById.get(taskId);
      // 已是其直接子任务且同列 → 无操作
      if (prev && prev.parent_id === info.taskId && prev.status === info.status) return;
      void store.reparentTask(taskId, info.taskId).then(() => {
        // reparent 后状态随目标列流转
        const cur = usePmBoardStore.getState().tasks.find((t) => t.id === taskId);
        if (cur && cur.status !== info.status) {
          void usePmBoardStore.getState().moveTask(taskId, info.status).then((ok) => {
            if (ok) revealTask(taskId, info.status);
          });
        } else {
          // 已是目标列状态：卡片躲在父任务那一页下面也要翻出来
          revealTask(taskId, info.status);
        }
      });
      return;
    }

    // 拖到列空白 → 状态流转（保持父级关系不变）
    const task = tasksById.get(taskId);
    if (task && task.status === info.status) return; // 同列去重
    void store.moveTask(taskId, info.status).then((ok) => {
      if (ok) revealTask(taskId, info.status);
    });
  };

  const handleDragCancel = () => {
    setActiveId(null);
    setOverColumn(null);
  };

  return (
    <DndContext
      sensors={sensors}
      collisionDetection={closestCorners}
      onDragStart={handleDragStart}
      onDragOver={handleDragOver}
      onDragEnd={handleDragEnd}
      onDragCancel={handleDragCancel}
      accessibility={{
        announcements: {
          onDragStart: ({ active }) => `Picked up task ${String(active.id)}.`,
          onDragOver: ({ active, over }) =>
            over
              ? `Dragging task ${String(active.id)} over ${String(over.id)}.`
              : `Dragging task ${String(active.id)}.`,
          onDragEnd: ({ active, over }) =>
            over
              ? `Task ${String(active.id)} dropped over ${String(over.id)}.`
              : `Task ${String(active.id)} dropped.`,
          onDragCancel: ({ active }) => `Dragging was cancelled. Task ${String(active.id)}.`,
        },
      }}
    >
      <div className="flex h-full flex-col">
        <div className="flex min-h-0 flex-1 gap-3 p-3">
          {BOARD_COLUMNS.map((col) => (
            <KanbanColumn
              // key 带 projectId：切换项目时重建列，页码回到第 1 页
              key={`${projectId}-${col.status}`}
              status={col.status}
              i18nKey={col.i18nKey}
              tasks={columnRoots(col.status)}
              onOpenTask={onOpenTask}
              pageSize={pageSize}
              page={pages[col.status] ?? 1}
              onPageChange={(next) => handlePageChange(col.status, next)}
              isOver={overColumn === col.status}
            />
          ))}
        </div>
        <p className="px-3 pb-2 text-10 text-text-tertiary ">
          {t("pm.board.dragHint")} · {projectId}
        </p>
      </div>

      <DragOverlay>
        {activeTask ? (
          <TaskCard
            task={activeTask}
            onOpenTask={() => {}}
            className="rotate-2 cursor-grabbing shadow-lg"
          />
        ) : null}
      </DragOverlay>
    </DndContext>
  );
}
