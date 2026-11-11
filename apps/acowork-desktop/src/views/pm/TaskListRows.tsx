/**
 * 列表视图的行 / 单元格渲染。
 *
 * 拆成独立文件的原因：`TaskListView` 已经承载了「派生链 + 交互 + 弹层」，
 * 再把 20 个单元格的投影塞进去就没人能读了。这里只有**投影**：给定一行
 * 任务和它的关联数据，画出一行 `<tr>`。
 *
 * 复用而非重抄（调整 2）：优先级用 `PriorityBadge`、类型用 `TaskTypeIcon`、
 * 成员用 `MemberAvatar`。状态胶囊是项目里唯一没有公共常量的部分（
 * `BOARD_COLUMNS` 只带 `i18nKey`），它的 class 映射在 `taskListLabels.ts`。
 *
 * 长文本一律单行 ellipsis + `title` 属性（标题 / 描述 / 父任务），行高恒定
 * `ROW_HEIGHT`，不允许被内容撑开。
 */

import { Minus, Plus } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { PriorityBadge } from "./PriorityBadge";
import { TaskTypeIcon } from "./TaskTypeIcon";
import { MemberAvatar } from "./pmMembers";
import {
  INDENT_PER_DEPTH,
  REVIEW_LABEL_KEY,
  ROW_HEIGHT,
  STATUS_DOT_CLASS,
  STATUS_LABEL_KEY,
  STATUS_PILL_CLASS,
  TD_BASE_CN,
} from "./taskListLabels";
import {
  dueDaysLeft,
  formatDateTime,
  formatDueDate,
  isDueOverdue,
} from "./taskFormatters";
import { cn } from "../../lib/utils";
import type { ResolvedMember } from "./pmMembers";
import type { PmListColumn, PmListRow } from "../../lib/pm-list-columns";
import type { PmTaskResponse, TaskStatus } from "../../lib/pm-types";

interface TaskListRowProps {
  row: PmListRow;
  columns: PmListColumn[];
  selected: boolean;
  collapsed: boolean;
  /** 该任务有没有子任务（决定折叠开关是 + / − 可点按钮，还是淡化的 − 占位）。 */
  hasChildren: boolean;
  childStats?: { total: number; done: number };
  parentTitle: string;
  member?: ResolvedMember;
  assigneeLabel: string;
  creatorLabel: string;
  /** 离线只读：状态胶囊不再打开菜单（与看板卡片的只读语义一致）。 */
  offline: boolean;
  onOpenTask: (taskId: string) => void;
  onToggleSelect: (taskId: string) => void;
  onToggleCollapse: (taskId: string) => void;
  onOpenStatus: (e: React.MouseEvent<HTMLElement>) => void;
  /** 行内改优先级（点优先级格子弹菜单）。 */
  onOpenPriority: (e: React.MouseEvent<HTMLElement>) => void;
  /** 行内改负责人（点负责人格子弹菜单）。 */
  onOpenAssignee: (e: React.MouseEvent<HTMLElement>) => void;
}

export function TaskListRow({
  row,
  columns,
  selected,
  collapsed,
  hasChildren,
  childStats,
  parentTitle,
  member,
  assigneeLabel,
  creatorLabel,
  offline,
  onOpenTask,
  onToggleSelect,
  onToggleCollapse,
  onOpenStatus,
  onOpenPriority,
  onOpenAssignee,
}: TaskListRowProps) {
  const { t } = useTranslation();
  const task = row.task;
  const finished = task.status === "done" || task.status === "cancelled";

  return (
    <tr
      data-task-id={task.id}
      onClick={() => onOpenTask(task.id)}
      className={cn(
        "cursor-pointer transition-colors hover:bg-[var(--color-accent)]/5",
        selected && "bg-[var(--color-accent)]/10",
        finished && "opacity-70",
      )}
      style={{ height: ROW_HEIGHT }}
    >
      {/* 复选框列 `pl-3 pr-0`：与表头同一列、同一内边距，两行复选框上下
          对齐；12px 左边距与工具栏 / 页脚的 `px-3` 一致。 */}
      <td className={cn(TD_BASE_CN, "pl-3 pr-0")} onClick={(e) => e.stopPropagation()}>
        <input
          type="checkbox"
          aria-label={t("pm.list.selectRow", { title: task.title })}
          checked={selected}
          onChange={() => onToggleSelect(task.id)}
          className="h-3.5 w-3.5 cursor-pointer accent-[var(--color-accent)]"
        />
      </td>

      {columns.map((column, i) => (
        <td
          key={column.key}
          /* 末列 `pr-3`：表头右侧不再有「＋」占位列，右边界由最后一列收住。 */
          className={cn(
            TD_BASE_CN,
            column.align === "right" && "text-right",
            i === columns.length - 1 && "pr-3",
          )}
          title={cellTooltip(column.key, task, { parentTitle, assigneeLabel, creatorLabel })}
        >
          {column.key === "title" ? (
            <TitleCell
              task={task}
              depth={row.depth}
              hasChildren={hasChildren}
              collapsed={collapsed}
              onToggleCollapse={onToggleCollapse}
            />
          ) : column.key === "status" ? (
            <StatusCell
              task={task}
              offline={offline}
              onOpenStatus={onOpenStatus}
              label={t(STATUS_LABEL_KEY[task.status])}
            />
          ) : (
            <PlainCell
              columnKey={column.key}
              task={task}
              childStats={childStats}
              parentTitle={parentTitle}
              member={member}
              assigneeLabel={assigneeLabel}
              creatorLabel={creatorLabel}
              offline={offline}
              onOpenPriority={onOpenPriority}
              onOpenAssignee={onOpenAssignee}
            />
          )}
        </td>
      ))}
    </tr>
  );
}

/** 任务列：折叠开关（+ / −）+ 类型图标 + 标题 + 阻塞 / 待审标记。
 *
 * 任务 id 不拼在标题后面：它有自己的列（`pmListColumn("id")`，字段弹层可勾选），
 * 拼一份进标题既重复又让长标题提前被截断。 */
function TitleCell({
  task,
  depth,
  hasChildren,
  collapsed,
  onToggleCollapse,
}: {
  task: PmTaskResponse;
  depth: number;
  hasChildren: boolean;
  collapsed: boolean;
  onToggleCollapse: (taskId: string) => void;
}) {
  const { t } = useTranslation();
  const needsReview =
    task.status === "submitted" ||
    (task.status === "pending" && task.review_status === "pending");
  return (
    <div
      className="flex min-w-0 items-center gap-1"
      style={{ paddingLeft: depth * INDENT_PER_DEPTH }}
    >
      {/* 折叠开关用 + / −（树形控件的通用语法），不用 10px 的 ▾：小字号三角
          在 34px 行里只剩一个小点，看不出"能不能点"。
          − = 已展开，+ = 已收起；无子任务的行给同样大小的 − 占位（淡化、
          不进 Tab 序），缩进列宽因此恒定，标题不会逐行左右错位。 */}
      {hasChildren ? (
        <button
          type="button"
          aria-label={collapsed ? t("pm.task.expand") : t("pm.task.collapse")}
          aria-expanded={!collapsed}
          onClick={(e) => {
            e.stopPropagation();
            onToggleCollapse(task.id);
          }}
          className="shrink-0 rounded p-0.5 text-text-tertiary transition-colors hover:bg-zinc-200 hover:text-text-secondary dark:hover:bg-zinc-700"
        >
          {collapsed ? (
            <Plus className="h-3 w-3" aria-hidden />
          ) : (
            <Minus className="h-3 w-3" aria-hidden />
          )}
        </button>
      ) : (
        <span className="shrink-0 p-0.5 text-text-tertiary opacity-25" aria-hidden>
          <Minus className="h-3 w-3" />
        </span>
      )}
      {/* 类型图标与标题同字号（12px）：它属于标题的一部分，不是元信息。 */}
      <TaskTypeIcon type={task.type} className="shrink-0 text-xs leading-none" />
      <span className="min-w-0 truncate font-medium text-text">{task.title}</span>
      {task.is_blocked && (
        <span className="shrink-0 text-10" title={t("pm.list.col.blocked")} aria-hidden>
          🔒
        </span>
      )}
      {needsReview && (
        <span
          className="h-1.5 w-1.5 shrink-0 rounded-full bg-amber-500"
          title={t("pm.list.review.pending")}
          aria-hidden
        />
      )}
    </div>
  );
}

/** 状态列：胶囊 + 点，点击开状态菜单；离线时不可点。 */
function StatusCell({
  task,
  offline,
  onOpenStatus,
  label,
}: {
  task: PmTaskResponse;
  offline: boolean;
  onOpenStatus: (e: React.MouseEvent<HTMLElement>) => void;
  label: string;
}) {
  const { t } = useTranslation();
  return (
    <button
      type="button"
      disabled={offline}
      onClick={(e) => {
        e.stopPropagation();
        onOpenStatus(e);
      }}
      title={offline ? t("pm.offlineReadonlyHint") : t("pm.list.changeStatus")}
      className={cn(
        "inline-flex max-w-full items-center gap-1 rounded-full px-2 py-0.5 text-11 leading-tight text-white",
        STATUS_PILL_CLASS[task.status],
        offline ? "cursor-default" : "cursor-pointer hover:brightness-95",
      )}
    >
      <span
        className={cn("h-1.5 w-1.5 shrink-0 rounded-full", STATUS_DOT_CLASS)}
        aria-hidden
      />
      <span className="min-w-0 truncate">{label}</span>
    </button>
  );
}

/** 除 title / status 外的所有列。 */
function PlainCell({
  columnKey,
  task,
  childStats,
  parentTitle,
  member,
  assigneeLabel,
  creatorLabel,
  offline,
  onOpenPriority,
  onOpenAssignee,
}: {
  columnKey: string;
  task: PmTaskResponse;
  childStats?: { total: number; done: number };
  parentTitle: string;
  member?: ResolvedMember;
  assigneeLabel: string;
  creatorLabel: string;
  /** 离线只读：行内改值的入口全部禁用。 */
  offline: boolean;
  onOpenPriority: (e: React.MouseEvent<HTMLElement>) => void;
  onOpenAssignee: (e: React.MouseEvent<HTMLElement>) => void;
}) {
  const { t } = useTranslation();
  const dash = <span className="text-text-tertiary">—</span>;
  /* 元信息列（日期 / 计数 / id）用 `text-11`（11px）：比标题小一档，
     但不再掉到 `text-10`（10px）—— 10px 在 12px 的标题旁边太碎。 */
  const mono = "font-mono text-11";

  switch (columnKey) {
    case "priority":
      // 行内改优先级：与状态同一套语义（点格子弹菜单改）。
      // 离线只读时退化成纯展示（徽章不可点）。
      return (
        <button
          type="button"
          disabled={offline}
          onClick={(e) => {
            e.stopPropagation();
            onOpenPriority(e);
          }}
          title={offline ? t("pm.offlineReadonlyHint") : t("pm.list.changePriority")}
          className={cn(
            "inline-flex max-w-full items-center rounded",
            !offline && "cursor-pointer hover:opacity-80",
          )}
        >
          <PriorityBadge priority={task.priority} />
        </button>
      );
    case "assignee":
      // 行内改负责人：同上。未指派也是一个可点的入口（点了才能指派出去）。
      return (
        <button
          type="button"
          disabled={offline}
          onClick={(e) => {
            e.stopPropagation();
            onOpenAssignee(e);
          }}
          title={offline ? t("pm.offlineReadonlyHint") : t("pm.list.changeAssignee")}
          className={cn(
            "flex min-w-0 items-center gap-1 rounded text-left",
            !offline && "cursor-pointer hover:opacity-80",
          )}
        >
          {!task.assignee ? (
            <span className="text-text-tertiary">{t("pm.task.unassigned")}</span>
          ) : (
            <>
              {member && <MemberAvatar member={member} size={16} className="shrink-0" />}
              <span className="min-w-0 truncate">{assigneeLabel || task.assignee}</span>
            </>
          )}
        </button>
      );
    case "due_at": {
      if (!task.due_at) return dash;
      const overdue = isDueOverdue(task.due_at, task.status);
      const left = dueDaysLeft(task.due_at);
      return (
        <span
          className={cn(
            mono,
            overdue && "font-medium text-red-600 dark:text-red-400",
            !overdue && left !== null && left <= 1 && "text-amber-600 dark:text-amber-400",
          )}
        >
          {formatDueDate(task.due_at)}
        </span>
      );
    }
    case "type":
      return <span className="truncate">{t(`pm.taskType.${task.type}`)}</span>;
    case "review_status":
      return <span className="truncate">{t(REVIEW_LABEL_KEY[task.review_status])}</span>;
    case "start_at":
      return task.start_at ? <span className={mono}>{formatDateTime(task.start_at)}</span> : dash;
    case "created_by":
      return <span className="truncate">{creatorLabel || task.created_by}</span>;
    case "parent":
      return parentTitle ? (
        <span className="truncate">{parentTitle}</span>
      ) : (
        <span className="text-text-tertiary">{t("pm.task.noParent")}</span>
      );
    case "children": {
      const total = childStats?.total ?? 0;
      if (!total) return dash;
      return (
        <span className={mono}>
          {t("pm.task.completed", { done: childStats?.done ?? 0, total })}
        </span>
      );
    }
    case "depends": {
      const n = task.depends_on?.length ?? 0;
      return n ? <span className={mono}>{n}</span> : dash;
    }
    case "blocked": {
      const n = task.blocked_by?.length ?? 0;
      if (!task.is_blocked && !n) return dash;
      return (
        <span className="inline-flex items-center gap-1 text-red-600 dark:text-red-400">
          <span aria-hidden>🔒</span>
          {n || 1}
        </span>
      );
    }
    case "attachments": {
      const n = task.attachments?.length ?? 0;
      return n ? <span className={mono}>📎 {n}</span> : dash;
    }
    case "submitted_at":
      return task.submitted_at ? (
        <span className={mono}>{formatDateTime(task.submitted_at)}</span>
      ) : (
        dash
      );
    case "created_at":
      return <span className={mono}>{formatDateTime(task.created_at)}</span>;
    case "updated_at":
      return <span className={mono}>{formatDateTime(task.updated_at)}</span>;
    case "dispatch": {
      const n = task.dispatch_attempts ?? 0;
      if (!n) return dash;
      // ≥5 是服务端放弃阈值（`dispatch_attempts` 注释），3 次起就该看了。
      return (
        <span
          className={cn(
            mono,
            n >= 3 && "font-semibold text-red-600 dark:text-red-400",
          )}
        >
          {n}
        </span>
      );
    }
    case "id":
      return <span className={cn(mono, "text-text-tertiary")}>{task.id}</span>;
    case "description":
      return task.description ? (
        <span className="truncate text-text-tertiary">{task.description}</span>
      ) : (
        dash
      );
    default:
      return dash;
  }
}

/** 单元格 tooltip：只在会被截断的列上挂，其它列不制造噪音。 */
function cellTooltip(
  key: string,
  task: PmTaskResponse,
  extra: { parentTitle: string; assigneeLabel: string; creatorLabel: string },
): string | undefined {
  switch (key) {
    case "title":
      return task.title;
    case "description":
      return task.description || undefined;
    case "parent":
      return extra.parentTitle || undefined;
    case "assignee":
      return task.assignee ? extra.assigneeLabel || task.assignee : undefined;
    case "created_by":
      return task.created_by;
    default:
      return undefined;
  }
}

/** 分组标题行：组名 + 条数，横跨全部列。 */
export function TaskListGroupHeaderRow({
  label,
  count,
  colCount,
}: {
  label: string;
  count: number;
  colCount: number;
}) {
  return (
    <tr>
      <td
        colSpan={colCount}
        className="sticky-none border-b border-border-divider bg-panel-inset px-3 py-1"
      >
        <span className="flex items-center gap-1.5 text-11 font-medium text-text-secondary">
          <span aria-hidden>▾</span>
          <span className="min-w-0 truncate">{label}</span>
          <span className="rounded bg-zinc-200 px-1 tabular-nums text-10 text-text-tertiary dark:bg-zinc-800">
            {count}
          </span>
        </span>
      </td>
    </tr>
  );
}

/** 空态：项目本来没任务，与「筛选后无命中」是两种提示（后者必须给出出口）。 */
export function TaskListEmptyRow({
  colCount,
  isEmptyProject,
  emptyProjectLabel,
  noMatchLabel,
  clearFiltersLabel,
  onClearFilters,
}: {
  colCount: number;
  isEmptyProject: boolean;
  emptyProjectLabel: string;
  noMatchLabel: string;
  clearFiltersLabel: string;
  onClearFilters?: () => void;
}) {
  return (
    <tr>
      <td colSpan={colCount} className="border-b border-border-divider">
        <div className="flex flex-col items-center justify-center gap-1 py-10 text-center">
          <div className="text-2xl" aria-hidden>
            {isEmptyProject ? "📋" : "🔍"}
          </div>
          <p className="text-11 text-text-secondary">
            {isEmptyProject ? emptyProjectLabel : noMatchLabel}
          </p>
          {!isEmptyProject && onClearFilters && (
            <button
              type="button"
              onClick={onClearFilters}
              className="mt-1 rounded px-2 py-0.5 text-10 text-[var(--color-accent)] hover:bg-zinc-100 dark:hover:bg-zinc-800"
            >
              {clearFiltersLabel}
            </button>
          )}
        </div>
      </td>
    </tr>
  );
}

/**
 * 加载态：占位行。
 *
 * 刻意不是空表 —— 空表在「项目本来没任务」和「还在拉数据」之间不可区分，
 * 用户会以为数据丢了。行数取一个够填满首屏的值。
 */
export function TaskListSkeletonRows({
  colCount,
  rows = 6,
}: {
  colCount: number;
  rows?: number;
}) {
  return (
    <>
      {Array.from({ length: rows }, (_, i) => (
        <tr key={i} style={{ height: ROW_HEIGHT }}>
          <td className={TD_BASE_CN} />
          <td colSpan={colCount - 1} className={TD_BASE_CN}>
            <div
              className="h-3 w-2/3 animate-pulse rounded bg-zinc-100 dark:bg-zinc-800"
              aria-label="loading"
            />
          </td>
        </tr>
      ))}
    </>
  );
}

/** 状态语义在 lib 之外的唯一入口（分组标题也用它）。 */
export function statusLabelKey(status: TaskStatus): string {
  return STATUS_LABEL_KEY[status];
}
