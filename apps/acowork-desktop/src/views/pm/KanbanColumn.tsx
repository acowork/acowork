/**
 * KanbanColumn — 看板列（T2-3 + T2-9 + T2-15 虚拟化）。
 *
 * 对齐 UX 设计 §3.3：
 * - useDroppable：整列作为拖放目标（拖到列空白 = 状态流转）
 * - 列头：状态名 + 数量徽章（列根数量）
 * - 父子树：深度优先扁平化为行数组（depth 缩进），用 @tanstack/react-virtual 虚拟化长列表
 * - 拖拽进行中回退全量渲染：保证所有 draggable/droppable 节点挂载，dnd-kit 正常命中
 * - 空状态：pm.board.empty
 */

import { useMemo, useRef, useState } from "react";
import { useDndMonitor, useDroppable } from "@dnd-kit/core";
import { useVirtualizer } from "@tanstack/react-virtual";
import { useTranslation } from "../../i18n/useTranslation";
import { usePmBoardStore } from "../../stores/pm/boardStore";
import { usePmHealthStore } from "../../stores/pm/healthStore";
import { TaskCard } from "./TaskCard";
import { OverlayScrollbar } from "../../components/common/OverlayScrollbar";
import { cn } from "../../lib/utils";
import type { PmTaskResponse, TaskStatus } from "../../lib/pm-types";

interface KanbanColumnProps {
  status: TaskStatus;
  i18nKey: string;
  tasks: PmTaskResponse[];
  onOpenTask: (taskId: string) => void;
  /** 拖拽进行中且目标列为空时展示空态提示 */
  isOver?: boolean;
}

/** 扁平化行：任务 + 其在列内树中的深度 */
interface FlatRow {
  task: PmTaskResponse;
  depth: number;
}

export function KanbanColumn({ status, i18nKey, tasks, onOpenTask, isOver }: KanbanColumnProps) {
  const { t } = useTranslation();
  const healthy = usePmHealthStore((s) => s.healthy);
  const childrenOf = usePmBoardStore((s) => s.childrenOf);
  const { setNodeRef, isOver: dropOver } = useDroppable({
    id: `column-${status}`,
    data: { type: "column", status },
    disabled: healthy === false,
  });

  const active = isOver ?? dropOver;

  // 拖拽进行中 → 全量渲染（避免虚拟化卸载 draggable/droppable 节点导致 dnd 失效）
  const [isDragging, setIsDragging] = useState(false);
  useDndMonitor({
    onDragStart: () => setIsDragging(true),
    onDragEnd: () => setIsDragging(false),
    onDragCancel: () => setIsDragging(false),
  });

  // 深度优先遍历：列根 → 同列子任务（保持树序）
  const flatRows = useMemo<FlatRow[]>(() => {
    const rows: FlatRow[] = [];
    const walk = (task: PmTaskResponse, depth: number) => {
      rows.push({ task, depth });
      for (const child of childrenOf(task.id)) {
        if (child.status === status) walk(child, depth + 1);
      }
    };
    for (const task of tasks) walk(task, 0);
    return rows;
  }, [tasks, childrenOf, status]);

  const scrollRef = useRef<HTMLDivElement>(null);
  const virtualizer = useVirtualizer({
    count: flatRows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 64,
    overscan: 6,
    getItemKey: (index) => flatRows[index].task.id,
  });

  return (
    <section
      ref={setNodeRef}
      className={cn(
        "flex min-h-0 min-w-0 flex-1 flex-col rounded-md border border-border-outer bg-zinc-50/70  dark:bg-zinc-900/40",
        active && "border-[var(--color-accent)] bg-[var(--color-accent)]/5",
      )}
      aria-label={t(i18nKey)}
    >
      {/* 列头 */}
      <header className="flex shrink-0 items-center gap-1.5 px-2.5 py-2">
        <h3 className="text-11 font-semibold uppercase tracking-wide text-text-secondary ">
          {t(i18nKey)}
        </h3>
        <span
          className={cn(
            "rounded-full px-1.5 py-px text-10 font-medium tabular-nums",
            status === "submitted"
              ? "bg-amber-100 text-amber-700 dark:bg-amber-900/50 dark:text-amber-300"
              : "bg-zinc-200 text-text-secondary dark:bg-zinc-700 ",
          )}
        >
          {tasks.length}
        </span>
      </header>

      {/* 任务列表 */}
      {flatRows.length === 0 ? (
        <div className="min-h-0 flex-1 px-2 pb-2">
          <div
            className={cn(
              "flex h-16 items-center justify-center rounded-md border border-dashed text-10 text-text-tertiary ",
              active && "border-[var(--color-accent)] text-[var(--color-accent)]",
            )}
          >
            {active ? t("pm.board.dropHint") ?? "" : t("pm.board.empty")}
          </div>
        </div>
      ) : isDragging ? (
        // 拖拽中：全量渲染
        <OverlayScrollbar fill="flex" className="px-2 pb-2">
          <div className="space-y-1.5">
            {flatRows.map((row) => (
              <TaskCard
                key={row.task.id}
                task={row.task}
                onOpenTask={onOpenTask}
                depth={row.depth}
              />
            ))}
          </div>
        </OverlayScrollbar>
      ) : (
        // 虚拟化渲染
        <OverlayScrollbar fill="flex" viewportRef={scrollRef} className="px-2 pb-2">
          <div style={{ height: virtualizer.getTotalSize(), position: "relative" }}>
            {virtualizer.getVirtualItems().map((vi) => {
              const row = flatRows[vi.index];
              return (
                <div
                  key={row.task.id}
                  data-index={vi.index}
                  ref={virtualizer.measureElement}
                  className="absolute left-0 top-0 w-full"
                  style={{ transform: `translateY(${vi.start}px)` }}
                >
                  <div className="pb-1.5">
                    <TaskCard
                      task={row.task}
                      onOpenTask={onOpenTask}
                      depth={row.depth}
                    />
                  </div>
                </div>
              );
            })}
          </div>
        </OverlayScrollbar>
      )}
    </section>
  );
}
