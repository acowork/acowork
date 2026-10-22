/**
 * useDocTreeStore — doc 目录树状态 + 文档/目录 CRUD 协调。
 *
 * 目录树 = 按需加载：`nodes[dirId]` 缓存每目录的直接子项
 * （`GET /api/tree?dir_id=`，服务端 TreeNode 非递归）。展开未加载目录时
 * 拉取该层；结构变更（新建/改名/删除）成功后统一 `refreshDir` 重拉父层，
 * 保证与服务端一致（doc 树以文件系统为真相，不做本地乐观 patch）。
 *
 * 选中态：`openDocId` 驱动右侧编辑器；删除当前打开文档时自动清空。
 * `selectedNode` 记录树中最后被点击的节点（目录或文档），驱动右侧
 * 设置面板的权限卡片（设计 §9）。
 */

import { create } from "zustand";
import * as docApi from "../../lib/doc-api";
import { log } from "../../lib/logger";
import { DOC_ROOT_DIR_ID } from "../../lib/doc-types";
import type { DirMeta, DocMeta, DocTreeNode } from "../../lib/doc-types";
import { useDocEditorStore } from "./editorStore";

/** 树中选中的节点（权限卡片的作用对象，设计 §9） */
export interface DocSelectedNode {
  kind: PermissionKindLike;
  id: string;
  name: string;
}

type PermissionKindLike = "doc" | "dir";

interface DocTreeState {
  /** dir_id → 已加载直接子项 */
  nodes: Record<string, DocTreeNode>;
  /** dir_id → 展开 */
  expanded: Record<string, boolean>;
  /** dir_id → 首次/强制加载中 */
  loadingDirs: Record<string, boolean>;
  /** 最近一次结构操作错误（sidebar toast） */
  error: string | null;
  /** 根是否加载过 */
  rootReady: boolean;
  /** `refreshVisible` 进行中（防 30s 轮询与手动点击打架） */
  refreshingVisible: boolean;
  /** 树中最后点击的节点（目录或文档）；权限卡片作用对象 */
  selectedNode: DocSelectedNode | null;

  selectNode: (node: DocSelectedNode | null) => void;

  loadDir: (dirId: string, opts?: { force?: boolean }) => Promise<boolean>;
  toggleDir: (dirId: string) => Promise<void>;
  expandDir: (dirId: string) => Promise<void>;
  collapseDir: (dirId: string) => void;
  refreshDir: (dirId: string) => Promise<boolean>;
  /**
   * 强制重拉所有已展开目录的子项 — 专给 30s 轮询和手动刷新按钮用。
   *
   * 为何不用 `refreshDir(root)`：TreeNode 是非递归的，mcp 工具如果往已
   * 展开的子目录（"研发"之类）加文件，刷根目录只更新根的 dirs/files 列表，
   * 子目录内部 cache 仍是旧的，用户在树里看到的还是旧内容。
   *
   * 折叠目录不刷：折叠的目录用户看不见内容，刷它浪费请求；用户展开时
   * `toggleDir` 仍按需拉取（cache 命中直接复用，符合既有约定）。
   */
  refreshVisible: () => Promise<void>;

  createDir: (parentDirId: string, name: string) => Promise<DirMeta | null>;
  renameDir: (dirId: string, parentDirId: string, newName: string) => Promise<boolean>;
  deleteDir: (dirId: string, parentDirId: string) => Promise<boolean>;

  createDoc: (parentDirId: string, title: string) => Promise<DocMeta | null>;
  renameDoc: (docId: string, parentDirId: string, baseVersion: number, newTitle: string) => Promise<boolean>;
  deleteDoc: (docId: string, parentDirId: string) => Promise<boolean>;

  clearError: () => void;
  reset: () => void;
}

export const useDocTreeStore = create<DocTreeState>((set, get) => ({
  nodes: {},
  expanded: { [DOC_ROOT_DIR_ID]: true },
  loadingDirs: {},
  error: null,
  rootReady: false,
  refreshingVisible: false,
  selectedNode: null,

  selectNode: (node) => set({ selectedNode: node }),

  loadDir: async (dirId, { force = false } = {}): Promise<boolean> => {
    const cached = get().nodes[dirId];
    if (cached && !force) return true;
    if (get().loadingDirs[dirId]) return false;
    set((s) => ({ loadingDirs: { ...s.loadingDirs, [dirId]: true } }));
    try {
      const node = await docApi.getTree(dirId);
      set((s) => ({
        nodes: { ...s.nodes, [dirId]: node },
        loadingDirs: { ...s.loadingDirs, [dirId]: false },
        rootReady: s.rootReady || dirId === DOC_ROOT_DIR_ID,
      }));
      return true;
    } catch (e) {
      set((s) => ({ loadingDirs: { ...s.loadingDirs, [dirId]: false } }));
      log.warn(`[doc:tree] loadDir ${dirId} failed:`, e);
      return false;
    }
  },

  refreshDir: (dirId) => get().loadDir(dirId, { force: true }),

  refreshVisible: async () => {
    // 单次刷新防重入：30s 轮询和手动按钮可能并发；`loadDir` 内部还有
    // `loadingDirs[dirId]` 二次防御，避免同一目录并发请求。
    if (get().refreshingVisible) return;
    set({ refreshingVisible: true });
    try {
      const ids = Object.keys(get().expanded).filter((id) => get().expanded[id]);
      await Promise.all(ids.map((id) => get().loadDir(id, { force: true })));
    } finally {
      set({ refreshingVisible: false });
    }
  },

  toggleDir: async (dirId) => {
    const isOpen = get().expanded[dirId];
    if (isOpen) {
      set((s) => ({ expanded: { ...s.expanded, [dirId]: false } }));
      return;
    }
    // 展开：先加载成功再置展开（失败保持折叠）
    const ok = await get().loadDir(dirId);
    if (ok) {
      set((s) => ({ expanded: { ...s.expanded, [dirId]: true } }));
    }
  },

  expandDir: async (dirId) => {
    if (get().expanded[dirId]) return;
    await get().toggleDir(dirId);
  },

  collapseDir: (dirId) => {
    set((s) => ({ expanded: { ...s.expanded, [dirId]: false } }));
  },

  createDir: async (parentDirId, name) => {
    try {
      const dir = await docApi.createDir({ parent_dir_id: parentDirId, name });
      // 展开父目录（若折叠）并刷新
      set((s) => ({ expanded: { ...s.expanded, [parentDirId]: true } }));
      await get().refreshDir(parentDirId);
      // 自动展开新目录
      set((s) => ({ expanded: { ...s.expanded, [dir.dir_id]: true } }));
      return dir;
    } catch (e) {
      log.warn("[doc:tree] createDir failed:", e);
      set({ error: e instanceof Error ? e.message : String(e) });
      return null;
    }
  },

  renameDir: async (dirId, parentDirId, newName) => {
    try {
      await docApi.renameDir(dirId, { new_name: newName });
      const sel = get().selectedNode;
      if (sel?.kind === "dir" && sel.id === dirId) {
        set({ selectedNode: { ...sel, name: newName } });
      }
      await get().refreshDir(parentDirId);
      return true;
    } catch (e) {
      log.warn("[doc:tree] renameDir failed:", e);
      set({ error: e instanceof Error ? e.message : String(e) });
      return false;
    }
  },

  deleteDir: async (dirId, parentDirId) => {
    try {
      await docApi.deleteDir(dirId);
      // 目录级联删除（含其下所有文档）：移除该目录的展开态与缓存。
      // 编辑器若正打开被删目录下的文档，后续保存会因文档不存在而报错，
      // 由 editorStore.save 的错误路径提示用户（无法在此精确枚举被删 doc）。
      // 选中节点在被删子树内 → 清空（权限卡片不能指向鬼魂节点）。遍历必须在
      // 删缓存之前进行：从 dirId 往下走才能覆盖全部已加载后代（树节点
      // 只有加载后才能被点击选中，缓存遍历足以判定命中）。
      const sel = get().selectedNode;
      if (sel) {
        const stack = [dirId];
        let hit = sel.kind === "dir" && sel.id === dirId;
        while (stack.length && !hit) {
          const id = stack.pop()!;
          const n = get().nodes[id];
          if (!n) continue;
          if (sel.kind === "doc" && n.files.some((f) => f.doc_id === sel.id)) hit = true;
          if (sel.kind === "dir" && n.dirs.some((d) => d.dir_id === sel.id)) hit = true;
          n.dirs.forEach((d) => stack.push(d.dir_id));
        }
        if (hit) set({ selectedNode: null });
      }
      set((s) => {
        const expanded = { ...s.expanded };
        delete expanded[dirId];
        const nodes = { ...s.nodes };
        delete nodes[dirId];
        return { expanded, nodes };
      });
      await get().refreshDir(parentDirId);
      return true;
    } catch (e) {
      log.warn("[doc:tree] deleteDir failed:", e);
      set({ error: e instanceof Error ? e.message : String(e) });
      return false;
    }
  },

  createDoc: async (parentDirId, title) => {
    try {
      const meta = await docApi.createDoc({ parent_dir_id: parentDirId, title, content: "" });
      set((s) => ({ expanded: { ...s.expanded, [parentDirId]: true } }));
      await get().refreshDir(parentDirId);
      // 新建后自动在编辑器打开（dirty guard 由 editorStore.requestOpen 处理）
      await useDocEditorStore.getState().requestOpen(meta.doc_id);
      // 权限卡片跟随切换到新文档（设计 §9）
      set({ selectedNode: { kind: "doc", id: meta.doc_id, name: meta.name } });
      return meta;
    } catch (e) {
      log.warn("[doc:tree] createDoc failed:", e);
      set({ error: e instanceof Error ? e.message : String(e) });
      return null;
    }
  },

  renameDoc: async (docId, parentDirId, baseVersion, newTitle) => {
    try {
      await docApi.renameDoc(docId, baseVersion, newTitle);
      // 权限卡片跟随改名（id 不变，名字变了）
      const sel = get().selectedNode;
      if (sel?.kind === "doc" && sel.id === docId) {
        set({ selectedNode: { ...sel, name: newTitle } });
      }
      await get().refreshDir(parentDirId);
      return true;
    } catch (e) {
      log.warn("[doc:tree] renameDoc failed:", e);
      set({ error: e instanceof Error ? e.message : String(e) });
      return false;
    }
  },

  deleteDoc: async (docId, parentDirId) => {
    try {
      await docApi.deleteDoc(docId);
      // 若删除的是正在编辑的文档 → 关闭编辑器（防后续保存打已删文档）
      const ed = useDocEditorStore.getState();
      if (ed.doc?.meta.doc_id === docId) {
        ed.closeDoc();
      }
      // 选中节点被删 → 回退到打开的文档或清空（权限卡片不能指向鬼魂节点）
      const sel = get().selectedNode;
      if (sel?.kind === "doc" && sel.id === docId) {
        set({ selectedNode: null });
      }
      await get().refreshDir(parentDirId);
      return true;
    } catch (e) {
      log.warn("[doc:tree] deleteDoc failed:", e);
      set({ error: e instanceof Error ? e.message : String(e) });
      return false;
    }
  },

  clearError: () => set({ error: null }),

  reset: () =>
    set({
      nodes: {},
      expanded: { [DOC_ROOT_DIR_ID]: true },
      loadingDirs: {},
      error: null,
      rootReady: false,
      refreshingVisible: false,
      selectedNode: null,
    }),
}));
