/**
 * TaskDetailDialog — 任务详情居中对话框（T2-6，原 TaskDetailDrawer）。
 *
 * 布局对齐全局统一 dialog chrome（GlobalSearchDialog / TaskEditDialog 同款）：
 * - 遮罩 `fixed inset-0 z-50 flex items-center justify-center bg-modal-overlay`，
 *   mousedown 遮罩关闭；面板 stopPropagation
 * - 面板 `max-w-3xl max-h-[70vh] rounded-md border bg-modal-surface shadow-xl`
 * - Tabs（按需显示）：概述 / 描述 / 子任务 / 依赖 / 附件
 * - 子任务树 SubtaskTree（同 dialog 内导航到子任务）
 * - 底部：编辑 / 删除 / 状态快速流转
 * - 关闭时焦点回归原 Task Card
 *
 * 曾经的右侧滑出抽屉形态已退役：pm/doc 视图的 40px 右轨改为承载项目/文档
 * 设置面板（见 PmSettingsPanel / AppLayout），任务详情不再是右侧表面。
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { usePmTaskDetailStore } from "../../stores/pm/taskDetailStore";
import { usePmBoardStore } from "../../stores/pm/boardStore";
import { useTranslation } from "../../i18n/useTranslation";
import { PriorityBadge } from "./PriorityBadge";
import { TaskTypeIcon } from "./TaskTypeIcon";
import { SubtaskTree } from "./SubtaskTree";
import { ConfirmDialog } from "../../components/common/ConfirmDialog";
import { OverlayScrollbar } from "../../components/common/OverlayScrollbar";
import { showToast } from "../../components/common/ToastProvider";
import { attachmentUrl } from "../../lib/pm-api";
import { cn } from "../../lib/utils";
import type { PmTaskResponse, TaskStatus } from "../../lib/pm-types";
import { useActorLabel } from "./pmMembers";

interface TaskDetailDialogProps {
  taskId: string;
  onClose: () => void;
  onEdit?: (task: PmTaskResponse) => void;
  onAddSubtask?: (parentId: string) => void;
}

type TabId = "overview" | "description" | "subtasks" | "dependencies" | "attachments";

const TASK_STATUS_I18N: Record<TaskStatus, string> = {
  pending: "pm.board.pending",
  in_progress: "pm.board.inProgress",
  submitted: "pm.board.submitted",
  done: "pm.board.done",
  rejected: "pm.board.rejected",
  cancelled: "pm.board.cancelled",
};

function statusLabel(t: (k: string) => string, status: TaskStatus): string {
  return t(TASK_STATUS_I18N[status]);
}

function formatDateTime(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

/** 统一的居中遮罩 + 面板外壳（三种内容状态共用）。 */
function DialogShell({
  dialogRef,
  ariaLabel,
  onClose,
  children,
}: {
  dialogRef?: React.Ref<HTMLDivElement>;
  ariaLabel: string;
  onClose: () => void;
  children: React.ReactNode;
}) {
  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-modal-overlay"
      onMouseDown={onClose}
    >
      <div
        ref={dialogRef}
        tabIndex={-1}
        role="dialog"
        aria-modal="true"
        aria-label={ariaLabel}
        className="relative z-10 flex max-h-[70vh] w-full max-w-3xl flex-col overflow-hidden rounded-md border border-border-outer bg-modal-surface text-text shadow-xl outline-none"
        onMouseDown={(e) => e.stopPropagation()}
      >
        {children}
      </div>
    </div>
  );
}

export function TaskDetailDialog({ taskId, onClose, onEdit, onAddSubtask }: TaskDetailDialogProps) {
  const { t } = useTranslation();
  const detail = usePmTaskDetailStore((s) => s.detail);
  const attachments = usePmTaskDetailStore((s) => s.attachments);
  const loading = usePmTaskDetailStore((s) => s.loading);
  const uploading = usePmTaskDetailStore((s) => s.uploading);
  const error = usePmTaskDetailStore((s) => s.error);
  const openTask = usePmTaskDetailStore((s) => s.openTask);
  const refresh = usePmTaskDetailStore((s) => s.refresh);
  const uploadAttachment = usePmTaskDetailStore((s) => s.uploadAttachment);
  const deleteAttachment = usePmTaskDetailStore((s) => s.deleteAttachment);

  const [activeTab, setActiveTab] = useState<TabId>("overview");
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [confirmDeleteAtt, setConfirmDeleteAtt] = useState<string | null>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const dialogRef = useRef<HTMLDivElement>(null);
  const tabRefs = useRef<Array<HTMLButtonElement | null>>([]);

  const handleTabKeyDown = (e: React.KeyboardEvent, index: number) => {
    if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
    e.preventDefault();
    const dir = e.key === "ArrowRight" ? 1 : -1;
    const next = (index + dir + tabs.length) % tabs.length;
    setActiveTab(tabs[next].id);
    tabRefs.current[next]?.focus();
  };

  const tabs: { id: TabId; label: string }[] = [
    { id: "overview", label: t("pm.task.overview") },
    { id: "description", label: t("pm.task.description") },
  ];
  if (detail && detail.depends_on.length > 0) tabs.push({ id: "dependencies", label: t("pm.task.dependencies") });
  tabs.push({ id: "attachments", label: t("pm.task.attachments") });
  // 子任务 Tab：有子任务或允许添加时显示
  tabs.push({ id: "subtasks", label: t("pm.task.subtasks") });

  // 打开时拉取详情 + 聚焦对话框
  useEffect(() => {
    openTask(taskId);
    dialogRef.current?.focus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [taskId]);

  // ESC 关闭
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  const assigneeName = useActorLabel(detail?.assignee ?? null);
  const creatorName = useActorLabel(detail?.created_by ?? null);

  const removeTask = usePmBoardStore((s) => s.removeTask);

  const handleDelete = useCallback(async () => {
    setConfirmDelete(false);
    if (!detail) return;
    // 级联删除：从 api 直接调用（boardStore 无 delete，用 api + remove）
    const { deleteTask } = await import("../../lib/pm-api");
    try {
      await deleteTask(detail.id, { cascade: true });
      removeTask(detail.id);
      showToast({ type: "success", message: t("pm.task.taskDeleted") });
      onClose();
    } catch (e) {
      showToast({ type: "error", message: `删除失败: ${e instanceof Error ? e.message : e}` });
    }
  }, [detail, removeTask, onClose, t]);

  const handleDeleteAttachment = useCallback(async () => {
    if (!confirmDeleteAtt) return;
    const aid = confirmDeleteAtt;
    setConfirmDeleteAtt(null);
    const ok = await deleteAttachment(aid);
    if (ok) {
      showToast({ type: "success", message: t("pm.task.attachmentDeleted") });
    } else {
      showToast({ type: "error", message: t("pm.task.attachmentDeleteFailed") });
    }
  }, [confirmDeleteAtt, deleteAttachment, t]);

  if (loading && !detail) {
    return (
      <DialogShell ariaLabel={t("pm.task.details")} onClose={onClose}>
        <div className="p-6">
          <div className="h-5 w-48 animate-pulse rounded bg-zinc-100 dark:bg-zinc-800" />
          <div className="mt-4 h-3 w-32 animate-pulse rounded bg-zinc-100 dark:bg-zinc-800" />
          <div className="mt-6 h-40 animate-pulse rounded bg-zinc-100 dark:bg-zinc-800" />
        </div>
      </DialogShell>
    );
  }

  if (!detail) {
    return (
      <DialogShell ariaLabel={t("pm.task.details")} onClose={onClose}>
        <div className="p-6">
          <p className="text-xs text-text-tertiary">
            {error ?? t("pm.task.taskNotFound")}
          </p>
          <button
            type="button"
            onClick={onClose}
            className="mt-3 rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-100  dark:hover:bg-zinc-700"
          >
            {t("common.close") ?? "Close"}
          </button>
        </div>
      </DialogShell>
    );
  }

  return (
    <>
      <DialogShell dialogRef={dialogRef} ariaLabel={detail.title || t("pm.task.details")} onClose={onClose}>
        {/* 头部：标题 + 徽章 + 关闭
            flex-col：header 内是"标题行 + tab 条"两行堆叠。写成 flex 行
            容器时两者左右并排抢宽度——标题被压到逐字折行、tab 溢出面板。*/}
        <header className="flex shrink-0 flex-col border-b border-border-divider px-4 py-3 min-h-[var(--ui-dialog-zone-h)]">
          <div className="flex items-start justify-between gap-2">
            <div className="min-w-0 flex-1">
              <h2 className="break-words text-sm font-semibold text-text ">
                {detail.title}
              </h2>
              <div className="mt-1.5 flex flex-wrap items-center gap-x-2 gap-y-1 text-11 text-text-tertiary ">
                <TaskTypeIcon type={detail.type} className="text-xs" />
                <PriorityBadge priority={detail.priority} />
                <span>{statusLabel(t, detail.status)}</span>
              </div>
            </div>
            <button
              type="button"
              onClick={onClose}
              className="shrink-0 rounded-md px-1.5 py-0.5 text-sm text-text-tertiary hover:bg-zinc-200 hover:text-zinc-700 dark:hover:bg-zinc-700 dark:hover:text-zinc-200"
              aria-label={t("common.close") ?? "Close"}
            >
              ×
            </button>
          </div>

          {/* Tabs */}
          <div
            role="tablist"
            aria-label={t("pm.task.tabs")}
            className="mt-3 flex items-center gap-1 border-b border-border-divider min-h-[var(--ui-dialog-zone-h)]"
          >
            {tabs.map((tab, i) => (
              <button
                key={tab.id}
                ref={(el) => {
                  tabRefs.current[i] = el;
                }}
                type="button"
                role="tab"
                id={`pm-tab-${tab.id}`}
                aria-selected={activeTab === tab.id}
                aria-controls={`pm-panel-${tab.id}`}
                tabIndex={activeTab === tab.id ? 0 : -1}
                onClick={() => setActiveTab(tab.id)}
                onKeyDown={(e) => handleTabKeyDown(e, i)}
                className={cn(
                  "-mb-px border-b-2 px-2 py-1 text-11 font-medium transition-colors",
                  activeTab === tab.id
                    ? "border-[var(--color-accent)] text-text "
                    : "border-transparent text-text-tertiary hover:text-zinc-600 dark:hover:text-zinc-300",
                )}
              >
                {tab.label}
              </button>
            ))}
          </div>
        </header>

        {/* 内容区 */}
        <OverlayScrollbar fill="flex" className="px-4 py-3">
          {error && (
            <div className="mb-3 rounded-md bg-amber-50 px-3 py-2 text-xs text-amber-700 dark:bg-amber-950/40 dark:text-amber-300">
              {error}
              <button type="button" onClick={() => refresh()} className="ml-2 font-medium underline">
                {t("common.retry")}
              </button>
            </div>
          )}

          <div
            role="tabpanel"
            id={`pm-panel-${activeTab}`}
            aria-labelledby={`pm-tab-${activeTab}`}
            tabIndex={0}
          >
          {activeTab === "overview" && (
            <dl className="space-y-2 text-xs">
              <Field label={t("pm.task.assignee")} value={assigneeName ?? "—"} />
              <Field
                label={t("pm.task.dueDate")}
                value={detail.due_at ? formatDateTime(detail.due_at) : "—"}
              />
              <Field label={t("pm.task.createdBy")} value={creatorName ?? "—"} />
              <Field label={t("pm.task.created")} value={formatDateTime(detail.created_at)} />
              <Field label={t("pm.task.updated")} value={formatDateTime(detail.updated_at)} />
              {detail.parent_id && (
                <Field
                  label={t("pm.task.parentTask")}
                  value={String(detail.parent_id)}
                  className="break-all"
                />
              )}
            </dl>
          )}

          {activeTab === "description" && (
            <div className="text-xs leading-relaxed text-text-secondary ">
              {detail.description ? (
                <p className="whitespace-pre-wrap break-words">{detail.description}</p>
              ) : (
                <p className="italic text-text-tertiary">{t("pm.noDescription")}</p>
              )}
            </div>
          )}

          {activeTab === "dependencies" && (
            <ul className="space-y-1 text-xs">
              {detail.depends_on.length === 0 ? (
                <li className="text-text-tertiary">{t("pm.task.noDependencies")}</li>
              ) : (
                detail.depends_on.map((dep, i) => (
                  <li key={i} className="flex items-center gap-1.5 rounded-md bg-zinc-100 px-2 py-1.5 dark:bg-zinc-800">
                    <span aria-hidden>🔗</span>
                    <span className="break-all">{dep.task_id}</span>
                    <span className="ml-auto text-text-tertiary">({dep.kind})</span>
                  </li>
                ))
              )}
            </ul>
          )}

          {activeTab === "attachments" && (
            <div className="space-y-3">
              {/* 上传按钮 */}
              <div className="flex items-center gap-2">
                <button
                  type="button"
                  disabled={uploading}
                  onClick={() => fileInputRef.current?.click()}
                  className="inline-flex items-center gap-1.5 rounded-md border border-zinc-300 bg-white px-2.5 py-1.5 text-xs font-medium text-text-secondary transition-colors hover:bg-zinc-50 disabled:cursor-not-allowed disabled:opacity-50 dark:border-zinc-600 dark:bg-zinc-800  dark:hover:bg-zinc-700"
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
                    const meta = await uploadAttachment(file);
                    if (meta) {
                      showToast({ type: "success", message: t("pm.task.attachmentUploaded") });
                    } else {
                      showToast({ type: "error", message: t("pm.task.attachmentUploadFailed") });
                    }
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
                          className="flex h-20 flex-col items-center justify-center gap-0.5 text-text-tertiary hover:bg-zinc-50  dark:hover:bg-zinc-700"
                        >
                          <span className="text-lg" aria-hidden>📄</span>
                          <span className="max-w-full truncate px-1 text-10">{att.filename}</span>
                        </a>
                      )}
                      {/* 删除按钮（opacity 模式：hover/focus 可见，DOM 常驻可聚焦） */}
                      <button
                        type="button"
                        aria-label={t("pm.task.deleteAttachment")}
                        title={t("pm.task.deleteAttachment")}
                        onClick={() => setConfirmDeleteAtt(att.id)}
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
          )}

          {activeTab === "subtasks" && (
            <div>
              <SubtaskTree rootId={detail.id} onSelect={(id) => openTask(id)} />
              <button
                type="button"
                onClick={() => onAddSubtask?.(detail.id)}
                className="mt-2 w-full rounded-md border border-dashed border-zinc-300 px-3 py-1.5 text-xs text-text-tertiary hover:border-[var(--color-accent)] hover:text-[var(--color-accent)] dark:border-zinc-600"
              >
                + {t("pm.task.addSubtask")}
              </button>
            </div>
          )}
          </div>
        </OverlayScrollbar>

        {/* 底部操作 */}
        <footer className="flex shrink-0 items-center gap-2 border-t border-border-divider px-4 py-2.5">
          {onEdit && (
            <button
              type="button"
              onClick={() => onEdit(detail)}
              className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-100  dark:hover:bg-zinc-700"
            >
              {t("pm.task.edit")}
            </button>
          )}
          <button
            type="button"
            onClick={() => setConfirmDelete(true)}
            className="rounded-md px-3 py-1.5 text-xs font-medium text-red-600 hover:bg-red-50 dark:text-red-400 dark:hover:bg-red-950/40"
          >
            {t("pm.task.delete")}
          </button>

          <div className="ml-auto flex items-center gap-1.5">
            <StatusMoveButton
              task={detail}
              onMoved={refresh}
            />
          </div>
        </footer>
      </DialogShell>

      <ConfirmDialog
        open={confirmDelete}
        title={t("pm.task.deleteTaskTitle")}
        message={`${t("pm.task.deleteTaskCascadeDesc")} "${detail.title}"?`}
        confirmLabel={t("common.delete")}
        destructive
        onConfirm={handleDelete}
        onCancel={() => setConfirmDelete(false)}
      />

      <ConfirmDialog
        open={confirmDeleteAtt !== null}
        title={t("pm.task.deleteAttachmentTitle")}
        message={t("pm.task.deleteAttachmentDesc")}
        confirmLabel={t("common.delete")}
        destructive
        onConfirm={handleDeleteAttachment}
        onCancel={() => setConfirmDeleteAtt(null)}
      />
    </>
  );
}

function Field({ label, value, className }: { label: string; value: string; className?: string }) {
  return (
    <div className="flex gap-2">
      <dt className="w-20 shrink-0 text-text-tertiary ">{label}</dt>
      <dd className={cn("min-w-0 flex-1 text-text-secondary ", className)}>{value}</dd>
    </div>
  );
}

/** 状态快速流转按钮组：按当前状态显示下一步动作 */
function StatusMoveButton({ task, onMoved }: { task: PmTaskResponse; onMoved: () => void }) {
  const { t } = useTranslation();
  const moveTask = usePmBoardStore((s) => s.moveTask);

  // Agent 新建、人类尚未批准 → 服务端审批闸门会 409 review_not_approved。
  // 此时不渲染"移到进行中"按钮（卡片上有批准/拒绝按钮可用），别给一个
  // 必然失败的操作。批准后 review_status 变 approved，按钮自然回来。
  if (task.status === "pending" && task.review_status === "pending") return null;

  const next: Partial<Record<TaskStatus, TaskStatus>> = {
    pending: "in_progress",
    in_progress: "submitted",
    submitted: "done",
    rejected: "in_progress",
  };
  const target = next[task.status];
  if (!target) return null;

  const label =
    task.status === "submitted"
      ? t("pm.task.approve")
      : task.status === "rejected"
        ? t("pm.task.restart")
        : `${t("pm.task.moveTo")} ${t(`pm.board.${target === "in_progress" ? "inProgress" : target}`)}`;

  const handle = async () => {
    await moveTask(task.id, target);
    onMoved();
  };

  return (
    <button
      type="button"
      onClick={handle}
      className="rounded-md bg-zinc-800 px-3 py-1.5 text-xs font-medium text-white hover:bg-zinc-700 dark:bg-zinc-700 dark:hover:bg-zinc-600"
    >
      {label}
    </button>
  );
}
