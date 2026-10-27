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
 * children need a parent id, so 附件 / 子任务 / 删除 only exist once the row is
 * persisted. Saving a new task therefore stays in the box and unlocks those
 * sections, instead of closing the dialog the way the old form did.
 *
 * Height is FIXED (`h-[560px]`, capped by `max-h-[86vh]`) with the body
 * scrolling. Sections appearing after the first save, or jumping to a child
 * row, must not resize the dialog — that resize was the "跳来跳去" the old
 * tab shell produced.
 *
 * Status transitions are deliberately absent: they belong to the board (drag
 * between columns) and to the task card's 批准/拒绝 buttons, both of which call
 * the same `boardStore.moveTask`.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { Trash2 } from "lucide-react";
import { usePmTaskDetailStore } from "../../stores/pm/taskDetailStore";
import { usePmBoardStore } from "../../stores/pm/boardStore";
import { useTranslation } from "../../i18n/useTranslation";
import { PriorityBadge } from "./PriorityBadge";
import { TaskTypeIcon } from "./TaskTypeIcon";
import { SubtaskTree } from "./SubtaskTree";
import { ConfirmDialog } from "../../components/common/ConfirmDialog";
import { OverlayScrollbar } from "../../components/common/OverlayScrollbar";
import { StyledInput, StyledTextarea } from "../../components/common/StyledInput";
import { Dropdown } from "../../components/common/Dropdown";
import { showToast } from "../../components/common/ToastProvider";
import { attachmentUrl, deleteTask } from "../../lib/pm-api";
import { useActorLabel } from "./pmMembers";
import { useTaskForm } from "./useTaskForm";
import type { PmAttachmentMeta, TaskStatus } from "../../lib/pm-types";

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

const STATUS_I18N: Record<TaskStatus, string> = {
  pending: "pm.board.pending",
  in_progress: "pm.board.inProgress",
  submitted: "pm.board.submitted",
  done: "pm.board.done",
  rejected: "pm.board.rejected",
  cancelled: "pm.board.cancelled",
};

function formatDateTime(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

export function TaskEditDialog({ onClose }: { onClose: () => void }) {
  const mode = usePmTaskDetailStore((s) => s.mode);
  const detail = usePmTaskDetailStore((s) => s.detail);
  const attachments = usePmTaskDetailStore((s) => s.attachments);
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
  // A persisted row is what unlocks the three server-backed sections.
  const hasRow = !isCreate && !!detail;

  const projectId = storeProjectId ?? detail?.project_id ?? null;
  const form = useTaskForm({
    projectId: projectId ?? "",
    initial: isCreate ? null : detail,
    defaultParentId: isCreate ? createParentId : null,
  });

  const fileInputRef = useRef<HTMLInputElement>(null);
  const dialogRef = useRef<HTMLDivElement>(null);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [confirmDeleteAtt, setConfirmDeleteAtt] = useState<string | null>(null);
  const [confirmDiscard, setConfirmDiscard] = useState(false);
  // What to run once the user confirms discarding. Cancelling, closing, and
  // navigating to another row are all navigations away from a dirty form, and
  // each wants a different destination.
  const [pendingDiscard, setPendingDiscard] = useState<(() => void) | null>(null);

  const creatorName = useActorLabel(detail?.created_by ?? null);

  useEffect(() => {
    dialogRef.current?.focus();
  }, [mode, detail?.id]);

  /**
   * Navigate away from the form, asking first when it holds unsaved edits.
   * Without this an accidental ESC or backdrop click ate a half-written task —
   * the old form had no guard at all.
   */
  const navigate = useCallback(
    (after?: () => void) => {
      if (form.dirty) {
        setPendingDiscard(() => after ?? null);
        setConfirmDiscard(true);
        return;
      }
      if (after) {
        after();
        return;
      }
      // Saving a new task lands on the new row so its attachments are one click
      // away; cancelling a subtask form goes back to the parent it came from.
      form.reset();
      if (isCreate && returnTo) void openTask(returnTo);
      else onClose();
    },
    [form, isCreate, returnTo, openTask, onClose],
  );

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !form.saving) navigate();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [navigate, form.saving]);

  const handleSave = useCallback(async () => {
    const outcome = await form.save();
    if (!outcome.ok) {
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
    // Re-read the row: `is_blocked` / `depth` / `parent_id` are server-derived,
    // and a new row must become a real row before 附件 / 子任务 can attach to it.
    void openTask(outcome.taskId);
  }, [form, openTask, t]);

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

  const titleOf = useTaskTitleResolver();

  return (
    <>
      <div className="fixed inset-0 z-50 flex items-center justify-center">
        {/* House pattern (MemberManagerDialog, ConfirmDialog): a separate
            overlay layer with onClick, so a click that starts inside the panel
            and ends outside does not close the form over the user's draft. */}
        <div className="absolute inset-0 bg-modal-overlay" onClick={() => navigate()} />
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
          {/* 头部 */}
          <header className="flex shrink-0 items-center justify-between border-b border-border-divider px-5 py-3 min-h-[var(--ui-dialog-zone-h)]">
            <h3 id="pm-edit-task-title" className="min-w-0 truncate text-sm font-semibold">
              {isCreate ? t("pm.newTask") : t("pm.task.edit")}
            </h3>
            <button
              type="button"
              onClick={() => navigate()}
              className="shrink-0 rounded-md px-1.5 py-0.5 text-sm text-text-tertiary hover:bg-zinc-200 hover:text-zinc-700 dark:hover:bg-zinc-700 dark:hover:text-zinc-200"
              aria-label={t("common.close")}
            >
              ×
            </button>
          </header>

          {/* 表单区 */}
          <OverlayScrollbar fill="flex" className="px-5 py-4">
            <div className="space-y-3">
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

              {/* 审计信息 — 只读，来自服务端，不属于表单字段 */}
              {hasRow && detail && (
                <div className="rounded-md bg-zinc-50 px-3 py-2 text-11 text-text-tertiary dark:bg-zinc-800/60">
                  <div className="flex flex-wrap items-center gap-x-2.5 gap-y-1">
                    <TaskTypeIcon type={detail.type} className="text-xs" />
                    <PriorityBadge priority={detail.priority} />
                    <span>{t(STATUS_I18N[detail.status])}</span>
                    <span aria-hidden>·</span>
                    <span>
                      {t("pm.task.createdBy")} {creatorName ?? "—"}
                    </span>
                    <span aria-hidden>·</span>
                    <span>{formatDateTime(detail.created_at)}</span>
                    <span aria-hidden>·</span>
                    <span>
                      {t("pm.task.updated")} {formatDateTime(detail.updated_at)}
                    </span>
                  </div>
                  {/* A dispatch failure is a durable task fact — surfacing it
                      here is the only place a stuck row explains itself. */}
                  {detail.status === "pending" && !detail.dispatched_at && detail.dispatch_error && (
                    <p className="mt-1.5 rounded bg-amber-100 px-2 py-1 text-amber-800 dark:bg-amber-950/50 dark:text-amber-300">
                      {detail.dispatch_attempts >= 5
                        ? t("pm.task.dispatchAbandoned", { error: detail.dispatch_error })
                        : t("pm.task.dispatchFailed", {
                            attempts: detail.dispatch_attempts,
                            error: detail.dispatch_error,
                          })}
                    </p>
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

              {/* 三个下拉一行 — 原来是 2 列网格，负责人被挤到日期那一行 */}
              <div className="grid grid-cols-3 gap-3">
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
              </div>

              {/* 两个时间一行 */}
              <div className="grid grid-cols-2 gap-3">
                <Field label={t("pm.task.startDate")}>
                  <StyledInput
                    type="date"
                    value={form.values.startAt}
                    onChange={(e) => form.setStartAt(e.target.value)}
                  />
                </Field>
                <Field label={t("pm.task.dueDate")}>
                  <StyledInput
                    type="date"
                    value={form.values.dueAt}
                    onChange={(e) => form.setDueAt(e.target.value)}
                  />
                </Field>
              </div>

              <Field label={t("pm.task.description")}>
                <StyledTextarea
                  value={form.values.description}
                  onChange={(e) => form.setDescription(e.target.value)}
                  rows={3}
                  placeholder={t("pm.task.descriptionPlaceholder")}
                />
              </Field>

              <Field label={t("pm.task.parentTask")}>
                <Dropdown
                  value={form.values.parentId}
                  onChange={form.setParentId}
                  options={form.parentOptions}
                />
              </Field>

              <Field label={t("pm.task.dependencies")}>
                {form.dependencyOptions.length === 0 ? (
                  <p className="text-xs text-text-tertiary">
                    {detail && detail.depends_on.length > 0
                      ? detail.depends_on
                          .map((d) => `${titleOf(d.task_id) ?? d.task_id}（${d.kind}）`)
                          .join("、")
                      : t("pm.task.noCandidates")}
                  </p>
                ) : (
                  <OverlayScrollbar
                    maxHeight="7rem"
                    containerClassName="rounded-md border border-border-outer"
                    className="space-y-1 p-2"
                  >
                    {form.dependencyOptions.map((opt) => (
                      <label
                        key={opt.value}
                        className="flex cursor-pointer items-center gap-2 text-xs text-text-secondary"
                      >
                        <input
                          type="checkbox"
                          checked={form.values.dependsOn.includes(opt.value)}
                          onChange={() => form.toggleDependency(opt.value)}
                          className="accent-[var(--color-accent)]"
                        />
                        <span className="min-w-0 flex-1 truncate">{opt.label}</span>
                      </label>
                    ))}
                  </OverlayScrollbar>
                )}
              </Field>

              {/* ── 以下三段需要已落盘的 task id ── */}
              {hasRow && detail && (
                <>
                  <SectionDivider label={t("pm.task.attachments")} />
                  <Attachments
                    attachments={attachments}
                    uploading={uploading}
                    fileInputRef={fileInputRef}
                    onUpload={handleUpload}
                    onDelete={(aid) => setConfirmDeleteAtt(aid)}
                  />

                  <SectionDivider label={t("pm.task.subtasks")} />
                  <div>
                    <SubtaskTree
                      rootId={detail.id}
                      onSelect={(id) => navigate(() => void openTask(id))}
                    />
                    <button
                      type="button"
                      onClick={() =>
                        navigate(() =>
                          openCreate({
                            projectId: detail.project_id,
                            parentId: detail.id,
                            returnTo: detail.id,
                          }),
                        )
                      }
                      className="mt-2 w-full rounded-md border border-dashed border-zinc-300 px-3 py-1.5 text-xs text-text-tertiary hover:border-[var(--color-accent)] hover:text-[var(--color-accent)] dark:border-zinc-600"
                    >
                      + {t("pm.task.addSubtask")}
                    </button>
                  </div>
                </>
              )}
            </div>
          </OverlayScrollbar>

          {/* 底部操作 — 删除居左，与保存物理隔离 */}
          <footer className="flex shrink-0 items-center gap-2 border-t border-border-divider px-5 min-h-[var(--ui-dialog-zone-h)]">
            {hasRow && (
              // Same destructive treatment as the memory panel's Delete Node
              // (MemoryNodeDetail.tsx): bordered, tinted, iconed. A bare red
              // text link next to two solid buttons reads as a third neutral
              // action, which is the wrong thing to be next to 保存.
              <button
                type="button"
                onClick={() => setConfirmDelete(true)}
                className="inline-flex items-center justify-center gap-1 rounded-md border border-red-200 bg-red-50 px-3 py-1.5 text-xs font-medium text-red-700 hover:bg-red-100 dark:border-red-900 dark:bg-red-950 dark:text-red-400 dark:hover:bg-red-900"
              >
                <Trash2 className="h-3 w-3" aria-hidden />
                {t("pm.task.delete")}
              </button>
            )}
            <div className="flex-1" />
            <button
              type="button"
              onClick={() => navigate()}
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
          setConfirmDiscard(false);
          const after = pendingDiscard;
          setPendingDiscard(null);
          if (after) after();
          else navigate();
        }}
        onCancel={() => {
          setConfirmDiscard(false);
          setPendingDiscard(null);
        }}
      />
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
              className="group relative overflow-hidden rounded-md border border-border-outer bg-card-surface"
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

/** Board-derived task id -> title, for parent / dependency labels. */
function useTaskTitleResolver() {
  const tasks = usePmBoardStore((s) => s.tasks);
  return useCallback(
    (id?: string | null) => (id ? tasks.find((t) => t.id === id)?.title ?? null : null),
    [tasks],
  );
}

function Field({
  label,
  required,
  children,
}: {
  label: string;
  required?: boolean;
  children: React.ReactNode;
}) {
  return (
    <label className="block">
      <span className="mb-1 block text-11 font-medium text-text-tertiary">
        {label}
        {required && <span className="ml-0.5 text-red-500">*</span>}
      </span>
      {children}
    </label>
  );
}
