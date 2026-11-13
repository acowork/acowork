/**
 * TaskEditDialog — the one task surface.
 *
 * This is the box that has always served 新建任务 and 编辑任务, extended with
 * the three capabilities that used to live in the retired TaskDetailDialog:
 * 附件上传, 子任务, 删除. The detail dialog is gone rather than the form,
 * because a read-only copy of the same nine fields is a second place for the
 * two faces to drift apart (it printed a bare parent uuid where the form
 * showed a picker), and because it turned a simple box into a five-tab shell
 * whose height changed with every tab.
 *
 * Gating is a server fact, not a layout preference: attachments upload to
 * `POST /tasks/{tid}/attachments` (core/acowork-pm/src/api/routes.rs:106) and
 * children need a parent id, so 文件上传 / 子任务 / 删除 only exist once the row
 * is persisted.
 *
 * The body is TABS (基本信息 / 关联任务 / 文件上传 / 提交记录), on the same
 * shell as GlobalSearchDialog: a fixed-height strip under the title and one
 * body region that scrolls. Stacking four unrelated kinds of content in one
 * column produced a very long scroll in which the thing the reader came for —
 * what the agent actually handed in — sat below a file grid. Tabs are a
 * partition of that column, not a second copy of the fields.
 *
 * Saving is "submit, then close": the request goes out first, the box closes
 * only after the server confirms the row, and a failed save keeps the box open
 * with the draft intact (see `handleSave`). That is true for every shape of
 * the form — 新建任务 / 编辑任务 / 新建子任务 — so the box never survives a
 * successful save and the user never has to click twice to be done.
 *
 * Height is FIXED (`h-[560px]`, capped by `max-h-[86vh]`) with the body
 * scrolling. Sections appearing after the first save, jumping to a child row,
 * or switching tabs must not resize the dialog — that resize was the "跳来跳去"
 * the retired detail shell produced. Inside that fixed height the zones follow
 * one rule: the header is the dialog's identity (title + close), the body is
 * the row's fields (状态 included — it is one more "what is this task now"),
 * and the footer is the FORM's commit path with 删除 parked at the far left,
 * away from 保存. 描述 absorbs whatever height the column has left over instead
 * of the form ending in a dead strip above the footer.
 *
 * Leaving a dirty form is guarded exactly once (`requestLeave`); the confirm's
 * own button exits directly (`leave`). Calling back into the guard from the
 * confirm re-armed the same dialog, which is why 放弃修改 used to do nothing.
 *
 * Status transitions live in the board (drag between columns) and on the task
 * card's 批准/拒绝 buttons, both of which call the same `boardStore.moveTask`.
 * The 基本信息 tab carries a third door to the same write (`TaskStatusMove`):
 * not every view lets the user drag a card, and dragging is a precise act that
 * not every user can perform. It writes the server immediately — it does not
 * pass through 保存 — which is why it is not wired into `useTaskForm`.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { Trash2, Play } from "lucide-react";
import { usePmTaskDetailStore } from "../../stores/pm/taskDetailStore";
import { usePmBoardStore } from "../../stores/pm/boardStore";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import { SubtaskTree } from "./SubtaskTree";
import { TaskStatusMove } from "./TaskStatusMove";
import { DependencyPicker, ParentTaskPicker } from "./TaskTreePicker";
import { ConfirmDialog } from "../../components/common/ConfirmDialog";
import { OverlayScrollbar } from "../../components/common/OverlayScrollbar";
import { StyledInput, StyledTextarea } from "../../components/common/StyledInput";
import { DatePicker } from "../../components/common/DatePicker";
import { Dropdown } from "../../components/common/Dropdown";
import { showToast } from "../../components/common/ToastProvider";
import { attachmentUrl, deleteTask } from "../../lib/pm-api";
import { canViewExecSession, openTaskExecSession } from "../../lib/pmTaskExecSession";
import { useActorLabel } from "./pmMembers";
import { TaskActivitySection } from "./TaskActivity";
import { formatBytes, formatDateTime } from "./taskFormatters";
import { useTaskForm, type TaskForm } from "./useTaskForm";
import {
  resolveTaskTab,
  TASK_EDIT_TAB_I18N,
  visibleTaskTabs,
  type TaskEditTab,
} from "./taskEditTabs";
import type { PmAttachmentMeta, PmTaskResponse } from "../../lib/pm-types";

const TASK_TYPES = ["task", "feature", "bug", "chore", "checkpoint", "milestone"] as const;
const PRIORITIES = ["low", "normal", "high", "urgent"] as const;

const TYPE_I18N: Record<string, string> = {
  task: "pm.taskType.task",
  feature: "pm.taskType.feature",
  bug: "pm.taskType.bug",
  chore: "pm.taskType.chore",
  checkpoint: "pm.taskType.checkpoint",
  milestone: "pm.taskType.milestone",
};

const PRIORITY_I18N: Record<string, string> = {
  low: "pm.priority.low",
  normal: "pm.priority.normal",
  high: "pm.priority.high",
  urgent: "pm.priority.urgent",
};

export function TaskEditDialog({ onClose }: { onClose: () => void }) {
  const mode = usePmTaskDetailStore((s) => s.mode);
  const detail = usePmTaskDetailStore((s) => s.detail);
  const attachments = usePmTaskDetailStore((s) => s.attachments);
  const activity = usePmTaskDetailStore((s) => s.activity);
  const replying = usePmTaskDetailStore((s) => s.replying);
  const addReply = usePmTaskDetailStore((s) => s.addReply);
  const uploading = usePmTaskDetailStore((s) => s.uploading);
  const error = usePmTaskDetailStore((s) => s.error);
  const storeProjectId = usePmTaskDetailStore((s) => s.projectId);
  const createParentId = usePmTaskDetailStore((s) => s.createParentId);
  const returnTo = usePmTaskDetailStore((s) => s.returnTo);
  const openTask = usePmTaskDetailStore((s) => s.openTask);
  const openCreate = usePmTaskDetailStore((s) => s.openCreate);
  const refresh = usePmTaskDetailStore((s) => s.refresh);
  const uploadAttachment = usePmTaskDetailStore((s) => s.uploadAttachment);
  const deleteAttachment = usePmTaskDetailStore((s) => s.deleteAttachment);
  const removeTask = usePmBoardStore((s) => s.removeTask);
  const { t } = useTranslation();

  const isCreate = mode === "create";
  // A persisted row is what unlocks 文件上传 / 提交记录 / 子任务 / 删除 — the two
  // server-keyed tabs are picked by `visibleTaskTabs`, the rest by `detail`.
  const hasRow = !isCreate && !!detail;

  const projectId = storeProjectId ?? detail?.project_id ?? null;
  const form = useTaskForm({
    projectId: projectId ?? "",
    initial: isCreate ? null : detail,
    defaultParentId: isCreate ? createParentId : null,
  });
  // `form` is a fresh object every render (the hook returns a literal). Pull out
  // the one stable member the exit path needs, so `leave` / `requestLeave` do
  // not change identity on every keystroke and the ESC listener is not
  // re-subscribed while the user types.
  const resetForm = form.reset;

  const fileInputRef = useRef<HTMLInputElement>(null);
  const dialogRef = useRef<HTMLDivElement>(null);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [openingExec, setOpeningExec] = useState(false);
  const [confirmDeleteAtt, setConfirmDeleteAtt] = useState<string | null>(null);
  const [confirmDiscard, setConfirmDiscard] = useState(false);
  // What to run once the user confirms discarding. Cancelling, closing, and
  // navigating to another row are all navigations away from a dirty form, and
  // each wants a different destination.
  const [pendingDiscard, setPendingDiscard] = useState<(() => void) | null>(null);
  // Which tab is open. The strip is the same chrome as GlobalSearchDialog's,
  // for the same reason: a task carries four kinds of content and stacking all
  // of them made one very long column.
  const [tab, setTab] = useState<TaskEditTab>("basic");

  const creatorName = useActorLabel(detail?.created_by ?? null);

  // A different row — or a different mode — is a different surface. Land on
  // 基本信息 and take focus, instead of leaving the reader on 提交记录 of a task
  // they are no longer looking at. Keyed on the row's IDENTITY, so a store
  // refresh (same id, new object) never yanks the open tab away.
  useEffect(() => {
    setTab("basic");
    dialogRef.current?.focus();
  }, [mode, detail?.id]);

  /**
   * The unconditional exit: the form is on its way out, whoever asked for it.
   *
   * Three destinations — jump to another row (`after`), go back to the parent a
   * subtask form was opened from, or close. `reset()` runs on the closing path
   * so an abandoned draft cannot resurrect when the same row is opened again
   * (`useTaskForm` keeps the draft for the row's lifetime); navigation does not
   * need it, because the form re-keys on the new row.
   */
  const leave = useCallback(
    (after?: () => void) => {
      if (after) {
        after();
        return;
      }
      // Cancelling a subtask form goes back to the parent it came from.
      resetForm();
      if (isCreate && returnTo) void openTask(returnTo);
      else onClose();
    },
    [resetForm, isCreate, returnTo, openTask, onClose],
  );

  /**
   * Navigate away from the form, asking first when it holds unsaved edits.
   * Without this an accidental ESC or backdrop click ate a half-written task —
   * the old form had no guard at all.
   *
   * The guard is consulted exactly ONCE per exit. The confirm's own button
   * calls `leave()`, never this: routing it back through the guard re-armed the
   * very dialog the user had just accepted (`dirty` is still true at that
   * moment, because discarding IS the act of throwing the draft away), so
   * 放弃修改 did nothing and the only way out was 取消. Acceptance of the
   * confirm *is* the answer to the guard.
   */
  const requestLeave = useCallback(
    (after?: () => void) => {
      if (form.dirty) {
        setPendingDiscard(() => after ?? null);
        setConfirmDiscard(true);
        return;
      }
      leave(after);
    },
    [form.dirty, leave],
  );

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !form.saving) requestLeave();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [requestLeave, form.saving]);

  const handleSave = useCallback(async () => {
    const outcome = await form.save();
    if (!outcome.ok) {
      // The row did not make it: keep the box open with the draft intact, so
      // the user can fix it and try again instead of losing what they typed.
      showToast({
        type: outcome.reason === "title_required" ? "warning" : "error",
        message:
          outcome.reason === "title_required"
            ? t("pm.taskTitleRequired")
            : `${t("pm.task.taskSaveFailed")}: ${outcome.message}`,
      });
      return;
    }
    showToast({ type: "success", message: t("pm.task.taskSaved") });
    // 保存成功 → 关闭弹框。三种形态一样：新建任务、编辑任务、在任务里新建
    // 子任务。行已经在服务端了，弹框没有留下的理由；关掉之后看板上的那一行
    // 就是结果的唯一呈现（`save()` 内部已经 reload 过看板）。
    onClose();
  }, [form, onClose, t]);

  const handleDelete = useCallback(async () => {
    if (!detail) return;
    setConfirmDelete(false);
    const parentId = detail.parent_id;
    try {
      // `boardStore.removeTask` only edits the local board — the row is gone
      // once the server says so, and its children go with it (cascade).
      await deleteTask(detail.id, { cascade: true });
      removeTask(detail.id);
      showToast({ type: "success", message: t("pm.task.taskDeleted") });
      // Landing matters: deleting a child from inside its parent's box should
      // leave the parent open, not drop the reader onto the board.
      if (parentId) void openTask(parentId);
      else onClose();
    } catch (e) {
      showToast({
        type: "error",
        message: t("pm.task.deleteFailed", {
          error: e instanceof Error ? e.message : String(e),
        }),
      });
    }
  }, [detail, removeTask, openTask, onClose, t]);

  /**
   * 底栏的「查看执行」：解析这条任务的执行 session 并切过去。
   *
   * 走 `requestLeave` 而不是自己 `onClose` —— 表单里有未保存的改动时，跳去
   * 执行面同样是一次"离开"，该由同一道闸门问一次；确认后被接受的草稿逻辑
   * 与取消 / ESC 完全一致。跳转失败（解析不到 / session 已删）时**不**离开，
   * 弹框留着，用户正好看到那句提示。
   */
  const handleOpenExec = useCallback(async () => {
    if (!detail || openingExec) return;
    setOpeningExec(true);
    try {
      const result = await openTaskExecSession(detail.project_id, detail.id);
      if (result === "gone") {
        showToast({ type: "info", message: t("chatView.execSessionGone") });
      } else if (result === "not_found") {
        showToast({ type: "info", message: t("pm.review.execNotFound") });
      }
    } catch {
      showToast({ type: "error", message: t("pm.review.execNotFound") });
    } finally {
      setOpeningExec(false);
    }
  }, [detail, openingExec, t]);

  const handleUpload = useCallback(
    async (file: File) => {
      const meta = await uploadAttachment(file);
      showToast({
        type: meta ? "success" : "error",
        message: meta ? t("pm.task.attachmentUploaded") : t("pm.task.attachmentUploadFailed"),
      });
    },
    [uploadAttachment, t],
  );

  const handleDeleteAttachment = useCallback(
    async (aid: string) => {
      setConfirmDeleteAtt(null);
      const ok = await deleteAttachment(aid);
      if (!ok) showToast({ type: "error", message: t("pm.task.attachmentDeleteFailed") });
    },
    [deleteAttachment, t],
  );

  // 给当前行加子任务：开一张新建表单并预设父任务，取消时回到这一行
  // （`returnTo`）。它是一次"离开脏表单"的导航，所以走同一个守卫。
  const addSubtask = useCallback(() => {
    if (!detail) return;
    requestLeave(() =>
      openCreate({ projectId: detail.project_id, parentId: detail.id, returnTo: detail.id }),
    );
  }, [detail, requestLeave, openCreate]);

  // Which tabs this form can fill (a server fact, see `visibleTaskTabs`), and
  // where the open tab lands if the strip changes underneath it.
  const tabs = visibleTaskTabs(hasRow);
  const activeTab = resolveTaskTab(tab, tabs);

  // The row the panels may speak about. `detail` alone is not it: 新建任务 can be
  // opened while a row is still cached (the store keeps the last detail), and a
  // NEW task has no children, no timeline and no audit line — printing the old
  // row's would be a lie about the row being written.
  const row = hasRow ? detail : null;

  return (
    <>
      <div className="fixed inset-0 z-50 flex items-center justify-center">
        {/* House pattern (MemberManagerDialog, ConfirmDialog): a separate
            overlay layer with onClick, so a click that starts inside the panel
            and ends outside does not close the form over the user's draft. */}
        <div className="absolute inset-0 bg-modal-overlay" onClick={() => requestLeave()} />
        <div
          ref={dialogRef}
          tabIndex={-1}
          role="dialog"
          aria-modal="true"
          aria-labelledby="pm-edit-task-title"
          // Fixed height: the box never resizes when sections appear or when
          // the reader jumps to a child row.
          className="relative z-10 flex h-[560px] max-h-[86vh] w-full max-w-[640px] flex-col overflow-hidden rounded-md border border-border-outer bg-modal-surface shadow-xl outline-none"
        >
          {/* 头部只有身份 + 关闭。行级动作（状态流转 / 删除）是这一行的属性，
              归 基本信息 的元信息条，见 `BasicInfoTab`。 */}
          <header className="flex shrink-0 items-center justify-between gap-2 border-b border-border-divider px-5 py-3 min-h-[var(--ui-dialog-zone-h)]">
            <h3 id="pm-edit-task-title" className="min-w-0 truncate text-sm font-semibold">
              {isCreate ? t("pm.newTask") : t("pm.task.edit")}
            </h3>
            <button
              type="button"
              onClick={() => requestLeave()}
              className="flex h-7 w-7 shrink-0 items-center justify-center rounded-md text-sm text-text-tertiary transition-colors hover:bg-zinc-200 hover:text-zinc-700 dark:hover:bg-zinc-700 dark:hover:text-zinc-200"
              aria-label={t("common.close")}
            >
              ×
            </button>
          </header>

          {/* Tab strip — the same shell as GlobalSearchDialog's Zone 2: a
              fixed-height, divider-separated row that never scrolls, directly
              under the title. 关联任务 / 文件上传 / 提交记录 used to be stacked in
              one column below the fields, which is what made this box a very
              long scroll. Height stays fixed (`h-[560px]`), so switching tabs
              never resizes the dialog. */}
          <div
            role="tablist"
            className="flex shrink-0 items-center justify-center gap-1 border-b border-border-divider px-5 min-h-[var(--ui-dialog-zone-h)]"
          >
            {tabs.map((key) => (
              <button
                key={key}
                id={`task-tab-${key}`}
                type="button"
                role="tab"
                aria-selected={activeTab === key}
                aria-controls="task-tabpanel"
                data-testid={`task-tab-${key}`}
                onClick={() => setTab(key)}
                className={`rounded-full px-3 py-1.5 text-xs font-medium transition-colors ${
                  activeTab === key
                    ? "bg-zinc-200 text-text dark:bg-zinc-700"
                    : "text-text-tertiary hover:bg-zinc-100 dark:hover:bg-zinc-700/50"
                }`}
              >
                {t(TASK_EDIT_TAB_I18N[key])}
              </button>
            ))}
          </div>

          {/* 一个标签页的内容区，滚动归它自己 */}
          <OverlayScrollbar fill="flex" className="px-5 py-4">
            {/* One panel, swapped by the strip above: `aria-labelledby` points
                at whichever tab owns the body right now. */}
            {/* gap-3 instead of space-y-3, plus min-h-full: the panel is the
                column 基本信息 fills, so its 描述 field can absorb the height the
                fixed box has left over. Other tabs keep their natural height
                (children do not grow without `flex-1`). */}
            <div
              id="task-tabpanel"
              role="tabpanel"
              aria-labelledby={`task-tab-${activeTab}`}
              className="flex min-h-full flex-col gap-3"
            >
              {/* A load failure is a fact about the ROW, not about one tab, so
                  it sits above the tab body and stays visible on every tab. */}
              {error && (
                <div className="rounded-md bg-amber-50 px-3 py-2 text-xs text-amber-700 dark:bg-amber-950/40 dark:text-amber-300">
                  {error}
                  <button
                    type="button"
                    onClick={() => void refresh()}
                    className="ml-2 font-medium underline"
                  >
                    {t("common.retry")}
                  </button>
                </div>
              )}

              {activeTab === "basic" && (
                <BasicInfoTab
                  form={form}
                  row={row}
                  creatorName={creatorName}
                  onMoved={() => void refresh()}
                />
              )}

              {activeTab === "related" && (
                <RelatedTasksTab
                  form={form}
                  row={row}
                  onOpenTask={(id) => requestLeave(() => void openTask(id))}
                  onAddSubtask={addSubtask}
                />
              )}

              {/* 文件上传 / 提交记录 只对已落盘的行开放（`visibleTaskTabs`
                  已经把这两个标签从新建表单里摘掉了），`row` 只是空值收窄。 */}
              {activeTab === "files" && row && (
                <Attachments
                  attachments={attachments}
                  uploading={uploading}
                  fileInputRef={fileInputRef}
                  onUpload={handleUpload}
                  onDelete={(aid) => setConfirmDeleteAtt(aid)}
                />
              )}

              {activeTab === "submissions" && row && (
                <TaskActivitySection
                  activity={activity}
                  attachments={attachments}
                  replying={replying}
                  onReply={addReply}
                />
              )}
            </div>
          </OverlayScrollbar>

          {/* 底部：危险动作在最左（远离 保存），表单提交在右。
              状态流转是这一行的属性，落在 基本信息 的「状态」字段里；删除是整个
              对话框里唯一能毁掉这一行的动作，所以它留在最远的一角。 */}
          <footer className="flex shrink-0 items-center gap-2 border-t border-border-divider px-5 min-h-[var(--ui-dialog-zone-h)]">
            {hasRow && (
              // Solid red + white text, the same weight as the review strip's
              // 拒绝 on a task card. The footer's other two buttons are a
              // neutral 取消 and the accent 保存, so 删除 must not read as a
              // third equal-weight action: a tinted outline did exactly that.
              <button
                type="button"
                onClick={() => setConfirmDelete(true)}
                className="inline-flex items-center justify-center gap-1 rounded-md bg-red-600 px-3 py-1.5 text-xs font-medium text-white hover:bg-red-500 dark:bg-red-700 dark:hover:bg-red-600"
              >
                <Trash2 className="h-3 w-3" aria-hidden />
                {t("pm.task.delete")}
              </button>
            )}
            {/* 删除右边、离 保存 最远的那一段：这颗不决定任务去哪，只回答
                "它干了什么"。只在有执行 session 的三态上出现（与看板卡片同一
                个判断，`canViewExecSession`），安静的文字按钮 —— 底栏的重量
                分配是 实心红删除 | 安静查看 …… 安静取消 | 强调保存。 */}
            {hasRow && detail && canViewExecSession(detail.status) && (
              <button
                type="button"
                onClick={() => requestLeave(() => void handleOpenExec())}
                disabled={openingExec}
                className="inline-flex items-center justify-center gap-1 rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-100 disabled:opacity-50 dark:hover:bg-zinc-700"
              >
                <Play className="h-3 w-3" aria-hidden />
                {t("pm.review.openExec")}
              </button>
            )}
            <div className="flex-1" />
            <button
              type="button"
              onClick={() => requestLeave()}
              disabled={form.saving}
              className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-100 disabled:opacity-50 dark:hover:bg-zinc-700"
            >
              {t("common.cancel")}
            </button>
            <button
              type="button"
              onClick={() => void handleSave()}
              disabled={form.saving}
              className="rounded-md btn-accent px-4 py-1.5 text-xs font-medium disabled:opacity-50"
            >
              {form.saving ? t("common.saving") : t("pm.task.save")}
            </button>
          </footer>
        </div>
      </div>

      {hasRow && detail && (
        <ConfirmDialog
          open={confirmDelete}
          title={t("pm.task.deleteTaskTitle")}
          message={`${t("pm.task.deleteTaskCascadeDesc")} "${detail.title}"?`}
          confirmLabel={t("common.delete")}
          destructive
          onConfirm={() => void handleDelete()}
          onCancel={() => setConfirmDelete(false)}
        />
      )}

      <ConfirmDialog
        open={!!confirmDeleteAtt}
        title={t("pm.task.deleteAttachmentTitle")}
        message={t("pm.task.deleteAttachmentDesc")}
        confirmLabel={t("common.delete")}
        destructive
        onConfirm={() => {
          if (confirmDeleteAtt) void handleDeleteAttachment(confirmDeleteAtt);
        }}
        onCancel={() => setConfirmDeleteAtt(null)}
      />

      <ConfirmDialog
        open={confirmDiscard}
        title={t("pm.task.discardChangesTitle")}
        message={t("pm.task.discardChangesDesc")}
        confirmLabel={t("pm.task.discardChangesConfirm")}
        destructive
        onConfirm={() => {
          // Acceptance IS the answer to the dirty guard: leave directly.
          // Bouncing back through `requestLeave` re-opened this dialog, because
          // `dirty` is still true at this instant (discarding is exactly what
          // makes it false), so 放弃修改 looked like it did nothing.
          const after = pendingDiscard;
          setConfirmDiscard(false);
          setPendingDiscard(null);
          leave(after ?? undefined);
        }}
        onCancel={() => {
          setConfirmDiscard(false);
          setPendingDiscard(null);
        }}
      />
    </>
  );
}

// ── Tab panels ────────────────────────────────────────────────────────

/**
 * 基本信息 — the task's own fields, plus the audit block the server owns.
 *
 * Read-only server facts (creator, timestamps, a failed dispatch) live with
 * the editable fields on purpose: they are what the editor needs in front of
 * them while editing, and a separate 详情 tab would put a row's facts one click
 * away from the fields they explain.
 */
function BasicInfoTab({
  form,
  row,
  creatorName,
  onMoved,
}: {
  form: TaskForm;
  row: PmTaskResponse | null;
  creatorName: string | null;
  /** 状态流转写的是服务端那一行，不走表单的保存路径。 */
  onMoved: () => void;
}) {
  const { t } = useTranslation();
  // A column, not a stack of margins: 描述 takes whatever height is left in the
  // fixed-height box (≈30px slack when editing, ≈75px when creating), so the
  // form ends at the footer instead of leaving a dead strip above it.
  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3">
      {row && (
        // 只印"改不了的服务端事实"（创建人/时间、派发失败原因）。类型 / 优先级
        // / 状态 的可编辑控件就在下面的字段行里，同一件事印两遍只会让人疑惑
        // 哪一处才是当前值。
        <div
          data-testid="task-audit"
          className="rounded-md bg-zinc-50 px-3 py-2 text-11 text-text-tertiary dark:bg-zinc-800/60"
        >
          <div className="flex flex-wrap items-center gap-x-2.5 gap-y-1">
            <span>
              {t("pm.task.createdBy")} {creatorName ?? "—"}
            </span>
            <span aria-hidden>·</span>
            <span>{formatDateTime(row.created_at)}</span>
            <span aria-hidden>·</span>
            <span>
              {t("pm.task.updated")} {formatDateTime(row.updated_at)}
            </span>
          </div>
          {/* A dispatch failure is a durable task fact — surfacing it here is
              the only place a stuck row explains itself. */}
          {row.status === "pending" && !row.dispatched_at && row.dispatch_error && (
            <p className="mt-1.5 rounded bg-amber-100 px-2 py-1 text-amber-800 dark:bg-amber-950/50 dark:text-amber-300">
              {row.dispatch_attempts >= 5
                ? t("pm.task.dispatchAbandoned", { error: row.dispatch_error })
                : t("pm.task.dispatchFailed", {
                    attempts: row.dispatch_attempts,
                    error: row.dispatch_error,
                  })}
            </p>
          )}
          {/* 未排期 = 没有开始时间 = 扫描器不派发（fail-closed）。清掉了开始
              时间的任务在这里必须自己说出来,否则它和"等扫描器"长得一样。 */}
          {row.status === "pending" && !row.dispatched_at && !row.start_at && (
            <p className="mt-1.5">{t("pm.task.unscheduled")}</p>
          )}
        </div>
      )}

      <Field label={t("pm.task.title")} required>
        <StyledInput
          value={form.values.title}
          onChange={(e) => form.setTitle(e.target.value)}
          placeholder={t("pm.taskTitlePlaceholder")}
          autoFocus
        />
      </Field>

      {/* 分类三件一行（满行），负责人 + 两个日期一行（满行）。
          不留半行空档、也不靠"把日期拉宽"来填空：日期只有 10 个字符，拉宽
          只是把空白从行尾挪进输入框里。
          状态流转坐进这一行：它跟类型 / 优先级一样是"这条任务现在是什么"，
          而且 3 + 3 正好两行排满。新建时还没有行，就只剩两个格子。 */}
      <div
        data-testid="row-what"
        className={cn("grid gap-3", row ? "grid-cols-3" : "grid-cols-2")}
      >
        <Field label={t("pm.task.type")}>
          <Dropdown
            value={form.values.type}
            onChange={(v) => form.setType(v as (typeof TASK_TYPES)[number])}
            options={TASK_TYPES.map((tt) => ({ value: tt, label: t(TYPE_I18N[tt]) }))}
          />
        </Field>
        <Field label={t("pm.task.priority")}>
          <Dropdown
            value={form.values.priority}
            onChange={(v) => form.setPriority(v as (typeof PRIORITIES)[number])}
            options={PRIORITIES.map((p) => ({ value: p, label: t(PRIORITY_I18N[p]) }))}
          />
        </Field>

        {/* 状态：立刻写服务端那一行（看板拖拽不是唯一路径）。它长得像表单字段，
            但不经过 保存 —— 选完就落定。终态（已取消）自己没有合法流转，
            TaskStatusMove 会返回 null，这一格留空。 */}
        {row && (
          <Field label={t("pm.task.status")}>
            <TaskStatusMove task={row} onMoved={onMoved} />
          </Field>
        )}
      </div>

      <div data-testid="row-when" className="grid grid-cols-3 gap-3">
        <Field label={t("pm.task.assignee")}>
          {form.hasMembers ? (
            <Dropdown
              value={form.values.assignee}
              onChange={form.setAssignee}
              options={form.assigneeOptions}
              placeholder={{ value: "", label: t("pm.task.unassigned") }}
            />
          ) : (
            <p className="rounded-md border border-dashed border-zinc-300 px-2 py-1.5 text-11 text-text-tertiary dark:border-zinc-600">
              {t("pm.assigneeNoMembersHint")}
            </p>
          )}
        </Field>

        {/* 用组件库的 DatePicker，不用原生 `<input type="date">`：原生弹层由
            WebView2 自己绘制（不属页面 DOM，CSS 管不到），箭头尺寸和底部按钮的
            底色都失控，也正是"看着业余"的来源。宽度跟这一行的其他格子对齐。 */}
        <Field label={t("pm.task.startDate")}>
          <DatePicker value={form.values.startAt} onChange={form.setStartAt} />
        </Field>
        <Field label={t("pm.task.dueDate")}>
          <DatePicker value={form.values.dueAt} onChange={form.setDueAt} />
        </Field>
      </div>

      <Field label={t("pm.task.description")} className="flex min-h-0 flex-1 flex-col">
        <StyledTextarea
          value={form.values.description}
          onChange={(e) => form.setDescription(e.target.value)}
          rows={5}
          placeholder={t("pm.task.descriptionPlaceholder")}
          // 撑满这一列剩下的高度：死空间变成书写区，5 行作保底。
          className="min-h-[7.5rem] flex-1 resize-none"
        />
      </Field>
    </div>
  );
}

/**
 * 关联任务 — everything on this tab is "another task": the parent this row
 * hangs from, the tasks it depends on, and the children hanging from it.
 *
 * 依赖 lives here rather than on 基本信息 because it is a relation, not a field
 * of this row, and because it is the one section that grows without bound (a
 * checkbox per candidate task) — exactly what made the single column unusable.
 */
function RelatedTasksTab({
  form,
  row,
  onOpenTask,
  onAddSubtask,
}: {
  form: TaskForm;
  row: PmTaskResponse | null;
  onOpenTask: (id: string) => void;
  onAddSubtask: () => void;
}) {
  const { t } = useTranslation();
  // 候选集是这一棵树里的一块，不是一袋条目 —— 直接读看板那一份数组（和
  // SubtaskTree 同源，不额外发请求），排除规则在 `lib/taskTree` 里。
  const tasks = usePmBoardStore((s) => s.tasks);
  const selfId = row?.id ?? null;
  return (
    <>
      <Field label={t("pm.task.parentTask")}>
        <ParentTaskPicker
          tasks={tasks}
          selfId={selfId}
          value={form.values.parentId}
          onChange={form.setParentId}
        />
      </Field>

      <Field label={t("pm.task.dependencies")}>
        <DependencyPicker
          tasks={tasks}
          selfId={selfId}
          selected={form.values.dependsOn}
          onToggle={form.toggleDependency}
        />
      </Field>

      {/* Children need a parent id, so this is the one block a NEW task cannot
          fill: the tab stays (parent + dependencies are create-time fields),
          the list does not. */}
      {row ? (
        <div className="pt-1">
          <SectionDivider label={t("pm.task.subtasks")} />
          <SubtaskTree rootId={row.id} onSelect={onOpenTask} />
          <button
            type="button"
            onClick={onAddSubtask}
            className="mt-2 w-full rounded-md border border-dashed border-zinc-300 px-3 py-1.5 text-xs text-text-tertiary hover:border-[var(--color-accent)] hover:text-[var(--color-accent)] dark:border-zinc-600"
          >
            + {t("pm.task.addSubtask")}
          </button>
        </div>
      ) : (
        <p className="text-xs text-text-tertiary">{t("pm.task.subtasksNeedRow")}</p>
      )}
    </>
  );
}

// ── Sections ──────────────────────────────────────────────────────────

function SectionDivider({ label }: { label: string }) {
  return (
    <div className="flex items-center gap-2 pt-1">
      <span className="text-11 font-medium uppercase tracking-wide text-text-tertiary">{label}</span>
      <span className="h-px flex-1 bg-border-divider" />
    </div>
  );
}

function Attachments({
  attachments,
  uploading,
  fileInputRef,
  onUpload,
  onDelete,
}: {
  attachments: PmAttachmentMeta[];
  uploading: boolean;
  fileInputRef: React.RefObject<HTMLInputElement | null>;
  onUpload: (file: File) => void | Promise<void>;
  onDelete: (aid: string) => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="space-y-2">
      <div className="flex items-center gap-2">
        <button
          type="button"
          disabled={uploading}
          onClick={() => fileInputRef.current?.click()}
          className="inline-flex items-center gap-1.5 rounded-md border border-zinc-300 bg-white px-2.5 py-1.5 text-xs font-medium text-text-secondary transition-colors hover:bg-zinc-50 disabled:cursor-not-allowed disabled:opacity-50 dark:border-zinc-600 dark:bg-zinc-800 dark:hover:bg-zinc-700"
        >
          <span aria-hidden>⬆</span>
          {uploading ? t("pm.task.uploading") : t("pm.task.uploadAttachment")}
        </button>
        <input
          ref={fileInputRef}
          type="file"
          className="hidden"
          aria-label={t("pm.task.uploadAttachment")}
          onChange={async (e) => {
            const file = e.target.files?.[0];
            e.target.value = "";
            if (!file) return;
            await onUpload(file);
          }}
        />
      </div>

      {attachments.length === 0 ? (
        <p className="text-xs text-text-tertiary">{t("pm.task.noAttachments")}</p>
      ) : (
        <ul className="grid grid-cols-3 gap-2">
          {attachments.map((att) => (
            <li
              key={att.id}
              className="group relative overflow-hidden rounded-md border border-border-outer bg-panel-block"
            >
              {att.kind === "image" ? (
                <a href={attachmentUrl(att.id)} target="_blank" rel="noreferrer">
                  <img
                    src={attachmentUrl(att.id, { thumb: true })}
                    alt={att.filename}
                    className="h-20 w-full object-cover"
                    loading="lazy"
                  />
                </a>
              ) : (
                <a
                  href={attachmentUrl(att.id, { download: true })}
                  className="flex h-20 flex-col items-center justify-center gap-0.5 text-text-tertiary hover:bg-zinc-50 dark:hover:bg-zinc-700"
                >
                  <span className="text-lg" aria-hidden>📄</span>
                  <span className="max-w-full truncate px-1 text-10">{att.filename}</span>
                </a>
              )}
              {/* opacity (not conditional render) keeps the control focusable */}
              <button
                type="button"
                aria-label={t("pm.task.deleteAttachment")}
                title={t("pm.task.deleteAttachment")}
                onClick={() => onDelete(att.id)}
                className="absolute right-1 top-1 flex h-5 w-5 items-center justify-center rounded-full bg-black/50 text-10 text-white opacity-0 transition-opacity hover:bg-red-600 focus-visible:opacity-100 group-hover:opacity-100"
              >
                ✕
              </button>
              <div className="px-1.5 py-1 text-9 text-text-tertiary">
                <span className="block truncate">{att.filename}</span>
                <span>{formatBytes(att.size)}</span>
              </div>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function Field({
  label,
  required,
  children,
  className,
}: {
  label: string;
  required?: boolean;
  children: React.ReactNode;
  /** Extra classes on the wrapper — 描述 uses it to become the growing row. */
  className?: string;
}) {
  return (
    <label className={cn("block", className)}>
      <span className="mb-1 block text-11 font-medium text-text-tertiary">
        {label}
        {required && <span className="ml-0.5 text-red-500">*</span>}
      </span>
      {children}
    </label>
  );
}
