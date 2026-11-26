/**
 * DocTreeSidebar — doc 视图左侧目录树（设计 §7）。
 *
 * - 根 = 文档库根（root 固定，不可改名/删除）；目录可展开/折叠（懒加载）。
 * - 行内操作：目录 → 新建子目录/新建文档/重命名/删除；文档 → 重命名/删除。
 * - 删除进回收站（ConfirmDialog 说明可恢复）；回收站入口在底部。
 * - 新建/重命名用 inline input（Enter 确认 / Esc 取消 / blur 提交）。
 *
 * a11y：树形结构 role="tree"，行可聚焦；方向键展开/折叠在 D2-7 细化。
 */

import { useEffect, useRef, useState } from "react";
import {
  BookOpen,
  ChevronRight,
  FilePlus2,
  FileText,
  Folder,
  FolderPlus,
  FolderOpen,
  Loader2,
  Pencil,
  Plus,
  RefreshCw,
  Trash2,
} from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { useDocTreeStore } from "../../stores/doc/treeStore";
import { useDocEditorStore } from "../../stores/doc/editorStore";
import { useDocHealthStore } from "../../stores/doc/healthStore";
import { useToast } from "../../components/common/ToastProvider";
import { ConfirmDialog } from "../../components/common/ConfirmDialog";
import { cn } from "../../lib/utils";
import { listRowHover, listRowSelected } from "../../lib/ui-styles";
import { CAPSULE_PANE_CN } from "../../components/common/capsule";
import { DOC_ROOT_DIR_ID } from "../../lib/doc-types";
import type { DirMeta, DocMeta, LibraryMeta } from "../../lib/doc-types";
import { LibraryIcon } from "./LibraryIcon";
import { DocLibraryDialog } from "./DocLibraryDialog";

/** inline 编辑态：新建或重命名 */
type InlineEdit =
  | { mode: "create"; kind: "dir" | "doc"; parentDirId: string }
  | { mode: "rename"; kind: "dir" | "doc"; id: string; parentDirId: string; value: string }
  | null;

/** 待确认删除目标（目录级联删除需强调） */
type DeleteTarget =
  | { kind: "doc"; doc: DocMeta; parentDirId: string }
  | { kind: "dir"; dir: DirMeta; parentDirId: string }
  | null;

export function DocTreeSidebar({ width }: { width?: number }) {
  const { t } = useTranslation();
  const toast = useToast();
  const healthy = useDocHealthStore((s) => s.healthy);
  const rootReady = useDocTreeStore((s) => s.rootReady);
  const nodes = useDocTreeStore((s) => s.nodes);
  const expanded = useDocTreeStore((s) => s.expanded);
  const loadingDirs = useDocTreeStore((s) => s.loadingDirs);
  const refreshingVisible = useDocTreeStore((s) => s.refreshingVisible);
  const refreshVisible = useDocTreeStore((s) => s.refreshVisible);
  const selectedDocId = useDocEditorStore((s) => s.doc?.meta.doc_id ?? null);
  const treeError = useDocTreeStore((s) => s.error);
  const clearTreeError = useDocTreeStore((s) => s.clearError);

  const [editing, setEditing] = useState<InlineEdit>(null);
  const [deleteTarget, setDeleteTarget] = useState<DeleteTarget>(null);
  // 文档库对话框：新建 / 编辑（名称、图标、描述、所有者）。
  const [libraryDialog, setLibraryDialog] = useState<
    | { mode: "create" }
    | { mode: "edit"; library: LibraryMeta }
    | null
  >(null);

  // 首次挂载：加载根（树根不自动展开，懒加载第一层）
  const loadDir = useDocTreeStore((s) => s.loadDir);
  useEffect(() => {
    if (!rootReady && healthy !== false) {
      void loadDir(DOC_ROOT_DIR_ID);
    }
  }, [rootReady, healthy, loadDir]);

  // store 层操作错误 → toast（错误已在 store 记录并 clear）
  useEffect(() => {
    if (!treeError) return;
    toast.addToast({ type: "error", message: treeError });
    clearTreeError();
  }, [treeError, toast, clearTreeError]);

  const handleCreate = (kind: "dir" | "doc", parentDirId: string) => {
    if (!healthy) return;
    setEditing({ mode: "create", kind, parentDirId });
  };
  const handleRename = (kind: "dir" | "doc", id: string, parentDirId: string, value: string) => {
    if (!healthy) return;
    setEditing({ mode: "rename", kind, id, parentDirId, value });
  };
  const requestDelete = (target: DeleteTarget) => {
    if (!healthy) return;
    setDeleteTarget(target);
  };

  return (
    <aside
      className={cn(CAPSULE_PANE_CN, "h-full shrink-0 bg-nav-surface text-xs")}
      style={{ width: width ?? 240 }}
      aria-label={t("doc.sidebarLabel")}
    >
      {/* ── 头部：标题 + 新建按钮 ─────────────────────────────
          高度绑 --ui-list-header-h，与 pm 搜索栏 / harness、settings
          标题栏同一高度，四个列表胶囊顶边才能横向对齐。 */}
      <div className="flex min-h-[var(--ui-list-header-h)] items-center gap-1 border-b border-border-divider px-3">
        <BookOpen className="mr-1 h-3.5 w-3.5 text-text-tertiary" aria-hidden />
        <span className="flex-1 truncate font-medium text-text-secondary ">
          {t("doc.title")}
        </span>
        {/* 根层只能是文档库：这里不再提供「根目录新建目录/文档」——
            新建文档库走下面这个 + 或底部的整宽按钮，对话框收齐图标/描述/成员/所有者。 */}
        <IconBtn
          label={t("doc.library.newLibrary")}
          disabled={healthy === false}
          onClick={() => setLibraryDialog({ mode: "create" })}
          title={t("doc.library.newLibrary")}
        >
          <Plus className="h-3.5 w-3.5" />
        </IconBtn>
        <IconBtn
          label={t("common.refresh")}
          disabled={healthy === false || refreshingVisible}
          onClick={() => void refreshVisible()}
          title={t("common.refresh")}
        >
          {refreshingVisible ? (
            <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
          ) : (
            <RefreshCw className="h-3.5 w-3.5" aria-hidden />
          )}
        </IconBtn>
      </div>

      {/* ── 树 ─────────────────────────────────────────────── */}
      <div className="min-h-0 flex-1 overflow-y-auto px-1 py-1" role="tree" aria-label={t("doc.treeLabel")}>
        <DirContents
          dirId={DOC_ROOT_DIR_ID}
          depth={0}
          nodes={nodes}
          expanded={expanded}
          loadingDirs={loadingDirs}
          selectedDocId={selectedDocId}
          editing={editing}
          onEditChange={setEditing}
          onRequestDelete={requestDelete}
          onCreate={handleCreate}
          onRename={handleRename}
          onLoadDir={loadDir}
          onEditLibrary={(library) => setLibraryDialog({ mode: "edit", library })}
        />
      </div>

      {/* ── 底部：新建文档库（与 ProjectSidebar 的底部 + 同一位置/同一语法） ── */}
      <div className="p-1.5">
        <button
          type="button"
          onClick={() => setLibraryDialog({ mode: "create" })}
          disabled={healthy === false}
          className="flex w-full items-center justify-center gap-1 rounded-md px-0 py-[var(--ui-btn-py)] text-xs font-medium text-text-secondary transition-colors hover:bg-[var(--color-accent)]/10 focus-visible:bg-nav-control disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:bg-transparent"
          aria-label={t("doc.library.newLibrary")}
          title={t("doc.library.newLibrary")}
        >
          <Plus className="h-3.5 w-3.5" />
          <span className="truncate">{t("doc.library.newLibrary")}</span>
        </button>
      </div>

      {/* ── 底部：离线提示（回收站入口已迁入右侧齿轮轨唤出的 DocSettingsPanel） ─────────────────────────── */}
      {healthy === false && (
        <div className="border-t border-border-divider p-1">
          <div className="rounded-md bg-warning-surface px-2 py-1 text-11 text-warning-quiet">
            {t("doc.offlineHint")}
          </div>
        </div>
      )}

      {/* ── 删除确认 ────────────────────────────────────────── */}
      <ConfirmDialog
        open={deleteTarget !== null}
        title={deleteTarget?.kind === "dir" ? t("doc.deleteDirTitle") : t("doc.deleteDocTitle")}
        message={
          deleteTarget?.kind === "dir"
            ? t("doc.deleteDirMsg", { name: deleteTarget.dir.name })
            : deleteTarget?.kind === "doc"
              ? t("doc.deleteDocMsg", { name: deleteTarget.doc.name })
              : ""
        }
        confirmLabel={t("common.delete")}
        destructive
        onCancel={() => setDeleteTarget(null)}
        onConfirm={async () => {
          const target = deleteTarget;
          setDeleteTarget(null);
          if (!target) return;
          if (target.kind === "doc") {
            const ok = await useDocTreeStore
              .getState()
              .deleteDoc(target.doc.doc_id, target.parentDirId);
            if (ok) toast.addToast({ type: "success", message: t("doc.deletedToTrash", { name: target.doc.name }) });
          } else {
            const ok = await useDocTreeStore
              .getState()
              .deleteDir(target.dir.dir_id, target.parentDirId);
            if (ok) toast.addToast({ type: "success", message: t("doc.deletedToTrash", { name: target.dir.name }) });
          }
        }}
      />

      {libraryDialog?.mode === "create" && (
        <DocLibraryDialog mode="create" onClose={() => setLibraryDialog(null)} />
      )}
      {libraryDialog?.mode === "edit" && (
        <DocLibraryDialog
          mode="edit"
          library={libraryDialog.library}
          onClose={() => setLibraryDialog(null)}
        />
      )}
    </aside>
  );
}

/** 行尾小图标按钮（hover 时出现；键盘聚焦也可见） */
function IconBtn({
  label,
  onClick,
  disabled,
  title,
  children,
}: {
  label: string;
  onClick: () => void;
  disabled?: boolean;
  title?: string;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      aria-label={label}
      title={title ?? label}
      disabled={disabled}
      onClick={(e) => {
        e.stopPropagation();
        onClick();
      }}
      className="rounded p-1 text-text-tertiary transition-colors hover:bg-[var(--color-accent)]/10 hover:text-text-secondary focus-visible:bg-nav-item-hover focus-visible:text-text-secondary disabled:opacity-30 dark:focus-visible:text-text"
    >
      {children}
    </button>
  );
}

/** 某个目录的内容（dirs + files）。dirId = root 时渲染为顶层列表 */
function DirContents({
  dirId,
  depth,
  nodes,
  expanded,
  loadingDirs,
  selectedDocId,
  editing,
  onEditChange,
  onRequestDelete,
  onCreate,
  onRename,
  onLoadDir,
  onEditLibrary,
}: {
  dirId: string;
  depth: number;
  nodes: Record<string, import("../../lib/doc-types").DocTreeNode>;
  expanded: Record<string, boolean>;
  loadingDirs: Record<string, boolean>;
  selectedDocId: string | null;
  editing: InlineEdit;
  onEditChange: (e: InlineEdit) => void;
  onRequestDelete: (t: DeleteTarget) => void;
  onCreate: (kind: "dir" | "doc", parentDirId: string) => void;
  onRename: (kind: "dir" | "doc", id: string, parentDirId: string, value: string) => void;
  onLoadDir: (dirId: string, opts?: { force?: boolean }) => Promise<boolean>;
  onEditLibrary: (library: LibraryMeta) => void;
}) {
  const node = nodes[dirId];
  const loading = loadingDirs[dirId];

  if (!node) {
    if (loading) {
      return (
        <div className="flex items-center gap-1 px-2 py-1 text-text-tertiary" style={{ paddingLeft: depth * 14 + 8 }}>
          <Loader2 className="h-3 w-3 animate-spin" aria-hidden />
          <span className="text-11">…</span>
        </div>
      );
    }
    return null;
  }

  const isEmpty = node.dirs.length === 0 && node.files.length === 0;
  const creatingHere = editing?.mode === "create" && editing.parentDirId === dirId;

  const isRootLevel = dirId === DOC_ROOT_DIR_ID;

  return (
    <div>
      {node.dirs.map((dir) =>
        isRootLevel ? (
          <LibraryRow
            key={dir.dir_id}
            library={dir}
            isLast={dir === node.dirs[node.dirs.length - 1]}
            nodes={nodes}
            expanded={expanded}
            loadingDirs={loadingDirs}
            selectedDocId={selectedDocId}
            editing={editing}
            onEditChange={onEditChange}
            onRequestDelete={onRequestDelete}
            onCreate={onCreate}
            onRename={onRename}
            onLoadDir={onLoadDir}
            onEditLibrary={onEditLibrary}
          />
        ) : (
          <DirRow
            key={dir.dir_id}
            dir={dir}
            parentDirId={dirId}
            depth={depth}
            nodes={nodes}
            expanded={expanded}
            loadingDirs={loadingDirs}
            selectedDocId={selectedDocId}
            editing={editing}
            onEditChange={onEditChange}
            onRequestDelete={onRequestDelete}
            onCreate={onCreate}
            onRename={onRename}
            onLoadDir={onLoadDir}
            onEditLibrary={onEditLibrary}
          />
        ),
      )}
      {node.files.map((file) => (
        <DocRow
          key={file.doc_id}
          doc={file}
          parentDirId={dirId}
          depth={depth}
          selectedDocId={selectedDocId}
          editing={editing}
          onEditChange={onEditChange}
          onRequestDelete={onRequestDelete}
          onRename={onRename}
        />
      ))}
      {isEmpty && dirId === DOC_ROOT_DIR_ID && <div className="h-2" />}
      {creatingHere && (
        <CreateRow
          kind={editing.kind}
          depth={dirId === DOC_ROOT_DIR_ID ? 0 : depth + 1}
          parentDirId={dirId}
          onDone={() => onEditChange(null)}
        />
      )}
    </div>
  );
}

/**
 * 文档库行（root 的直接子节点）。
 *
 * 视觉对齐 `ProjectSidebar` 的项目行，而不是树里的目录行：库是「像项目
 * 一样的容器」，两者在侧栏里就该长得一样 —— 同 `px-3 py-2.5`、同 18px
 * 图标位、同 `font-medium` 名称、同 `bg-accent-strong` 选中态、同
 * `row-divider-b` 分隔线。库**下面**才是目录树，所以这一行同时保留了树的
 * 展开/折叠箭头（目录行没有的 18px 图标位正好由库自己的图标占据）。
 *
 * 与目录行的行为差异都来自同一个数据事实 —— 库的元数据（名称/图标/描述/
 * 所有者）在 `DocLibraryDialog` 里一次收齐，所以这里没有 inline 重命名，
 * 「编辑」打开的是库对话框；描述不进行内（行高必须和项目行一致），挂在
 * `title` 上做悬浮提示。
 */
function LibraryRow({
  library,
  isLast,
  nodes,
  expanded,
  loadingDirs,
  selectedDocId,
  editing,
  onEditChange,
  onRequestDelete,
  onCreate,
  onRename,
  onLoadDir,
  onEditLibrary,
}: {
  library: DirMeta;
  isLast: boolean;
  nodes: Record<string, import("../../lib/doc-types").DocTreeNode>;
  expanded: Record<string, boolean>;
  loadingDirs: Record<string, boolean>;
  selectedDocId: string | null;
  editing: InlineEdit;
  onEditChange: (e: InlineEdit) => void;
  onRequestDelete: (t: DeleteTarget) => void;
  onCreate: (kind: "dir" | "doc", parentDirId: string) => void;
  onRename: (kind: "dir" | "doc", id: string, parentDirId: string, value: string) => void;
  onLoadDir: (dirId: string, opts?: { force?: boolean }) => Promise<boolean>;
  onEditLibrary: (library: LibraryMeta) => void;
}) {
  const { t } = useTranslation();
  const isOpen = !!expanded[library.dir_id];
  const childCount = nodes[library.dir_id]?.files.length ?? 0;
  // 选中态与项目行同语义：右侧面板此刻作用在这一个库上。取原始值，
  // 避免每次 store 变化都换一个新选择器结果。
  const selectedDirId = useDocTreeStore((s) =>
    s.selectedNode?.kind === "dir" ? s.selectedNode.id : null,
  );
  const selected = selectedDirId === library.dir_id;
  const childCreating =
    editing?.mode === "create" && editing.parentDirId === library.dir_id;

  return (
    <div role="treeitem" aria-expanded={isOpen} aria-selected={selected}>
      <div
        className={cn(
          "group relative flex w-full items-center gap-1 rounded-md pr-1 transition-colors duration-150",
          selected ? listRowSelected : cn("text-text-secondary", listRowHover),
          !isLast && !isOpen && "row-divider-b",
        )}
        title={library.description ?? undefined}
      >
        <button
          type="button"
          onClick={toggle}
          aria-current={selected ? "page" : undefined}
          className="flex min-w-0 flex-1 items-center gap-2 rounded-md px-2 py-2.5 text-left focus:outline-none focus-visible:ring-1 focus-visible:ring-current"
        >
          <ChevronRight
            className={cn(
              "h-3 w-3 shrink-0 transition-transform",
              isOpen && "rotate-90",
              selected ? "text-white/80" : "text-text-tertiary",
            )}
            aria-hidden
          />
          {/* 字形吃 `currentColor`：选中行刷白时图标跟着白（默认的 docs
              产品图标是位图，不受影响，本身就是深色描边）。 */}
          <LibraryIcon
            icon={library.icon}
            size={18}
            className={selected ? "text-white" : undefined}
          />
          <span
            className={cn(
              "min-w-0 flex-1 truncate font-medium",
              selected ? "text-white" : "",
            )}
          >
            {library.name}
          </span>
          {/* 库内文档数：与项目行的计数徽章同位置同语法 */}
          {childCount > 0 && (
            <span
              className={cn(
                "shrink-0 rounded-full px-1.5 text-10 group-hover:opacity-0",
                selected
                  ? "bg-white/20 text-white"
                  : "bg-surface text-text-tertiary",
              )}
            >
              {childCount}
            </span>
          )}
        </button>
        <span className="hidden shrink-0 items-center gap-0 group-hover:flex group-focus-within:flex">
          <IconBtn label={t("doc.newDoc")} onClick={() => onCreate("doc", library.dir_id)}>
            <FilePlus2 className="h-3 w-3" />
          </IconBtn>
          <IconBtn label={t("doc.newDir")} onClick={() => onCreate("dir", library.dir_id)}>
            <FolderPlus className="h-3 w-3" />
          </IconBtn>
          <IconBtn
            label={t("doc.library.editTitle")}
            onClick={() => onEditLibrary(library)}
          >
            <Pencil className="h-3 w-3" />
          </IconBtn>
          <IconBtn
            label={t("doc.delete")}
            onClick={() =>
              onRequestDelete({ kind: "dir", dir: library, parentDirId: DOC_ROOT_DIR_ID })
            }
          >
            <Trash2 className="h-3 w-3" />
          </IconBtn>
        </span>
      </div>

      {isOpen && (
        <DirContents
          dirId={library.dir_id}
          depth={1}
          nodes={nodes}
          expanded={expanded}
          loadingDirs={loadingDirs}
          selectedDocId={selectedDocId}
          editing={editing}
          onEditChange={onEditChange}
          onRequestDelete={onRequestDelete}
          onCreate={onCreate}
          onRename={onRename}
          onLoadDir={onLoadDir}
          onEditLibrary={onEditLibrary}
        />
      )}
      {childCreating && (
        <CreateRow
          kind={editing.kind}
          depth={1}
          parentDirId={library.dir_id}
          onDone={() => onEditChange(null)}
        />
      )}
    </div>
  );

  function toggle() {
    useDocTreeStore
      .getState()
      .selectNode({ kind: "dir", id: library.dir_id, name: library.name });
    if (isOpen) useDocTreeStore.getState().collapseDir(library.dir_id);
    else void useDocTreeStore.getState().expandDir(library.dir_id);
  }
}

/** 目录行 */
function DirRow({
  dir,
  parentDirId,
  depth,
  nodes,
  expanded,
  loadingDirs,
  selectedDocId,
  editing,
  onEditChange,
  onRequestDelete,
  onCreate,
  onRename,
  onLoadDir,
  onEditLibrary,
}: {
  dir: DirMeta;
  parentDirId: string;
  depth: number;
  nodes: Record<string, import("../../lib/doc-types").DocTreeNode>;
  expanded: Record<string, boolean>;
  loadingDirs: Record<string, boolean>;
  selectedDocId: string | null;
  editing: InlineEdit;
  onEditChange: (e: InlineEdit) => void;
  onRequestDelete: (t: DeleteTarget) => void;
  onCreate: (kind: "dir" | "doc", parentDirId: string) => void;
  onRename: (kind: "dir" | "doc", id: string, parentDirId: string, value: string) => void;
  onLoadDir: (dirId: string, opts?: { force?: boolean }) => Promise<boolean>;
  onEditLibrary: (library: LibraryMeta) => void;
}) {
  const { t } = useTranslation();
  const isOpen = !!expanded[dir.dir_id];
  // depth 0 = root 的直接子节点 = 文档库（有图标/描述/所有者）；
  // 再往下才是普通目录，不渲染库语义。
  const isLibrary = depth === 0;
  const isLoading = !!loadingDirs[dir.dir_id];
  const childCount = nodes[dir.dir_id]?.files.length ?? 0;

  // inline 重命名当前行
  if (editing?.mode === "rename" && editing.kind === "dir" && editing.id === dir.dir_id) {
    return (
      <RenameRow
        initial={dir.name}
        depth={depth}
        onSubmit={async (name) => {
          const ok = await useDocTreeStore.getState().renameDir(dir.dir_id, parentDirId, name);
          onEditChange(null);
          return ok;
        }}
        onCancel={() => onEditChange(null)}
      />
    );
  }

  const childCreating = editing?.mode === "create" && editing.parentDirId === dir.dir_id;

  return (
    <div role="treeitem" aria-expanded={isOpen} aria-selected={false}>
      <div
        className={cn(
          cn("group flex cursor-pointer items-center gap-0.5 rounded-md py-1 pr-1 text-text-secondary", listRowHover),
        )}
        style={{ paddingLeft: depth * 14 + 4 }}
      >
        <span
          role="button"
          tabIndex={0}
          aria-label={isOpen ? t("doc.collapse") : t("doc.expand")}
          className="flex min-w-0 flex-1 items-center gap-1.5 rounded py-0.5 select-none focus:outline-none focus-visible:ring-1 focus-visible:ring-current"
          onClick={() => void toggle()}
          onKeyDown={(e) => {
            // 键盘导航：→ 展开 / ← 折叠
            if (e.key === "ArrowRight" && !isOpen) {
              e.preventDefault();
              void toggle();
            } else if (e.key === "ArrowLeft" && isOpen) {
              e.preventDefault();
              void toggle();
            }
          }}
        >
          {/* 顶层 = 文档库：图标是库自己的（`DirMeta.icon`），展开态用
              文件夹开合表达；库内目录仍是文件夹图标。 */}
          {isLoading ? (
            <Loader2 className="h-3.5 w-3.5 shrink-0 animate-spin text-text-tertiary" aria-hidden />
          ) : isLibrary ? (
            <LibraryIcon icon={dir.icon} size={16} />
          ) : isOpen ? (
            <FolderOpen className="h-3.5 w-3.5 shrink-0 text-info" aria-hidden />
          ) : (
            <Folder className="h-3.5 w-3.5 shrink-0 text-info" aria-hidden />
          )}
          {/* `font-medium` 与 chat 侧边栏的 agent 名一致（见 SectionPane）——
              行高比 pm/harness/settings 密一档，字重保持同档才不会显轻。 */}
          <span className="truncate font-medium">{dir.name}</span>
          {isOpen && childCount > 0 && (
            <span className="ml-0.5 text-10 text-text-secondary ">{childCount}</span>
          )}
        </span>
        <span className="hidden shrink-0 items-center gap-0 group-hover:flex group-focus-within:flex">
          <IconBtn label={t("doc.newDoc")} onClick={() => onCreate("doc", dir.dir_id)}>
            <FilePlus2 className="h-3 w-3" />
          </IconBtn>
          <IconBtn label={t("doc.newDir")} onClick={() => onCreate("dir", dir.dir_id)}>
            <FolderPlus className="h-3 w-3" />
          </IconBtn>
          <IconBtn
            label={isLibrary ? t("doc.library.editTitle") : t("doc.rename")}
            onClick={() =>
              isLibrary
                ? onEditLibrary(dir)
                : onRename("dir", dir.dir_id, parentDirId, dir.name)
            }
          >
            <Pencil className="h-3 w-3" />
          </IconBtn>
          <IconBtn
            label={t("doc.delete")}
            onClick={() => onRequestDelete({ kind: "dir", dir, parentDirId })}
          >
            <Trash2 className="h-3 w-3" />
          </IconBtn>
        </span>
      </div>

      {isOpen && (
        <DirContents
          dirId={dir.dir_id}
          depth={depth + 1}
          nodes={nodes}
          expanded={expanded}
          loadingDirs={loadingDirs}
          selectedDocId={selectedDocId}
          editing={editing}
          onEditChange={onEditChange}
          onRequestDelete={onRequestDelete}
          onCreate={onCreate}
          onRename={onRename}
          onLoadDir={onLoadDir}
          onEditLibrary={onEditLibrary}
        />
      )}
      {childCreating && (
        <CreateRow
          kind={editing.kind}
          depth={depth + 1}
          parentDirId={dir.dir_id}
          onDone={() => onEditChange(null)}
        />
      )}
    </div>
  );

  async function toggle() {
    // 点击即选中（右侧权限卡片作用对象，设计 §9），展开/折叠照旧。
    useDocTreeStore.getState().selectNode({ kind: "dir", id: dir.dir_id, name: dir.name });
    if (isOpen) {
      useDocTreeStore.getState().collapseDir(dir.dir_id);
    } else {
      await useDocTreeStore.getState().expandDir(dir.dir_id);
    }
  }
}

/** 文档行 */
function DocRow({
  doc,
  parentDirId,
  depth,
  selectedDocId,
  editing,
  onEditChange,
  onRequestDelete,
  onRename,
}: {
  doc: DocMeta;
  parentDirId: string;
  depth: number;
  selectedDocId: string | null;
  editing: InlineEdit;
  onEditChange: (e: InlineEdit) => void;
  onRequestDelete: (t: DeleteTarget) => void;
  onRename: (kind: "dir" | "doc", id: string, parentDirId: string, value: string) => void;
}) {
  const { t } = useTranslation();
  const requestOpen = useDocEditorStore((s) => s.requestOpen);
  const selected = selectedDocId === doc.doc_id;

  if (editing?.mode === "rename" && editing.kind === "doc" && editing.id === doc.doc_id) {
    return (
      <RenameRow
        initial={doc.name}
        depth={depth}
        onSubmit={async (name) => {
          const ok = await useDocTreeStore
            .getState()
            .renameDoc(doc.doc_id, parentDirId, doc.version, name);
          // 改名后名称变了，编辑器打开的最新元数据也需刷新（重拉）
          if (ok && useDocEditorStore.getState().doc?.meta.doc_id === doc.doc_id) {
            await useDocEditorStore.getState().reload();
          }
          onEditChange(null);
          return ok;
        }}
        onCancel={() => onEditChange(null)}
      />
    );
  }

  return (
    <div role="treeitem" aria-selected={selected}>
      <div
        className={cn(
          cn("group flex items-center gap-1 rounded-md py-1 pr-1 text-text-secondary", listRowHover),
          selected &&
            // 选中态只吃背景色，字不跟着变 accent —— 文字 + 背景双高亮太刺眼
            // （`listRowSelected` 的约定：只染底色，label 保留自身前景色）。
            listRowSelected,
        )}
        style={{ paddingLeft: depth * 14 + 20 }}
      >
        <button
          type="button"
          onClick={() => {
            // 选中跟随打开（权限卡片切到本文档，设计 §9）
            useDocTreeStore
              .getState()
              .selectNode({ kind: "doc", id: doc.doc_id, name: doc.name });
            void requestOpen(doc.doc_id);
          }}
          className="flex min-w-0 flex-1 items-center gap-1.5 py-0.5 text-left select-none"
        >
          <FileText className="h-3.5 w-3.5 shrink-0 text-text-tertiary" aria-hidden />
          {/* 字重恒定，不靠 selected 切换 —— 否则同一棵树里目录恒 medium、
              文件只在选中时 medium，展开一层就忽粗忽细。选中态由行底色
              （accent/10 + accent 文字色）承担，见上方容器。 */}
          <span className="truncate font-medium">{doc.name}</span>
        </button>
        <span className="hidden shrink-0 items-center group-hover:flex group-focus-within:flex">
          <IconBtn
            label={t("doc.rename")}
            onClick={() => onRename("doc", doc.doc_id, parentDirId, doc.name)}
          >
            <Pencil className="h-3 w-3" />
          </IconBtn>
          <IconBtn
            label={t("doc.delete")}
            onClick={() => onRequestDelete({ kind: "doc", doc, parentDirId })}
          >
            <Trash2 className="h-3 w-3" />
          </IconBtn>
        </span>
      </div>
    </div>
  );
}

/** inline 新建行（Enter 提交 / Esc 取消） */
function CreateRow({
  kind,
  depth,
  parentDirId,
  onDone,
}: {
  kind: "dir" | "doc";
  depth: number;
  parentDirId: string;
  onDone: () => void;
}) {
  const { t } = useTranslation();
  const [value, setValue] = useState("");

  const submit = async () => {
    const name = value.trim();
    if (!name) {
      onDone();
      return;
    }
    if (kind === "dir") {
      await useDocTreeStore.getState().createDir(parentDirId, name);
    } else {
      await useDocTreeStore.getState().createDoc(parentDirId, name);
    }
    onDone();
  };

  return (
    <div style={{ paddingLeft: depth * 14 + 20 }} className="py-0.5 pr-1">
      <input
        autoFocus
        value={value}
        placeholder={kind === "dir" ? t("doc.dirNamePlaceholder") : t("doc.docNamePlaceholder")}
        onChange={(e) => setValue(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") void submit();
          else if (e.key === "Escape") onDone();
        }}
        onBlur={() => void submit()}
        className="w-full rounded border border-[var(--color-accent)] bg-input px-1.5 py-0.5 text-xs outline-none"
        aria-label={kind === "dir" ? t("doc.newDir") : t("doc.newDoc")}
      />
    </div>
  );
}

/** inline 重命名行 */
function RenameRow({
  initial,
  depth,
  onSubmit,
  onCancel,
}: {
  initial: string;
  depth: number;
  onSubmit: (name: string) => Promise<boolean>;
  onCancel: () => void;
}) {
  const { t } = useTranslation();
  const [value, setValue] = useState(initial);
  const inputRef = useRef<HTMLInputElement>(null);

  // 全选现有名字，方便直接替换
  useEffect(() => {
    inputRef.current?.focus();
    inputRef.current?.select();
  }, []);

  const submit = async () => {
    const name = value.trim();
    if (!name) {
      onCancel();
      return;
    }
    const ok = await onSubmit(name);
    if (!ok) onCancel();
  };

  return (
    <div style={{ paddingLeft: depth * 14 + 20 }} className="py-0.5 pr-1">
      <input
        ref={inputRef}
        value={value}
        onChange={(e) => setValue(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") void submit();
          else if (e.key === "Escape") onCancel();
        }}
        onBlur={() => void submit()}
        className="w-full rounded border border-[var(--color-accent)] bg-input px-1.5 py-0.5 text-xs outline-none"
        aria-label={t("doc.rename")}
      />
    </div>
  );
}
