/**
 * TaskEditDialog — 创建 / 编辑任务对话框（T2-7）。
 *
 * 对齐 UX 设计 §3.5：
 * - 模态对话框，宽 640px
 * - 字段顺序：标题 → 类型/优先级 → 描述 → 指派/截止 → 父任务/依赖
 * - 仅"标题"必填，其余字段均可后补
 * - 父任务下拉：同项目内可选，含"无"（顶层任务）
 * - 指派下拉：仅列出该项目成员（联动指派），含"未指派"；无成员时提示先添加
 * - 依赖：简单多选（同项目任务）
 *
 * 服务端契约（T2-0 记录的偏差）：
 * - CreateTask 不含 assignee/due_at → 创建成功后用 updateTask 补充
 * - 附件上传服务端后补（T2-10），本期表单不含附件区
 */

import { useCallback, useEffect, useMemo, useState } from "react";
import { createTask, updateTask } from "../../lib/pm-api";
import { useAgentStore } from "../../stores/agentStore";
import { usePmBoardStore } from "../../stores/pm/boardStore";
import { usePmProjectStore } from "../../stores/pm/projectStore";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { useTranslation } from "../../i18n/useTranslation";
import { Dropdown } from "../../components/common/Dropdown";
import { OverlayScrollbar } from "../../components/common/OverlayScrollbar";
import { StyledInput, StyledTextarea } from "../../components/common/StyledInput";
import { showToast } from "../../components/common/ToastProvider";
import type { PmTaskResponse, Priority, TaskType } from "../../lib/pm-types";
import { memberOptions, usePmDirectoryUsers, useSelfAccount } from "./pmMembers";

/**
 * A `<input type="date">` value means *local* calendar day, so the stored
 * instant is local midnight: parse with a time component (`T00:00:00`, no
 * suffix → local per spec). `new Date("YYYY-MM-DD")` would parse as UTC
 * midnight and shift the rendered date a day behind for users west of UTC.
 * Paired with `isoToLocalDate` so an edit round-trip never drifts.
 */
export function localDateFieldToIso(dateField: string): string {
  return new Date(`${dateField}T00:00:00`).toISOString();
}

/** The local calendar day of a stored instant, for the date input. */
export function isoToLocalDate(iso?: string | null): string {
  if (!iso) return "";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso.slice(0, 10);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

export interface TaskEditDialogProps {
  mode: "create" | "edit";
  projectId: string;
  /** 编辑模式：初始任务 */
  initial?: PmTaskResponse | null;
  /** 创建模式：父任务 id（如从 Drawer 添加子任务） */
  defaultParentId?: string | null;
  onClose: () => void;
  onSaved?: () => void;
}

const TASK_TYPES: TaskType[] = ["task", "feature", "bug", "chore", "checkpoint", "milestone"];
const PRIORITIES: Priority[] = ["low", "normal", "high", "urgent"];

const TYPE_I18N: Record<TaskType, string> = {
  task: "pm.taskType.task",
  feature: "pm.taskType.feature",
  bug: "pm.taskType.bug",
  chore: "pm.taskType.chore",
  checkpoint: "pm.taskType.checkpoint",
  milestone: "pm.taskType.milestone",
};

const PRIORITY_I18N: Record<Priority, string> = {
  low: "pm.priority.low",
  normal: "pm.priority.normal",
  high: "pm.priority.high",
  urgent: "pm.priority.urgent",
};

export function TaskEditDialog({
  mode,
  projectId,
  initial,
  defaultParentId = null,
  onClose,
  onSaved,
}: TaskEditDialogProps) {
  const { t } = useTranslation();
  const agents = useAgentStore((s) => s.agents);
  const fetchAgents = useAgentStore((s) => s.fetchAgents);
  const boardTasks = usePmBoardStore((s) => s.tasks);
  const reload = usePmBoardStore((s) => s.reload);

  // 项目成员 — 联动指派：assignee 必须是项目成员。
  // selector 返回数组元素引用（find），非新建对象，符合 store 契约。
  const project = usePmProjectStore((s) => s.projects.find((p) => p.id === projectId));
  const memberCount = project?.members.length ?? 0;
  const selfAccount = useSelfAccount();
  // local 模式：`authStore.account` 恒 null，但 PM 仍可能有 `kind:"user"`
  // 成员（`instance_id` 是 "human"/"unknown" 哨兵），用本地 profile 命名。
  const localProfile = useUserProfileStore((s) => s.profile);
  const directory = usePmDirectoryUsers();

  const [title, setTitle] = useState(initial?.title ?? "");
  const [type, setType] = useState<TaskType>(initial?.type ?? "task");
  const [priority, setPriority] = useState<Priority>(initial?.priority ?? "normal");
  const [description, setDescription] = useState(initial?.description ?? "");
  const [assignee, setAssignee] = useState<string>(initial?.assignee ?? "");
  const [dueAt, setDueAt] = useState<string>(isoToLocalDate(initial?.due_at));
  const [startAt, setStartAt] = useState<string>(isoToLocalDate(initial?.start_at));
  const [parentId, setParentId] = useState<string>(initial?.parent_id ?? defaultParentId ?? "");
  const [dependsOn, setDependsOn] = useState<string[]>(
    initial?.depends_on?.map((d) => d.task_id) ?? [],
  );
  const [saving, setSaving] = useState(false);

  // 打开时确保 agent 列表已加载（用于指派下拉）
  useEffect(() => {
    if (Object.keys(agents).length === 0) void fetchAgents();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // ESC 关闭（保存中忽略）
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !saving) onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose, saving]);

  // 父任务候选：同项目内、排除自身（编辑时）与其后代
  const parentCandidates = useMemo(() => {
    if (mode === "edit" && initial) {
      return boardTasks.filter(
        (task) => task.id !== initial.id && task.parent_id !== initial.id,
      );
    }
    return boardTasks;
  }, [boardTasks, mode, initial]);

  // ADR-073: dropdown value 是 instance_id — 与 task.assignee 语义一致
  // （Agent → instance UUID；User → user_id，ADR-076 §决策 11）。
  // 联动指派：仅列出项目成员，服务端强校验 assignee ∈ members。成员来源
  // 必须是 project.members 本身，不能是 agentStore —— 否则 `kind: "user"`
  // 的成员（恒为项目创建者）不在下拉里，用户没法把任务指派给自己。
  const assigneeOptions = useMemo(
    () => memberOptions(project?.members ?? [], agents, selfAccount, localProfile, directory),
    [project?.members, agents, selfAccount, localProfile, directory],
  );

  const parentOptions = useMemo(
    () => [
      { value: "", label: t("pm.task.noParent") },
      ...parentCandidates.map((task) => ({
        value: task.id,
        label: task.title,
      })),
    ],
    [parentCandidates, t],
  );

  const depOptions = useMemo(
    () =>
      boardTasks
        .filter((task) => task.id !== initial?.id)
        .map((task) => ({
          value: task.id,
          label: task.title,
        })),
    [boardTasks, initial],
  );

  const handleSave = useCallback(async () => {
    const trimmed = title.trim();
    if (!trimmed) {
      showToast({ type: "warning", message: t("pm.taskTitleRequired") });
      return;
    }
    setSaving(true);
    try {
      if (mode === "create") {
        const created = await createTask(projectId, {
          title: trimmed,
          description: description.trim() || undefined,
          type,
          priority,
          parent_task_id: parentId || null,
          depends_on: dependsOn.map((id) => ({ task_id: id, kind: "relates" })),
        });
        // CreateTask 不含 assignee/due_at → PATCH 补充
        if (assignee || dueAt || startAt) {
          await updateTask(created.id, {
            assignee: assignee || null,
            due_at: dueAt ? localDateFieldToIso(dueAt) : null,
            start_at: startAt ? localDateFieldToIso(startAt) : null,
          });
        }
      } else if (initial) {
        await updateTask(initial.id, {
          title: trimmed,
          description: description.trim() || undefined,
          type,
          priority,
          assignee: assignee || null,
          due_at: dueAt ? localDateFieldToIso(dueAt) : null,
          start_at: startAt ? localDateFieldToIso(startAt) : null,
          depends_on: dependsOn.map((id) => ({ task_id: id, kind: "relates" })),
        });
        // 父任务变化 → reparent
        if (parentId !== initial.parent_id) {
          const { reparentTask } = await import("../../lib/pm-api");
          await reparentTask(initial.id, parentId || null);
        }
      }
      await reload();
      showToast({ type: "success", message: t("pm.task.taskSaved") });
      onSaved?.();
      onClose();
    } catch (e) {
      showToast({ type: "error", message: `${t("pm.task.taskSaveFailed")}: ${e instanceof Error ? e.message : e}` });
    } finally {
      setSaving(false);
    }
  }, [title, description, type, priority, assignee, dueAt, startAt, parentId, dependsOn, mode, initial, projectId, reload, onClose, onSaved, t]);

  const toggleDep = (id: string) => {
    setDependsOn((prev) =>
      prev.includes(id) ? prev.filter((x) => x !== id) : [...prev, id],
    );
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div className="absolute inset-0 bg-modal-overlay" onClick={onClose} />
      <div
        className="relative z-10 flex max-h-[90vh] w-full max-w-[640px] flex-col overflow-hidden rounded-md border border-border-outer bg-modal-surface shadow-xl"
        role="dialog"
        aria-modal="true"
        aria-labelledby="pm-edit-task-title"
      >
        {/* 头部 */}
        <header className="flex shrink-0 items-center justify-between border-b border-border-divider px-5 py-3 min-h-[var(--ui-dialog-zone-h)]">
          <h3 id="pm-edit-task-title" className="text-sm font-semibold">
            {mode === "create" ? t("pm.newTask") : t("pm.task.edit")}
          </h3>
          <button
            type="button"
            onClick={onClose}
            className="rounded-md px-1.5 py-0.5 text-sm text-text-tertiary hover:bg-zinc-200 hover:text-zinc-700 dark:hover:bg-zinc-700 dark:hover:text-zinc-200"
            aria-label={t("common.close") ?? "Close"}
          >
            ×
          </button>
        </header>

        {/* 表单区 */}
        <OverlayScrollbar fill="flex" className="px-5 py-4">
          <div className="space-y-3">
          <Field label={t("pm.task.title")} required>
            <StyledInput
              value={title}
              onChange={(e) => setTitle(e.target.value)}
              placeholder={t("pm.taskTitlePlaceholder")}
              autoFocus
            />
          </Field>

          <div className="grid grid-cols-2 gap-3">
            <Field label={t("pm.task.type")}>
              <Dropdown
                value={type}
                onChange={(v) => setType(v as TaskType)}
                options={TASK_TYPES.map((ttype) => ({
                  value: ttype,
                  label: t(TYPE_I18N[ttype]),
                }))}
              />
            </Field>
            <Field label={t("pm.task.priority")}>
              <Dropdown
                value={priority}
                onChange={(v) => setPriority(v as Priority)}
                options={PRIORITIES.map((p) => ({
                  value: p,
                  label: t(PRIORITY_I18N[p]),
                }))}
              />
            </Field>
          </div>

          <Field label={t("pm.task.description")}>
            <StyledTextarea
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              rows={3}
              placeholder={t("pm.task.descriptionPlaceholder")}
            />
          </Field>

          <div className="grid grid-cols-2 gap-3">
            <Field label={t("pm.task.assignee")}>
              {memberCount === 0 ? (
                <p className="rounded-md border border-dashed border-zinc-300 px-2 py-1.5 text-11 text-text-tertiary dark:border-zinc-600">
                  {t("pm.assigneeNoMembersHint")}
                </p>
              ) : (
                <Dropdown
                  value={assignee}
                  onChange={setAssignee}
                  options={assigneeOptions}
                  placeholder={{ value: "", label: t("pm.task.unassigned") }}
                />
              )}
            </Field>
            <Field label={t("pm.task.startDate")}>
              <StyledInput
                type="date"
                value={startAt}
                onChange={(e) => setStartAt(e.target.value)}
              />
            </Field>
            <Field label={t("pm.task.dueDate")}>
              <StyledInput
                type="date"
                value={dueAt}
                onChange={(e) => setDueAt(e.target.value)}
              />
            </Field>
          </div>

          <Field label={t("pm.task.parentTask")}>
            <Dropdown
              value={parentId}
              onChange={setParentId}
              options={parentOptions}
            />
          </Field>

          <Field label={t("pm.task.dependencies")}>
            {depOptions.length === 0 ? (
              <p className="text-xs text-text-tertiary">{t("pm.task.noCandidates")}</p>
            ) : (
              <OverlayScrollbar
                maxHeight="7rem"
                containerClassName="rounded-md border border-border-outer"
                className="space-y-1 p-2"
              >
                {depOptions.map((opt) => (
                  <label
                    key={opt.value}
                    className="flex cursor-pointer items-center gap-2 text-xs text-text-secondary "
                  >
                    <input
                      type="checkbox"
                      checked={dependsOn.includes(opt.value)}
                      onChange={() => toggleDep(opt.value)}
                      className="accent-[var(--color-accent)]"
                    />
                    <span className="min-w-0 flex-1 truncate">{opt.label}</span>
                  </label>
                ))}
              </OverlayScrollbar>
            )}
          </Field>
          </div>
        </OverlayScrollbar>

        {/* 底部操作 */}
        <footer className="flex shrink-0 items-center justify-end gap-2 border-t border-border-divider px-5 min-h-[var(--ui-dialog-zone-h)]">
          <button
            type="button"
            onClick={onClose}
            className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-100 disabled:opacity-50  dark:hover:bg-zinc-700"
            disabled={saving}
          >
            {t("common.cancel")}
          </button>
          <button
            type="button"
            onClick={handleSave}
            className="rounded-md btn-accent px-4 py-1.5 text-xs font-medium disabled:opacity-50"
            disabled={saving}
          >
            {saving ? t("common.saving") : t("pm.task.save")}
          </button>
        </footer>
      </div>
    </div>
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
      <span className="mb-1 block text-11 font-medium text-text-tertiary ">
        {label}
        {required && <span className="ml-0.5 text-red-500" aria-hidden>*</span>}
      </span>
      {children}
    </label>
  );
}

