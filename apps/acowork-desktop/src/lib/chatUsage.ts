/**
 * per-chat token 用量（群聊/单聊 usage 视图）的前端数据层。
 *
 * 数据链路（chat-session-token-accounting-plan §4.6 阶段二）：台账行落在
 * 每个 agent 参与者自己的 Runtime 私有 SQLite（`chat_token_usage`），
 * 前端按 chat 名册里的 agent 扇出 Gateway
 * `GET /api/agents/{id}/chat-usage/{chatId}`（透明代理 → Runtime），在
 * 客户端合并：跨 agent 求和 + per-model 明细摊平。404 = 该 agent 从未为
 * 这个 chat 调过 LLM（纯人类群聊是常态），按"无行"处理，不是错误。
 *
 * 与 `tokenStats.ts` 同一套展示原语：模型占比走
 * `computeTokenSegments`，成本 miss 显示 "—" 绝不伪造 0。
 */
import type { TokenStatItem } from "./types";
import { getGatewayUrl } from "./config";
import { with503Retry } from "./httpRetry";
import { log } from "./logger";

/** 与 runtime `SessionTokens` 的 serde 形状一一对应（snake_case）。 */
export interface ChatUsageTokens {
  last_input: number;
  last_output: number;
  total_input: number;
  total_output: number;
  last_cache_read: number;
  last_cache_write: number;
  total_cache_read: number;
  total_cache_write: number;
}

/** 与 runtime `ModelUsageEntry` 的 serde 形状一一对应。 */
export interface ChatUsageModelEntry {
  input: number;
  output: number;
  cache_read: number;
  cache_write: number;
  /** `null` = 无已知价格（miss 不估计，ADR-027 poison rule）。 */
  cost: number | null;
}

/** 与 `acowork_sqlite::ChatUsageRow` 的 serde 形状一一对应。 */
export interface ChatUsageRow {
  chat_session_id: string;
  chat_kind: string | null;
  /** RFC3339。 */
  created_at: string;
  last_active_at: string;
  llm_call_count: number;
  tokens: ChatUsageTokens;
  /** key = `"{provider}|{model}"`；空明细序列化为 `null`。 */
  model_usage: Record<string, ChatUsageModelEntry> | null;
  /**
   * 路由器投影历史窗口（Block B）最近一轮占用的 tokens ——
   * `chat_router.history_max_tokens` 预算的占用量。
   *
   * `null` = 未测量（v10 之前写的行，或从未投影过历史窗口的轮次）：
   * 界面渲染破折号，绝不伪造 0（ADR-027「宁可 miss 也不估计」）。
   *
   * 这**不是** provider 报的 prompt tokens：那个数还含系统提示词与
   * 工具 schema，常常超过历史预算，不能拿来当预算占用量。
   */
  history_tokens?: number | null;
}

/** 一个 agent 参与者对这张 chat 的台账（`null` = 404，从未记账）。 */
export interface AgentChatUsage {
  agentId: string;
  row: ChatUsageRow | null;
}

/** 跨 agent 合并后的展示模型（usage 菜单与右侧面板共用一份口径）。 */
export interface ChatUsageSummary {
  /** 有台账行的 agent（名册里没有账的参与者被过滤掉）。 */
  perAgent: AgentChatUsage[];
  llmCallCount: number;
  totalInput: number;
  totalOutput: number;
  totalCacheRead: number;
  totalCacheWrite: number;
  /**
   * 任一 agent 的任一 bucket 无价 → `null`（与 `computeAgentTotals` 同规则）。
   * 一个计价 bucket 都没有（名册里没人记过账）也是 `null`：那是"没有数据"，
   * 不是"花了 0 元"。
   */
  totalCost: number | null;
  /** 跨 agent 摊平后的 per-model 明细（喂 `computeTokenSegments`）。 */
  modelItems: TokenStatItem[];
  /** 各 agent `last_active_at` 的最大值（epoch ms；全无 → `null`）。 */
  lastActiveMs: number | null;
  /**
   * 路由器投影历史窗口（Block B）的占用量（tokens）——跨 agent 取
   * **最大值**而非求和：多个 agent 各自投影自己的窗口，预算也是各自
   * 的，求和会得出一个没有对应预算的数。
   *
   * `null` = 没有 agent 测量过（未测量 ≠ 0）。
   */
  historyTokens: number | null;
}

/**
 * 纯合并函数（单测入口）：把逐 agent 的台账行折叠成一个展示模型。
 * fan-out 与 fetch 失败恢复不属于这里——调用方把拉不到的 agent 直接
 * 从输入里剔除或标 `row: null`。
 */
export function summarizeChatUsage(perAgent: AgentChatUsage[]): ChatUsageSummary {
  const rows = perAgent.filter((a) => a.row !== null);
  const merged = new Map<string, TokenStatItem>();
  let totalCost: number | null = 0;
  let lastActiveMs: number | null = null;
  const out: ChatUsageSummary = {
    perAgent: rows,
    llmCallCount: 0,
    totalInput: 0,
    totalOutput: 0,
    totalCacheRead: 0,
    totalCacheWrite: 0,
    totalCost: 0,
    modelItems: [],
    lastActiveMs: null,
    historyTokens: null,
  };
  for (const { row } of rows) {
    if (!row) continue;
    out.llmCallCount += row.llm_call_count;
    out.totalInput += row.tokens.total_input;
    out.totalOutput += row.tokens.total_output;
    out.totalCacheRead += row.tokens.total_cache_read;
    out.totalCacheWrite += row.tokens.total_cache_write;
    // The history-window occupancy is a per-turn snapshot, so the merged
    // value is the MAX across participants (each agent projects its own
    // window against its own budget) — never a sum.
    if (
      typeof row.history_tokens === "number" &&
      (out.historyTokens === null || row.history_tokens > out.historyTokens)
    ) {
      out.historyTokens = row.history_tokens;
    }
    const active = Date.parse(row.last_active_at);
    if (!Number.isNaN(active) && (lastActiveMs === null || active > lastActiveMs)) {
      lastActiveMs = active;
    }
    for (const [key, entry] of Object.entries(row.model_usage ?? {})) {
      const sep = key.indexOf("|");
      const provider = sep >= 0 ? key.slice(0, sep) : "unknown";
      const model = sep >= 0 ? key.slice(sep + 1) : key;
      const mapKey = `${provider}|${model}`;
      const item =
        merged.get(mapKey) ??
        {
          provider,
          model,
          total_input: 0,
          total_output: 0,
          total_cache_read: 0,
          total_cache_write: 0,
          total_cost: 0,
        };
      item.total_input += entry.input;
      item.total_output += entry.output;
      item.total_cache_read += entry.cache_read;
      item.total_cache_write += entry.cache_write;
      if (entry.cost == null) {
        item.total_cost = null;
      } else if (item.total_cost !== null) {
        item.total_cost += entry.cost;
      }
      merged.set(mapKey, item);
      if (entry.cost == null) {
        totalCost = null;
      } else if (totalCost !== null) {
        totalCost += entry.cost;
      }
    }
  }
  out.totalCost = merged.size === 0 ? null : totalCost;
  out.lastActiveMs = lastActiveMs;
  out.modelItems = [...merged.values()];
  return out;
}

/**
 * 拉取一张 chat 的全部 agent 台账并合并。`null` = 没有数据（`agentIds`
 * 为空，或名册里没有一个 agent 记过账——纯人类群聊的常态），调用方按
 * "无数据"渲染、不弹错；它不表示"求和为 0"。单个 agent 拉不到（未注册 /
 * 离线 / 无行）退化为该参与者缺席，不污染其他 agent 的数字。
 */
export async function fetchChatUsage(
  agentIds: string[],
  chatSessionId: string,
): Promise<ChatUsageSummary | null> {
  if (agentIds.length === 0) return null;
  const settled = await Promise.all(
    agentIds.map(async (agentId): Promise<AgentChatUsage> => {
      try {
        const resp = await with503Retry(
          () =>
            fetch(
              `${getGatewayUrl()}/api/agents/${agentId}/chat-usage/${chatSessionId}`,
            ),
          { tag: `fetchChatUsage(${agentId})`, logger: log },
        );
        // 404 = 该 agent 从未为这张 chat 调用过 LLM：合法的"没账"。
        if (!resp.ok || resp.status === 404) return { agentId, row: null };
        return { agentId, row: (await resp.json()) as ChatUsageRow };
      } catch (e) {
        log.warn(`fetchChatUsage(${agentId}) failed: ${String(e)}`);
        return { agentId, row: null };
      }
    }),
  );
  const summary = summarizeChatUsage(settled);
  // A roster where nobody booked a call (the common human-only group chat)
  // has no data to show — not a report of zero spend.
  return summary.perAgent.length === 0 ? null : summary;
}
