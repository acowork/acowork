/**
 * 列表视图的呈现常量（class 映射 + i18n 键映射）。
 *
 * 为什么单独一个文件：`TaskListView` 与 `TaskListRows` 都要用状态胶囊配色和
 * 状态 / 审核状态的标签键，放任何一边都会让另一边反向依赖对方的模块。
 *
 * 与 `lib/pm-list-columns` 的分工：那边只有**数据**（取值、宽度、对齐），
 * Tailwind class 一律不进 lib；这里是视图层的样式表。
 */

import type { ReviewStatus, TaskStatus } from "../../lib/pm-types";
import type { PmListColumn, PmColumnWidths } from "../../lib/pm-list-columns";

/** 行高（紧凑表格的唯一起算值；表头同高，滚动时 sticky 贴顶）。 */
export const ROW_HEIGHT = 34;
/** 行首复选框列宽（不参与字段配置，恒定第一列）。 */
export const CHECK_COL_WIDTH = 30;
/** 表头末尾「＋」（打开字段弹层）列宽。 */
export const ADD_COL_WIDTH = 36;
/** 子任务每层缩进。 */
export const INDENT_PER_DEPTH = 13;

/**
 * 状态 → Tailwind class 映射。
 *
 * 与看板列头状态点同一语义（`SubtaskTree` 的 `STATUS_STYLE` 是同一组配色的
 * 另一处投影）：项目里没有公共的状态色常量（`BOARD_COLUMNS` 只带
 * `i18nKey`），所以列表视图自己持一份 **Tailwind class** 映射，写法对齐
 * `PriorityBadge` 的 `STYLES` —— 不写死 hex，深浅色各一档。
 */
export const STATUS_PILL_CLASS: Record<TaskStatus, string> = {
  pending: "bg-zinc-100 text-zinc-700 dark:bg-zinc-800 dark:text-zinc-300",
  in_progress: "bg-blue-100 text-blue-700 dark:bg-blue-950/60 dark:text-blue-400",
  submitted: "bg-amber-100 text-amber-700 dark:bg-amber-950/60 dark:text-amber-400",
  done: "bg-green-100 text-green-700 dark:bg-green-950/60 dark:text-green-400",
  rejected: "bg-red-100 text-red-700 dark:bg-red-950/60 dark:text-red-400",
  cancelled: "bg-zinc-100 text-text-tertiary dark:bg-zinc-800",
};

/** 状态点配色（状态菜单里的图例，与胶囊同一组语义）。 */
export const STATUS_DOT_CLASS: Record<TaskStatus, string> = {
  pending: "bg-zinc-400",
  in_progress: "bg-blue-500",
  submitted: "bg-amber-500",
  done: "bg-green-500",
  rejected: "bg-red-500",
  cancelled: "bg-zinc-400",
};

export const STATUS_LABEL_KEY: Record<TaskStatus, string> = {
  pending: "pm.board.pending",
  in_progress: "pm.board.inProgress",
  submitted: "pm.board.submitted",
  done: "pm.board.done",
  rejected: "pm.board.rejected",
  cancelled: "pm.board.cancelled",
};

export const REVIEW_LABEL_KEY: Record<ReviewStatus, string> = {
  not_required: "pm.list.review.notRequired",
  pending: "pm.list.review.pending",
  approved: "pm.list.review.approved",
  rejected: "pm.list.review.rejected",
};

/** 表头单元格的公共 class（sticky + 发丝下沿 + 面板底色）。 */
export const TH_BASE_CN =
  "sticky top-0 z-[2] border-b border-border-divider bg-panel-block px-2 text-left font-medium text-text-tertiary hover:text-text";

/** 数据行的公共 class：hover 底色 + 发丝下沿 + 单行不换行。 */
export const TD_BASE_CN =
  "overflow-hidden whitespace-nowrap border-b border-border-soft px-2 text-text-secondary";

/**
 * 表格最小宽度估算（`table-layout: fixed` + `min-width` 让列很多时横向滚动，
 * 表头与表体在同一滚动容器里，sticky 表头贴顶）。
 *
 * `title` 列没有声明宽度，这里按 240px 估一个下限：它实际会吸收剩余空间，
 * 窄屏时至少保住这个宽度，不至于把标题压成三个字符。
 */
export function estimateTableWidth(
  columns: readonly PmListColumn[],
  widths: PmColumnWidths,
): number {
  const body = columns.reduce((sum, c) => {
    const explicit = widths[c.key] ?? c.width;
    return sum + (explicit ?? 240);
  }, 0);
  return CHECK_COL_WIDTH + body + ADD_COL_WIDTH;
}
