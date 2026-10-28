/**
 * TaskStatusMove — 弹框底部的状态流转控件。
 *
 * 看板拖拽是改状态的主路径，但不是唯一路径：有的视图里卡片不可拖（只读
 * 视图、移动端），而拖卡片本身也是一个需要鼠标精确操作的动作 —— 用户只想
 * 改一个状态时，弹框里必须有一条等价的路径，否则"改状态"就只剩一个口子。
 *
 * 合法目标是服务端 `validate_transition`
 * （`core/acowork-pm/src/store/tree.rs:570`）的镜像：客户端只列服务端会
 * 接受的流转，服务端才是唯一裁决者（`PATCH /tasks/{tid}` 的 `status`）。
 * 拖拽能到达的四个主列（待办 / 进行中 / 已提交 / 已完成）都在里面，另外
 * 还能取消、能回退 —— 这两条拖拽给不了。
 *
 * 审批闸门：Agent 新建、人类尚未批准的任务（`status=pending` 且
 * `review_status=pending`）不能直接进 `in_progress` —— 服务端
 * `ensure_creation_approved`（同一文件）会 409 `review_not_approved`。所以
 * 这种任务不列 in_progress，改列"批准"：批准（review）只翻 `review_status`，
 * 之后才能动状态 —— 服务端文档描述的正是这两步。
 */
import { useCallback, useMemo, useState } from "react";
import { usePmBoardStore } from "../../stores/pm/boardStore";
import { useTranslation } from "../../i18n/useTranslation";
import { Dropdown } from "../../components/common/Dropdown";
import { showToast } from "../../components/common/ToastProvider";
import type { PmTaskResponse, TaskStatus } from "../../lib/pm-types";

const STATUS_I18N: Record<TaskStatus, string> = {
  pending: "pm.board.pending",
  in_progress: "pm.board.inProgress",
  submitted: "pm.board.submitted",
  done: "pm.board.done",
  rejected: "pm.board.rejected",
  cancelled: "pm.board.cancelled",
};

/**
 * 服务端 `validate_transition` 的镜像。
 *
 * 客户端只负责"不给出服务端会拒绝的选项"，最终裁决在服务端
 * （`PmError::InvalidStateTransition`）。
 */
export const STATUS_TRANSITIONS: Record<TaskStatus, TaskStatus[]> = {
  pending: ["in_progress", "rejected", "cancelled"],
  in_progress: ["pending", "submitted", "cancelled"],
  submitted: ["done", "rejected"],
  done: ["in_progress"],
  rejected: ["pending", "in_progress", "cancelled"],
  cancelled: [],
};

/** "批准"不是状态，是审批动作 —— 用一个不会撞上状态名的伪值表示。 */
const APPROVE = "__approve__";

interface TaskStatusMoveProps {
  /** 服务端那一行（状态、审核状态都是服务端的）。 */
  task: PmTaskResponse;
  /** 状态落定后调用 —— 让弹框重新读一次该行（状态是服务端派生的）。 */
  onMoved: () => void;
}

export function TaskStatusMove({ task, onMoved }: TaskStatusMoveProps) {
  const { t } = useTranslation();
  const moveTask = usePmBoardStore((s) => s.moveTask);
  const reviewTask = usePmBoardStore((s) => s.reviewTask);
  const [busy, setBusy] = useState(false);
  // 选完之后回到 ""：控件回到"当前状态"的显示，提示"已经生效了"。
  const [picked, setPicked] = useState("");

  // Agent 新建、人类未批准 —— 进 in_progress 会被服务端闸门拒绝。
  const gated = task.status === "pending" && task.review_status === "pending";

  const options = useMemo(() => {
    const targets = STATUS_TRANSITIONS[task.status].filter(
      (s) => !(gated && s === "in_progress"),
    );
    return [
      ...(gated ? [{ value: APPROVE, label: t("pm.task.approve") }] : []),
      ...targets.map((s) => ({ value: s, label: t(STATUS_I18N[s]) })),
    ];
  }, [task.status, gated, t]);

  const handle = useCallback(
    async (value: string) => {
      setPicked("");
      // 重新选中了当前状态（占位项）：什么也不做。
      if (!value || busy) return;
      setBusy(true);
      try {
        if (value === APPROVE) {
          // 批准只翻 review_status（服务端 review_task 的创建审批分支），
          // 状态留给用户下一步自己动。
          const ok = await reviewTask(task.id, true);
          if (ok) onMoved();
          return;
        }
        const ok = await moveTask(task.id, value as TaskStatus);
        if (!ok) return;
        showToast({
          type: "success",
          message: `${t("pm.task.moveTo")} ${t(STATUS_I18N[value as TaskStatus])}`,
        });
        onMoved();
      } finally {
        setBusy(false);
      }
    },
    [busy, moveTask, reviewTask, onMoved, task.id, t],
  );

  // 没有任何合法流转（已取消是终态）—— 不画一个点不动的控件。
  if (options.length === 0) return null;

  return (
    <Dropdown
      size="small"
      value={picked}
      onChange={(v) => void handle(v)}
      options={options}
      // 占位项显示当前状态：控件自己就是"这条任务现在在哪一列"。
      placeholder={{ value: "", label: t(STATUS_I18N[task.status]), selectable: true }}
      disabled={busy}
      title={t("pm.task.moveTo")}
      aria-label={t("pm.task.moveTo")}
    />
  );
}
