/**
 * AgentTasksPanel — bot 单聊右侧的任务 tab（`activeTab === "tasks"`）。
 *
 * 界面与项目群聊右侧的任务面板完全同一份代码（[`TasksColumnFlow`]），只是把
 * 数据换成「指派给这个 bot 的任务」：`task.assignee` 是 agent 实例 UUID，与
 * `selectedAgentId` 同一身份体系，跨项目聚合一并取回（`loadAssigned`）。
 *
 * 面板外壳（pane 头 + 宽度）由 `RightPanel` 提供，所以这里不画自己的头 ——
 * 那正是"界面一模一样"的另一半：同样的头、同样的四张状态卡。
 *
 * 边界（有意为之，不是遗漏）：「他的任务」= `assignee == 这个 bot` 的任务。
 * 这是平台里唯一权威的口径 —— `claim_task` 就是按它判归属（未指派的任务被
 * claim 时才把 assignee 原子地写成认领者，已指派的任务只有 assignee 本人能
 * claim）。所以 bot 自己提、还没被人批准开工的提案（`assignee: null` +
 * `review_status: pending`）不在这里；真要露出提案，得给 `TaskFilter` 加
 * `created_by` 维度，而不是在客户端猜。
 */

import { useEffect } from "react";
import { assigneeScope, sameScope, usePmBoardStore } from "../../stores/pm/boardStore";
import { DEFAULT_BOARD_PAGE_SIZE } from "../../lib/pm-board-settings";
import { TasksColumnFlow } from "./TasksColumnFlow";

export function AgentTasksPanel({ agentId }: { agentId: string }) {
  const loadAssigned = usePmBoardStore((s) => s.loadAssigned);
  const boardScope = usePmBoardStore((s) => s.scope);

  useEffect(() => {
    void loadAssigned(agentId);
  }, [agentId, loadAssigned]);

  // 只在这个 bot 自己的作用域已经落到 store 里时才画卡片：换 bot 的那一帧
  // 别把上一个 bot 的任务先显示出来。
  // 外壳（pane 头 / 宽度 / 表面）由 `RightPanel` 提供，这里只占满它给出的
  // 内容区（`fill="flex"` 的滚动根需要一个定高的 flex 列）。
  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-hidden">
      <TasksColumnFlow
        ready={sameScope(boardScope, assigneeScope(agentId))}
        pageSize={DEFAULT_BOARD_PAGE_SIZE}
        scopeKey={agentId}
      />
    </div>
  );
}