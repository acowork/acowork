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
import type { PmAttachmentMeta, PmTaskResponse } from "../../lib/pm-types";

interface PmTaskDetailState {
  taskId: string | null;
  detail: PmTaskResponse | null;
  attachments: PmAttachmentMeta[];
  loading: boolean;
  /** 附件上传中（防止重复提交） */
  uploading: boolean;
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
  updateDetail: (patch: Partial<PmTaskResponse>) => void;
  clear: () => void;
}

/** The three faces of the one task dialog. */
export type TaskDialogMode = "edit" | "create";

export const usePmTaskDetailStore = create<PmTaskDetailState>((set, get) => ({
  taskId: null,
  detail: null,
  attachments: [],
  loading: false,
  uploading: false,
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
      const [detail, attachments] = await Promise.all([
        pmApi.getTask(taskId),
        pmApi.listTaskAttachments(taskId).catch(() => [] as PmAttachmentMeta[]),
      ]);
      set({ detail, attachments, projectId: detail.project_id, loading: false, error: null });
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
      loading: false,
      uploading: false,
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
    const taskId = get().taskId;
    if (!taskId) return;
    try {
      const [detail, attachments] = await Promise.all([
        pmApi.getTask(taskId),
        pmApi.listTaskAttachments(taskId).catch(() => [] as PmAttachmentMeta[]),
      ]);
      set({ detail, attachments });
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
      uploading: false,
      error: null,
      mode: null,
      projectId: null,
      createParentId: null,
      returnTo: null,
    }),
}));
