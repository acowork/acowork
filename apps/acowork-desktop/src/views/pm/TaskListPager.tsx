/**
 * TaskListPager — 列表视图页脚的分页控件。
 *
 * 与看板列的分页（`KanbanColumn` 的 ‹ page/total ›）同一套交互语义，差别只在
 * 分页单元：列表按「根任务子树」装箱（见 `lib/pm-list-columns` 的
 * `paginatePmListGroups`），一页的行数是预算而不是精确值，所以页码文案报的是
 * 实际累计出来的行号（`page.startRow` / `page.endRow`），不是
 * `pageIndex * pageSize + 1`。
 *
 * 每页行数（`metadata.pmListPageSize`）也在这里改：只有页脚这一个入口，不另开
 * 设置项 —— 它在列表视图里随时可见，比藏在齿轮面板里好用。
 *
 * 纯展示组件：页码状态（`pageIndex`）在 `TaskListView`，每页行数在
 * `ProjectBoard`（和视图 / 列配置同一条乐观写 metadata 路径）。
 */

import { ChevronLeft, ChevronRight } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { Dropdown } from "../../components/common/Dropdown";
import { PM_LIST_PAGE_SIZE_OPTIONS } from "../../lib/pm-view-settings";
import type { PmListPage } from "../../lib/pm-list-columns";

interface TaskListPagerProps {
  page: PmListPage;
  /** 每页行数预算（`metadata.pmListPageSize`）。 */
  pageSize: number;
  onPageSizeChange: (size: number) => void;
  onPrev: () => void;
  onNext: () => void;
}

/**
 * 翻页箭头的按钮 class。
 *
 * 点击区用 `h-6 w-6`（24px）而不是贴着图标的 `p-0.5`（≈16px）—— 16px
 * 的命中框在页脚这种密集控件里很难点中。图标保持 12px，只把命中区放大。
 */
const ARROW_CN =
  "inline-flex h-6 w-6 items-center justify-center rounded transition-colors hover:bg-zinc-100 hover:text-text-secondary disabled:cursor-not-allowed disabled:opacity-30 dark:hover:bg-zinc-700";

export function TaskListPager({
  page,
  pageSize,
  onPageSizeChange,
  onPrev,
  onNext,
}: TaskListPagerProps) {
  const { t } = useTranslation();
  const hasRows = page.totalRows > 0;

  return (
    <span className="flex shrink-0 items-center gap-1.5">
      {hasRows && (
        <span className="tabular-nums">
          {t("pm.list.page.range", {
            start: page.startRow,
            end: page.endRow,
            total: page.totalRows,
          })}
        </span>
      )}
      {page.pageCount > 1 && (
        <span className="flex items-center gap-0.5">
          <button
            type="button"
            onClick={onPrev}
            disabled={page.pageIndex <= 0}
            aria-label={t("pm.board.pagePrev")}
            className={ARROW_CN}
          >
            <ChevronLeft className="h-3 w-3" aria-hidden />
          </button>
          <span className="tabular-nums">
            {page.pageIndex + 1}/{page.pageCount}
          </span>
          <button
            type="button"
            onClick={onNext}
            disabled={page.pageIndex >= page.pageCount - 1}
            aria-label={t("pm.board.pageNext")}
            className={ARROW_CN}
          >
            <ChevronRight className="h-3 w-3" aria-hidden />
          </button>
        </span>
      )}
      <span className="flex items-center gap-1">
        <span className="sr-only">{t("pm.list.page.size")}</span>
        {/* 24px 高 + `text-xs`（12px）：`h-5 text-10` 的组合在 14px 基准下
            只有 10px 字、20px 高，页脚那行几乎读不出来；12px 与表格
            标题、工具栏按钮同一档。走公共 Dropdown 而不是手写 <select>：
            原生 option 列表由 WebView2 绘制，无法用主题色着色。 */}
        <Dropdown
          value={String(pageSize)}
          onChange={(v) => onPageSizeChange(Number(v))}
          aria-label={t("pm.list.page.size")}
          size="small"
          className="h-6 w-auto rounded border-border-outer bg-page-bg px-1.5 text-xs text-text-secondary"
          options={PM_LIST_PAGE_SIZE_OPTIONS.map((n) => ({
            value: String(n),
            label: t("pm.list.page.perPage", { count: n }),
          }))}
        />
      </span>
    </span>
  );
}
