/**
 * usePmProjectStore — 项目列表 + 选中项。
 *
 * ⚠️ Selector 契约（强制）：
 * 任何 `usePmProjectStore((s) => ...)` 的 selector **必须返回稳定引用**
 * （store 字段或函数引用）。禁止在 selector 里 `new Map / new Set / filter / map`
 * 等创建新对象 —— zustand v5 + React 18 的 `useSyncExternalStore` 用
 * `Object.is` 比较 snapshot，新引用会触发 "getSnapshot should be cached"
 * 警告 + 无限重渲染 + "Maximum update depth exceeded"。
 * 如需派生数据，请改成 `usePmProjectStore((s) => s.field)` + `useMemo`。
 *
 * 对齐 UX 设计 §6.1：进入 /projects 路由触发加载；创建/删除/改名后主动 reload。
 * 数据常驻 store（离开路由不清空），后台 reload() 静默刷新。
 */

import { create } from "zustand";
import * as pmApi from "../../lib/pm-api";
import { log } from "../../lib/logger";
import type { PmMemberKind, PmProject, UpdateProjectInput } from "../../lib/pm-types";

interface PmProjectState {
  projects: PmProject[];
  selected: PmProject | null;
  loading: boolean;
  error: string | null;
  /** 上次成功加载时间戳（用于 30s 缓存） */
  updatedAt: number | null;
  /** 项目任务计数（侧边栏徽章）：pid → { total, submitted } */
  counts: Record<string, { total: number; submitted: number }>;

  // ── 新建项目对话框状态 ──────────────────────────────────────
  // 提升到全局 store，让多个入口（sidebar "+"、空状态中央按钮、未来
  // 命令面板 / 快捷键 / Agent MCP 工具）共享同一份 UI 状态，
  // 取代脆弱的 `document.getElementById(...)?.click()` DOM 反查。
  /** 新建项目对话框是否打开 */
  creating: boolean;
  /** 打开新建项目对话框（任意入口调用） */
  openCreate: () => void;
  /** 关闭新建项目对话框（取消 / 创建成功后调用） */
  closeCreate: () => void;

  loadProjects: (opts?: { silent?: boolean }) => Promise<void>;
  selectProject: (pid: string | null) => void;
  createProject: (title: string, description?: string) => Promise<PmProject | null>;
  updateProjectMeta: (pid: string, patch: UpdateProjectInput) => Promise<boolean>;
  /** 添加项目成员。`kind` 区分 Agent 实例 / 人类账号（ADR-076，缺省 agent）。
   * 成功返回 true，失败设置 error */
  addMember: (pid: string, instanceId: string, kind?: PmMemberKind) => Promise<boolean>;
  /** 移除项目成员。成员名下有未完成任务时服务端返回 409 */
  removeMember: (pid: string, instanceId: string) => Promise<boolean>;
  /** 转让群主。`newOwner` 须为当前人类成员；仅当前群主或 admin 可发起（否则服务端 403） */
  transferOwner: (pid: string, newOwner: string) => Promise<boolean>;
  deleteProject: (pid: string) => Promise<boolean>;
  refreshCounts: () => Promise<void>;
  clear: () => void;
}

const CACHE_MS = 30_000;

export const usePmProjectStore = create<PmProjectState>((set, get) => ({
  projects: [],
  selected: null,
  loading: false,
  error: null,
  updatedAt: null,
  counts: {},
  creating: false,

  openCreate: () => set({ creating: true }),
  closeCreate: () => set({ creating: false }),

  loadProjects: async ({ silent = false } = {}) => {
    // 缓存命中（非强制刷新且距上次 < 30s）→ 静默跳过
    if (!silent && get().updatedAt && Date.now() - get().updatedAt! < CACHE_MS) {
      return;
    }
    if (get().loading) return;
    set({ loading: !silent, error: null });
    try {
      const projects = await pmApi.listProjects();
      // 保留选中项（若仍存在）；否则自动选中第一个
      const selectedId = get().selected?.id;
      const selected =
        projects.find((p) => p.id === selectedId) ?? projects[0] ?? null;
      set({ projects, selected, loading: false, error: null, updatedAt: Date.now() });
      // 异步刷新计数徽章（不阻塞主流程）
      void get().refreshCounts();
    } catch (e) {
      set({ loading: false, error: e instanceof Error ? e.message : String(e) });
      log.warn("[pm:project] loadProjects failed:", e);
    }
  },

  selectProject: (pid) => {
    if (!pid) {
      set({ selected: null });
      return;
    }
    const found = get().projects.find((p) => p.id === pid) ?? null;
    set({ selected: found });
  },

  createProject: async (title, description = "") => {
    try {
      const project = await pmApi.createProject({ title, description });
      set((s) => ({
        projects: [...s.projects, project],
        selected: project,
        updatedAt: Date.now(),
      }));
      void get().refreshCounts();
      return project;
    } catch (e) {
      log.warn("[pm:project] createProject failed:", e);
      set({ error: e instanceof Error ? e.message : String(e) });
      return null;
    }
  },

  updateProjectMeta: async (pid, patch) => {
    try {
      const updated = await pmApi.updateProject(pid, patch);
      set((s) => ({
        projects: s.projects.map((p) => (p.id === pid ? updated : p)),
        selected: s.selected?.id === pid ? updated : s.selected,
        updatedAt: Date.now(),
      }));
      return true;
    } catch (e) {
      log.warn("[pm:project] updateProjectMeta failed:", e);
      set({ error: e instanceof Error ? e.message : String(e) });
      return false;
    }
  },

  addMember: async (pid, instanceId, kind) => {
    try {
      const updated = await pmApi.addProjectMember(pid, {
        instance_id: instanceId,
        ...(kind ? { kind } : {}),
      });
      set((s) => ({
        projects: s.projects.map((p) => (p.id === pid ? updated : p)),
        selected: s.selected?.id === pid ? updated : s.selected,
        updatedAt: Date.now(),
      }));
      return true;
    } catch (e) {
      log.warn("[pm:project] addMember failed:", e);
      set({ error: e instanceof Error ? e.message : String(e) });
      // re-throw：调用方（MemberManagerDialog）据此拿到 PmApiError 并映射本地化文案
      throw e;
    }
  },

  removeMember: async (pid, instanceId) => {
    try {
      const updated = await pmApi.removeProjectMember(pid, instanceId);
      set((s) => ({
        projects: s.projects.map((p) => (p.id === pid ? updated : p)),
        selected: s.selected?.id === pid ? updated : s.selected,
        updatedAt: Date.now(),
      }));
      return true;
    } catch (e) {
      log.warn("[pm:project] removeMember failed:", e);
      set({ error: e instanceof Error ? e.message : String(e) });
      // re-throw：调用方据此区分 409 member_has_open_tasks 等场景
      throw e;
    }
  },

  transferOwner: async (pid, newOwner) => {
    try {
      const updated = await pmApi.transferProjectOwner(pid, newOwner);
      set((s) => ({
        projects: s.projects.map((p) => (p.id === pid ? updated : p)),
        selected: s.selected?.id === pid ? updated : s.selected,
        updatedAt: Date.now(),
      }));
      return true;
    } catch (e) {
      log.warn("[pm:project] transferOwner failed:", e);
      set({ error: e instanceof Error ? e.message : String(e) });
      // re-throw：调用方（MemberManagerDialog）据此拿到 PmApiError 并映射本地化文案
      throw e;
    }
  },

  deleteProject: async (pid) => {
    try {
      await pmApi.deleteProject(pid);
      set((s) => {
        const projects = s.projects.filter((p) => p.id !== pid);
        const selected =
          s.selected?.id === pid ? (projects[0] ?? null) : s.selected;
        const counts = { ...s.counts };
        delete counts[pid];
        return { projects, selected, counts, updatedAt: Date.now() };
      });
      return true;
    } catch (e) {
      log.warn("[pm:project] deleteProject failed:", e);
      set({ error: e instanceof Error ? e.message : String(e) });
      return false;
    }
  },

  refreshCounts: async () => {
    const projects = get().projects;
    const counts: Record<string, { total: number; submitted: number }> = {};
    await Promise.all(
      projects.map(async (p) => {
        try {
          const tasks = await pmApi.listProjectTasks(p.id);
          counts[p.id] = {
            total: tasks.length,
            submitted: tasks.filter((t) => t.status === "submitted").length,
          };
        } catch {
          // 单项目拉取失败不阻塞整体
          counts[p.id] = { total: 0, submitted: 0 };
        }
      }),
    );
    set({ counts });
  },

  clear: () => set({ projects: [], selected: null, counts: {}, updatedAt: null }),
}));
