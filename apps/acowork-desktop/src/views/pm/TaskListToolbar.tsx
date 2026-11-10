/**
 * 列表视图工具栏 — 搜索 / 筛选 / 分组 / 字段。
 *
 * 只负责**投影与入口**：筛选值、分组值、弹层开关都由 `TaskListView` 持有
 * （弹层是同一套 `PmPopover`，同一时刻只开一个，所以状态必须集中在容器，
 * 否则两处入口会打开两个互相不知道对方存在的浮层）。
 *
 * 激活态：按钮有选中值时高亮并显示计数（`状态 2`），`清除筛选` 只在任一
 * 筛选生效时出现 —— 常驻一个点了没变化的按钮是噪音。
 */

import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import type { PmListFilters, PmListGroupBy } from "../../lib/pm-list-columns";

const BTN_CN =
  "inline-flex h-[26px] shrink-0 items-center gap-1 rounded-md border border-border-outer bg-panel-block px-2 text-11 text-text-secondary hover:bg-zinc-100 dark:hover:bg-zinc-800";

const BTN_ACTIVE_CN =
  "border-[var(--color-accent)]/40 bg-[var(--color-accent)]/10 text-[var(--color-accent)]";

export type ToolbarPopoverSpec =
  | { kind: "filter"; field: "status" | "priority" | "assignee" }
  | { kind: "group" }
  | { kind: "fields" };

interface TaskListToolbarProps {
  filters: PmListFilters;
  filtersActive: boolean;
  groupBy: PmListGroupBy;
  columnCount: number;
  onOpen: (spec: ToolbarPopoverSpec) => (e: React.MouseEvent<HTMLElement>) => void;
  onQueryChange: (q: string) => void;
  onClearFilters: () => void;
}

export function TaskListToolbar({
  filters,
  filtersActive,
  groupBy,
  columnCount,
  onOpen,
  onQueryChange,
  onClearFilters,
}: TaskListToolbarProps) {
  const { t } = useTranslation();
  const counts: Record<"status" | "priority" | "assignee", number> = {
    status: filters.status.length,
    priority: filters.priority.length,
    assignee: filters.assignee.length,
  };

  return (
    <div className="flex shrink-0 flex-wrap items-center gap-1.5 border-b border-border-divider px-3 py-1.5">
      <label className="relative flex h-[26px] min-w-[140px] flex-1 items-center sm:max-w-[220px]">
        <span className="pointer-events-none absolute left-2 text-text-tertiary" aria-hidden>
          ⌕
        </span>
        <input
          value={filters.query}
          onChange={(e) => onQueryChange(e.target.value)}
          placeholder={t("pm.list.searchPlaceholder")}
          aria-label={t("pm.list.searchPlaceholder")}
          className="h-full w-full rounded-md border border-border-outer bg-panel-block pl-6 pr-2 text-11 text-text outline-none placeholder:text-text-tertiary focus:border-[var(--color-accent)]"
        />
      </label>

      {(["status", "priority", "assignee"] as const).map((field) => (
        <button
          key={field}
          type="button"
          onClick={onOpen({ kind: "filter", field })}
          className={cn(BTN_CN, counts[field] > 0 && BTN_ACTIVE_CN)}
        >
          {t(`pm.list.col.${field}`)}
          {counts[field] > 0 && <span className="tabular-nums">{counts[field]}</span>}
          <span className="text-9 opacity-60" aria-hidden>
            ▾
          </span>
        </button>
      ))}

      {filtersActive && (
        <button
          type="button"
          onClick={onClearFilters}
          className="inline-flex h-[26px] shrink-0 items-center rounded-md px-2 text-11 text-[var(--color-accent)] hover:bg-zinc-100 dark:hover:bg-zinc-800"
        >
          {t("pm.list.clearFilters")}
        </button>
      )}

      <span className="mx-0.5 h-4 w-px shrink-0 bg-border-divider" />

      <button
        type="button"
        onClick={onOpen({ kind: "group" })}
        className={cn(BTN_CN, groupBy !== "none" && BTN_ACTIVE_CN)}
      >
        {t("pm.list.groupBy")}
        <span className="text-9 opacity-60" aria-hidden>
          ▾
        </span>
        <span className="max-w-[90px] truncate font-medium">
          {groupBy === "none" ? t("pm.list.group.none") : t(`pm.list.group.${groupBy}`)}
        </span>
      </button>

      <div className="ml-auto flex items-center gap-1.5">
        <span className="hidden text-10 text-text-tertiary md:inline">
          {t("pm.list.fields.count", { count: columnCount })}
        </span>
        <button type="button" onClick={onOpen({ kind: "fields" })} className={BTN_CN}>
          {t("pm.list.fields.title")}
          <span className="text-9 opacity-60" aria-hidden>
            ▾
          </span>
        </button>
      </div>
    </div>
  );
}
