/**
 * 列表视图的呈现常量（class 映射 + i18n 键映射）。
 *
 * 为什么单独一个文件：`TaskListView` 与 `TaskListRows` 都要用状态胶囊配色和
 * 状态 / 审核状态的标签键，放任何一边都会让另一边反向依赖对方的模块。
 *
 * 与 `lib/pm-list-columns` 的分工：那边只有**数据**（取值、宽度、对齐），
 * Tailwind class 一律不进 lib；这里是视图层的样式表。
 */

import { SURFACE_HEAD_CN } from "../../components/common/capsule";
import type { ReviewStatus, TaskStatus } from "../../lib/pm-types";
import type { PmListColumn, PmColumnWidths } from "../../lib/pm-list-columns";

/** 行高（紧凑表格的唯一起算值；表头同高，滚动时 sticky 贴顶）。 */
export const ROW_HEIGHT = 34;
/**
 * 行首复选框列宽（不参与字段配置，恒定第一列）。
 *
 * 「左内边距 12px + 复选框 14px」= 26px：12px 让列表的第一列与上方工具栏
 * 的 `px-3` 左右对齐（工具栏、页脚都是 12px 边距，列表窄一点就会显得
 * 整个版面左边贴边、右边却空着一大截）。列再宽一点，复选框与任务标题之间
 * 就多出一段看不懂的空白。
 */
export const CHECK_COL_WIDTH = 26;
/** 子任务每层缩进。 */
export const INDENT_PER_DEPTH = 13;

/**
 * 状态 → Tailwind class 映射。
 *
 * 实底胶囊 + 白字（`--color-pill-*`，见 globals.css 的 solid pill fills 段）：
 * 色相仍是全局高亮色体系里的 rose / amber / blue / emerald，只是加深到白字
 * 在深浅两套主题下都 ≥4.5:1。中性档（pending / cancelled）用 zinc —— 这两档
 * 没有颜色语义，硬塞进红绿蓝只会让人误读成「失败」或「进行中」。
 *
 * 与看板列头状态点同一语义（`SubtaskTree` 的 `STATUS_STYLE` 是同一组配色的
 * 另一处投影）：项目里没有公共的状态色常量（`BOARD_COLUMNS` 只带
 * `i18nKey`），所以列表视图自己持一份 **Tailwind class** 映射，写法对齐
 * `PriorityBadge` 的 `STYLES`。
 */
export const STATUS_PILL_CLASS: Record<TaskStatus, string> = {
  pending: "bg-pill-neutral",
  in_progress: "bg-pill-blue",
  submitted: "bg-pill-amber",
  done: "bg-pill-green",
  rejected: "bg-pill-red",
  cancelled: "bg-pill-neutral",
};

/** 胶囊内的小圆点：底色已经是同一族的颜色，点必须反白才看得见。 */
export const STATUS_DOT_CLASS = "bg-white/70";

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

/**
 * 表头单元格的公共 class（sticky + 发丝下沿 + 标题底色）。
 *
 * 底色是 `SURFACE_HEAD_CN`（第四级阶梯里的"内容标题带"，与看板卡片的标题条
 * 同档），行/表体是 `SURFACE_CONTENT_CN`（卡片正文那一档）：表头是一条标题
 * 带，下面一片数据是它的内容。表头必须不透明，否则滚动的数据行会从 sticky
 * 表头下面透出来。
 *
 * `align-middle`：表格单元格的默认 `vertical-align` 是 baseline，基线对齐
 * 会让行内的表单控件（复选框）挂在文字基线上而不是行心，于是表头那枚
 * 全选框看上去贴着上边框、和下面的数据行复选框也不在同一条水平线上。
 * 显式居中，表头与数据行才共用同一条中线。
 */
export const TH_BASE_CN = `sticky top-0 z-[2] align-middle border-b border-border-divider ${SURFACE_HEAD_CN} px-2 text-left font-medium text-text-tertiary hover:text-text`;

/**
 * 数据行的公共 class：hover 底色 + 发丝下沿 + 单行不换行。
 *
 * 行分隔线用 `--color-border-divider`（内部分隔线的既有 token），**不是**
 * `border-border-soft`：后者在本项目里没有声明，对应的自定义属性取不到值
 * 时 `border-color` 会回落到 `currentColor` —— 也就是行文字色，于是每条
 * 行分隔线都变成一条高对比亮线。
 */
export const TD_BASE_CN =
  "overflow-hidden whitespace-nowrap align-middle border-b border-border-divider px-2 text-text-secondary";

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
  return CHECK_COL_WIDTH + body;
}
