/**
 * KanbanColumn — 看板列（T2-3 + T2-9 + T2-15 虚拟化 + 分页）。
 *
 * 对齐 UX 设计 §3.3：
 * - useDroppable：整列作为拖放目标（拖到列空白 = 状态流转）
 * - 列头：状态名 + 数量徽章（列根数量），下沿发丝线 —— 与右侧设置面板
 *   的 pane 头同一语法（面板外壳 + 头部发丝线 + 卡片流）
 * - 父子树：深度优先扁平化为行数组（depth 缩进），用 @tanstack/react-virtual 虚拟化长列表
 * - 分页：按列根任务切页（子任务随父任务整棵展示，不拆散树），
 *   单页数量来自项目 metadata；列底区间 + 上下页仅在有下一页时出现
 * - 拖拽进行中回退全量渲染：保证所有 draggable/droppable 节点挂载，dnd-kit 正常命中
 * - 空状态：pm.board.empty
 * - 可折叠（`onToggleCollapsed`）：窄容器里（群聊右侧任务面板）4 列纵向排列，
 *   默认只留列头，点列头展开。不传这两个 props 时渲染与看板完全一致。
 * - `variant="card"`：列本身画成"设置卡片"（`ListBox` box 那一套）——标题条
 *   `panel-block` + 正文 `panel-inset`，圆角 md，而不是整列一片 `right-panel`。
 *   看板的列是铺满列宽的面板，只有一个底色；侧栏里列要和面板撞色时才用 card。
 */

import { useEffect, useMemo, useRef, useState } from "react";
import { useDndMonitor, useDroppable } from "@dnd-kit/core";
import { useVirtualizer } from "@tanstack/react-virtual";
import { ChevronLeft, ChevronRight } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { usePmBoardStore } from "../../stores/pm/boardStore";
import { usePmHealthStore } from "../../stores/pm/healthStore";
import { TaskCard } from "./TaskCard";
import { OverlayScrollbar } from "../../components/common/OverlayScrollbar";
import {
  CAPSULE_PANE_CN,
  SURFACE_CONTENT_CN,
  SURFACE_PANEL_CN,
} from "../../components/common/capsule";
import { cn } from "../../lib/utils";
import { boardPageBounds, boardPageSlice } from "../../lib/pm-board-settings";
import { COUNT_BADGE_BASE_CN } from "../../components/common/sidebarCountBadge";
import { STATUS_PILL_CLASS } from "./taskListLabels";
import type { PmTaskResponse, TaskStatus } from "../../lib/pm-types";

interface KanbanColumnProps {
  status: TaskStatus;
  i18nKey: string;
  tasks: PmTaskResponse[];
  onOpenTask: (taskId: string) => void;
  /** 单页显示的列根任务数（项目级显示设置） */
  pageSize: number;
  /** 当前页码（1-indexed）；由看板持有，便于拖动后揭示卡片 */
  page: number;
  onPageChange: (page: number) => void;
  /** 拖拽进行中且目标列为空时展示空态提示 */
  isOver?: boolean;
  /** 折叠（只渲染列头）。默认 `false` = 看板的常开列。 */
  collapsed?: boolean;
  /**
   * `pane`（默认）= 看板的列：整列一片 `bg-right-panel`。
   * `card` = 设置卡片语法：`rounded-md` 外壳 + 标题条 `panel-block` +
   * 正文 `panel-inset`，落在 `bg-right-panel` 的面板上才有层次。
   */
  variant?: "pane" | "card";
  /** 给了就变可折叠列：列头成为展开/收起按钮。 */
  onToggleCollapsed?: () => void;
}

/**
 * 可折叠列（侧栏里的状态卡片）展开后的高度上限：内容少就按内容高度长，
 * 超过就卡内滚动。看板列不受影响（它们由看板均分列宽列高）。
 *
 * 用 vh 而不是父容器百分比：这一档的父链高度是按内容算的（卡片要"贴着内容"），
 * 百分比 max-height 在不定高包含块上会被当成 none —— 等于没上限。
 */
const FOLDED_COLUMN_BODY_MAX_HEIGHT = "60vh";

/** 扁平化行：任务 + 其在列内树中的深度 */
interface FlatRow {
  task: PmTaskResponse;
  depth: number;
}

export function KanbanColumn({
  status,
  i18nKey,
  tasks,
  onOpenTask,
  pageSize,
  page,
  onPageChange,
  isOver,
  collapsed = false,
  onToggleCollapsed,
  variant = "pane",
}: KanbanColumnProps) {
  const { t } = useTranslation();
  // 设置卡片语法：标题条 = 外壳那一层（`panel-block`），正文下沉一档
  // （`panel-inset`）+ 顶沿发丝线。与 `ListBox` box / `ExpandableRow` 一致。
  const card = variant === "card";
  const bodySurface = card ? SURFACE_CONTENT_CN : "";
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

  // 分页按列根任务切（整棵子树跟着父任务走），避免翻页把父子拆散。
  // 页码归看板持有：删除 / 拖动 / 修改单页数量后越界，这里收敛后上报。
  const bounds = boardPageBounds(tasks.length, pageSize, page);
  useEffect(() => {
    if (bounds.page !== page) onPageChange(bounds.page);
  }, [bounds.page, page, onPageChange]);

  const pageRoots = useMemo(
    () => boardPageSlice(tasks, pageSize, bounds.page),
    [tasks, pageSize, bounds.page],
  );

  // 深度优先遍历：列根 → 同列子任务（保持树序）
  const flatRows = useMemo<FlatRow[]>(() => {
    const rows: FlatRow[] = [];
    const walk = (task: PmTaskResponse, depth: number) => {
      rows.push({ task, depth });
      for (const child of childrenOf(task.id)) {
        if (child.status === status) walk(child, depth + 1);
      }
    };
    for (const task of pageRoots) walk(task, 0);
    return rows;
  }, [pageRoots, childrenOf, status]);

  const scrollRef = useRef<HTMLDivElement>(null);

  // 翻页后回到顶部：上一页的滚动位置对新一页没有意义。
  useEffect(() => {
    if (scrollRef.current) scrollRef.current.scrollTop = 0;
  }, [bounds.page]);

  const virtualizer = useVirtualizer({
    count: flatRows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => 64,
    overscan: 6,
    getItemKey: (index) => flatRows[index].task.id,
  });

  // 列头计数胶囊穿状态色 —— 与列表模式的状态胶囊同一套 token（STATUS_PILL_CLASS），
  // 四列一眼看出是哪种状态；白字靠 `--color-pill-*` 的加深档。
  const countBadge = cn(COUNT_BADGE_BASE_CN, STATUS_PILL_CLASS[status], "text-white");

  return (
    <section
      ref={setNodeRef}
      className={cn(
        // pane（看板）：列 = 面板（不是卡片），复用设置面板同一个 pane 外壳
        // CAPSULE_PANE_CN（rounded-xl + 外发丝线 + overflow-hidden）+ right-panel
        // 底色；里面的任务才是 ListBox 卡片（panel-block + panel-inset 正文）。
        // card（窄侧栏）：列自己就是那张设置卡片，标题条那一层当外壳。
        card
          ? "flex min-h-0 flex-col overflow-hidden rounded-md border border-border-outer bg-panel-block"
          : cn(CAPSULE_PANE_CN, SURFACE_PANEL_CN),
        // 可折叠列在纵向容器里都按内容高度排（`flex-none`）：折叠 = 只剩列头，
        // 展开 = 列头 + 内容，超过上限的那部分由正文自己的滚动区吃掉 ——
        // 不能写 `flex-1`，那会把列撑成均分的高度，内容少时留下一大片空白。
        onToggleCollapsed ? "flex-none" : "min-w-0 flex-1",
        active && "border-[var(--color-accent)] bg-[var(--color-accent)]/5",
      )}
      aria-label={t(i18nKey)}
    >
      {/* 列头 —— 与设置面板头部同一关系：一行标题 + 下沿发丝线。可折叠时整行
          是按钮（chevron 指示状态，命中区就是标题栏）；折叠时不画那条发丝线，
          卡片外框本来就在同一行上，两条紧贴会看起来像一根加粗的线。 */}
      <header
        className={cn(
          "flex shrink-0 items-center gap-1.5 px-2.5 py-2",
          !collapsed && "border-b border-border-divider",
        )}
      >
        {onToggleCollapsed ? (
          <button
            type="button"
            onClick={onToggleCollapsed}
            aria-expanded={!collapsed}
            className="flex min-w-0 flex-1 items-center gap-1.5 text-left"
          >
            <ChevronRight
              className={cn(
                "h-3 w-3 shrink-0 text-text-tertiary transition-transform",
                !collapsed && "rotate-90",
              )}
              aria-hidden
            />
            <h3 className="min-w-0 flex-1 truncate text-11 font-medium text-text-secondary ">
              {t(i18nKey)}
            </h3>
            <span className={countBadge}>
              {tasks.length}
            </span>
          </button>
        ) : (
          <>
            <h3 className="min-w-0 flex-1 truncate text-11 font-medium text-text-secondary ">
              {t(i18nKey)}
            </h3>
            <span className={countBadge}>
              {tasks.length}
            </span>
          </>
        )}
      </header>

      {/* 任务列表 */}
      {collapsed ? null : flatRows.length === 0 ? (
        <div className={cn("min-h-0 flex-1 p-2", bodySurface)}>
          <div
            className={cn(
              "flex h-16 items-center justify-center rounded-md border border-dashed border-border-outer text-10 text-text-tertiary ",
              active && "border-[var(--color-accent)] text-[var(--color-accent)]",
            )}
          >
            {active ? t("pm.board.dropHint") ?? "" : t("pm.board.empty")}
          </div>
        </div>
      ) : isDragging ? (
        // 拖拽中：全量渲染
        <OverlayScrollbar
          fill="flex"
          maxHeight={onToggleCollapsed ? FOLDED_COLUMN_BODY_MAX_HEIGHT : undefined}
          className={cn("p-2", bodySurface)}
        >
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
        <OverlayScrollbar
          fill="flex"
          viewportRef={scrollRef}
          maxHeight={onToggleCollapsed ? FOLDED_COLUMN_BODY_MAX_HEIGHT : undefined}
          className={cn("p-2", bodySurface)}
        >
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

      {/* 分页条：只有真的超过一页才出现，避免给短列增加噪音 */}
      {!collapsed && bounds.totalPages > 1 && (
        <footer
          className={cn(
            "flex shrink-0 items-center justify-between gap-2 border-t border-border-divider px-2 py-1 text-10 text-text-tertiary",
            bodySurface,
          )}
        >
          <span className="truncate tabular-nums" aria-live="polite">
            {t("pm.board.pageRange", {
              start: bounds.start,
              end: bounds.end,
              total: tasks.length,
            })}
          </span>
          <span className="flex shrink-0 items-center gap-0.5">
            <button
              type="button"
              onClick={() => onPageChange(Math.max(1, bounds.page - 1))}
              disabled={bounds.page <= 1}
              aria-label={t("pm.board.pagePrev")}
              className="rounded p-0.5 transition-colors hover:bg-[var(--color-accent)]/10 hover:text-text-secondary disabled:cursor-not-allowed disabled:opacity-30 dark:hover:bg-[var(--color-accent)]/15"
            >
              <ChevronLeft className="h-3 w-3" aria-hidden />
            </button>
            <span className="px-0.5 tabular-nums">
              {bounds.page}/{bounds.totalPages}
            </span>
            <button
              type="button"
              onClick={() => onPageChange(Math.min(bounds.totalPages, bounds.page + 1))}
              disabled={bounds.page >= bounds.totalPages}
              aria-label={t("pm.board.pageNext")}
              className="rounded p-0.5 transition-colors hover:bg-[var(--color-accent)]/10 hover:text-text-secondary disabled:cursor-not-allowed disabled:opacity-30 dark:hover:bg-[var(--color-accent)]/15"
            >
              <ChevronRight className="h-3 w-3" aria-hidden />
            </button>
          </span>
        </footer>
      )}
    </section>
  );
}
