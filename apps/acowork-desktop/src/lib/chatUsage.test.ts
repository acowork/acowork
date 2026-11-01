/**
 * `chatUsage.ts` 数据层单测：跨 agent 合并的算术契约。
 *
 * 锁定的行为（plan §4.6 阶段二）：
 * - 合计 = 有台账行的 agent 求和，缺席（`row: null`）不贡献也不报错；
 * - per-model 摊平按 `"{provider}|{model}"` 合并，cost 的 miss
 *   （`null`）沿 ADR-027 poison rule 传染：任一 bucket 无价 → 该模型
 *   与总成本都是 `null`，绝不伪造 0；
 * - `lastActiveMs` 取各 agent 的最大值，非法时间戳被忽略。
 */
import { describe, expect, it } from "vitest";
import { summarizeChatUsage, type AgentChatUsage, type ChatUsageRow } from "./chatUsage";

function row(over: Partial<ChatUsageRow> & Pick<ChatUsageRow, "chat_session_id">): ChatUsageRow {
  return {
    chat_kind: "user_group",
    created_at: "2026-11-01T00:00:00Z",
    last_active_at: "2026-11-01T00:00:00Z",
    llm_call_count: 1,
    tokens: {
      last_input: 0,
      last_output: 0,
      total_input: 0,
      total_output: 0,
      last_cache_read: 0,
      last_cache_write: 0,
      total_cache_read: 0,
      total_cache_write: 0,
    },
    model_usage: null,
    ...over,
  };
}

describe("summarizeChatUsage", () => {
  it("sums the agents that carry a row and skips the absent ones", () => {
    const perAgent: AgentChatUsage[] = [
      {
        agentId: "a-1",
        row: row({
          chat_session_id: "c-1",
          llm_call_count: 3,
          tokens: {
            ...row("c-1").tokens,
            total_input: 100,
            total_output: 10,
            total_cache_read: 60,
          },
          last_active_at: "2026-11-01T01:00:00Z",
        }),
      },
      { agentId: "a-2", row: null },
      {
        agentId: "a-3",
        row: row({
          chat_session_id: "c-1",
          llm_call_count: 2,
          tokens: {
            ...row("c-1").tokens,
            total_input: 50,
            total_output: 5,
          },
          last_active_at: "2026-11-01T02:00:00Z",
        }),
      },
    ];
    const s = summarizeChatUsage(perAgent);
    expect(s.perAgent.map((a) => a.agentId)).toEqual(["a-1", "a-3"]);
    expect(s.llmCallCount).toBe(5);
    expect(s.totalInput).toBe(150);
    expect(s.totalOutput).toBe(15);
    expect(s.totalCacheRead).toBe(60);
    expect(s.lastActiveMs).toBe(Date.parse("2026-11-01T02:00:00Z"));
  });

  it("merges per-model buckets across agents and propagates the cost miss", () => {
    const s = summarizeChatUsage([
      {
        agentId: "a-1",
        row: row({
          chat_session_id: "c-1",
          model_usage: {
            "openai|gpt-4o": { input: 100, output: 10, cache_read: 0, cache_write: 0, cost: 0.5 },
          },
        }),
      },
      {
        agentId: "a-2",
        row: row({
          chat_session_id: "c-1",
          model_usage: {
            // Same bucket on another agent — the numbers fold…
            "openai|gpt-4o": { input: 20, output: 2, cache_read: 5, cache_write: 0, cost: 0.1 },
            // …and a priced-missing bucket reads as "no price known"
            // for that model AND the grand total — never a fabricated 0
            // (ADR-027 poison rule).
            "ollama|llama": { input: 7, output: 3, cache_read: 0, cache_write: 0, cost: null },
          },
        }),
      },
    ]);
    const gpt = s.modelItems.find((i) => i.model === "gpt-4o")!;
    expect(gpt.total_input).toBe(120);
    expect(gpt.total_output).toBe(12);
    expect(gpt.total_cost).toBeCloseTo(0.6);
    const llama = s.modelItems.find((i) => i.model === "llama")!;
    expect(llama.total_cost).toBeNull();
    expect(s.totalCost).toBeNull();
  });

  it("an all-miss roster reads as an empty summary, not an error", () => {
    const s = summarizeChatUsage([
      { agentId: "a-1", row: null },
      { agentId: "a-2", row: null },
    ]);
    expect(s.perAgent).toHaveLength(0);
    expect(s.llmCallCount).toBe(0);
    // 没有计价 bucket 就是没有测量，不是"花了 0 元"（ADR-027：miss 绝不伪造成 0）。
    expect(s.totalCost).toBeNull();
    expect(s.modelItems).toHaveLength(0);
    expect(s.lastActiveMs).toBeNull();
  });
});
