/**
 * PM 看板显示偏好 — 单页任务数量。
 *
 * 偏好按项目保存：写进 `PmProject.metadata.boardPageSize`，随项目走服务端
 * 同步（而不是浏览器 localStorage），换机器 / 换账号仍然一致。
 *
 * `metadata` 是任意 JSON，后端 PATCH 时整体替换，所以写入一律走
 * [`withBoardPageSize`]（先展开旧 metadata 再回写），读取一律走
 * [`parseBoardPageSize`]：缺失 / 非法值回退默认 20，越界按上下限收敛 ——
 * 旧项目零迁移。
 */

/** 未设置时的单页任务数。 */
export const DEFAULT_BOARD_PAGE_SIZE = 20;

/** 设置面板提供的一档选择；默认值必须落在其中。 */
export const BOARD_PAGE_SIZE_OPTIONS = [10, 20, 30, 50, 100] as const;

export const MIN_BOARD_PAGE_SIZE = 5;
export const MAX_BOARD_PAGE_SIZE = 200;

/** `metadata.boardPageSize` 的键名（写进项目 metadata）。 */
export const BOARD_PAGE_SIZE_METADATA_KEY = "boardPageSize";

/** 把任意数字收敛到合法区间（整数）。 */
export function clampBoardPageSize(value: number): number {
  const rounded = Math.round(value);
  return Math.min(Math.max(rounded, MIN_BOARD_PAGE_SIZE), MAX_BOARD_PAGE_SIZE);
}

/** 从项目 metadata 解析单页任务数；缺失 / 非法 → 默认 20。 */
export function parseBoardPageSize(
  metadata: Record<string, unknown> | null | undefined,
): number {
  const raw = metadata?.[BOARD_PAGE_SIZE_METADATA_KEY];
  if (typeof raw !== "number" || !Number.isFinite(raw)) return DEFAULT_BOARD_PAGE_SIZE;
  return clampBoardPageSize(raw);
}

/**
 * 生成回写用 metadata：先合并旧字段再覆盖 `boardPageSize`。
 *
 * 后端对 metadata 是整体替换，直接发 `{ boardPageSize }` 会把图标等
 * 兄弟字段清空 —— 这是「先合并再回写」规则的唯一写入口。
 */
export function withBoardPageSize(
  metadata: Record<string, unknown> | null | undefined,
  pageSize: number,
): Record<string, unknown> {
  return {
    ...(metadata ?? {}),
    [BOARD_PAGE_SIZE_METADATA_KEY]: clampBoardPageSize(pageSize),
  };
}

/** 一页的区间信息（1-indexed）。总数为 0 时 start / end 均为 0。 */
export interface BoardPageBounds {
  /** 当前页，已按总页数收敛 */
  page: number;
  totalPages: number;
  start: number;
  end: number;
}

/** 计算分页区间；页码越界（删除 / 拖动任务后）自动收敛到有效页。 */
export function boardPageBounds(
  total: number,
  pageSize: number,
  page: number,
): BoardPageBounds {
  const size = clampBoardPageSize(pageSize);
  const count = Math.max(0, Math.trunc(total));
  const totalPages = Math.max(1, Math.ceil(count / size));
  const requested = Number.isFinite(page) ? Math.trunc(page) : 1;
  const current = Math.min(Math.max(requested || 1, 1), totalPages);
  return {
    page: current,
    totalPages,
    start: count === 0 ? 0 : (current - 1) * size + 1,
    end: count === 0 ? 0 : Math.min(current * size, count),
  };
}

/** 按页码切出当前页的条目（页码同样先收敛）。 */
export function boardPageSlice<T>(items: T[], pageSize: number, page: number): T[] {
  const size = clampBoardPageSize(pageSize);
  const { page: current } = boardPageBounds(items.length, size, page);
  return items.slice((current - 1) * size, current * size);
}

/**
 * 某任务在其所在列的分页页码（1-indexed）；不在该列 → null。
 *
 * 分页切的是列根，子任务跟着父任务整棵展示，所以要沿 `parent_id` 一路
 * 向上走到「本列的列根」，再用列根在 `roots`（列根顺序，与渲染顺序一致）
 * 里的下标换算页码。拖放落定后调用它，把卡片翻到看得见的那一页。
 */
export function boardPageForTask(
  tasks: readonly BoardPageTask[],
  roots: readonly Pick<BoardPageTask, "id">[],
  taskId: string,
  status: string,
  pageSize: number,
): number | null {
  const byId = new Map(tasks.map((task) => [task.id, task]));
  let cursor = byId.get(taskId);
  const visited = new Set<string>();
  while (cursor && cursor.status === status && !visited.has(cursor.id)) {
    visited.add(cursor.id);
    const parent = cursor.parent_id ? byId.get(cursor.parent_id) : undefined;
    if (!parent || parent.status !== status) break; // 父不在本列 → 已是列根
    cursor = parent;
  }
  if (!cursor || cursor.status !== status) return null;
  const index = roots.findIndex((root) => root.id === cursor!.id);
  if (index < 0) return null;
  return Math.floor(index / clampBoardPageSize(pageSize)) + 1;
}

/** [`boardPageForTask`] 需要的最小任务形状（避免 lib 依赖业务类型）。 */
export interface BoardPageTask {
  id: string;
  status: string;
  parent_id: string | null;
}
