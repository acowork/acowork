/**
 * SubtaskTree — 子任务树（T2-6）。
 *
 * 对齐 UX 设计 §3.4：递归展示子任务（最多 5 级），支持折叠。
 * - 每行：折叠箭头 + 完成状态 + 标题 + 状态
 * - 子任务从 boardStore.tasks 派生（与看板同源，无额外请求）
 * - 点击行 → 切换到该子任务详情（同 Drawer 内导航）
 *
 * ⚠️ Selector 契约（boardStore 顶部强制）：
 * 订阅原始字段 s.tasks（稳定引用），children 派生在 useMemo 内完成，
 * 整棵子树共享一份 byId / childrenByParent 索引，避免重复 O(N) 构建。
 */

import { useMemo, useState } from "react";
import { usePmBoardStore } from "../../stores/pm/boardStore";
import { useTranslation } from "../../i18n/useTranslation";
import { TaskTypeIcon } from "./TaskTypeIcon";
import { cn } from "../../lib/utils";
import { listRowHover } from "../../lib/ui-styles";
import type { PmTaskResponse, TaskStatus } from "../../lib/pm-types";

const STATUS_STYLE: Partial<Record<TaskStatus, string>> = {
  done: "text-ok",
  in_progress: "text-info",
  submitted: "text-warning",
  rejected: "text-danger-text",
  cancelled: "text-text-tertiary ",
  pending: "text-text-tertiary ",
};

function StatusDot({ status }: { status: TaskStatus }) {
  return (
    <span
      className={cn("h-1.5 w-1.5 shrink-0 rounded-full bg-current", STATUS_STYLE[status])}
      aria-hidden
    />
  );
}

interface SubtaskRowProps {
  task: PmTaskResponse;
  depth: number;
  byId: Map<string, PmTaskResponse>;
  childrenByParent: Map<string, PmTaskResponse[]>;
  onSelect: (taskId: string) => void;
}

function SubtaskRow({ task, depth, byId, childrenByParent, onSelect }: SubtaskRowProps) {
  const { t } = useTranslation();
  const children = childrenByParent.get(task.id) ?? [];
  const [collapsed, setCollapsed] = useState(false);
  const hasChildren = children.length > 0;

  return (
    <li className="space-y-0.5">
      <div
        className={cn(
          "flex w-full cursor-pointer items-center gap-1.5 rounded px-1.5 py-1 text-left text-xs",
          listRowHover,
          depth > 0 && "ml-3",
        )}
        style={{ paddingLeft: 6 + depth * 14 }}
        onClick={() => onSelect(task.id)}
        role="button"
        tabIndex={0}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            onSelect(task.id);
          }
        }}
        aria-label={task.title}
      >
        {hasChildren ? (
          <button
            type="button"
            className="w-4 shrink-0 text-center text-10 text-text-tertiary hover:text-text-tertiary"
            onClick={(e) => {
              e.stopPropagation();
              setCollapsed((v) => !v);
            }}
            aria-label={collapsed ? t("pm.task.expand") : t("pm.task.collapse")}
            aria-expanded={!collapsed}
          >
            {collapsed ? "▸" : "▾"}
          </button>
        ) : (
          <span className="w-4 shrink-0" aria-hidden />
        )}
        <StatusDot status={task.status} />
        <TaskTypeIcon type={task.type} className="shrink-0 text-10" />
        <span
          className={cn(
            "min-w-0 flex-1 truncate",
            task.status === "done" && "text-text-tertiary line-through ",
          )}
        >
          {task.title}
        </span>
      </div>

      {hasChildren && !collapsed && (
        <ul className="space-y-0.5">
          {children.map((child) => (
            <SubtaskRow
              key={child.id}
              task={child}
              depth={depth + 1}
              byId={byId}
              childrenByParent={childrenByParent}
              onSelect={onSelect}
            />
          ))}
        </ul>
      )}
    </li>
  );
}

interface TreeIndex {
  byId: Map<string, PmTaskResponse>;
  childrenByParent: Map<string, PmTaskResponse[]>;
}

function buildIndex(tasks: PmTaskResponse[]): TreeIndex {
  const byId = new Map<string, PmTaskResponse>();
  const childrenByParent = new Map<string, PmTaskResponse[]>();
  for (const t of tasks) {
    byId.set(t.id, t);
    if (t.parent_id) {
      const list = childrenByParent.get(t.parent_id) ?? [];
      list.push(t);
      childrenByParent.set(t.parent_id, list);
    }
  }
  // 子任务按 created_at 升序（与 boardStore.childrenOf 行为对齐）
  for (const list of childrenByParent.values()) {
    list.sort((a, b) => a.created_at.localeCompare(b.created_at));
  }
  return { byId, childrenByParent };
}

export function SubtaskTree({
  rootId,
  onSelect,
}: {
  rootId: string;
  onSelect: (taskId: string) => void;
}) {
  // 订阅稳定引用；派生在 useMemo 内完成（符合 boardStore 顶部 selector 契约）
  const tasks = usePmBoardStore((s) => s.tasks);
  const { byId, childrenByParent } = useMemo(() => buildIndex(tasks), [tasks]);

  const root = byId.get(rootId);
  const children = childrenByParent.get(rootId) ?? [];

  if (!root || children.length === 0) {
    return (
      <p className="px-1 py-2 text-xs text-text-tertiary ">
        {root ? "暂无子任务" : ""}
      </p>
    );
  }

  return (
    <ul className="space-y-0.5">
      {children.map((child) => (
        <SubtaskRow
          key={child.id}
          task={child}
          depth={0}
          byId={byId}
          childrenByParent={childrenByParent}
          onSelect={onSelect}
        />
      ))}
    </ul>
  );
}
