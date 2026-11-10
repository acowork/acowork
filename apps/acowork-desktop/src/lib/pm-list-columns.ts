/**
 * 列表视图的列注册表 + 表格派生纯逻辑。
 *
 * 这里只有**数据**：列元信息（宽度 / 对齐 / i18n 键 / 取值）与
 * 筛选 → 排序 → 树扁平化 → 分组 这条派生链。单元格怎么画（优先级配色、
 * 类型 emoji、成员头像）留在视图层，复用 `PriorityBadge` / `TaskTypeIcon`
 * / `MemberAvatar` —— 本模块**不**持有任何 Tailwind class。
 *
 * 字段集合与 `PmTaskResponse` 一一对应（见 `pm-types.ts`），所以加列不需要
 * 新请求：看板已经全量加载过 `tasks`，列表只是换一种投影。
 *
 * 排序 / 分组刻意返回**行数组**而不是新任务对象：行只带 `task` + `depth`，
 * 视图层不再二次索引。
 */

import type {
  Priority,
  PmTaskResponse,
  TaskStatus,
  TaskType,
} from "./pm-types";

/** 列的稳定 key（写进 `metadata.pmListColumns` 的就是它，改名即迁移）。 */
export type PmListColumnKey =
  | "title"
  | "status"
  | "priority"
  | "assignee"
  | "due_at"
  | "type"
  | "review_status"
  | "start_at"
  | "created_by"
  | "parent"
  | "children"
  | "depends"
  | "blocked"
  | "attachments"
  | "submitted_at"
  | "created_at"
  | "updated_at"
  | "dispatch"
  | "id"
  | "description";

/**
 * 派生链取值时要读的关联信息。
 *
 * 这些量都是 `tasks` 的派生（父标题、子任务完成数），服务端不存，所以由
 * 调用方（视图层，持有 byId 索引）注入 —— 纯函数因此可以脱离 store 单测。
 */
export interface PmListValueCtx {
  /** 父任务标题；无父 → `""` */
  parentTitle(task: PmTaskResponse): string;
  /** 直接子任务总数 */
  childCount(task: PmTaskResponse): number;
  /** 直接子任务里 `done` 的数量 */
  childDone(task: PmTaskResponse): number;
}

export interface PmListColumn {
  key: PmListColumnKey;
  /** 表头 / 字段弹层用的 i18n 键（`pm.list.col.*`） */
  labelKey: string;
  /**
   * 默认宽度（px）。`title` 例外：它**不写宽度**，由 `table-layout: fixed`
   * 吸收剩余空间 —— 全列写死 px 时浏览器会把富余平摊给每一列，宽屏读起来
   * 很稀。用户手动拖过 `title` 之后才给它显式宽度。
   */
  width: number | null;
  align: "left" | "right";
  /** 默认显示（v1 = 5 个核心列）。 */
  defaultVisible: boolean;
  /** 不可隐藏（`title` 唯一）。字段弹层里禁用其勾选框。 */
  required: boolean;
  /**
   * 排序取值（必填）。每个列自己声明"这一列按什么比"：枚举走权重数组
   * （状态 / 优先级 / 类型）、时间走 ISO 串、派生列（父任务 / 子任务 / 依赖）
   * 走 `ctx`。必填是为了让漏写变成编译错误，而不是悄悄退化成字段名字符串。
   */
  sortVal: (task: PmTaskResponse, ctx: PmListValueCtx) => string | number;
}

/** 状态列的排序权重 = 看板流转顺序（与 `BOARD_COLUMNS` 同一语义）。 */
export const STATUS_ORDER: TaskStatus[] = [
  "pending",
  "in_progress",
  "submitted",
  "done",
  "rejected",
  "cancelled",
];

/** 优先级排序权重：高在前（与看板读法一致，紧急最重）。 */
export const PRIORITY_ORDER: Priority[] = ["urgent", "high", "normal", "low"];

/** 类型分组顺序（与 `TaskTypeIcon` 的枚举顺序一致）。 */
export const TYPE_ORDER: TaskType[] = [
  "task",
  "feature",
  "bug",
  "chore",
  "checkpoint",
  "milestone",
];

/** 无负责人在筛选 / 分组里的哨兵值（`assignee === null`）。 */
export const UNASSIGNED = "__unassigned__";

const text = (v: string | null | undefined): string => v ?? "";

export const PM_LIST_COLUMNS: PmListColumn[] = [
  {
    key: "title",
    labelKey: "pm.list.col.title",
    width: null,
    align: "left",
    defaultVisible: true,
    required: true,
    sortVal: (t) => t.title.toLowerCase(),
  },
  {
    key: "status",
    labelKey: "pm.list.col.status",
    width: 104,
    align: "left",
    defaultVisible: true,
    required: false,
    sortVal: (t) => STATUS_ORDER.indexOf(t.status),
  },
  {
    key: "priority",
    labelKey: "pm.list.col.priority",
    width: 84,
    align: "left",
    defaultVisible: true,
    required: false,
    sortVal: (t) => PRIORITY_ORDER.indexOf(t.priority),
  },
  {
    // 排序按 instance_id（显示名要 join agentStore，纯函数拿不到）；
    // 未指派恒排最后 —— 哨兵值取一个比任何 id 都大的字符串。
    key: "assignee",
    labelKey: "pm.list.col.assignee",
    width: 142,
    align: "left",
    defaultVisible: true,
    required: false,
    sortVal: (t) => t.assignee ?? "\uffff",
  },
  {
    key: "due_at",
    labelKey: "pm.list.col.dueAt",
    width: 104,
    align: "left",
    defaultVisible: true,
    required: false,
    sortVal: (t) => t.due_at ?? "\uffff",
  },
  {
    key: "type",
    labelKey: "pm.list.col.type",
    width: 92,
    align: "left",
    defaultVisible: false,
    required: false,
    sortVal: (t) => TYPE_ORDER.indexOf(t.type),
  },
  {
    key: "review_status",
    labelKey: "pm.list.col.reviewStatus",
    width: 100,
    align: "left",
    defaultVisible: false,
    required: false,
    sortVal: (t) => t.review_status,
  },
  {
    key: "start_at",
    labelKey: "pm.list.col.startAt",
    width: 132,
    align: "left",
    defaultVisible: false,
    required: false,
    sortVal: (t) => t.start_at ?? "\uffff",
  },
  {
    key: "created_by",
    labelKey: "pm.list.col.createdBy",
    width: 130,
    align: "left",
    defaultVisible: false,
    required: false,
    sortVal: (t) => t.created_by,
  },
  {
    key: "parent",
    labelKey: "pm.list.col.parent",
    width: 190,
    align: "left",
    defaultVisible: false,
    required: false,
    sortVal: (t, ctx) => ctx.parentTitle(t) || "\uffff",
  },
  {
    key: "children",
    labelKey: "pm.list.col.children",
    width: 76,
    align: "right",
    defaultVisible: false,
    required: false,
    sortVal: (t, ctx) => ctx.childCount(t),
  },
  {
    key: "depends",
    labelKey: "pm.list.col.depends",
    width: 76,
    align: "right",
    defaultVisible: false,
    required: false,
    sortVal: (t) => t.depends_on?.length ?? 0,
  },
  {
    key: "blocked",
    labelKey: "pm.list.col.blocked",
    width: 76,
    align: "left",
    defaultVisible: false,
    required: false,
    sortVal: (t) => t.blocked_by?.length ?? 0,
  },
  {
    key: "attachments",
    labelKey: "pm.list.col.attachments",
    width: 70,
    align: "right",
    defaultVisible: false,
    required: false,
    sortVal: (t) => t.attachments?.length ?? 0,
  },
  {
    key: "submitted_at",
    labelKey: "pm.list.col.submittedAt",
    width: 132,
    align: "left",
    defaultVisible: false,
    required: false,
    sortVal: (t) => t.submitted_at ?? "\uffff",
  },
  {
    key: "created_at",
    labelKey: "pm.list.col.createdAt",
    width: 132,
    align: "left",
    defaultVisible: false,
    required: false,
    sortVal: (t) => t.created_at,
  },
  {
    key: "updated_at",
    labelKey: "pm.list.col.updatedAt",
    width: 132,
    align: "left",
    defaultVisible: false,
    required: false,
    sortVal: (t) => t.updated_at,
  },
  {
    key: "dispatch",
    labelKey: "pm.list.col.dispatch",
    width: 84,
    align: "right",
    defaultVisible: false,
    required: false,
    sortVal: (t) => t.dispatch_attempts ?? 0,
  },
  {
    key: "id",
    labelKey: "pm.list.col.id",
    width: 96,
    align: "left",
    defaultVisible: false,
    required: false,
    sortVal: (t) => t.id,
  },
  {
    key: "description",
    labelKey: "pm.list.col.description",
    width: 280,
    align: "left",
    defaultVisible: false,
    required: false,
    sortVal: (t) => text(t.description),
  },
];

const COLUMN_MAP: Map<PmListColumnKey, PmListColumn> = new Map(
  PM_LIST_COLUMNS.map((c) => [c.key, c]),
);

/** 列元信息；未知 key → `undefined`（调用方负责过滤脏数据）。 */
export function pmListColumn(key: string): PmListColumn | undefined {
  return COLUMN_MAP.get(key as PmListColumnKey);
}

/** v1 默认列（5 个）。 */
export const DEFAULT_PM_LIST_COLUMNS: PmListColumnKey[] = PM_LIST_COLUMNS.filter(
  (c) => c.defaultVisible,
).map((c) => c.key);

/** 全部列 key（字段弹层的「可添加」候选池 = 本数组减去已显示）。 */
export const ALL_PM_LIST_COLUMN_KEYS: PmListColumnKey[] = PM_LIST_COLUMNS.map(
  (c) => c.key,
);

// ── 派生链：筛选 → 排序 → 树 → 分组 ─────────────────────────────────

/** 表格行：任务 + 缩进层级（0 = 根）。 */
export interface PmListRow {
  task: PmTaskResponse;
  depth: number;
}

/**
 * 手动拖过的列宽（`key → px`）。
 *
 * v1 只在视图内存态生效，不写 metadata（决策 4）。类型放在 lib 里是为了让
 * 「列宽是列的派生状态」这件事只有一处定义；持久化时它的落点是
 * `metadata.pmListWidths`。
 */
export type PmColumnWidths = Partial<Record<PmListColumnKey, number>>;

export interface PmListFilters {
  /** 标题 / ID / 描述 / 负责人显示名 */
  query: string;
  status: TaskStatus[];
  priority: Priority[];
  /** `instance_id` 集合；`UNASSIGNED` = 未指派 */
  assignee: string[];
}

export const EMPTY_PM_LIST_FILTERS: PmListFilters = {
  query: "",
  status: [],
  priority: [],
  assignee: [],
};

export function hasActivePmListFilters(f: PmListFilters): boolean {
  return (
    f.query.trim() !== "" ||
    f.status.length > 0 ||
    f.priority.length > 0 ||
    f.assignee.length > 0
  );
}

/**
 * 搜索命中的判定面：标题 + 任务 ID + 描述 + 负责人显示名。
 *
 * `assigneeLabel` 由调用方注入（显示名要 join agentStore / 用户目录），
 * 缺省时退化成只按 ID 匹配 —— 与「不猜名字」的原则一致：解析不出来就不要
 * 假装能搜到。
 */
export function filterPmListTasks(
  tasks: readonly PmTaskResponse[],
  filters: PmListFilters,
  assigneeLabel: (id: string | null) => string,
): PmTaskResponse[] {
  const q = filters.query.trim().toLowerCase();
  const statusSet = new Set(filters.status);
  const prioritySet = new Set(filters.priority);
  const assigneeSet = new Set(filters.assignee);
  return tasks.filter((t) => {
    if (q) {
      const hay = `${t.title}\n${t.id}\n${t.description ?? ""}\n${assigneeLabel(t.assignee)}`;
      if (!hay.toLowerCase().includes(q)) return false;
    }
    if (statusSet.size && !statusSet.has(t.status)) return false;
    if (prioritySet.size && !prioritySet.has(t.priority)) return false;
    if (assigneeSet.size && !assigneeSet.has(t.assignee ?? UNASSIGNED)) return false;
    return true;
  });
}

/** 排序方向：`1` 升序、`-1` 降序、`0` 不排序（列头第三态）。 */
export type PmListSortDir = 1 | -1 | 0;

export interface PmListSort {
  key: PmListColumnKey | null;
  dir: PmListSortDir;
}

export const NO_PM_LIST_SORT: PmListSort = { key: null, dir: 0 };

/**
 * 列头点击的三态循环：升序 → 降序 → 取消。
 *
 * 换列时从升序重新开始（用户切列的意图是"先看这列的小值"）。
 */
export function nextPmListSort(current: PmListSort, key: PmListColumnKey): PmListSort {
  if (current.key !== key) return { key, dir: 1 };
  if (current.dir === 1) return { key, dir: -1 };
  if (current.dir === -1) return NO_PM_LIST_SORT;
  return { key, dir: 1 };
}

/**
 * 稳定排序：同值时保持 `tasks` 里的原始顺序（服务端返回序）。
 *
 * `Array.prototype.sort` 在现代引擎里已稳定，这里显式写成带索引兜底的形态，
 * 是为了让"未排序时顺序 = 服务端顺序"这条契约在实现层可读。
 */
export function sortPmListTasks(
  tasks: readonly PmTaskResponse[],
  sort: PmListSort,
  ctx: PmListValueCtx,
): PmTaskResponse[] {
  const column = sort.key ? pmListColumn(sort.key) : undefined;
  if (!column || sort.dir === 0) return tasks.slice();
  const value = column.sortVal;
  return tasks
    .map((task, index) => ({ task, index }))
    .sort((a, b) => {
      const va = value(a.task, ctx);
      const vb = value(b.task, ctx);
      if (va < vb) return -1 * sort.dir;
      if (va > vb) return 1 * sort.dir;
      return a.index - b.index;
    })
    .map((x) => x.task);
}

/**
 * 折叠集合的类型别名（`Set` 而不是数组：视图层每次切换只改一个 id，
 * 用 Set 让"是否折叠"是 O(1)）。
 */
export type PmListCollapsed = ReadonlySet<string>;

/**
 * 把（已筛选、已排序的）任务扁平化成表格行。
 *
 * 三条规则，都是为了让"树"和"筛选结果"不打架：
 *   1. 父任务不在筛选结果里时，子任务**升为根**（否则整枝消失，用户以为
 *      筛选坏了）；
 *   2. 折叠把子任务从**行集**里去掉（父任务本身保留）。所以页脚的"显示 N 条"
 *      和分页装箱都会随折叠变化 —— 这是有意的：折叠的语义就是"这些行现在
 *      不想看"，让用户看到的计数等于他看到的行；
 *   3. 环保护：`parent_id` 自指 / 互指的脏数据不会让 walk 无限递归。
 */
export function flattenPmListRows(
  tasks: readonly PmTaskResponse[],
  sort: PmListSort,
  ctx: PmListValueCtx,
  collapsed: PmListCollapsed,
): PmListRow[] {
  const present = new Set(tasks.map((t) => t.id));
  const childrenByParent = new Map<string, PmTaskResponse[]>();
  const roots: PmTaskResponse[] = [];
  for (const t of tasks) {
    if (t.parent_id && present.has(t.parent_id)) {
      const list = childrenByParent.get(t.parent_id);
      if (list) list.push(t);
      else childrenByParent.set(t.parent_id, [t]);
    } else {
      roots.push(t);
    }
  }
  const out: PmListRow[] = [];
  const walk = (task: PmTaskResponse, depth: number, trail: Set<string>) => {
    out.push({ task, depth });
    if (collapsed.has(task.id)) return;
    const kids = childrenByParent.get(task.id);
    if (!kids || !kids.length) return;
    for (const kid of sortPmListTasks(kids, sort, ctx)) {
      if (trail.has(kid.id)) continue; // 脏数据环：不重复下钻
      walk(kid, depth + 1, new Set([...trail, kid.id]));
    }
  };
  for (const root of sortPmListTasks(roots, sort, ctx)) {
    walk(root, 0, new Set([root.id]));
  }
  return out;
}

/** 分组维度（`none` = 平铺）。 */
export type PmListGroupBy = "none" | "status" | "priority" | "assignee" | "type";

export interface PmListGroup {
  /** 分组标识：状态 / 优先级 / 类型枚举值，负责人 = `instance_id` 或 `UNASSIGNED` */
  id: string;
  rows: PmListRow[];
}

/**
 * 按维度分组，返回**有序**的组数组（组内保持传入行序）。
 *
 * 组名不在这里解析：`id` 是枚举值或身份 id，翻译成可见标签是视图层的事
 * （`pm.board.*` / `pm.priority.*` / 成员显示名）。排序权重在这里给全，
 * 因为组的先后是数据语义（紧急在前），不是样式。
 */
export function groupPmListRows(
  rows: readonly PmListRow[],
  groupBy: PmListGroupBy,
): PmListGroup[] {
  if (groupBy === "none") {
    return rows.length ? [{ id: "", rows: rows.slice() }] : [];
  }
  const buckets = new Map<string, PmListRow[]>();
  for (const row of rows) {
    const id = groupKeyOf(row.task, groupBy);
    const list = buckets.get(id);
    if (list) list.push(row);
    else buckets.set(id, [row]);
  }
  const ordered = [...buckets.entries()].sort(
    (a, b) =>
      groupOrder(a[0], groupBy) - groupOrder(b[0], groupBy) ||
      a[0].localeCompare(b[0]),
  );
  return ordered.map(([id, groupRows]) => ({ id, rows: groupRows }));
}

function groupKeyOf(task: PmTaskResponse, groupBy: Exclude<PmListGroupBy, "none">): string {
  switch (groupBy) {
    case "status":
      return task.status;
    case "priority":
      return task.priority;
    case "type":
      return task.type;
    case "assignee":
      return task.assignee ?? UNASSIGNED;
  }
}

function groupOrder(id: string, groupBy: Exclude<PmListGroupBy, "none">): number {
  switch (groupBy) {
    case "status":
      return STATUS_ORDER.indexOf(id as TaskStatus);
    case "priority":
      return PRIORITY_ORDER.indexOf(id as Priority);
    case "type":
      return TYPE_ORDER.indexOf(id as TaskType);
    // 已指派在前、未指派垫底；已指派之间按 id 字典序（localeCompare 兜底）。
    case "assignee":
      return id === UNASSIGNED ? 1 : 0;
  }
}

/**
 * 字段弹层的「上移 / 下移」：返回新数组，不在原数组上 splice。
 *
 * `title` 列不参与换位（它必须留在第 0 位）：隐藏它已经被弹层禁用，
 * 若还能把它挪到中间，表格里"任务"列会跑到别的位置，读起来像列错位。
 */
export function movePmListColumn(
  keys: readonly PmListColumnKey[],
  key: PmListColumnKey,
  delta: -1 | 1,
): PmListColumnKey[] {
  if (key === "title") return keys.slice();
  const from = keys.indexOf(key);
  if (from < 0) return keys.slice();
  let to = from + delta;
  // 第 0 位是 title 的地盘。
  if (to < 1) to = 1;
  if (to >= keys.length) return keys.slice();
  const next = keys.slice();
  const [moved] = next.splice(from, 1);
  next.splice(to, 0, moved);
  return next;
}

/* ── 分页 ────────────────────────────────────────────────────────────
 *
 * 分页单元是**根任务子树**（一个 `depth === 0` 的行 + 紧跟其后的所有
 * `depth > 0` 行），不是单行：按行硬切会把父任务留在上一页、子任务跑到下一页，
 * 缩进结构当场失真。所以这里先把行还原成子树，再按「累计行数不超过
 * `pageSize`」装页 —— 一页的实际行数可能少于 `pageSize`（最后一个子树放不下
 * 就整棵挪到下一页），但绝不会劈开一棵子树。
 *
 * 只有一个子树本身就超过 `pageSize` 时也要给出这一页（否则翻页会死循环），
 * 所以预算判断带 `|| 当前页为空` 的兜底。
 *
 * 分组时分页在组内进行（组的先后顺序不变），跨页的组会在下一页重新出标题行。
 */

/** 一个分页单元：一个根任务及其（未折叠的）后代行。 */
export interface PmListUnit {
  groupIndex: number;
  rows: PmListRow[];
}

/** 分页结果：本页要渲染的组 + 页码信息。 */
export interface PmListPage {
  /** 本页的组（组内只含落在本页的单元；空组已剔除，顺序与传入一致）。 */
  groups: PmListGroup[];
  /** 当前页（0-based，已 clamp 到 `[0, pageCount-1]`）。 */
  pageIndex: number;
  /** 总页数，至少 1（空列表也是 1 页，页脚不会显示 0/0）。 */
  pageCount: number;
  /** 本页渲染的任务行数（不含分组标题行）。 */
  rowCount: number;
  /** 本页第一行的行号（1-based，用于页脚文案；装箱按子树来，所以不是 pageSize 的整数倍）。 */
  startRow: number;
  /** 本页最后一行的行号（含）。空列表为 0。 */
  endRow: number;
  /** 全部任务行数（筛选后的总量）。 */
  totalRows: number;
  /** 分页单元总数（= 根任务数）。 */
  unitCount: number;
}

/**
 * 把（已分组的）行切成子树单元。
 *
 * `flattenPmListRows` 保证一棵子树的行是连续的（深度 0 开头，深度 >0 属于
 * 当前单元），这里只做线性切分；万一上游顺序变了（脏数据 / 新调用方），
 * 遇到非连续的深度 >0 行就把它当作新单元的开头，绝不丢行。
 */
export function splitPmListUnits(groups: readonly PmListGroup[]): PmListUnit[] {
  const units: PmListUnit[] = [];
  groups.forEach((group, groupIndex) => {
    let current: PmListUnit | null = null;
    for (const row of group.rows) {
      if (row.depth === 0 || !current) {
        current = { groupIndex, rows: [row] };
        units.push(current);
      } else {
        current.rows.push(row);
      }
    }
  });
  return units;
}

/**
 * 按子树单元分页。`pageIndex` 越界会被 clamp（数据变少时不会白屏）。
 *
 * `pageSize < 1` 视为不分页（整表一页）：调用方（页脚的下拉）不该给出这种值，
 * 但纯函数不能因为脏值把列表切成 0 行的页。
 */
export function paginatePmListGroups(
  groups: readonly PmListGroup[],
  pageSize: number,
  pageIndex: number,
): PmListPage {
  const units = splitPmListUnits(groups);
  const totalRows = units.reduce((sum, unit) => sum + unit.rows.length, 0);
  const empty: PmListPage = {
    groups: [],
    pageIndex: 0,
    pageCount: 1,
    rowCount: 0,
    startRow: 0,
    endRow: 0,
    totalRows,
    unitCount: units.length,
  };
  if (!units.length) return empty;
  if (pageSize < 1) {
    return {
      groups: groups.map((g) => ({ id: g.id, rows: g.rows.slice() })),
      pageIndex: 0,
      pageCount: 1,
      rowCount: totalRows,
      startRow: 1,
      endRow: totalRows,
      totalRows,
      unitCount: units.length,
    };
  }

  // 贪心装箱：累计行数触到 pageSize 就换页（单棵子树超预算时独占一页）。
  const pages: PmListUnit[][] = [];
  let bucket: PmListUnit[] = [];
  let bucketRows = 0;
  for (const unit of units) {
    if (bucket.length && bucketRows + unit.rows.length > pageSize) {
      pages.push(bucket);
      bucket = [];
      bucketRows = 0;
    }
    bucket.push(unit);
    bucketRows += unit.rows.length;
  }
  if (bucket.length) pages.push(bucket);

  const index = Math.min(Math.max(Math.trunc(pageIndex) || 0, 0), pages.length - 1);
  const pageUnits = pages[index];
  const rows = pageUnits.reduce((sum, unit) => sum + unit.rows.length, 0);
  // 行号是累计出来的（装箱不均匀），不是 `pageIndex * pageSize + 1`。
  const before = pages
    .slice(0, index)
    .reduce((sum, p) => sum + p.reduce((s, u) => s + u.rows.length, 0), 0);

  // 组顺序保持原样，组内行按单元顺序拼接。
  const byGroup = new Map<number, PmListRow[]>();
  for (const unit of pageUnits) {
    const list = byGroup.get(unit.groupIndex);
    if (list) list.push(...unit.rows);
    else byGroup.set(unit.groupIndex, unit.rows.slice());
  }
  const pageGroups = groups
    .map((group, i) => ({ id: group.id, rows: byGroup.get(i) ?? [] }))
    .filter((group) => group.rows.length > 0);

  return {
    groups: pageGroups,
    pageIndex: index,
    pageCount: pages.length,
    rowCount: rows,
    startRow: before + 1,
    endRow: before + rows,
    totalRows,
    unitCount: units.length,
  };
}
