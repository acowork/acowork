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
import { cn } from "../../lib/utils";
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

const ARROW_CN =
  "rounded p-0.5 transition-colors hover:bg-zinc-100 hover:text-text-secondary disabled:cursor-not-allowed disabled:opacity-30 dark:hover:bg-zinc-700";

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
      <label className="flex items-center gap-1">
        <span className="sr-only">{t("pm.list.page.size")}</span>
        <select
          value={pageSize}
          onChange={(e) => onPageSizeChange(Number(e.target.value))}
          aria-label={t("pm.list.page.size")}
          className={cn(
            "h-5 rounded border border-border-outer bg-page-bg px-1 text-10 text-text-secondary",
            "outline-none focus:border-[var(--color-accent)]",
          )}
        >
          {PM_LIST_PAGE_SIZE_OPTIONS.map((n) => (
            <option key={n} value={n}>
              {t("pm.list.page.perPage", { count: n })}
            </option>
          ))}
        </select>
      </label>
    </span>
  );
}
