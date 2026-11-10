/**
 * ProjectBoard — 右侧任务容器（T2-3 + T2-7 + 列表视图）。
 *
 * 对齐 UX 设计 §3.1/§3.2：
 * - 组合 ProjectHeader（标题/描述/统计/视图切换/新建/菜单）+ 任务区
 * - 三态：未选中项目 → 空提示；加载中 → 骨架；错误 → 重试
 * - 新建任务：通过 onNewTask 上抛给 ProjectsView（打开 TaskEditDialog 新建态）
 *
 * 任务区有两种视图（`metadata.pmView`，见 `lib/pm-view-settings`）：
 * `board` = 四列状态卡片（默认，也是改动前的唯一形态），`list` = 一行一个
 * 任务。两种视图共用同一个 `boardStore`，本组件只负责选一个挂载。
 *
 * 三项列表偏好（视图 / 显示列 / 每页行数）都按项目存 metadata，写入是**乐观**
 * 的：`projectStore.updateProjectMeta` 等服务端返回后才更新 store，所以这里用
 * `pendingView` / `pendingColumns` / `pendingPageSize` 做本地镜像 —— 点击立即
 * 生效，PM 服务不可用时照样能切（列表是纯读视图，离线时看数据反而更有用），
 * 写失败只 toast：镜像撤销后视图自然回到 metadata 里的真值。
 *
 * 镜像的撤销按身份比对（见 `writePref`），连点两下时先返回的请求不会把后一次
 * 的切换抹掉；写成功后 store 更新与镜像撤销落在同一次提交里（都在 await 之后的
 * 同一个微任务），所以不会回闪。
 */

import {
  useCallback,
  useEffect,
  useState,
  type Dispatch,
  type SetStateAction,
} from "react";
import { usePmProjectStore } from "../../stores/pm/projectStore";
import { showToast } from "../../components/common/ToastProvider";
import { useTranslation } from "../../i18n/useTranslation";
import { CAPSULE_PANE_CN } from "../../components/common/capsule";
import { cn } from "../../lib/utils";
import { parseBoardPageSize } from "../../lib/pm-board-settings";
import {
  parsePmListColumns,
  parsePmListPageSize,
  parsePmView,
  withPmListColumns,
  withPmListPageSize,
  withPmView,
  type PmViewMode,
} from "../../lib/pm-view-settings";
import type { PmListColumnKey } from "../../lib/pm-list-columns";
import { ProjectHeader } from "./ProjectHeader";
import { KanbanBoard } from "./KanbanBoard";
import { TaskListView } from "./TaskListView";
import type { PmProject } from "../../lib/pm-types";

interface ProjectBoardProps {
  project: PmProject | null;
  loading?: boolean;
  error?: string | null;
  onRetry?: () => void;
  onOpenTask?: (taskId: string) => void;
  /** 新建任务（打开 TaskEditDialog 新建态） */
  onNewTask?: () => void;
}

export function ProjectBoard({
  project,
  loading,
  error,
  onRetry,
  onOpenTask,
  onNewTask,
}: ProjectBoardProps) {
  const { t } = useTranslation();
  const updateProjectMeta = usePmProjectStore((s) => s.updateProjectMeta);
  const openTask = onOpenTask ?? (() => {});
  const newTask = onNewTask ?? (() => {});

  // 服务端真值（metadata）+ 本地乐观镜像。镜像优先，直到 metadata 追上或写失败。
  const [pendingView, setPendingView] = useState<PmViewMode | null>(null);
  const [pendingColumns, setPendingColumns] = useState<PmListColumnKey[] | null>(null);
  const [pendingPageSize, setPendingPageSize] = useState<number | null>(null);

  const view = pendingView ?? parsePmView(project?.metadata);
  const columns = pendingColumns ?? parsePmListColumns(project?.metadata);
  const pageSize = pendingPageSize ?? parsePmListPageSize(project?.metadata);

  // 换项目：本地镜像作废，视图配置跟着新项目的 metadata 走。
  useEffect(() => {
    setPendingView(null);
    setPendingColumns(null);
    setPendingPageSize(null);
  }, [project?.id]);

  /**
   * 乐观写项目偏好的公共通道（视图 / 显示列 / 每页行数三处共用）。
   *
   * 合并语义不在这里：`merge` 必须是 `lib/pm-view-settings` 的 `with*` 函数，
   * 否则 metadata 的兄弟字段（图标、`boardPageSize`…）会被整体覆盖清掉。
   * 本函数只管三件事：先立镜像、发一次 PATCH、按身份撤销自己那一次镜像。
   */
  const writePref = useCallback(
    async <T,>(
      next: T,
      mirror: Dispatch<SetStateAction<T | null>>,
      merge: (metadata: Record<string, unknown> | null | undefined, value: T) => Record<string, unknown>,
      failedKey: string,
    ) => {
      if (!project) return;
      mirror(next);
      const ok = await updateProjectMeta(project.id, { metadata: merge(project.metadata, next) });
      // 只撤销"自己那一次"：连点两下时先返回的请求不能把后一次的切换抹掉。
      mirror((cur) => (cur === next ? null : cur));
      if (!ok) showToast({ type: "error", message: t(failedKey) });
    },
    [project, updateProjectMeta, t],
  );

  const changeView = useCallback(
    async (next: PmViewMode) => {
      if (next === view) return;
      await writePref(next, setPendingView, withPmView, "pm.view.saveFailed");
    },
    [view, writePref],
  );

  const changeColumns = useCallback(
    (next: PmListColumnKey[]) =>
      writePref(next, setPendingColumns, withPmListColumns, "pm.list.columnsSaveFailed"),
    [writePref],
  );

  const changePageSize = useCallback(
    (next: number) =>
      writePref(next, setPendingPageSize, withPmListPageSize, "pm.list.pageSizeSaveFailed"),
    [writePref],
  );

  // 未选中项目 → 空提示
  if (!project) {
    return (
      <main className={cn(CAPSULE_PANE_CN, "min-w-0 flex-1 items-center justify-center bg-page-bg")}>
        <div className="text-center">
          <div className="text-3xl">📋</div>
          <p className="mt-2 text-xs text-text-tertiary ">
            {t("pm.selectProjectHint")}
          </p>
        </div>
      </main>
    );
  }

  // 加载骨架
  if (loading) {
    return (
      <main className={cn(CAPSULE_PANE_CN, "min-w-0 flex-1 bg-page-bg")}>
        <div className="shrink-0 border-b border-border-divider px-4 py-3">
          <div className="h-5 w-40 animate-pulse rounded bg-zinc-100 dark:bg-zinc-800" />
          <div className="mt-2 h-3 w-64 animate-pulse rounded bg-zinc-100 dark:bg-zinc-800" />
        </div>
        <div className="flex flex-1 gap-3 p-3">
          {[0, 1, 2, 3].map((i) => (
            <div key={i} className="flex-1 animate-pulse rounded-md bg-zinc-100 dark:bg-zinc-800" />
          ))}
        </div>
      </main>
    );
  }

  return (
    <main className={cn(CAPSULE_PANE_CN, "min-w-0 flex-1 bg-page-bg")}>
      <ProjectHeader
        project={project}
        onNewTask={newTask}
        view={view}
        onViewChange={changeView}
      />
      {error && (
        <div className="flex shrink-0 items-center justify-between gap-2 border-b border-border-divider bg-amber-50 px-4 py-2 text-xs text-amber-700 dark:bg-amber-950/40 dark:text-amber-300">
          <span className="min-w-0 truncate">{error}</span>
          {onRetry && (
            <button
              type="button"
              onClick={onRetry}
              className="shrink-0 rounded-md px-2 py-0.5 text-11 font-medium text-amber-800 hover:bg-amber-100 dark:text-amber-200 dark:hover:bg-amber-900"
            >
              {t("common.retry")}
            </button>
          )}
        </div>
      )}
      <div className="min-h-0 flex-1">
        {view === "list" ? (
          <TaskListView
            project={project}
            columns={columns}
            onColumnsChange={changeColumns}
            pageSize={pageSize}
            onPageSizeChange={changePageSize}
            onOpenTask={openTask}
          />
        ) : (
          <KanbanBoard
            projectId={project.id}
            pageSize={parseBoardPageSize(project.metadata)}
            onOpenTask={openTask}
          />
        )}
      </div>
    </main>
  );
}
