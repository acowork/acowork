/**
 * TaskListView — PM 列表视图（一行一个任务的传统 PM 表格）。
 *
 * 与看板视图同源：任务数据只有 `usePmBoardStore.tasks` 一份（全量加载），
 * 本视图不新增 store、不发新请求、不加网络层。它只做「筛选 → 排序 →
 * 树扁平化 → 分组」这条派生链（纯函数在 `lib/pm-list-columns`，可脱离
 * React 单测），单元格复用 `PriorityBadge` / `TaskTypeIcon` / `MemberAvatar`。
 *
 * 视图状态归属（决策 2 / 4）：
 *   - 进项目 metadata：当前视图（`pmView`）、显示列（`pmListColumns`）、
 *     每页行数（`pmListPageSize`）；
 *   - 内存态：排序 / 筛选 / 分组 / 折叠 / 多选 / 列宽 / 页码。列宽刻意不持久化
 *     —— 每次鼠标拖动都写同步元数据是噪音；后续要落盘时加
 *     `metadata.pmListWidths`，写入必须走 `pm-view-settings` 的合并语义。
 *
 * 列宽策略：`table-layout: fixed` 下所有列都写死 px 时，浏览器把富余宽度
 * 平摊给每一列，宽屏读起来很稀。所以 `title` 列**不写宽度**（auto 吸收
 * 剩余），其余列用注册表默认宽；用户拖过 `title` 之后才给它显式 px。
 *
 * 离线（`healthy === false`）：读与本地视图状态照常可用（列表是纯读视图，
 * 离线时看数据反而更有用），**写**入口全部禁用 —— 状态胶囊、批量操作、
 * 行内新建，与看板的只读语义一致。
 *
 * 分页（`metadata.pmListPageSize`，默认 50 行/页，页脚可改 20/50/100/200）：
 * 装箱单元是**根任务子树**而不是单行 —— 按行硬切会把父任务留在上一页、子任务
 * 跑到下一页，缩进结构当场失真。代价是一页的实际行数可能少于预算（最后一棵
 * 子树放不下就整棵挪到下一页），页脚因此报累计行号而不是"每页 N 条"。
 * 筛选 / 排序 / 分组一变就回第一页（行序已经重排，停在第 3 页看到的是不相干
 * 的一批）。
 *
 * 已知限制（v1，交付差异清单同步列出）：
 *   - 无虚拟滚动：每页最多 `pageSize` 行，长列表靠翻页而不是窗口化渲染；
 *   - 无拖拽：改状态走状态胶囊，改父子关系仍只有看板。
 */

import { useCallback, useEffect, useMemo, useState } from "react";
import { usePmBoardStore } from "../../stores/pm/boardStore";
import { usePmHealthStore } from "../../stores/pm/healthStore";
import { useAgentStore } from "../../stores/agentStore";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { useTranslation } from "../../i18n/useTranslation";
import { showToast } from "../../components/common/ToastProvider";
import { OverlayScrollbar } from "../../components/common/OverlayScrollbar";
import { TaskFieldPicker } from "./TaskFieldPicker";
import { PmPopover, PmPopoverItem, PmPopoverSection } from "./pmPopover";
import { PmColumnResizer } from "./PmColumnResizer";
import { TaskListToolbar, type ToolbarPopoverSpec } from "./TaskListToolbar";
import { TaskListBulkBar, type BulkField } from "./TaskListBulkBar";
import { TaskListPager } from "./TaskListPager";
import {
  TaskListEmptyRow,
  TaskListGroupHeaderRow,
  TaskListRow,
  TaskListSkeletonRows,
} from "./TaskListRows";
import {
  resolveActorLabel,
  resolveMembers,
  usePmDirectoryUsers,
  useSelfAccount,
  withOwner,
} from "./pmMembers";
import { STATUS_TRANSITIONS } from "./TaskStatusMove";
import {
  ADD_COL_WIDTH,
  CHECK_COL_WIDTH,
  STATUS_DOT_CLASS,
  STATUS_LABEL_KEY,
  TH_BASE_CN,
  estimateTableWidth,
} from "./taskListLabels";
import { cn } from "../../lib/utils";
import { createTask, updateTask } from "../../lib/pm-api";
import {
  DEFAULT_PM_LIST_COLUMNS,
  EMPTY_PM_LIST_FILTERS,
  NO_PM_LIST_SORT,
  UNASSIGNED,
  filterPmListTasks,
  flattenPmListRows,
  groupPmListRows,
  hasActivePmListFilters,
  nextPmListSort,
  paginatePmListGroups,
  pmListColumn,
  type PmColumnWidths,
  type PmListGroupBy,
  type PmListColumnKey,
  type PmListFilters,
  type PmListSort,
  type PmListValueCtx,
} from "../../lib/pm-list-columns";
import type {
  PmProject,
  PmTaskResponse,
  Priority,
  TaskStatus,
} from "../../lib/pm-types";

interface TaskListViewProps {
  project: PmProject;
  /** 显示列（`metadata.pmListColumns`，数组顺序即列顺序）。 */
  columns: PmListColumnKey[];
  /** 列配置变更（字段弹层的增删 / 上下移 / 重置）。 */
  onColumnsChange: (next: PmListColumnKey[]) => void;
  /** 每页行数预算（`metadata.pmListPageSize`）。 */
  pageSize: number;
  /** 页脚改每页行数（写 metadata，与列配置同一条路径）。 */
  onPageSizeChange: (size: number) => void;
  onOpenTask: (taskId: string) => void;
}

/**
 * 打开的弹层 = 描述 + 锚点。
 *
 * 锚点存在 state 里（不是 ref）：ref 不参与渲染，`setPopover` 之后渲染时
 * 读 ref 会拿到"上一次点击的元素"，弹层会贴错位置。
 */
/** 弹层意图（不含锚点）—— 判别式联合，`Omit<>` 会把判别式抹平，所以单独命名。 */
type OpenPopoverSpec =
  | ToolbarPopoverSpec
  | { kind: "columnMenu"; column: PmListColumnKey }
  | { kind: "statusMenu"; taskId: string }
  | { kind: "bulk"; field: BulkField };

type OpenPopover = OpenPopoverSpec & { anchor: HTMLElement };

export function TaskListView({
  project,
  columns,
  onColumnsChange,
  pageSize,
  onPageSizeChange,
  onOpenTask,
}: TaskListViewProps) {
  const { t } = useTranslation();
  const tasks = usePmBoardStore((s) => s.tasks);
  const loading = usePmBoardStore((s) => s.loading);
  const moveTask = usePmBoardStore((s) => s.moveTask);
  const reviewTask = usePmBoardStore((s) => s.reviewTask);
  const upsertTask = usePmBoardStore((s) => s.upsertTask);
  const healthy = usePmHealthStore((s) => s.healthy);
  const offline = healthy === false;

  // ── 视图状态（内存态，见文件头） ────────────────────────────────
  const [filters, setFilters] = useState<PmListFilters>(EMPTY_PM_LIST_FILTERS);
  const [sort, setSort] = useState<PmListSort>(NO_PM_LIST_SORT);
  const [groupBy, setGroupBy] = useState<PmListGroupBy>("none");
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(() => new Set());
  const [selected, setSelected] = useState<ReadonlySet<string>>(() => new Set());
  const [widths, setWidths] = useState<PmColumnWidths>({});
  const [pageIndex, setPageIndex] = useState(0);
  const [open, setOpen] = useState<OpenPopover | null>(null);
  const [creating, setCreating] = useState(false);
  const [createTitle, setCreateTitle] = useState("");
  const [savingRow, setSavingRow] = useState(false);

  // 切项目：视图状态归零。显示列跟着项目走（metadata），不在这里重置。
  useEffect(() => {
    setFilters(EMPTY_PM_LIST_FILTERS);
    setSort(NO_PM_LIST_SORT);
    setGroupBy("none");
    setCollapsed(new Set());
    setSelected(new Set());
    setWidths({});
    setOpen(null);
    setCreating(false);
    setCreateTitle("");
  }, [project.id]);

  /**
   * 结果集变了就回到第一页：筛选、排序、分组都会重排行，停在第 3 页的用户
   * 看到的会是一批不相干的任务。（页码越界本身由 `paginatePmListGroups`
   * clamp 住，这里管的是"语义上该回第一页"。）
   */
  useEffect(() => {
    setPageIndex(0);
  }, [filters, sort, groupBy]);

  // ── 身份解析（负责人 / 创建人 / 筛选选项） ──────────────────────
  const agents = useAgentStore((s) => s.agents);
  const self = useSelfAccount();
  const localProfile = useUserProfileStore((s) => s.profile);
  const directory = usePmDirectoryUsers();
  // `withOwner` 每次返回新数组，按 project 身份 memo 一次：`memberById` 与
  // `assigneeOptions` 都依赖它的引用稳定性。
  const roster = useMemo(() => withOwner(project), [project]);
  const memberById = useMemo(
    () =>
      new Map(
        resolveMembers(roster, agents, self, localProfile, directory).map((m) => [
          m.instance_id,
          m,
        ]),
      ),
    [roster, agents, self, localProfile, directory],
  );

  /** actor id → 显示名；解析不出来时退回裸 id（与 `useActorLabel` 同规则）。 */
  const labelOf = useCallback(
    (id: string | null): string => {
      if (!id) return "";
      const local = resolveActorLabel(agents, self, id);
      if (local && local !== id) return local;
      const user = directory.find((candidate) => candidate.user_id === id);
      return user ? user.display_name || user.username : (local ?? id);
    },
    [agents, self, directory],
  );

  const assigneeOptions = useMemo(
    () =>
      roster
        .map((m) => ({
          value: m.instance_id,
          label: memberById.get(m.instance_id)?.displayName ?? m.instance_id,
        }))
        .sort((a, b) => a.label.localeCompare(b.label)),
    [roster, memberById],
  );

  // ── 派生链：索引 → 筛选 → 排序 → 树 → 分组 ──────────────────────
  const { byId, childStats } = useMemo(() => {
    const byId = new Map<string, PmTaskResponse>();
    for (const task of tasks) byId.set(task.id, task);
    const childStats = new Map<string, { total: number; done: number }>();
    for (const task of tasks) {
      if (!task.parent_id || !byId.has(task.parent_id)) continue;
      const cur = childStats.get(task.parent_id) ?? { total: 0, done: 0 };
      cur.total += 1;
      if (task.status === "done") cur.done += 1;
      childStats.set(task.parent_id, cur);
    }
    return { byId, childStats };
  }, [tasks]);

  const valueCtx = useMemo<PmListValueCtx>(
    () => ({
      parentTitle: (task) =>
        task.parent_id ? (byId.get(task.parent_id)?.title ?? "") : "",
      childCount: (task) => childStats.get(task.id)?.total ?? 0,
      childDone: (task) => childStats.get(task.id)?.done ?? 0,
    }),
    [byId, childStats],
  );

  const filtered = useMemo(
    () => filterPmListTasks(tasks, filters, labelOf),
    [tasks, filters, labelOf],
  );
  const rows = useMemo(
    () => flattenPmListRows(filtered, sort, valueCtx, collapsed),
    [filtered, sort, valueCtx, collapsed],
  );
  const groups = useMemo(() => groupPmListRows(rows, groupBy), [rows, groupBy]);
  /**
   * 分页装箱（每页行数预算 `pageSize`，单元是根任务子树）。
   *
   * 分组标题行的计数刻意用**全量**（`groupTotals`）而不是本页行数：一个 37 条
   * 的组被切成两页时，第二页的标题写 12 会让人以为数据少了，写 37 才是"这个组
   * 一共这么多"。
   */
  const page = useMemo(
    () => paginatePmListGroups(groups, pageSize, pageIndex),
    [groups, pageSize, pageIndex],
  );
  const groupTotals = useMemo(
    () => new Map(groups.map((g) => [g.id, g.rows.length] as const)),
    [groups],
  );
  const visibleColumns = useMemo(
    () =>
      columns
        .map((key) => pmListColumn(key))
        .filter((c) => Boolean(c))
        .map((c) => c!),
    [columns],
  );

  // ── 交互 ────────────────────────────────────────────────────────
  const openAt = useCallback(
    (spec: OpenPopoverSpec) =>
      (e: React.MouseEvent<HTMLElement>) => {
        e.stopPropagation();
        setOpen({ ...spec, anchor: e.currentTarget });
      },
    [],
  );
  const close = useCallback(() => setOpen(null), []);

  const toggleCollapsed = useCallback((taskId: string) => {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(taskId)) next.delete(taskId);
      else next.add(taskId);
      return next;
    });
  }, []);

  const toggleSelected = useCallback((taskId: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(taskId)) next.delete(taskId);
      else next.add(taskId);
      return next;
    });
  }, []);

  /**
   * 全选 = **筛选结果的全部**，跨页，不是"本页"。
   *
   * 分页之后这两种语义都会有人按，取前者是因为批量操作的对象本来就是"我筛出来
   * 的那一批"（勾完翻页也还在选中态）；页脚的选中数（`pm.list.bulkSelected`）
   * 说的就是这批的数量。
   */
  const toggleSelectAll = useCallback(() => {
    setSelected((prev) => {
      const all = rows.map((r) => r.task.id);
      const everySelected = all.length > 0 && all.every((id) => prev.has(id));
      return everySelected ? new Set<string>() : new Set(all);
    });
  }, [rows]);

  const handleSort = useCallback(
    (key: PmListColumnKey) => setSort((prev) => nextPmListSort(prev, key)),
    [],
  );

  const hideColumn = useCallback(
    (key: PmListColumnKey) => {
      if (key === "title") return; // 任务列不可隐藏
      onColumnsChange(columns.filter((k) => k !== key));
    },
    [columns, onColumnsChange],
  );

  /**
   * 内联改状态：只列服务端会接受的流转（`STATUS_TRANSITIONS` 是
   * `validate_transition` 的镜像）；Agent 新建待批准的任务改走「批准」。
   */
  const applyStatus = useCallback(
    async (task: PmTaskResponse, to: TaskStatus | "__approve__") => {
      close();
      if (to === "__approve__") {
        await reviewTask(task.id, true);
        return;
      }
      await moveTask(task.id, to);
    },
    [close, moveTask, reviewTask],
  );

  /**
   * 批量改字段。
   *
   * 状态走 `moveTask`（与看板同一套乐观 + 回滚）；优先级 / 负责人 PATCH 后
   * 把服务端那一行**合并**回原行 —— `PATCH /tasks/:tid` 返回的是 `Task`，
   * 不含派生字段（`is_blocked` / `depth` / `parent_id`），整体替换会拆掉
   * 父子树，所以这里必须是 `{ ...prev, ...updated }`。
   *
   * 失败语义：一批 N 条就是 N 个独立请求，任何一条都不能改变其余条的结局，
   * 所以这里逐条记账（`applied` / `failed`），最后只报真实数字。之前的写法
   * 有两个洞 —— 状态分支丢掉 `moveTask` 的布尔返回值（它失败时已回滚并自己
   * 弹错误，但循环照跑，结尾仍报"已更新 N 个任务"），异常分支直接 `return`
   * （前面已经改掉的行留在库里，用户看到的是一次"失败"而无从知道改了几条）。
   * 失败的那批保留选中态，原地重试即可，不必重新筛。
   */
  const applyBulk = useCallback(
    async (patch: {
      status?: TaskStatus;
      priority?: Priority;
      assignee?: string | null;
    }) => {
      close();
      const ids = [...selected];
      if (!ids.length) return;
      let applied = 0;
      const failed: string[] = [];
      for (const id of ids) {
        if (patch.status) {
          // 必须接住返回值：`moveTask` 失败时回滚本地并返回 false。
          if (await moveTask(id, patch.status)) applied += 1;
          else failed.push(id);
          continue;
        }
        const prev = byId.get(id);
        // 行已经被别处删掉：既没改成，也不该留在选中态里（否则批量条上挂着
        // 一个不存在 id 的"已选 1 项"）。它不计入 applied，也不计入 failed。
        if (!prev) continue;
        try {
          const updated = await updateTask(id, {
            priority: patch.priority,
            assignee: patch.assignee,
          });
          upsertTask({ ...prev, ...updated });
          applied += 1;
        } catch {
          failed.push(id);
        }
      }
      if (!failed.length) {
        setSelected(new Set());
        showToast({ type: "success", message: t("pm.list.bulkDone", { count: applied }) });
        return;
      }
      setSelected(new Set(failed));
      showToast({
        type: "warning",
        message: t("pm.list.bulkPartial", { applied, failed: failed.length }),
      });
    },
    [selected, byId, close, moveTask, t, upsertTask],
  );

  /**
   * 行内新建：回车直接建（默认 type=task / priority=normal），失败保留输入。
   *
   * 刻意**不**走 `reload()`：`reload` 会先把 `boardStore.loading` 置真，
   * `ProjectsView` 随即把整块面板换成骨架，本组件被卸载 —— 排序 / 筛选 /
   * 分组 / 折叠 / 多选 / 列宽全丢。所以这里用 `upsertTask`，与批量改字段
   * 同一条写路径。`POST /tasks` 返回 `PmTask`（无派生字段），而行内新建
   * 固定是"无父、无依赖的根任务"，`depth / parent_id / is_blocked /
   * blocked_by` 取的是精确值而非猜测。
   */
  const createFromRow = useCallback(async () => {
    const title = createTitle.trim();
    if (!title || savingRow) return;
    setSavingRow(true);
    try {
      const created = await createTask(project.id, {
        title,
        type: "task",
        priority: "normal",
      });
      upsertTask({
        ...created,
        depth: 0,
        parent_id: null,
        is_blocked: false,
        blocked_by: [],
      });
      setCreateTitle("");
      // 默认序（服务端返回序）下新任务排在最后 → 翻到最后一页，否则用户建完
      // 看不见自己刚建的东西。有排序或筛选时不猜位置（它可能根本不在结果集里），
      // 保持当前页。页码越界由 `paginatePmListGroups` clamp，所以给个大数即可。
      if (!hasActivePmListFilters(filters) && sort.key === null) {
        setPageIndex(Number.MAX_SAFE_INTEGER);
      }
      showToast({ type: "success", message: t("pm.taskCreated") });
    } catch (e) {
      showToast({
        type: "error",
        message: t("pm.list.createRowFailed", {
          error: e instanceof Error ? e.message : String(e),
        }),
      });
    } finally {
      setSavingRow(false);
    }
  }, [createTitle, filters, project.id, savingRow, sort.key, t, upsertTask]);

  const filtersActive = hasActivePmListFilters(filters);
  const allSelected = rows.length > 0 && rows.every((r) => selected.has(r.task.id));
  const colCount = visibleColumns.length + 2;

  const groupLabelOf = useCallback(
    (id: string): string => {
      if (groupBy === "status") return t(STATUS_LABEL_KEY[id as TaskStatus]);
      if (groupBy === "priority") return t(`pm.priority.${id}`);
      if (groupBy === "type") return t(`pm.taskType.${id}`);
      if (id === UNASSIGNED) return t("pm.task.unassigned");
      return labelOf(id);
    },
    [groupBy, labelOf, t],
  );

  return (
    <div className="relative flex h-full min-h-0 flex-col bg-page-bg">
      <TaskListToolbar
        filters={filters}
        filtersActive={filtersActive}
        groupBy={groupBy}
        columnCount={visibleColumns.length}
        onOpen={(spec) => openAt(spec)}
        onQueryChange={(query) => setFilters((prev) => ({ ...prev, query }))}
        onClearFilters={() => setFilters(EMPTY_PM_LIST_FILTERS)}
      />

      {/* 表格的滚动视口。这里必须是 app 唯一的那根滚动条（`OverlayScrollbar`），
          不能是裸 `overflow-auto`：`overlayScrollCoverage.test.ts` 会扫全仓，
          裸滚动容器在主题化表面里会露出系统滚动条（Chromium 121+ 之后
          `::-webkit-scrollbar` 那套皮肤在标准属性非 auto 时整族失效）。
          `showXThumb` + `overflow-x-auto`：列多时要横向滚动，而视口默认是
          `overflow-x-hidden`，横向那根只能显式要。 */}
      <OverlayScrollbar fill="flex" showXThumb className="overflow-x-auto">
        <table
          className="w-full border-separate border-spacing-0 text-11"
          style={{
            tableLayout: "fixed",
            minWidth: estimateTableWidth(visibleColumns, widths),
          }}
        >
          <colgroup>
            <col style={{ width: CHECK_COL_WIDTH }} />
            {visibleColumns.map((c) => (
              <col
                key={c.key}
                style={{
                  // `title` 不写宽度 → fixed 布局下吸收剩余；拖过之后才落 px。
                  width:
                    c.key === "title" && widths[c.key] === undefined
                      ? undefined
                      : (widths[c.key] ?? c.width ?? undefined),
                }}
              />
            ))}
            <col style={{ width: ADD_COL_WIDTH }} />
          </colgroup>

          <thead>
            <tr>
              <th className={cn(TH_BASE_CN, "px-2")} aria-label={t("pm.list.selectAll")}>
                <input
                  type="checkbox"
                  aria-label={t("pm.list.selectAll")}
                  checked={allSelected}
                  onChange={toggleSelectAll}
                  className="ml-1 h-3.5 w-3.5 cursor-pointer accent-[var(--color-accent)]"
                />
              </th>
              {visibleColumns.map((c) => (
                <th
                  key={c.key}
                  data-column={c.key}
                  aria-sort={
                    sort.key === c.key && sort.dir !== 0
                      ? sort.dir === 1
                        ? "ascending"
                        : "descending"
                      : "none"
                  }
                  className={cn(TH_BASE_CN, "group/th", c.align === "right" && "text-right")}
                  title={t("pm.list.sortHint")}
                >
                  {/* 排序挂在内容层，**不是** `<th>` 上：列宽手柄也是 `<th>` 的
                      直接子元素，浏览器把 `click` 派发在 mousedown / mouseup
                      两个目标的共同祖先上 —— 手柄拖几百像素在同列内松手，共同
                      祖先就是这个 `<th>`，`mousedown` 的 `preventDefault()` 又
                      抑制不了 `click`，于是每次调列宽都顺带改一次排序。 */}
                  <span
                    className={cn(
                      "flex cursor-pointer select-none items-center gap-1",
                      c.align === "right" ? "justify-end" : "justify-start",
                      sort.key === c.key && "text-text",
                    )}
                    onClick={() => handleSort(c.key)}
                  >
                    <span className="min-w-0 truncate">{t(c.labelKey)}</span>
                    {sort.key === c.key && sort.dir !== 0 && (
                      <span className="shrink-0 text-9" aria-hidden>
                        {sort.dir === 1 ? "▲" : "▼"}
                      </span>
                    )}
                    <button
                      type="button"
                      aria-label={t("pm.list.columnMenu")}
                      onClick={openAt({ kind: "columnMenu", column: c.key })}
                      className="shrink-0 rounded px-0.5 text-9 opacity-0 hover:bg-zinc-200 focus:opacity-100 group-hover/th:opacity-100 dark:hover:bg-zinc-700"
                    >
                      ⋮
                    </button>
                  </span>
                  <PmColumnResizer
                    columnKey={c.key}
                    onWidth={(px) => setWidths((prev) => ({ ...prev, [c.key]: px }))}
                  />
                </th>
              ))}
              <th className={cn(TH_BASE_CN, "cursor-default px-1")}>
                <button
                  type="button"
                  aria-label={t("pm.list.fields.open")}
                  title={t("pm.list.fields.open")}
                  onClick={openAt({ kind: "fields" })}
                  className="rounded px-1 text-text-tertiary hover:bg-zinc-200 hover:text-text dark:hover:bg-zinc-700"
                >
                  ＋
                </button>
              </th>
            </tr>
          </thead>

          <tbody>
            {loading && tasks.length === 0 ? (
              <TaskListSkeletonRows colCount={colCount} />
            ) : rows.length === 0 ? (
              <TaskListEmptyRow
                colCount={colCount}
                // 项目本来就没有任务，和「筛选后没有命中」是两件事：后者必须
                // 给出清除筛选的出口，否则用户会以为数据丢了。
                isEmptyProject={tasks.length === 0}
                emptyProjectLabel={t("pm.list.empty")}
                noMatchLabel={t("pm.list.noMatch")}
                clearFiltersLabel={t("pm.list.clearFilters")}
                onClearFilters={
                  filtersActive ? () => setFilters(EMPTY_PM_LIST_FILTERS) : undefined
                }
              />
            ) : (
              page.groups.map((group) => (
                <TaskListFragment
                  key={`${groupBy}-${group.id}`}
                  groupLabel={groupBy === "none" ? null : groupLabelOf(group.id)}
                  groupCount={groupTotals.get(group.id) ?? group.rows.length}
                  colCount={colCount}
                >
                  {group.rows.map((row) => {
                    const task = row.task;
                    return (
                      <TaskListRow
                        key={task.id}
                        row={row}
                        columns={visibleColumns}
                        selected={selected.has(task.id)}
                        collapsed={collapsed.has(task.id)}
                        hasChildren={(childStats.get(task.id)?.total ?? 0) > 0}
                        childStats={childStats.get(task.id)}
                        parentTitle={
                          task.parent_id ? (byId.get(task.parent_id)?.title ?? "") : ""
                        }
                        member={task.assignee ? memberById.get(task.assignee) : undefined}
                        assigneeLabel={labelOf(task.assignee)}
                        creatorLabel={labelOf(task.created_by)}
                        offline={offline}
                        onOpenTask={onOpenTask}
                        onToggleSelect={toggleSelected}
                        onToggleCollapse={toggleCollapsed}
                        onOpenStatus={openAt({ kind: "statusMenu", taskId: task.id })}
                      />
                    );
                  })}
                </TaskListFragment>
              ))
            )}

            <tr>
              <td colSpan={colCount} className="border-b border-border-divider px-2 py-0.5">
                {creating ? (
                  <input
                    autoFocus
                    value={createTitle}
                    disabled={offline || savingRow}
                    placeholder={t("pm.list.newRowPlaceholder")}
                    onChange={(e) => setCreateTitle(e.target.value)}
                    onBlur={() => {
                      if (!createTitle.trim()) setCreating(false);
                    }}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") void createFromRow();
                      if (e.key === "Escape") {
                        setCreateTitle("");
                        setCreating(false);
                      }
                    }}
                    className="w-full rounded border border-input-border bg-input-bg px-2 py-1 text-11 text-text outline-none focus:border-[var(--color-accent)]"
                  />
                ) : (
                  <button
                    type="button"
                    disabled={offline}
                    onClick={() => setCreating(true)}
                    title={offline ? t("pm.offlineReadonlyHint") : t("pm.list.newRowHint")}
                    className="flex items-center gap-1 py-1 text-11 text-text-tertiary hover:text-[var(--color-accent)] disabled:cursor-not-allowed disabled:opacity-40"
                  >
                    <span aria-hidden>＋</span>
                    {t("pm.newTask")}
                  </button>
                )}
              </td>
            </tr>
          </tbody>
        </table>
      </OverlayScrollbar>

      <footer className="flex shrink-0 flex-wrap items-center justify-between gap-x-3 gap-y-1 border-t border-border-divider px-3 py-1 text-10 text-text-tertiary">
        <span>{t("pm.list.shownCount", { shown: rows.length, total: tasks.length })}</span>
        {/* 分页：装箱按子树来，所以页脚报的是累计行号 + 页码，不是"每页 N 条"。 */}
        {rows.length > 0 && (
          <TaskListPager
            page={page}
            pageSize={pageSize}
            onPageSizeChange={onPageSizeChange}
            onPrev={() => setPageIndex((i) => Math.max(0, i - 1))}
            onNext={() => setPageIndex((i) => Math.min(page.pageCount - 1, i + 1))}
          />
        )}
        <span>
          {t("pm.list.columnCount", { count: visibleColumns.length })}
          {groupBy !== "none" && ` · ${t("pm.list.groupBy")} ${t(`pm.list.group.${groupBy}`)}`}
        </span>
      </footer>

      {/* 弹层：同一时刻最多一个，锚点就是触发它的元素。 */}
      {open?.kind === "fields" && (
        <TaskFieldPicker
          anchor={open.anchor}
          columns={columns}
          onColumnsChange={onColumnsChange}
          onReset={() => onColumnsChange(DEFAULT_PM_LIST_COLUMNS.slice())}
          onClose={close}
        />
      )}

      {open?.kind === "columnMenu" &&
        (() => {
          const col = pmListColumn(open.column);
          if (!col) return null;
          return (
            <PmPopover anchor={open.anchor} onClose={close} width={180}>
              <PmPopoverSection>{t(col.labelKey)}</PmPopoverSection>
              <PmPopoverItem
                checked={sort.key === col.key && sort.dir === 1}
                onClick={() => {
                  setSort({ key: col.key, dir: 1 });
                  close();
                }}
              >
                {t("pm.list.sortAsc")}
              </PmPopoverItem>
              <PmPopoverItem
                checked={sort.key === col.key && sort.dir === -1}
                onClick={() => {
                  setSort({ key: col.key, dir: -1 });
                  close();
                }}
              >
                {t("pm.list.sortDesc")}
              </PmPopoverItem>
              <PmPopoverItem
                onClick={() => {
                  hideColumn(col.key);
                  close();
                }}
                disabled={col.required}
                title={col.required ? t("pm.list.fields.titleLocked") : undefined}
              >
                {t("pm.list.hideColumn")}
              </PmPopoverItem>
            </PmPopover>
          );
        })()}

      {open?.kind === "statusMenu" &&
        (() => {
          const task = byId.get(open.taskId);
          if (!task) return null;
          // 审批闸门与看板同一套：Agent 新建、人类未批准的任务不能直接进
          // in_progress（服务端 ensure_creation_approved 会 409），改列「批准」。
          const gated = task.status === "pending" && task.review_status === "pending";
          const targets = STATUS_TRANSITIONS[task.status].filter(
            (s) => !(gated && s === "in_progress"),
          );
          return (
            <PmPopover anchor={open.anchor} onClose={close} width={180}>
              <PmPopoverSection>{t("pm.task.status")}</PmPopoverSection>
              {gated && (
                <PmPopoverItem onClick={() => void applyStatus(task, "__approve__")}>
                  {t("pm.task.approve")}
                </PmPopoverItem>
              )}
              {targets.length === 0 && !gated && (
                <p className="px-3 py-1.5 text-10 text-text-tertiary">
                  {t("pm.list.statusNoTransition")}
                </p>
              )}
              {targets.map((s) => (
                <PmPopoverItem key={s} onClick={() => void applyStatus(task, s)}>
                  <span className="flex items-center gap-1.5">
                    <span
                      className={cn("h-1.5 w-1.5 shrink-0 rounded-full", STATUS_DOT_CLASS[s])}
                      aria-hidden
                    />
                    {t(STATUS_LABEL_KEY[s])}
                  </span>
                </PmPopoverItem>
              ))}
            </PmPopover>
          );
        })()}

      {open?.kind === "group" && (
        <PmPopover anchor={open.anchor} onClose={close} width={170}>
          <PmPopoverSection>{t("pm.list.groupBy")}</PmPopoverSection>
          {(["none", "status", "priority", "assignee", "type"] as PmListGroupBy[]).map(
            (option) => (
              <PmPopoverItem
                key={option}
                checked={groupBy === option}
                onClick={() => {
                  setGroupBy(option);
                  close();
                }}
              >
                {option === "none" ? t("pm.list.group.none") : t(`pm.list.group.${option}`)}
              </PmPopoverItem>
            ),
          )}
        </PmPopover>
      )}

      {(open?.kind === "filter" || open?.kind === "bulk") &&
        (() => {
          const field = open.field;
          const isBulk = open.kind === "bulk";
          const options: Array<{ value: string; label: string }> =
            field === "status"
              ? (Object.keys(STATUS_LABEL_KEY) as TaskStatus[]).map((s) => ({
                  value: s,
                  label: t(STATUS_LABEL_KEY[s]),
                }))
              : field === "priority"
                ? (["urgent", "high", "normal", "low"] as Priority[]).map((p) => ({
                    value: p,
                    label: t(`pm.priority.${p}`),
                  }))
                : [...assigneeOptions, { value: UNASSIGNED, label: t("pm.task.unassigned") }];
          const active = new Set<string>(filters[field] as string[]);
          return (
            <PmPopover anchor={open.anchor} onClose={close} width={210}>
              <PmPopoverSection>{t(`pm.list.col.${field}`)}</PmPopoverSection>
              {/* 长列表（状态/优先级/负责人可选项很多时）要有界滚动，且必须走
                  那根公共滚动条 —— 裸 `max-h-* overflow-y-auto` 会被
                  `overlayScrollCoverage` 判成未绑定容器。 */}
              <OverlayScrollbar maxHeight={256}>
                {options.map((o) => (
                  <PmPopoverItem
                    key={o.value}
                    checked={!isBulk && active.has(o.value)}
                    onClick={() => {
                      if (isBulk) {
                        if (field === "status") void applyBulk({ status: o.value as TaskStatus });
                        else if (field === "priority")
                          void applyBulk({ priority: o.value as Priority });
                        else
                          void applyBulk({
                            assignee: o.value === UNASSIGNED ? null : o.value,
                          });
                        return;
                      }
                      setFilters((prev) => {
                        const cur = prev[field] as string[];
                        const next = cur.includes(o.value)
                          ? cur.filter((v) => v !== o.value)
                          : [...cur, o.value];
                        return { ...prev, [field]: next } as PmListFilters;
                      });
                    }}
                  >
                    {o.label}
                  </PmPopoverItem>
                ))}
              </OverlayScrollbar>
            </PmPopover>
          );
        })()}

      {selected.size > 0 && !offline && (
        <TaskListBulkBar
          count={selected.size}
          onOpen={(field) => openAt({ kind: "bulk", field })}
          onClear={() => setSelected(new Set())}
        />
      )}
    </div>
  );
}

/**
 * 一个分组（或平铺时的一整段）在 `<tbody>` 里的片段。
 *
 * `<tbody>` 不能用 `<div>` 包（浏览器会把它从表格里提出来），所以这里必须
 * 是 `Fragment`；分组标题是横跨全部列的一行 `<tr>`。
 */
function TaskListFragment({
  groupLabel,
  groupCount,
  colCount,
  children,
}: {
  groupLabel: string | null;
  groupCount: number;
  colCount: number;
  children: React.ReactNode;
}) {
  return (
    <>
      {groupLabel !== null && (
        <TaskListGroupHeaderRow label={groupLabel} count={groupCount} colCount={colCount} />
      )}
      {children}
    </>
  );
}
