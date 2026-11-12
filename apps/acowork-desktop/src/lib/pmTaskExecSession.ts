/**
 * PM 任务 → 它的执行 session（「查看执行」的解析与跳转）。
 *
 * 任务记录里没有执行 session id：PM 只说"派给谁"，session 是目标 Runtime
 * 建的，谁建谁持有。唯一的跨域坐标写在**群聊**里 —— 派发行带 `task_id`
 * （ADR-106 D15），该任务的每轮工作在群里留一条 `execution_round` 引用行，
 * 带着 `agent_instance_id` / `agent_session_id` 三元组（ADR-106 D3）。所以
 * 解析 = 在项目群聊的转录里找这个任务的引用行，跳转 = 复用群聊里
 * "查看执行" 的同一套动作（选 agent → 冷开 exec tab → 切执行面）。
 *
 * 为什么扫转录而不是加一个服务端索引：现有契约里没有任何一处把
 * task 和 exec session 放进同一张表（PM 不知道 session，user 服务只知道
 * chat 行）。一个索引要横跨 PM / Gateway / user 三个进程，而"审核时看一眼
 * 执行过程"是一次点击 —— 客户端翻两页就够。
 */
import { getGatewayUrl } from "./config";
import { sessionExists } from "./session-control";
import { useAuthStore } from "../stores/authStore";
import { useAgentStore } from "../stores/agentStore";
import { useChatStore } from "../stores/chatStore";
import { useChatSessionStore } from "../stores/chatSessionStore";
import { useProjectGroupStore } from "../stores/projectGroupStore";
import { useLayoutStore } from "../stores/layoutStore";
import type { PmTaskResponse } from "./pm-types";

/** 每次读到用户服务 `/messages` 的上限（`chat_sessions_api::MAX_PAGE`）。 */
const PAGE = 200;

/**
 * ponytail: 最多往回翻 5 页（最新 1000 行）。
 *
 * 天花板：任务派发行早于群聊最新 1000 行时解析不到（按钮提示"未找到执行
 * 会话"）。群聊一页装 200 行、审核中的任务都是刚跑完的活，实测项目群
 * 549 行、最新任务落在倒数第 1 行；升级路径是让 user 服务按 `task_id`
 * 建一条可查索引（`GET /api/chat-sessions/{sid}/rounds?task_id=`）。
 */
const MAX_PAGES = 5;

/** 解析只需要转录行的这几个坐标，用结构类型而不是整个 `ChatMessageRow`。 */
interface RoundRowParts {
  kind: string;
  meta?: {
    task_id?: string;
    agent_instance_id?: string;
    agent_session_id?: string;
  };
}

export interface TaskExecSession {
  agentInstanceId: string;
  execSessionId: string;
}

export type TaskExecOpenResult =
  /** 已跳转。 */
  | "ok"
  /** 解析不到：项目还没有群聊，或群聊里没有这个任务的引用行。 */
  | "not_found"
  /** 引用行在，但执行 session 已被删除。 */
  | "gone";

/**
 * 这个状态的任务背后有没有执行 session 可看 —— 三态：干活中 / 交活待审 /
 * 已完成。`pending`（还没开工）/ `rejected` / `cancelled` 上没有"过程"可看。
 *
 * 一处判断，两个入口用（看板卡片、任务弹框底栏）：哪个界面把按钮摆出来是
 * 同一个事实，分成两份判断就迟早给出两种答案。
 */
export function canViewExecSession(status: PmTaskResponse["status"]): boolean {
  return status === "in_progress" || status === "submitted" || status === "done";
}

/**
 * 一页转录里该任务**最新**的执行 session。
 *
 * 行在页内按 seq 升序，所以从尾部往回扫：同一个任务续跑过多次时，最后一条
 * 引用行就是最后那次执行。缺 instance / session 的行定位不到任何 session
 * （D3：坐标不齐的引用行没有块），跳过而不是构造半个地址。
 */
export function taskExecSessionIn(
  rows: readonly RoundRowParts[],
  taskId: string,
): TaskExecSession | null {
  for (let i = rows.length - 1; i >= 0; i--) {
    const row = rows[i];
    if (row?.kind !== "execution_round") continue;
    if (row.meta?.task_id !== taskId) continue;
    const agentInstanceId = row.meta.agent_instance_id?.trim() ?? "";
    const execSessionId = row.meta.agent_session_id?.trim() ?? "";
    if (agentInstanceId && execSessionId) return { agentInstanceId, execSessionId };
  }
  return null;
}

/** 项目群聊的 session id（`project_group` 与项目一一对应，project_id 是它的幂等键）。 */
async function projectGroupChatId(projectId: string): Promise<string | null> {
  const find = (): string | null =>
    useProjectGroupStore.getState().groups.find((g) => g.project_id === projectId)?.session_id ??
    null;
  const cached = find();
  if (cached) return cached;
  // 侧边栏的 AgentList 开机就拉过一次；这里只兜"侧边栏还没落地就点了按钮"。
  await useProjectGroupStore.getState().fetchGroups();
  return find();
}

/** 一页转录（`offset` 从最新往回数，页内 seq 升序）。 */
async function readPage(chatSessionId: string, offset: number): Promise<RoundRowParts[]> {
  const token = useAuthStore.getState().accessToken;
  const resp = await fetch(
    `${getGatewayUrl()}/api/chat-sessions/${encodeURIComponent(chatSessionId)}/messages` +
      `?offset=${offset}&limit=${PAGE}`,
    { headers: token ? { Authorization: `Bearer ${token}` } : {} },
  );
  if (!resp.ok) {
    const text = await resp.text().catch(() => "");
    throw new Error(`chat messages ${chatSessionId}@${offset} → ${resp.status} ${text.slice(0, 200)}`);
  }
  const body = (await resp.json()) as { messages?: RoundRowParts[] };
  return body.messages ?? [];
}

/**
 * 「查看执行」：解析出任务的执行 session，切到那个执行面的聊天界面。
 *
 * 跳转动作用的是群聊里 "查看执行" 的同一组动作（`GroupChatPanel.jumpToExec`），
 * 只是一开始不在群聊页上，所以先把主区切回 chat —— `currentView` 归
 * `AppLayout` 所有，跨组件请求走 layoutStore 的 consume-once seq 契约。
 * 已删除的 session 不跳（否则冷开一个每轮加载都失败的空 exec tab）。
 */
export async function openTaskExecSession(
  projectId: string,
  taskId: string,
): Promise<TaskExecOpenResult> {
  const chatSessionId = await projectGroupChatId(projectId);
  if (!chatSessionId) return "not_found";
  let hit: TaskExecSession | null = null;
  for (let page = 0; page < MAX_PAGES; page++) {
    const rows = await readPage(chatSessionId, page * PAGE);
    hit = taskExecSessionIn(rows, taskId);
    if (hit) break;
    // 不满一页 = 已经到最老一行，没有更早的页可翻。
    if (rows.length < PAGE) break;
  }
  if (!hit) return "not_found";
  if (!(await sessionExists(hit.agentInstanceId, hit.execSessionId))) return "gone";

  useLayoutStore.getState().requestNavView("chat");
  useAgentStore.getState().selectAgent(hit.agentInstanceId);
  await useChatStore.getState().openSession(hit.agentInstanceId, hit.execSessionId, { prepend: true });
  useChatSessionStore.getState().setMode(hit.agentInstanceId, "exec");
  return "ok";
}