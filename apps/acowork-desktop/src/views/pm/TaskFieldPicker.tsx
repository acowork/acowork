/**
 * TaskFieldPicker — 列表视图的字段列配置弹层。
 *
 * 表头末尾的 `＋` 与工具栏的「字段」按钮打开的是同一个弹层（同一份
 * `columns` / `onChange`，所以两处入口不会有两套状态）。
 *
 * 三段：搜索、已显示（勾选 = 隐藏，配上移 / 下移调列序）、可添加（勾选 =
 * 显示，追加到末尾）；底部「重置为默认列」。
 *
 * 两条不变量由 `lib/pm-view-settings` 的归一化兜住，这里只负责不给出违反
 * 它们的入口：`title` 列的勾选框禁用（任务列不可隐藏），上移 / 下移对
 * `title` 是 no-op（`movePmListColumn`）。
 *
 * 勾选即生效（不需要「应用」）：列的增删是纯视图操作，写 metadata 的代价
 * 与「先改本地草稿、失败再回滚」相比，后者才是要防的（离线时 metadata 写
 * 不进去，见调用方的失败 toast）。
 */

import { useMemo, useState } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { PmPopover, PmPopoverItem, PmPopoverSection } from "./pmPopover";
import {
  PM_LIST_COLUMNS,
  movePmListColumn,
  pmListColumn,
  type PmListColumnKey,
} from "../../lib/pm-list-columns";

interface TaskFieldPickerProps {
  anchor: HTMLElement;
  /** 当前显示列（含顺序）。 */
  columns: PmListColumnKey[];
  onColumnsChange: (next: PmListColumnKey[]) => void;
  /** 重置为默认 5 列。 */
  onReset: () => void;
  onClose: () => void;
}

export function TaskFieldPicker({
  anchor,
  columns,
  onColumnsChange,
  onReset,
  onClose,
}: TaskFieldPickerProps) {
  const { t } = useTranslation();
  const [query, setQuery] = useState("");

  const { shown, addable } = useMemo(() => {
    const q = query.trim().toLowerCase();
    const matches = (key: PmListColumnKey) => {
      if (!q) return true;
      const col = pmListColumn(key);
      const label = col ? t(col.labelKey) : key;
      return label.toLowerCase().includes(q) || key.toLowerCase().includes(q);
    };
    return {
      shown: columns.filter(matches),
      addable: PM_LIST_COLUMNS.map((c) => c.key).filter(
        (k) => !columns.includes(k) && matches(k),
      ),
    };
  }, [columns, query, t]);

  const toggle = (key: PmListColumnKey) => {
    if (key === "title") return; // 任务列不可隐藏（弹层里也已禁用）
    if (columns.includes(key)) {
      onColumnsChange(columns.filter((k) => k !== key));
      return;
    }
    onColumnsChange([...columns, key]);
  };

  const move = (key: PmListColumnKey, delta: -1 | 1) => {
    onColumnsChange(movePmListColumn(columns, key, delta));
  };

  return (
    <PmPopover anchor={anchor} onClose={onClose} width={280}>
      <div className="px-2 pb-1 pt-1.5">
        <input
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder={t("pm.list.fields.searchPlaceholder")}
          aria-label={t("pm.list.fields.searchPlaceholder")}
          className="w-full rounded border border-input-border bg-input-bg px-2 py-1 text-xs text-text outline-none focus:border-[var(--color-accent)]"
        />
      </div>

      <PmPopoverSection count={shown.length}>
        {t("pm.list.fields.shown")}
      </PmPopoverSection>
      {shown.length === 0 && (
        <p className="px-3 py-1 text-10 text-text-tertiary">{t("pm.list.noMatch")}</p>
      )}
      {shown.map((key, index) => {
        const col = pmListColumn(key);
        if (!col) return null;
        return (
          <div key={key} className="flex items-center">
            <div className="min-w-0 flex-1">
              <PmPopoverItem
                checked
                onClick={() => toggle(key)}
                disabled={col.required}
                title={col.required ? t("pm.list.fields.titleLocked") : undefined}
              >
                {t(col.labelKey)}
              </PmPopoverItem>
            </div>
            <div className="flex shrink-0 items-center pr-2">
              <button
                type="button"
                onClick={() => move(key, -1)}
                disabled={col.required || index <= 1 || index === 0}
                title={t("pm.list.fields.moveUp")}
                aria-label={t("pm.list.fields.moveUp")}
                className="rounded px-1 text-10 text-text-tertiary hover:bg-zinc-100 disabled:opacity-30 dark:hover:bg-zinc-800"
              >
                ▲
              </button>
              <button
                type="button"
                onClick={() => move(key, 1)}
                disabled={col.required || index === shown.length - 1}
                title={t("pm.list.fields.moveDown")}
                aria-label={t("pm.list.fields.moveDown")}
                className="rounded px-1 text-10 text-text-tertiary hover:bg-zinc-100 disabled:opacity-30 dark:hover:bg-zinc-800"
              >
                ▼
              </button>
            </div>
          </div>
        );
      })}

      <PmPopoverSection count={addable.length}>
        {t("pm.list.fields.addable")}
      </PmPopoverSection>
      {addable.length === 0 && (
        <p className="px-3 py-1 text-10 text-text-tertiary">{t("pm.list.noMatch")}</p>
      )}
      {addable.map((key) => {
        const col = pmListColumn(key);
        if (!col) return null;
        return (
          <PmPopoverItem key={key} onClick={() => toggle(key)}>
            {t(col.labelKey)}
          </PmPopoverItem>
        );
      })}

      <div className="mt-1 flex items-center justify-between border-t border-border-divider px-2 py-1.5">
        <button
          type="button"
          onClick={onReset}
          className="rounded px-1.5 py-0.5 text-10 text-text-tertiary hover:bg-zinc-100 hover:text-text dark:hover:bg-zinc-800"
        >
          {t("pm.list.fields.reset")}
        </button>
        <button
          type="button"
          onClick={onClose}
          className="rounded bg-zinc-800 px-2 py-0.5 text-10 font-medium text-white hover:bg-zinc-700 dark:bg-zinc-700 dark:hover:bg-zinc-600"
        >
          {t("pm.list.fields.done")}
        </button>
      </div>
    </PmPopover>
  );
}
