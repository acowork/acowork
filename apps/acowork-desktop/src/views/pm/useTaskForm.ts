/**
 * useTaskForm — the task form's state, options and save path.
 *
 * Extracted from `TaskEditDialog` when the create/edit form and the task
 * detail were folded into one dialog (`TaskEditDialog`). The dialog owns chrome,
 * tabs and mode; this owns nothing but the form, so the same field set can
 * render read-only (view) or editable (edit/create) without a second copy of
 * the business rules living in the component.
 *
 * Save contract (verified against the server, not against the old client):
 * `CreateTask` on `POST /projects/:pid/tasks` accepts `assignee`, `due_at` and
 * `start_at` — see `CreateTask` in `core/acowork-pm/src/types.rs`. The previous
 * caller believed it did not and issued a follow-up `updateTask` for those three
 * fields, which cost a round-trip and, when that second call failed, left a
 * created-but-unassigned row behind (the catch only toasted). One call now.
 *
 * Parent task is the one field with its own endpoint (`reparentTask`) — the
 * server deliberately refuses `parent_id` on update (`types.rs`: "只能通过显式
 * 端点设置"), so an edit that moved the task still issues the second call.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createTask, reparentTask, updateTask } from "../../lib/pm-api";
import { useTranslation } from "../../i18n/useTranslation";
import { useAgentStore } from "../../stores/agentStore";
import { usePmBoardStore } from "../../stores/pm/boardStore";
import { usePmProjectStore } from "../../stores/pm/projectStore";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { memberOptions, usePmDirectoryUsers, useSelfAccount } from "./pmMembers";
import type { PmTaskResponse, Priority, TaskType } from "../../lib/pm-types";

// ── Date fields ───────────────────────────────────────────────────────

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

// ── Public shape ──────────────────────────────────────────────────────

export interface TaskFormValues {
  title: string;
  type: TaskType;
  priority: Priority;
  description: string;
  /** `""` = unassigned. Value is an `instance_id` / `user_id` (ADR-073). */
  assignee: string;
  /** Local calendar day (`YYYY-MM-DD`) or `""`. */
  dueAt: string;
  startAt: string;
  /** `""` = top level. */
  parentId: string;
  dependsOn: string[];
}

export type TaskSaveOutcome =
  | { ok: true; taskId: string }
  | { ok: false; reason: "title_required" }
  | { ok: false; reason: "failed"; message: string };

export interface UseTaskFormArgs {
  /** Target project. In edit mode this is the task's own project. */
  projectId: string;
  /** Present in edit mode; `null`/`undefined` means create. */
  initial?: PmTaskResponse | null;
  /** Create mode only: pre-selected parent (the "add subtask" entry). */
  defaultParentId?: string | null;
}

export interface TaskForm {
  values: TaskFormValues;
  setTitle: (v: string) => void;
  setType: (v: TaskType) => void;
  setPriority: (v: Priority) => void;
  setDescription: (v: string) => void;
  setAssignee: (v: string) => void;
  setDueAt: (v: string) => void;
  setStartAt: (v: string) => void;
  setParentId: (v: string) => void;
  toggleDependency: (id: string) => void;
  /** Assignee candidates = project members (server enforces the membership). */
  assigneeOptions: Array<{ value: string; label: string }>;
  parentOptions: Array<{ value: string; label: string }>;
  dependencyOptions: Array<{ value: string; label: string }>;
  /** False when the project has no members at all — the assignee row says why. */
  hasMembers: boolean;
  saving: boolean;
  /** Any field differs from what the row was opened with. */
  dirty: boolean;
  /** Throw away the draft and show the row's persisted values again. */
  reset: () => void;
  save: () => Promise<TaskSaveOutcome>;
}

// ── Hook ──────────────────────────────────────────────────────────────

export function useTaskForm({
  projectId,
  initial = null,
  defaultParentId = null,
}: UseTaskFormArgs): TaskForm {
  const isEdit = !!initial;
  const { t } = useTranslation();

  const agents = useAgentStore((s) => s.agents);
  const fetchAgents = useAgentStore((s) => s.fetchAgents);
  const boardTasks = usePmBoardStore((s) => s.tasks);
  const reloadBoard = usePmBoardStore((s) => s.reload);
  // Selector returns the array element (find), never a fresh object — the
  // zustand v5 snapshot contract.
  const project = usePmProjectStore((s) => s.projects.find((p) => p.id === projectId));
  const selfAccount = useSelfAccount();
  const localProfile = useUserProfileStore((s) => s.profile);
  const directory = usePmDirectoryUsers();

  const snapshot = useMemo<TaskFormValues>(
    () => ({
      title: initial?.title ?? "",
      type: initial?.type ?? "task",
      priority: initial?.priority ?? "normal",
      description: initial?.description ?? "",
      assignee: initial?.assignee ?? "",
      dueAt: isoToLocalDate(initial?.due_at),
      startAt: isoToLocalDate(initial?.start_at),
      parentId: initial?.parent_id ?? defaultParentId ?? "",
      dependsOn: initial?.depends_on?.map((d) => d.task_id) ?? [],
    }),
    [initial, defaultParentId],
  );

  const [values, setValues] = useState<TaskFormValues>(snapshot);
  const [saving, setSaving] = useState(false);

  // Re-key the form when the dialog switches rows (subtask-tree navigation,
  // create -> view after save). Without this the fields keep the previous
  // task's content — the exact class of bug a keyed surface avoids.
  //
  // Keyed on the row's IDENTITY, not on the object: a caller that re-parses or
  // re-maps its row on every render would otherwise reset the form mid-keystroke
  // and spin forever (adjust-state-during-render is React's sanctioned form of
  // this, and it costs no extra pass).
  const rowKey = `${initial?.id ?? "new"}|${defaultParentId ?? ""}`;
  const [keyedRow, setKeyedRow] = useState(rowKey);
  if (keyedRow !== rowKey) {
    setKeyedRow(rowKey);
    setValues(snapshot);
  }

  const patch = useCallback(
    (p: Partial<TaskFormValues>) => setValues((prev) => ({ ...prev, ...p })),
    [],
  );

  // Leaving an unsaved form has to put the row back, not keep the abandoned
  // draft: the form stays mounted across view <-> edit, so a "discard" that
  // only changed the mode would resurrect the typed-but-rejected values the
  // next time the user pressed 编辑.
  const reset = useCallback(() => setValues(snapshot), [snapshot]);

  // Assignee dropdown needs the agent registry.
  useEffect(() => {
    if (Object.keys(agents).length === 0) void fetchAgents();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // ADR-073: option value is `instance_id`, matching `task.assignee`.
  // ADR-076 §决策 11: candidates come from `project.members` (agents AND the
  // human creator), not from agentStore — otherwise the human member is
  // unassignable. The server validates assignee ∈ members.
  const assigneeOptions = useMemo(
    () => memberOptions(project?.members ?? [], agents, selfAccount, localProfile, directory),
    [project?.members, agents, selfAccount, localProfile, directory],
  );

  // Parent candidates: same project, minus the task itself and its direct
  // children (a task cannot parent its own child — that would cycle).
  const parentOptions = useMemo(() => {
    const candidates = isEdit
      ? boardTasks.filter(
          (task) => task.id !== initial?.id && task.parent_id !== initial?.id,
        )
      : boardTasks;
    return [
      { value: "", label: t("pm.task.noParent") },
      ...candidates.map((task) => ({ value: task.id, label: task.title })),
    ];
  }, [boardTasks, isEdit, initial, t]);

  const dependencyOptions = useMemo(
    () =>
      boardTasks
        .filter((task) => task.id !== initial?.id)
        .map((task) => ({ value: task.id, label: task.title })),
    [boardTasks, initial],
  );

  // Field-by-field compare would need a comparator per type; the values object
  // is flat scalars + one id array, so a structural compare is exact and cheap.
  const dirty = useMemo(
    () => JSON.stringify(values) !== JSON.stringify(snapshot),
    [values, snapshot],
  );

  // A second click while the first save is still in flight must not reach the
  // server: `saving` disables the button, but that state only lands on the
  // NEXT render, so two clicks inside one frame would both pass the guard and
  // create two rows. The in-flight promise is the real guard — a second
  // `save()` joins the run that is already on the wire instead of issuing
  // another request.
  const inflight = useRef<Promise<TaskSaveOutcome> | null>(null);

  const runSave = useCallback(async (): Promise<TaskSaveOutcome> => {
    const title = values.title.trim();
    if (!title) return { ok: false, reason: "title_required" };
    setSaving(true);
    try {
      const dependsOn = values.dependsOn.map((id) => ({ task_id: id, kind: "relates" as const }));
      const dueIso = values.dueAt ? localDateFieldToIso(values.dueAt) : null;
      const startIso = values.startAt ? localDateFieldToIso(values.startAt) : null;

      if (isEdit && initial) {
        await updateTask(initial.id, {
          title,
          description: values.description.trim() || undefined,
          type: values.type,
          priority: values.priority,
          assignee: values.assignee || null,
          due_at: dueIso,
          start_at: startIso,
          depends_on: dependsOn,
        });
        if (values.parentId !== (initial.parent_id ?? "")) {
          await reparentTask(initial.id, values.parentId || null);
        }
        await reloadBoard();
        return { ok: true, taskId: initial.id };
      }

      const created = await createTask(projectId, {
        title,
        description: values.description.trim() || undefined,
        type: values.type,
        priority: values.priority,
        assignee: values.assignee || null,
        due_at: dueIso,
        start_at: startIso,
        parent_task_id: values.parentId || null,
        depends_on: dependsOn,
      });
      await reloadBoard();
      return { ok: true, taskId: created.id };
    } catch (e) {
      return {
        ok: false,
        reason: "failed",
        message: e instanceof Error ? e.message : String(e),
      };
    } finally {
      setSaving(false);
    }
  }, [values, isEdit, initial, projectId, reloadBoard]);

  const save = useCallback((): Promise<TaskSaveOutcome> => {
    if (inflight.current) return inflight.current;
    const run = runSave();
    inflight.current = run;
    // Release the slot once the run settles, so the next save starts clean.
    void run.then(
      () => {
        inflight.current = null;
      },
      () => {
        inflight.current = null;
      },
    );
    return run;
  }, [runSave]);

  return {
    values,
    setTitle: (v) => patch({ title: v }),
    setType: (v) => patch({ type: v }),
    setPriority: (v) => patch({ priority: v }),
    setDescription: (v) => patch({ description: v }),
    setAssignee: (v) => patch({ assignee: v }),
    setDueAt: (v) => patch({ dueAt: v }),
    setStartAt: (v) => patch({ startAt: v }),
    setParentId: (v) => patch({ parentId: v }),
    toggleDependency: (id) =>
      setValues((prev) => ({
        ...prev,
        dependsOn: prev.dependsOn.includes(id)
          ? prev.dependsOn.filter((x) => x !== id)
          : [...prev.dependsOn, id],
      })),
    assigneeOptions,
    parentOptions,
    dependencyOptions,
    hasMembers: (project?.members.length ?? 0) > 0,
    saving,
    dirty,
    reset,
    save,
  };
}
