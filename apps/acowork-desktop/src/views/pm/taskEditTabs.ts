/**
 * The tab model of the one task surface (`TaskEditDialog`).
 *
 * Why tabs: a task carries four unrelated kinds of content — its own fields,
 * its relations, its files, and its submission timeline — and stacking all of
 * them in one column made a 1000px-tall scroll where the thing the reader came
 * for (what the agent actually handed in) sat below a file grid. Same shell as
 * `GlobalSearchDialog`: a fixed-height strip under the title, and one body
 * region that owns the scrolling.
 *
 * Kept as a pure module for the same reason `visibleTabOrder` is pure in
 * `GlobalSearchDialog`: *which* tabs exist is a server fact, not a layout
 * preference, and a rule that decides whether a surface is reachable must be
 * testable without rendering the dialog and its whole store graph.
 */

export type TaskEditTab = "basic" | "related" | "files" | "submissions";

/** Canonical render order. The strip reads left to right in this order. */
export const TASK_EDIT_TAB_ORDER: readonly TaskEditTab[] = [
  "basic",
  "related",
  "files",
  "submissions",
];

/**
 * i18n key per tab. Total over `TaskEditTab`, so adding a tab is a compile
 * error here rather than a tab that paints its own key path.
 */
export const TASK_EDIT_TAB_I18N: Record<TaskEditTab, string> = {
  basic: "pm.task.tabs.basic",
  related: "pm.task.tabs.related",
  files: "pm.task.tabs.files",
  submissions: "pm.task.tabs.submissions",
};

/**
 * The tabs a given form can actually fill.
 *
 * `hasRow` is "the task id exists on the server", not "the user is editing":
 *  - 文件上传 posts to `POST /tasks/{tid}/attachments`.
 *  - 提交记录 reads `GET /tasks/{tid}/activity`.
 * A NEW task has no id yet, so both would be empty boxes that fail on the
 * first write. They are dropped from the strip entirely rather than shown as
 * disabled placeholders — a hidden tab beats a tab that errors (ADR-101's rule
 * for GlobalSearchDialog's memory tab).
 *
 * 关联任务 stays in both modes: the parent picker and the dependency picker
 * write create-time fields; only the child list needs a persisted row, and the
 * panel simply leaves it out.
 */
export function visibleTaskTabs(hasRow: boolean): TaskEditTab[] {
  return TASK_EDIT_TAB_ORDER.filter(
    (tab) => hasRow || (tab !== "files" && tab !== "submissions"),
  );
}

/**
 * Where the open tab lands when the strip changes underneath it — the row is
 * saved and closed, or a create form is re-opened. An unreachable tab falls
 * back to 基本信息, which always exists; never to a blank body.
 */
export function resolveTaskTab(
  current: TaskEditTab,
  visible: readonly TaskEditTab[],
): TaskEditTab {
  return visible.includes(current) ? current : "basic";
}
