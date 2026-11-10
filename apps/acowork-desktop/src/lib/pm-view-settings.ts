/**
 * PM 视图偏好 — 当前视图（卡片 / 列表）+ 列表列配置。
 *
 * 与 [`pm-board-settings`] 的 `boardPageSize` 完全同一套做法：偏好按项目存进
 * `PmProject.metadata`，随服务端同步（**不是** localStorage），换机器 / 换账号
 * 仍然一致。
 *
 * `metadata` 是任意 JSON，后端 PATCH 时**整体替换**，所以写入一律走本模块的
 * `with*` 函数（先展开旧 metadata 再覆盖单键），直接发 `{ pmView }` 会把图标、
 * `boardPageSize` 等兄弟字段清空。读取一律走 `parse*`：缺失 / 非法值回退默认，
 * 旧项目零迁移。
 *
 * 列宽**不在这里**：v1 只在 `TaskListView` 内存态生效（决策 4 —— 避免把每次
 * 鼠标拖动都写进同步元数据）。后续要持久化时加 `metadata.pmListWidths`，
 * 写入同样必须走本模块的合并语义。
 */

import {
  ALL_PM_LIST_COLUMN_KEYS,
  DEFAULT_PM_LIST_COLUMNS,
  type PmListColumnKey,
} from "./pm-list-columns";

/** 两种任务视图。`board` 是默认，也是改动前的唯一形态。 */
export type PmViewMode = "board" | "list";

export const PM_VIEW_METADATA_KEY = "pmView";
export const PM_LIST_COLUMNS_METADATA_KEY = "pmListColumns";

/** 未设置 / 非法值时的视图。默认卡片 —— 打开项目的行为不因本次改动而变。 */
export const DEFAULT_PM_VIEW_MODE: PmViewMode = "board";

/** 从项目 metadata 解析视图；缺失 / 非 `"list"` 的非法值 → `"board"`。 */
export function parsePmView(
  metadata: Record<string, unknown> | null | undefined,
): PmViewMode {
  const raw = metadata?.[PM_VIEW_METADATA_KEY];
  return raw === "list" ? "list" : DEFAULT_PM_VIEW_MODE;
}

/** 生成回写用 metadata：先合并旧字段再覆盖 `pmView`。 */
export function withPmView(
  metadata: Record<string, unknown> | null | undefined,
  view: PmViewMode,
): Record<string, unknown> {
  return { ...(metadata ?? {}), [PM_VIEW_METADATA_KEY]: view };
}

/**
 * 列数组的归一化（读和写共用同一条规则，否则内存态和 metadata 会漂移）。
 *
 * 三条兜底，全部为了"脏数据不白屏"：
 *   1. 非数组 → 默认 5 列；
 *   2. 未知 key（列改名、手改过 JSON）→ 丢弃；
 *   3. `title` 必须在第 0 位（任务列不可隐藏，也不允许挪位）。
 * 重复 key 只保留第一次出现。
 */
function normalizePmListColumns(raw: unknown): PmListColumnKey[] {
  if (!Array.isArray(raw)) return DEFAULT_PM_LIST_COLUMNS.slice();
  const known = new Set<string>(ALL_PM_LIST_COLUMN_KEYS);
  const seen: PmListColumnKey[] = [];
  for (const item of raw) {
    if (typeof item !== "string" || !known.has(item)) continue;
    const key = item as PmListColumnKey;
    if (!seen.includes(key)) seen.push(key);
  }
  const rest = seen.filter((k) => k !== "title");
  return ["title", ...rest];
}

/** 从项目 metadata 解析显示列；缺失 / 非法 → 默认 5 列。 */
export function parsePmListColumns(
  metadata: Record<string, unknown> | null | undefined,
): PmListColumnKey[] {
  return normalizePmListColumns(metadata?.[PM_LIST_COLUMNS_METADATA_KEY]);
}

/** 生成回写用 metadata：先合并旧字段再覆盖 `pmListColumns`。 */
export function withPmListColumns(
  metadata: Record<string, unknown> | null | undefined,
  columns: readonly PmListColumnKey[],
): Record<string, unknown> {
  return {
    ...(metadata ?? {}),
    [PM_LIST_COLUMNS_METADATA_KEY]: normalizePmListColumns(columns),
  };
}

/**
 * 列表分页每页行数 —— 同样按项目存 metadata（不是 localStorage）。
 *
 * 分页单元是「根任务子树」（见 `pm-list-columns` 的 `paginatePmListGroups`），
 * 所以这个数是**行数预算**而不是"每页几个任务"：一页实际渲染的行数可能小于它
 * （最后一棵子树放不下就整棵挪到下一页）。默认 50 行 ≈ 一屏多一点，页脚下拉
 * 可改（`pm.list.page.sizes`）。
 */
export const PM_LIST_PAGE_SIZE_METADATA_KEY = "pmListPageSize";

export const DEFAULT_PM_LIST_PAGE_SIZE = 50;
export const MIN_PM_LIST_PAGE_SIZE = 10;
export const MAX_PM_LIST_PAGE_SIZE = 200;

/** 可选的每页行数（页脚下拉的选项，全部落在 clamp 区间内）。 */
export const PM_LIST_PAGE_SIZE_OPTIONS = [20, 50, 100, 200] as const;

export function clampPmListPageSize(value: number): number {
  const rounded = Math.round(value);
  if (!Number.isFinite(rounded)) return DEFAULT_PM_LIST_PAGE_SIZE;
  return Math.min(Math.max(rounded, MIN_PM_LIST_PAGE_SIZE), MAX_PM_LIST_PAGE_SIZE);
}

/** 从项目 metadata 解析每页行数；缺失 / 非数字 → 默认 50。 */
export function parsePmListPageSize(
  metadata: Record<string, unknown> | null | undefined,
): number {
  const raw = metadata?.[PM_LIST_PAGE_SIZE_METADATA_KEY];
  if (typeof raw !== "number" || !Number.isFinite(raw)) return DEFAULT_PM_LIST_PAGE_SIZE;
  return clampPmListPageSize(raw);
}

/** 生成回写用 metadata：先合并旧字段再覆盖 `pmListPageSize`。 */
export function withPmListPageSize(
  metadata: Record<string, unknown> | null | undefined,
  pageSize: number,
): Record<string, unknown> {
  return {
    ...(metadata ?? {}),
    [PM_LIST_PAGE_SIZE_METADATA_KEY]: clampPmListPageSize(pageSize),
  };
}
