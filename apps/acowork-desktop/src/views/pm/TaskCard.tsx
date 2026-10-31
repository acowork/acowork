/**
 * TaskCard — 看板任务卡片（T2-5）。
 *
 * 对齐 UX 设计 §3.4：
 * - 类型图标 + 标题
 * - 优先级徽章（PriorityBadge）
 * - 负责人（assignee 显示名，从 agentStore 解析）
 * - 截止日期（逾期红色高亮）
 * - 子任务计数（childrenOf）
 * - 阻塞标记（is_blocked → 🔒）
 * - dnd-kit useDraggable 作为拖拽源（T2-8 接线拖动流转）
 *
 * ⚠️ 源卡片不贴 dnd-kit 的 active transform。看板用 <DragOverlay> 画跟随指针的那
 * 一份，而 ActiveDraggableContext 里的 transform 带 scaleX/scaleY
 * （dnd-kit core 的 adjustScale：overRect / activeNodeRect），把它贴回源元素
 * 会在指针进入目标列（over = 整列 rect）时把卡片放大好几倍、再被列的
 * overflow-hidden 裁掉。over 还是自己时比值恰好是 1，所以“没离开源卡片时
 * 看起来正常”。源卡片留在原位、只降不透明度，跟随的那份交给 DragOverlay。
 *
 * 卡片造型跟随全应用 ListBox 卡片语法（与右侧设置面板同一套）：
 *   外壳  rounded-md + border-outer + bg-panel-block + overflow-hidden
 *   标题条 panel-block 上的 header 行
 *   正文  border-t border-border-divider + bg-panel-inset
 * 标题与正文是两段（发丝线 + 一档底色差），不是一整块平铺 —— 只对齐底色
 * 而不对齐分段，看上去就是两种卡片。待审核态把琥珀色放在边框 + 标题条上，
 * 分段关系不变。
 */

import { useState } from "react";
import { useDraggable, useDroppable } from "@dnd-kit/core";
import { usePmBoardStore } from "../../stores/pm/boardStore";
import { usePmHealthStore } from "../../stores/pm/healthStore";
import { useTranslation } from "../../i18n/useTranslation";
import { PriorityBadge } from "./PriorityBadge";
import { TaskTypeIcon } from "./TaskTypeIcon";
import { RejectDialog } from "./RejectDialog";
import { cn } from "../../lib/utils";
import type { PmTaskResponse } from "../../lib/pm-types";
import { useActorLabel } from "./pmMembers";

interface TaskCardProps {
  task: PmTaskResponse;
  onOpenTask: (taskId: string) => void;
  className?: string;
  /** 嵌套层级（用于缩进，0 = 列内根任务） */
  depth?: number;
}

export function TaskCard({ task, onOpenTask, className, depth = 0 }: TaskCardProps) {
  const { t } = useTranslation();
  const healthy = usePmHealthStore((s) => s.healthy);
  const childCount = usePmBoardStore((s) => s.childrenOf(task.id).length);
  const [rejecting, setRejecting] = useState(false);

  // 离线（healthy === false）时禁用拖拽源 —— 看板只读
  const draggableDisabled = healthy === false;
  const { attributes, listeners, setNodeRef: setDragRef, isDragging } = useDraggable({
    id: task.id,
    data: { type: "task", taskId: task.id, status: task.status, projectId: task.project_id },
    disabled: draggableDisabled,
  });

  // T2-9：卡片同时作为 droppable —— 拖到卡片上 → reparent 为子任务
  const { setNodeRef: setDropRef, isOver: dropOver } = useDroppable({
    id: `drop-${task.id}`,
    data: { type: "task", taskId: task.id, status: task.status },
    disabled: draggableDisabled,
  });

  const setRefs = (el: HTMLElement | null) => {
    setDragRef(el);
    setDropRef(el);
  };

  const assigneeName = useActorLabel(task.assignee);
  const creatorName = useActorLabel(task.created_by);

  /** 待人类审核：Agent 交活待验收（submitted），或 Agent 新建待批准开工（pending + review_status=pending） */
  const needsReview =
    task.status === "submitted" ||
    (task.status === "pending" && task.review_status === "pending");

  // 截止日期格式化：MM-DD（跨年带年份），逾期红色
  const dueLabel = task.due_at ? formatDue(task.due_at) : null;
  const overdue = task.due_at ? isOverdue(task.due_at, task.status) : false;

  const handleApprove = async () => {
    await usePmBoardStore.getState().reviewTask(task.id, true);
  };

  const handleReject = async (reason?: string) => {
    setRejecting(false);
    await usePmBoardStore.getState().reviewTask(task.id, false, reason);
  };

  return (
    <>
      <div
        ref={setRefs}
        {...attributes}
        {...listeners}
        role="button"
        tabIndex={0}
        onClick={() => onOpenTask(task.id)}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            onOpenTask(task.id);
          }
        }}
        className={cn(
          // 外壳 = ListBox 卡片：圆角 + 外发丝线 + panel-block，overflow-hidden
          // 让子块（标题条/正文）不再各自圆角，与设置面板的卡片同一副骨架。
          "group w-full overflow-hidden rounded-md border border-border-outer bg-panel-block text-left transition-colors",
          "hover:border-zinc-300 dark:hover:border-zinc-600",
          "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-accent)]",
          needsReview && "border-amber-300 dark:border-amber-700/60",
          isDragging && "opacity-40",
          dropOver && "ring-2 ring-[var(--color-accent)]",
          depth > 0 && "ml-4",
          className,
        )}
        aria-label={task.title}
      >
        {/* 标题条 —— 对应 ListBox 卡片 header 行；待审核时整条撑琥珀底色，
            at-a-glance 的语义不变。 */}
        <div
          className={cn(
            "flex items-start gap-1.5 px-2.5 py-1.5",
            needsReview && "bg-amber-50/70 dark:bg-amber-950/40",
          )}
        >
          <TaskTypeIcon type={task.type} className="mt-0.5 text-xs leading-none" />
          <span className="min-w-0 flex-1 text-xs font-medium leading-snug text-text ">
            {task.title}
          </span>
        </div>

        {/* 正文 —— border-t 发丝线 + panel-inset：与设置面板卡片的展开体
            完全同一关系（两段之间是线 + 一档明度差）。 */}
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1 border-t border-border-divider bg-panel-inset px-2.5 py-1.5 text-10 text-text-tertiary ">
          <PriorityBadge priority={task.priority} />

          {task.is_blocked && (
            <span className="inline-flex items-center gap-0.5 text-red-600 dark:text-red-400" title="blocked">
              🔒
            </span>
          )}

          {assigneeName && (
            <span className="inline-flex min-w-0 items-center gap-0.5">
              <span aria-hidden>👤</span>
              <span className="max-w-24 truncate">{assigneeName}</span>
            </span>
          )}

          {dueLabel && (
            <span
              className={cn(
                "inline-flex items-center gap-0.5",
                overdue ? "font-medium text-red-600 dark:text-red-400" : "",
              )}
            >
              <span aria-hidden>📅</span>
              {dueLabel}
            </span>
          )}

          {childCount > 0 && (
            <span className="inline-flex items-center gap-0.5" title="subtasks">
              <span aria-hidden>⊞</span>
              {childCount}
            </span>
          )}
        </div>

        {/* 待审核卡片 —— 创建者 + 批准/拒绝 inline 操作。两种形态：
            submitted = Agent 交活等验收；pending + review_status=pending =
            Agent 刚建的任务等人类批准开工（批准后 status 仍留在待办列，
            由 Agent 自己 pm_claim_task 推进）。第三段同样是发丝线分隔。 */}
        {needsReview && (
          <div className="border-t border-amber-200/70 bg-panel-inset px-2.5 py-1.5 dark:border-amber-800/40">
            {creatorName && (
              <p className="mb-1.5 text-10 text-text-tertiary ">
                {t("pm.review.createdByLabel")}{" "}
                <span className="font-medium text-amber-700 dark:text-amber-300">{creatorName}</span>
              </p>
            )}
            <div className="flex items-center gap-1.5">
              <button
                type="button"
                disabled={draggableDisabled}
                onClick={(e) => {
                  e.stopPropagation();
                  void handleApprove();
                }}
                onKeyDown={(e) => e.stopPropagation()}
                className="rounded bg-green-600 px-2 py-0.5 text-10 font-medium text-white hover:bg-green-500 disabled:cursor-not-allowed disabled:opacity-40 dark:bg-green-700 dark:hover:bg-green-600"
              >
                ✓ {t("pm.review.approve")}
              </button>
              <button
                type="button"
                disabled={draggableDisabled}
                onClick={(e) => {
                  e.stopPropagation();
                  setRejecting(true);
                }}
                onKeyDown={(e) => e.stopPropagation()}
                className="rounded bg-red-600 px-2 py-0.5 text-10 font-medium text-white hover:bg-red-500 dark:bg-red-700 dark:hover:bg-red-600"
              >
                ✗ {t("pm.review.reject")}
              </button>
            </div>
          </div>
        )}
      </div>

      {/* 拒绝确认 + 可选理由 */}
      <RejectDialog
        open={rejecting}
        taskTitle={task.title}
        onConfirm={handleReject}
        onCancel={() => setRejecting(false)}
      />
    </>
  );
}

/** 截止日期短格式：本年内 MM-DD，跨年 YYYY-MM-DD */
function formatDue(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso.slice(0, 10);
  const now = new Date();
  const sameYear = d.getFullYear() === now.getFullYear();
  const mm = String(d.getMonth() + 1).padStart(2, "0");
  const dd = String(d.getDate()).padStart(2, "0");
  return sameYear ? `${mm}-${dd}` : `${d.getFullYear()}-${mm}-${dd}`;
}

/** 逾期判断：有截止且未完成/未提交的已过截止时间 */
function isOverdue(iso: string, status: PmTaskResponse["status"]): boolean {
  if (status === "done" || status === "cancelled" || status === "rejected") return false;
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return false;
  return d.getTime() < Date.now();
}
