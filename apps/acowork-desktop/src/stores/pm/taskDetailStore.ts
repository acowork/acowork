/**
 * usePmTaskDetailStore — 任务弹框的**栈** + 每一层的行数据。
 *
 * ⚠️ Selector 契约（强制）：
 * 任何 `usePmTaskDetailStore((s) => ...)` 的 selector **必须返回稳定引用**
 * （store 字段或函数引用）。禁止在 selector 里 `new Map / new Set / filter / map`
 * 等创建新对象 —— zustand v5 + React 18 的 `useSyncExternalStore` 用
 * `Object.is` 比较 snapshot，新引用会触发 "getSnapshot should be cached"
 * 警告 + 无限重渲染 + "Maximum update depth exceeded"。
 * 如需派生数据，请改成 `usePmTaskDetailStore((s) => s.field)` + `useMemo`。
 *
 * 为什么是栈而不是"一格"：
 * 点子任务会**再开一个弹框**，盖在父任务弹框上面；关掉子任务要回到父任务，
 * 父任务的草稿还在。以前的实现只有一格（`taskId` / `detail`），点子任务等于
 * 把父任务整个替换掉，× 一关就全没了。
 *
 * 现在：
 * - `frames[]` —— 打开的弹框（底 → 顶），最后一个在最上面；
 * - `rows[key]` —— 每个弹框自己的行数据（详情 / 附件 / 活动流）。
 *   按 frame 的 key 归档，所以父任务那一层的数据不会被子任务覆盖，
 *   父任务的表单也不用重新挂载（草稿自然还在）。
 *
 * 行的写操作（刷新 / 上传 / 回复）都作用在**栈顶**那一层：只有最上面的弹框
 * 是可交互的，下面的被盖住。
 */

import { create } from "zustand";
import * as pmApi from "../../lib/pm-api";
import { log } from "../../lib/logger";
import type { PmAttachmentMeta, PmTaskActivity, PmTaskResponse } from "../../lib/pm-types";

/** The three faces of the one task dialog. */
export type TaskDialogMode = "edit" | "create";

/** One open task dialog. */
export interface TaskFrame {
  /** Identity of this open box — its row data is filed under this key. */
  key: string;
  mode: TaskDialogMode;
  /** The row this box shows; `null` in create mode. */
  taskId: string | null;
  /** Target project. Create mode carries it; edit mode derives it from the row. */
  projectId: string | null;
  /** Create mode only: the parent the new row is filed under. */
  createParentId: string | null;
}

/** The server state behind one frame. */
export interface TaskRow {
  detail: PmTaskResponse | null;
  attachments: PmAttachmentMeta[];
  activity: PmTaskActivity[];
  loading: boolean;
  uploading: boolean;
  replying: boolean;
  error: string | null;
}

interface PmTaskDetailState {
  /** The open boxes, bottom → top. The last one is the one on screen. */
  frames: TaskFrame[];
  /** Row data, filed under the frame's key. */
  rows: Record<string, TaskRow>;

  /** Open a task, replacing whatever is open (board / search entry points). */
  openTask: (taskId: string) => Promise<void>;
  /** Open a task ON TOP of the current one (navigating into a subtask). */
  pushTask: (taskId: string) => Promise<void>;
  /** Open a fresh create form, replacing whatever is open. */
  openCreate: (init: { projectId: string; parentId?: string | null }) => void;
  /** Open a fresh create form ON TOP of the current one ("add subtask"). */
  pushCreate: (init: { projectId: string; parentId?: string | null }) => void;
  /** Close the topmost box — the one below it becomes the visible one again. */
  pop: () => void;
  /** 后台静默刷新（栈顶那一行） */
  refresh: () => Promise<void>;
  /** 后台静默刷新（栈顶下面那一行）—— 子层改完后，下面的数据可能已过时 */
  refreshBelowTop: () => Promise<void>;
  /** 上传附件 → 成功追加到列表 */
  uploadAttachment: (file: File) => Promise<PmAttachmentMeta | null>;
  /** 删除附件 → 成功从列表移除 */
  deleteAttachment: (aid: string) => Promise<boolean>;
  /** 回复这条任务（写活动流，不动状态）→ 成功追加到 `activity` */
  addReply: (text: string) => Promise<boolean>;
  /** Close every open box. */
  clear: () => void;
}

const emptyRow = (): TaskRow => ({
  detail: null,
  attachments: [],
  activity: [],
  loading: false,
  uploading: false,
  replying: false,
  error: null,
});

/** Frame keys are handed out in order; they only have to be unique. */
let frameSeq = 0;
const nextKey = () => `tf${++frameSeq}`;

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

  /**
   * Patch one frame's row. A frame that was closed while its request was in
   * flight is gone from `rows` — the patch is dropped, so a late response
   * cannot resurrect a box the user already closed.
   */
  const patchRow = (
    key: string,
    patch: Partial<TaskRow> | ((row: TaskRow) => Partial<TaskRow>),
  ) =>
    set((s) => {
      const row = s.rows[key];
      if (!row) return s;
      const p = typeof patch === "function" ? patch(row) : patch;
      return { rows: { ...s.rows, [key]: { ...row, ...p } } };
    });

  /** Load one row into its frame. */
  const loadRow = async (key: string, taskId: string, showLoading: boolean) => {
    if (showLoading) patchRow(key, { loading: true, error: null });
    try {
      const [detail, attachments, activity] = await Promise.all([
        pmApi.getTask(taskId),
        pmApi.listTaskAttachments(taskId).catch(() => [] as PmAttachmentMeta[]),
        loadActivity(taskId),
      ]);
      patchRow(key, (row) => ({
        detail,
        attachments,
        activity: isSameActivity(row.activity, activity) ? row.activity : activity,
        loading: false,
        error: null,
      }));
    } catch (e) {
      patchRow(key, {
        loading: false,
        error: e instanceof Error ? e.message : String(e),
      });
      log.warn("[pm:detail] load failed:", e);
    }
  };

  /** The frame the writes act on: the topmost (only that one is clickable). */
  const topFrame = () => {
    const { frames } = get();
    return frames[frames.length - 1] ?? null;
  };

  const editFrame = (key: string, taskId: string): TaskFrame => ({
    key,
    mode: "edit",
    taskId,
    projectId: null,
    createParentId: null,
  });

  const createFrame = (
    key: string,
    projectId: string,
    createParentId: string | null,
  ): TaskFrame => ({
    key,
    mode: "create",
    taskId: null,
    projectId,
    createParentId,
  });

  return {
    frames: [],
    rows: {},

    openTask: (taskId) => {
      const key = nextKey();
      set({
        frames: [editFrame(key, taskId)],
        rows: { [key]: { ...emptyRow(), loading: true } },
      });
      return loadRow(key, taskId, false);
    },

    pushTask: (taskId) => {
      const key = nextKey();
      set((s) => ({
        frames: [...s.frames, editFrame(key, taskId)],
        rows: { ...s.rows, [key]: { ...emptyRow(), loading: true } },
      }));
      return loadRow(key, taskId, false);
    },

    openCreate: ({ projectId, parentId = null }) => {
      const key = nextKey();
      set({
        frames: [createFrame(key, projectId, parentId)],
        rows: { [key]: emptyRow() },
      });
    },

    pushCreate: ({ projectId, parentId = null }) => {
      const key = nextKey();
      set((s) => ({
        frames: [...s.frames, createFrame(key, projectId, parentId)],
        rows: { ...s.rows, [key]: emptyRow() },
      }));
    },

    pop: () =>
      set((s) => {
        if (s.frames.length <= 1) return { frames: [], rows: {} };
        const rows = { ...s.rows };
        delete rows[s.frames[s.frames.length - 1].key];
        return { frames: s.frames.slice(0, -1), rows };
      }),

    refresh: async () => {
      const top = topFrame();
      if (!top?.taskId) return;
      await loadRow(top.key, top.taskId, false);
    },

    /**
     * 刷新栈顶**下面**那一层（如果有）。
     *
     * 子任务那一层保存 / 删除之后，下面那层露出来给用户看 —— 它的行数据
     * 可能已经过时（子任务改名了、状态变了、被删了）。单层时代没有这个问题
     * （保存即关窗），栈化之后必须显式补一次刷新，否则用户看到的是上一秒
     * 的旧数据。
     */
    refreshBelowTop: async () => {
      const { frames } = get();
      const below = frames[frames.length - 2];
      if (!below?.taskId) return;
      await loadRow(below.key, below.taskId, false);
    },

    uploadAttachment: async (file) => {
      const top = topFrame();
      if (!top?.taskId || get().rows[top.key]?.uploading) return null;
      patchRow(top.key, { uploading: true, error: null });
      try {
        const meta = await pmApi.uploadAttachment(top.taskId, file);
        patchRow(top.key, (row) => ({
          attachments: [...row.attachments, meta],
          uploading: false,
        }));
        return meta;
      } catch (e) {
        patchRow(top.key, {
          uploading: false,
          error: e instanceof Error ? e.message : String(e),
        });
        log.warn("[pm:detail] uploadAttachment failed:", e);
        return null;
      }
    },

    deleteAttachment: async (aid) => {
      const top = topFrame();
      if (!top?.taskId || get().rows[top.key]?.uploading) return false;
      try {
        await pmApi.deleteAttachment(aid);
        patchRow(top.key, (row) => ({
          attachments: row.attachments.filter((a) => a.id !== aid),
        }));
        return true;
      } catch (e) {
        log.warn("[pm:detail] deleteAttachment failed:", e);
        return false;
      }
    },

    addReply: async (text) => {
      const top = topFrame();
      if (!top?.taskId || get().rows[top.key]?.replying) return false;
      const trimmed = text.trim();
      if (!trimmed) return false;
      patchRow(top.key, { replying: true, error: null });
      try {
        const entry = await pmApi.addTaskActivity(top.taskId, trimmed);
        patchRow(top.key, (row) => ({
          activity: [...row.activity, entry],
          replying: false,
        }));
        return true;
      } catch (e) {
        patchRow(top.key, {
          replying: false,
          error: e instanceof Error ? e.message : String(e),
        });
        log.warn("[pm:detail] addReply failed:", e);
        return false;
      }
    },

    clear: () => set({ frames: [], rows: {} }),
  };
});
