/**
 * usePmBoardStore — 看板任务数据 + 乐观更新。
 *
 * ⚠️ Selector 契约（强制）：
 * 任何 `usePmBoardStore((s) => ...)` 的 selector **必须返回稳定引用**
 * （store 字段或函数引用）。禁止在 selector 里 `new Map / new Set / filter / map`
 * 等创建新对象 —— zustand v5 + React 18 的 `useSyncExternalStore` 用
 * `Object.is` 比较 snapshot，新引用会触发 "getSnapshot should be cached"
 * 警告 + 无限重渲染 + "Maximum update depth exceeded"（KanbanBoard 曾踩过）。
 * 如需派生数据，请改成 `usePmBoardStore((s) => s.field)` + `useMemo`。
 *
 * 对齐 UX 设计 §6：
 * - 选中项目后加载 `TaskResponse[]`（含 parent_id/depth/is_blocked）
 * - 按列分组派生 + 看板树重建（parent_id → children）
 * - 状态拖动/审核/创建/编辑走乐观更新 + 失败回滚 + toast
 *
 * 看板列（对齐服务端 6 态，主看板展示 4 列）：
 *   pending / in_progress / submitted / done
 *   rejected / cancelled 单独收集（拒绝/取消辅助列，看板不主展示）
 */

import { create } from "zustand";
import * as pmApi from "../../lib/pm-api";
import { showToast } from "../../components/common/ToastProvider";
import { log } from "../../lib/logger";
import type { PmTaskResponse, TaskStatus } from "../../lib/pm-types";

/** 看板树节点：任务 + 其直接子任务列表 */
export interface PmBoardTree {
  task: PmTaskResponse;
  children: PmBoardTree[];
}

interface PmBoardState {
  projectId: string | null;
  tasks: PmTaskResponse[];
  loading: boolean;
  error: string | null;
  updatedAt: number | null;

  loadTasks: (pid: string, opts?: { silent?: boolean }) => Promise<void>;
  /** 按状态分组的扁平任务列表（主看板 4 列） */
  tasksByStatus: (status: TaskStatus) => PmTaskResponse[];
  /** 根任务（parent_id=null）按列分组 */
  rootTasksByStatus: (status: TaskStatus) => PmTaskResponse[];
  /** 列根集合：本列状态匹配 且（无父 或 父不在本列）——保证每个任务在列中只出现一次 */
  columnRoots: (status: TaskStatus) => PmTaskResponse[];
  /** 某任务的直接子任务（按 created_at 升序） */
  childrenOf: (taskId: string) => PmTaskResponse[];
  /** 构建整棵子树（递归） */
  buildTree: (taskId: string) => PmBoardTree | null;

  /** 状态拖动：乐观更新 + PATCH + 失败回滚。返回是否落定（失败为 false） */
  moveTask: (taskId: string, toStatus: TaskStatus) => Promise<boolean>;
  /** 审核（submitted → done/rejected）；comment 为拒绝理由（可选） */
  reviewTask: (taskId: string, approved: boolean, comment?: string) => Promise<boolean>;
  /** Reparent：乐观更新 parent + 失败回滚 */
  reparentTask: (taskId: string, newParent: string | null) => Promise<boolean>;
  /** 创建/编辑后 upsert 单条（服务端回写为准） */
  upsertTask: (task: PmTaskResponse) => void;
  /** 删除后移除 */
  removeTask: (taskId: string) => void;
  /** 全量重载（乐观失败后回滚用） */
  reload: () => Promise<void>;
  clear: () => void;
}

const CACHE_MS = 10_000;

export const usePmBoardStore = create<PmBoardState>((set, get) => {
  // 内存缓存：taskId → children（父→子，重建树用）
  let childrenCache = new Map<string, string[]>();

  function rebuildCache(tasks: PmTaskResponse[]) {
    const byId = new Map(tasks.map((t) => [t.id, t]));
    const cache = new Map<string, string[]>();
    for (const t of tasks) {
      if (t.parent_id && byId.has(t.parent_id)) {
        const list = cache.get(t.parent_id) ?? [];
        list.push(t.id);
        cache.set(t.parent_id, list);
      }
    }
    // 子任务按 created_at 升序
    for (const ids of cache.values()) {
      ids.sort((a, b) => {
        const ta = byId.get(a);
        const tb = byId.get(b);
        return (ta?.created_at ?? "").localeCompare(tb?.created_at ?? "");
      });
    }
    childrenCache = cache;
  }

  return {
    projectId: null,
    tasks: [],
    loading: false,
    error: null,
    updatedAt: null,

    loadTasks: async (pid, { silent = false } = {}) => {
      if (get().loading) return;
      if (!silent && get().projectId === pid && get().updatedAt) {
        const age = Date.now() - get().updatedAt!;
        if (age < CACHE_MS) return;
      }
      set({ loading: !silent, error: null, projectId: pid });
      try {
        const tasks = await pmApi.listProjectTasks(pid);
        rebuildCache(tasks);
        set({ tasks, loading: false, error: null, updatedAt: Date.now() });
      } catch (e) {
        set({ loading: false, error: e instanceof Error ? e.message : String(e) });
        log.warn("[pm:board] loadTasks failed:", e);
      }
    },

    tasksByStatus: (status) => get().tasks.filter((t) => t.status === status),

    rootTasksByStatus: (status) =>
      get().tasks.filter((t) => t.status === status && t.parent_id === null),

    /** 列根集合：本列状态匹配 且（无父 或 父不在本列）——保证每个任务在列中只出现一次 */
    columnRoots: (status) => {
      const byId = new Map(get().tasks.map((t) => [t.id, t]));
      return get().tasks.filter((t) => {
        if (t.status !== status) return false;
        if (t.parent_id === null) return true;
        const parent = byId.get(t.parent_id);
        return !parent || parent.status !== status;
      });
    },

    childrenOf: (taskId) => {
      const ids = childrenCache.get(taskId) ?? [];
      const byId = new Map(get().tasks.map((t) => [t.id, t]));
      return ids.map((id) => byId.get(id)).filter((t): t is PmTaskResponse => !!t);
    },

    buildTree: (taskId) => {
      const byId = new Map(get().tasks.map((t) => [t.id, t]));
      const build = (id: string): PmBoardTree | null => {
        const task = byId.get(id);
        if (!task) return null;
        const childIds = childrenCache.get(id) ?? [];
        return {
          task,
          children: childIds
            .map((cid) => build(cid))
            .filter((t): t is PmBoardTree => !!t),
        };
      };
      return build(taskId);
    },

    moveTask: async (taskId, toStatus) => {
      const prev = get().tasks;
      // 乐观更新：本地改 status（并同步 childrenCache 无需变更，parent 不变）
      set({
        tasks: prev.map((t) =>
          t.id === taskId ? { ...t, status: toStatus } : t,
        ),
      });
      try {
        const updated = await pmApi.updateTask(taskId, { status: toStatus });
        const next = get().tasks.map((t) =>
          t.id === taskId ? { ...t, ...updated, status: updated.status } : t,
        );
        set({ tasks: next, updatedAt: Date.now() });
        return true;
      } catch (e) {
        // 回滚
        set({ tasks: prev, error: e instanceof Error ? e.message : String(e) });
        showToast({ type: "error", message: `状态变更失败: ${e instanceof Error ? e.message : e}` });
        return false;
      }
    },

    reviewTask: async (taskId, approved, comment) => {
      const prev = get().tasks;
      // 乐观更新。服务端有两种审核语义，status 走向不同：
      //  - submitted（Agent 交活待验收）：approve → done
      //  - pending + review_status=pending（Agent 新建待批准开工）：approve 只翻
      //    review_status，status 留在 pending 等 Agent 自己 claim —— 若这里也乐观
      //    写成 done，卡片会闪跳到"已完成"列再弹回待办列。
      // 两种情况 reject 都是 → rejected。
      const cur = prev.find((t) => t.id === taskId);
      const keepsStatus = cur?.status === "pending";
      set({
        tasks: prev.map((t) => {
          if (t.id !== taskId) return t;
          if (approved && keepsStatus) return { ...t, review_status: "approved" };
          return { ...t, status: (approved ? "done" : "rejected") as TaskStatus };
        }),
      });
      try {
        const updated = await pmApi.reviewTask(taskId, approved, comment);
        const next = get().tasks.map((t) =>
          t.id === taskId ? { ...t, ...updated, status: updated.status } : t,
        );
        set({ tasks: next, updatedAt: Date.now() });
        showToast({ type: "success", message: approved ? "已批准" : "已拒绝" });
        return true;
      } catch (e) {
        set({ tasks: prev, error: e instanceof Error ? e.message : String(e) });
        showToast({ type: "error", message: `审核失败: ${e instanceof Error ? e.message : e}` });
        return false;
      }
    },

    reparentTask: async (taskId, newParent) => {
      const prev = get().tasks;
      // 乐观：更新 parent_id（若 newParent=null 则为根任务）
      set({
        tasks: prev.map((t) =>
          t.id === taskId ? { ...t, parent_id: newParent } : t,
        ),
      });
      try {
        await pmApi.reparentTask(taskId, newParent);
        // 深度/位置可能变化 → 全量刷新
        await get().reload();
        return true;
      } catch (e) {
        set({ tasks: prev, error: e instanceof Error ? e.message : String(e) });
        showToast({ type: "error", message: `移动失败: ${e instanceof Error ? e.message : e}` });
        return false;
      }
    },

    upsertTask: (task) => {
      set((s) => {
        const exists = s.tasks.some((t) => t.id === task.id);
        const tasks = exists
          ? s.tasks.map((t) => (t.id === task.id ? task : t))
          : [...s.tasks, task];
        rebuildCache(tasks);
        return { tasks, updatedAt: Date.now() };
      });
    },

    removeTask: (taskId) => {
      set((s) => {
        const tasks = s.tasks.filter((t) => t.id !== taskId);
        rebuildCache(tasks);
        return { tasks, updatedAt: Date.now() };
      });
    },

    reload: async () => {
      const pid = get().projectId;
      if (!pid) return;
      set({ loading: true });
      try {
        const tasks = await pmApi.listProjectTasks(pid);
        rebuildCache(tasks);
        set({ tasks, loading: false, error: null, updatedAt: Date.now() });
      } catch (e) {
        set({ loading: false, error: e instanceof Error ? e.message : String(e) });
      }
    },

    clear: () => {
      childrenCache = new Map();
      set({ projectId: null, tasks: [], updatedAt: null, error: null });
    },
  };
});
