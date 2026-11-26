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
import { withProjectIcon } from "../../lib/pm-icon-settings";
import type { PmMemberKind, PmProject, UpdateProjectInput } from "../../lib/pm-types";

/**
 * `createProject` 的一步到位草稿 —— 新建项目对话框一次收集的全部字段。
 *
 * 刻意与 REST 契约 `CreateProjectInput` 不同形：那份是「项目本体」的
 * 请求体，而这里描述的是用户意图（图标是本地文件、成员是待邀请集合），
 * 落到服务端要拆成 1 + N 次调用。
 */
export interface ProjectCreateDraft {
  title: string;
  description?: string;
  /** 自定义图标：上传端点按项目寻址，只能在项目存在之后落盘。 */
  iconFile?: File | null;
  /** 建群即邀请的成员（Agent 实例 / 人类账号，ADR-076 §决策 11）。 */
  members?: Array<{ instance_id: string; kind: PmMemberKind }>;
  /**
   * 群主（人类账号）。等于 `created_by` 或省略 = 不发起转让 —— 后端
   * `Project::owner()` 本来就回退到创建者。
   */
  ownerId?: string | null;
}

/** 创建结果：本体必然成功，附加步骤逐项报告哪些没落地。 */
export interface ProjectCreateOutcome {
  project: PmProject;
  warnings: Array<"icon" | "member" | "owner">;
}

interface PmProjectState {
  projects: PmProject[];
  selected: PmProject | null;
  loading: boolean;
  error: string | null;
  /**
   * HTTP status behind `error`, when the failure came from the PM API.
   *
   * The first load has to tell three situations apart, and the message text
   * cannot do it: `503` means the service is **still booting** (the Gateway
   * proxy has no upstream yet), which is normal for a few seconds on a cold
   * start and must not be rendered as "your projects are gone". `null` means
   * a network/DNS-level failure. Any other status is a real error.
   */
  errorStatus: number | null;
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
  /**
   * 一步到位创建项目：项目本体 + 图标 + 成员 + 群主，一次表单全部落地。
   *
   * 后三步只能排在创建之后 —— 图标上传与成员邀请端点都按 `pid` 寻址
   * （`POST /projects/:pid/icon`、`POST /projects/:pid/members`），而群主
   * 转让要求目标**已在册人类成员**（PM store `transfer_owner`）。附带
   * 的时序约束：邀请与转让都是 governance 行为（`require_governance`），
   * 创建者此刻正是 owner，所以**转让必须最后做**，否则后续请求全部 403。
   *
   * 本体失败返回 `null`（`error` 已写入 store）；本体成功则附加步骤的失败
   * 只降级成 `warnings[]`——项目已经存在，回滚它比报告失败更糟。
   */
  createProject: (draft: ProjectCreateDraft) => Promise<ProjectCreateOutcome | null>;
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
  errorStatus: null,
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
    set({ loading: !silent, error: null, errorStatus: null });
    try {
      const projects = await pmApi.listProjects();
      // 保留选中项（若仍存在）；否则自动选中第一个
      const selectedId = get().selected?.id;
      const selected =
        projects.find((p) => p.id === selectedId) ?? projects[0] ?? null;
      set({
        projects,
        selected,
        loading: false,
        error: null,
        errorStatus: null,
        updatedAt: Date.now(),
      });
      // 异步刷新计数徽章（不阻塞主流程）
      void get().refreshCounts();
    } catch (e) {
      set({
        loading: false,
        error: e instanceof Error ? e.message : String(e),
        errorStatus: e instanceof pmApi.PmApiError ? e.status : null,
      });
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

  createProject: async (draft) => {
    let project: PmProject;
    try {
      project = await pmApi.createProject({
        title: draft.title,
        description: draft.description ?? "",
      });
    } catch (e) {
      log.warn("[pm:project] createProject failed:", e);
      set({ error: e instanceof Error ? e.message : String(e) });
      return null;
    }
    set((s) => ({
      projects: [...s.projects, project],
      selected: project,
      updatedAt: Date.now(),
    }));
    void get().refreshCounts();

    const warnings: ProjectCreateOutcome["warnings"] = [];
    const warn = (w: ProjectCreateOutcome["warnings"][number]) => {
      if (!warnings.includes(w)) warnings.push(w);
    };

    // 图标：先上传落盘（服务端命名 `icon-NN.{ext}`），再把 ref 合并进
    // metadata 回写。PATCH 对 metadata 是**整体替换**，所以必须带旧字段。
    if (draft.iconFile) {
      try {
        const asset = await pmApi.uploadProjectIcon(project.id, draft.iconFile);
        const ok = await get().updateProjectMeta(project.id, {
          metadata: withProjectIcon(project.metadata, {
            kind: "custom",
            path: asset.relative_path,
          }),
        });
        if (!ok) warn("icon");
      } catch (e) {
        log.warn("[pm:project] createProject: icon upload failed:", e);
        warn("icon");
      }
    }

    // 成员：邀请失败（实例已卸载 / 目录不可用）不阻塞其余成员。
    for (const m of draft.members ?? []) {
      try {
        await get().addMember(project.id, m.instance_id, m.kind);
      } catch (e) {
        log.warn("[pm:project] createProject: addMember failed:", e);
        warn("member");
      }
    }

    // 群主：转让要求目标是已在册人类成员，不在集合里就先补一次邀请。
    // 放在最后 —— 转让成功后创建者不再是 owner，任何后续 governance
    // 请求都会被 `require_governance` 拒绝。
    const ownerId = draft.ownerId;
    if (ownerId && ownerId !== project.created_by) {
      try {
        const current = get().projects.find((p) => p.id === project.id);
        if (!current?.members.some((m) => m.instance_id === ownerId)) {
          await get().addMember(project.id, ownerId, "user");
        }
        await get().transferOwner(project.id, ownerId);
      } catch (e) {
        log.warn("[pm:project] createProject: transferOwner failed:", e);
        warn("owner");
      }
    }

    return { project, warnings };
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
    // Seed every known project first: the endpoint omits projects that hold
    // no tasks, and a missing key would render `undefined` in the badge.
    for (const p of projects) counts[p.id] = { total: 0, submitted: 0 };
    try {
      // One index-only call. This used to be `listProjectTasks(pid)` per
      // project — 83 task entities / 503 KB for the largest project in a
      // real store, fetched only to read `.length` and count `submitted`.
      // It also re-read every task.json twice on the server (list + the
      // per-task blocked_by check), so the board paid ~160 file opens per
      // render just to display two integers per project.
      const rows = await pmApi.listProjectCounts();
      for (const row of rows) {
        counts[row.project_id] = {
          total: row.total,
          submitted: row.submitted,
        };
      }
    } catch {
      // Keep the zeroed seed: a failed tally must not blank the board.
    }
    set({ counts });
  },

  clear: () => set({ projects: [], selected: null, counts: {}, updatedAt: null }),
}));
