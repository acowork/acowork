/**
 * 批量操作条 — 选中行后浮出的底部条。
 *
 * 浮层而不是表格内一行：批量条属于"当前选择"这个临时上下文，占表格行会
 * 让行数与状态栏计数对不上，也会跟着表格滚动。
 *
 * 三个动作（状态 / 优先级 / 负责人）都走 `TaskListView` 里的同一条批量
 * 路径：状态用 `moveTask`（看板同一套乐观 + 回滚），其余用 PATCH。
 * 离线时整条不出现 —— 选择本身仍然可用（视图状态），只是没有写入口。
 */

import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";

const BTN_CN =
  "inline-flex h-6 shrink-0 items-center gap-1 rounded-md border border-border-outer bg-panel-block px-2 text-11 text-text-secondary hover:bg-zinc-100 dark:hover:bg-zinc-800";

export type BulkField = "status" | "priority" | "assignee";

interface TaskListBulkBarProps {
  count: number;
  onOpen: (field: BulkField) => (e: React.MouseEvent<HTMLElement>) => void;
  onClear: () => void;
}

export function TaskListBulkBar({ count, onOpen, onClear }: TaskListBulkBarProps) {
  const { t } = useTranslation();
  return (
    <div
      className={cn(
        "pointer-events-auto absolute bottom-4 left-1/2 z-10 flex -translate-x-1/2 items-center gap-1.5",
        "rounded-lg border border-border-outer bg-modal-surface px-2.5 py-1.5 shadow-lg",
      )}
      role="toolbar"
      aria-label={t("pm.list.bulkTitle")}
    >
      <span className="shrink-0 text-11 font-medium text-text">
        {t("pm.list.bulkSelected", { count })}
      </span>
      <span className="mx-1 h-4 w-px shrink-0 bg-border-divider" />
      <button type="button" onClick={onOpen("status")} className={BTN_CN}>
        {t("pm.list.col.status")}
        <span className="text-9 opacity-60" aria-hidden>
          ▾
        </span>
      </button>
      <button type="button" onClick={onOpen("priority")} className={BTN_CN}>
        {t("pm.list.col.priority")}
        <span className="text-9 opacity-60" aria-hidden>
          ▾
        </span>
      </button>
      <button type="button" onClick={onOpen("assignee")} className={BTN_CN}>
        {t("pm.list.col.assignee")}
        <span className="text-9 opacity-60" aria-hidden>
          ▾
        </span>
      </button>
      <button
        type="button"
        onClick={onClear}
        aria-label={t("pm.list.bulkClear")}
        className="ml-1 shrink-0 rounded px-1.5 py-0.5 text-11 text-text-tertiary hover:bg-zinc-100 dark:hover:bg-zinc-800"
      >
        ✕
      </button>
    </div>
  );
}
