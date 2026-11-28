/**
 * taskTree — 项目任务的**结构**视图，以及"哪些任务不能当父任务 / 不能当依赖"
 * 这两条结构规则的唯一实现。
 *
 * 为什么单独成模块：挑父任务、挑依赖都不是"检索"，是**结构选择**。候选集由
 * 下面两条闭包定义，跟标题、时间、相关度毫无关系 —— 没有可匹配的东西，也没有
 * 可排序的东西，所以这里只有一个内存索引和两次图遍历。
 *
 * - `subtreeIds`：自己 + 全部后代。任务不能挂在自己的后代下面（成环）。
 * - `dependentClosureIds`：**传递**依赖自己的那些任务。让其中之一成为自己的
 *   依赖，`depends_on` 图就闭成环（服务端 `dependency_cycle` 会 409）。
 *
 * 依赖的**语义**（阻塞）也只有一处实现：`dependencySatisfied`，与服务端的
 * `dispatch_blocked` 同一条规则。
 *
 * 三者都在看板已经持有的那份数组上做纯内存遍历（N 个任务、每条边访问一次），
 * 所以选择器每次渲染重算都负担得起 —— 不需要接口，也不需要缓存失效策略。
 *
 * ⚠️ 环的判定按**所有**边算：服务端的传递检测只沿 `blocks` 边（依赖的唯一
 * 语义，存量 `relates` / `duplicates` 由迁移改写成 `blocks`），客户端多算一类
 * 边只是更保守，不会放进来一个服务端会拒的候选。
 */
import type { PmTaskResponse, TaskStatus } from "./pm-types";

export interface TaskTreeIndex {
  byId: Map<string, PmTaskResponse>;
  /** 父 id → 直接子任务（保持输入数组里的顺序）。 */
  childrenByParent: Map<string, PmTaskResponse[]>;
  /** 没有父任务、或父任务不在给定集合里的任务（后者是防御：筛选过的子集）。 */
  roots: PmTaskResponse[];
  /** `depends_on` 的反向边：这些任务在等我。 */
  dependents: Map<string, PmTaskResponse[]>;
}

/**
 * 一次遍历建好选择器需要的全部索引。
 *
 * 保持输入顺序：看板把任务按它自己的顺序交过来，选择器的行序就跟着走 —— 同一
 * 份数据在别处是什么顺序，在这里就是什么顺序，不引入第二种排序。
 */
export function buildTaskTree(tasks: PmTaskResponse[]): TaskTreeIndex {
  const byId = new Map<string, PmTaskResponse>();
  const childrenByParent = new Map<string, PmTaskResponse[]>();
  const dependents = new Map<string, PmTaskResponse[]>();

  for (const task of tasks) byId.set(task.id, task);

  for (const task of tasks) {
    // 父任务不在集合里 → 当成根：否则整枝会凭空消失。
    const parentId = task.parent_id && byId.has(task.parent_id) ? task.parent_id : null;
    if (parentId) {
      const siblings = childrenByParent.get(parentId);
      if (siblings) siblings.push(task);
      else childrenByParent.set(parentId, [task]);
    }
  }

  const roots = tasks.filter(
    (task) => !task.parent_id || !byId.has(task.parent_id),
  );

  for (const task of tasks) {
    for (const dep of task.depends_on ?? []) {
      const waiting = dependents.get(dep.task_id);
      if (waiting) waiting.push(task);
      else dependents.set(dep.task_id, [task]);
    }
  }

  return { byId, childrenByParent, roots, dependents };
}

/** 自己 + 全部后代。父任务的候选集要从全集里减掉这一整块。 */
export function subtreeIds(tree: TaskTreeIndex, rootId: string): Set<string> {
  const out = new Set<string>();
  if (!tree.byId.has(rootId)) return out;
  const queue = [rootId];
  while (queue.length > 0) {
    const id = queue.pop() as string;
    if (out.has(id)) continue;
    out.add(id);
    for (const child of tree.childrenByParent.get(id) ?? []) queue.push(child.id);
  }
  return out;
}

/**
 * 传递依赖 `rootId` 的任务集合（不含 `rootId` 自己）。
 *
 * 这些任务是依赖的**禁用**候选：让 `rootId` 依赖它们，等于在 `depends_on`
 * 图里插一条回边，服务端会以 `dependency_cycle` 拒绝。
 */
export function dependentClosureIds(tree: TaskTreeIndex, rootId: string): Set<string> {
  const out = new Set<string>();
  const queue = (tree.dependents.get(rootId) ?? []).map((task) => task.id);
  while (queue.length > 0) {
    const id = queue.pop() as string;
    if (id === rootId || out.has(id)) continue;
    out.add(id);
    for (const waiter of tree.dependents.get(id) ?? []) queue.push(waiter.id);
  }
  return out;
}

/** 根 → 父 的祖先链（不含自己）。用于把已选项所在的那一枝默认展开。 */
export function ancestorIds(tree: TaskTreeIndex, id: string): string[] {
  const out: string[] = [];
  const seen = new Set<string>([id]);
  let cursor = tree.byId.get(id)?.parent_id ?? null;
  while (cursor && !seen.has(cursor)) {
    seen.add(cursor);
    out.unshift(cursor);
    cursor = tree.byId.get(cursor)?.parent_id ?? null;
  }
  return out;
}

/**
 * 依赖是否已满足 —— 与服务端 `dispatch_blocked`（`store/sqlite/mod.rs`）**同一条
 * 规则**：只有 `done` / `cancelled` 算满足，`submitted`（等人类验收）仍然挡路。
 *
 * 只有一种语义（阻塞），所以没有 `kind` 参数：`blocks` / `relates` 不再是选择，
 * `kind` 只是历史字段（存量 `relates` 由服务端一次性迁移改写为 `blocks`）。
 * 前端若开始按 `kind` 分支，就等于把这个漏洞重新装回来。
 */
export function dependencySatisfied(status: TaskStatus | undefined): boolean {
  return status === "done" || status === "cancelled";
}

/** 已选项 + 它们各自的祖先 —— 选择面板打开时应当展开的节点集合。 */
export function expandedForSelection(tree: TaskTreeIndex, selected: string[]): Set<string> {
  const out = new Set<string>();
  for (const id of selected) for (const ancestor of ancestorIds(tree, id)) out.add(ancestor);
  return out;
}
