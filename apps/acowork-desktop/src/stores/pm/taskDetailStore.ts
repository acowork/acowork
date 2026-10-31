/**
 * usePmTaskDetailStore — 任务详情（Drawer 数据）。
 *
 * ⚠️ Selector 契约（强制）：
 * 任何 `usePmTaskDetailStore((s) => ...)` 的 selector **必须返回稳定引用**
 * （store 字段或函数引用）。禁止在 selector 里 `new Map / new Set / filter / map`
 * 等创建新对象 —— zustand v5 + React 18 的 `useSyncExternalStore` 用
 * `Object.is` 比较 snapshot，新引用会触发 "getSnapshot should be cached"
 * 警告 + 无限重渲染 + "Maximum update depth exceeded"。
 * 如需派生数据，请改成 `usePmTaskDetailStore((s) => s.field)` + `useMemo`。
 *
 * 对齐 UX 设计 §6：打开 Drawer 时拉取详情；编辑/状态变更后刷新。
 * 关闭 Drawer 不清缓存（再次打开立即显示旧值 + 后台刷新）。
 */

import { create } from "zustand";
import * as pmApi from "../../lib/pm-api";
import { log } from "../../lib/logger";
import type { PmAttachmentMeta, PmTaskActivity, PmTaskResponse } from "../../lib/pm-types";

interface PmTaskDetailState {
  taskId: string | null;
  detail: PmTaskResponse | null;
  attachments: PmAttachmentMeta[];
  /**
   * 任务活动历史（提交 / 审核 / 回复），服务端按时间升序。
   *
   * 与 `attachments` 同构：都是任务详情面上按需拉取的独立集合（PM 把它们
   * 存在 `task.json` 之外，看板列表因此不会被提交正文撑大）。
   */
  activity: PmTaskActivity[];
  loading: boolean;
  /** 附件上传中（防止重复提交） */
  uploading: boolean;
  /** 回复发送中（防止重复提交） */
  replying: boolean;
  error: string | null;

  /**
   * Which face of the single task dialog is showing; `null` = closed.
   *
   * `create` is the only mode without a persisted task, which is also the only
   * mode where the server-backed tabs (attachments / subtasks) cannot exist:
   * the upload endpoint is task-scoped
   * (`POST /tasks/{tid}/attachments`, `core/acowork-pm/src/api/routes.rs`).
   */
  mode: TaskDialogMode | null;
  /** Target project — set on `openCreate`; derived from `detail` otherwise. */
  projectId: string | null;
  /** Pre-selected parent task, create mode only ("add subtask" entry). */
  createParentId: string | null;
  /**
   * Task to land on after a create finishes. Set when create was entered from
   * an open task (the "add subtask" path) so the dialog does not drop the
   * reader out of the parent they were looking at.
   */
  returnTo: string | null;

  openTask: (taskId: string) => Promise<void>;
  /** Open the dialog in create mode (no task row exists yet). */
  openCreate: (init: { projectId: string; parentId?: string | null; returnTo?: string | null }) => void;
  /** Switch faces (e.g. a saved create becoming an edit of the new row). */
  setMode: (mode: TaskDialogMode) => void;
  /** 后台静默刷新（Drawer 已打开时） */
  refresh: () => Promise<void>;
  /** 上传附件 → 成功追加到列表 */
  uploadAttachment: (file: File) => Promise<PmAttachmentMeta | null>;
  /** 删除附件 → 成功从列表移除 */
  deleteAttachment: (aid: string) => Promise<boolean>;
  /** 回复这条任务（写活动流，不动状态）→ 成功追加到 `activity` */
  addReply: (text: string) => Promise<boolean>;
  updateDetail: (patch: Partial<PmTaskResponse>) => void;
  clear: () => void;
}

/** The three faces of the one task dialog. */
export type TaskDialogMode = "edit" | "create";

export const usePmTaskDetailStore = create<PmTaskDetailState>((set, get) => {
  /**
   * 拉活动历史。失败按空处理：任务面的其它部分（表单 / 附件 / 子任务）不该
   * 因为读不到历史而打不开——对端是旧版 PM 服务时没有这个端点，也必须能用。
   */
  const loadActivity = (taskId: string) =>
    pmApi.listTaskActivity(taskId).catch(() => [] as PmTaskActivity[]);

  /**
   * 活动流只追加、条目内容永不改写，所以"条目身份集合相等"就等于"内容相等"。
   * 相等时保留旧数组引用：一次刷新不该让已经渲染的行重新挂载
   * （AGENTS.md「Identity-preserving store updates」）。
   */
  const isSameActivity = (prev: PmTaskActivity[], next: PmTaskActivity[]) =>
    prev.length === next.length &&
    prev.every((e, i) => e.id === next[i].id && e.created_at === next[i].created_at);

  return {
  taskId: null,
  detail: null,
  attachments: [],
  activity: [],
  loading: false,
  uploading: false,
  replying: false,
  error: null,
  mode: null,
  projectId: null,
  createParentId: null,
  returnTo: null,

  openTask: async (taskId) => {
    set({
      taskId,
      mode: "edit",
      projectId: null,
      createParentId: null,
      returnTo: null,
      loading: true,
      error: null,
    });
    try {
      const [detail, attachments, activity] = await Promise.all([
        pmApi.getTask(taskId),
        pmApi.listTaskAttachments(taskId).catch(() => [] as PmAttachmentMeta[]),
        loadActivity(taskId),
      ]);
      set({ detail, attachments, activity, projectId: detail.project_id, loading: false, error: null });
    } catch (e) {
      set({ loading: false, error: e instanceof Error ? e.message : String(e) });
      log.warn("[pm:detail] openTask failed:", e);
    }
  },

  openCreate: ({ projectId, parentId = null, returnTo = null }) => {
    // A fresh row owns no server state: no detail, no attachments. Everything
    // the create face must not offer (attachments / subtasks / delete) keys off
    // `taskId === null`, so there is no second source of truth to drift.
    set({
      taskId: null,
      detail: null,
      attachments: [],
      activity: [],
      loading: false,
      uploading: false,
      replying: false,
      error: null,
      mode: "create",
      projectId,
      createParentId: parentId,
      returnTo,
    });
  },

  setMode: (mode) => {
    const { taskId, mode: current } = get();
    if (!current) return;
    // `create` is entered through `openCreate` only — never by switching, and a
    // taskless dialog cannot switch into the faces that need an id.
    if (mode === "create") return;
    if (!taskId) return;
    set({ mode });
  },

  refresh: async () => {
    const { taskId, activity } = get();
    if (!taskId) return;
    try {
      const [detail, attachments, nextActivity] = await Promise.all([
        pmApi.getTask(taskId),
        pmApi.listTaskAttachments(taskId).catch(() => [] as PmAttachmentMeta[]),
        loadActivity(taskId),
      ]);
      set({
        detail,
        attachments,
        activity: isSameActivity(activity, nextActivity) ? activity : nextActivity,
      });
    } catch (e) {
      log.warn("[pm:detail] refresh failed:", e);
    }
  },

  uploadAttachment: async (file) => {
    const taskId = get().taskId;
    if (!taskId || get().uploading) return null;
    set({ uploading: true, error: null });
    try {
      const meta = await pmApi.uploadAttachment(taskId, file);
      set((s) => ({ attachments: [...s.attachments, meta], uploading: false }));
      return meta;
    } catch (e) {
      set({
        uploading: false,
        error: e instanceof Error ? e.message : String(e),
      });
      log.warn("[pm:detail] uploadAttachment failed:", e);
      return null;
    }
  },

  deleteAttachment: async (aid) => {
    if (get().uploading) return false;
    try {
      await pmApi.deleteAttachment(aid);
      set((s) => ({
        attachments: s.attachments.filter((a) => a.id !== aid),
      }));
      return true;
    } catch (e) {
      log.warn("[pm:detail] deleteAttachment failed:", e);
      return false;
    }
  },

  addReply: async (text) => {
    const taskId = get().taskId;
    if (!taskId || get().replying) return false;
    const trimmed = text.trim();
    if (!trimmed) return false;
    set({ replying: true, error: null });
    try {
      const entry = await pmApi.addTaskActivity(taskId, trimmed);
      set((s) => ({ activity: [...s.activity, entry], replying: false }));
      return true;
    } catch (e) {
      set({
        replying: false,
        error: e instanceof Error ? e.message : String(e),
      });
      log.warn("[pm:detail] addReply failed:", e);
      return false;
    }
  },

  updateDetail: (patch) => {
    const detail = get().detail;
    if (!detail) return;
    set({ detail: { ...detail, ...patch } });
  },

  clear: () =>
    set({
      taskId: null,
      detail: null,
      attachments: [],
      activity: [],
      uploading: false,
      replying: false,
      error: null,
      mode: null,
      projectId: null,
      createParentId: null,
      returnTo: null,
    }),
  };
});
