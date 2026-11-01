// @vitest-environment jsdom
/**
 * ChatUsageIcon — the per-chat usage button on the Chat composers.
 *
 * Locked behaviors (plan §4.6 phase two):
 * - an empty agent roster (human-only DM / group) renders NOTHING —
 *   there is no router spend to show and a dead button reads as a bug;
 * - the popover opens on hover and shows the merged totals, the
 *   per-model share, and — only for a multi-agent ledger — the
 *   per-agent split (合计 + per-agent 明细, per the confirmed口径);
 * - a fresh chat with no recorded calls falls back to the empty copy.
 */
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ChatUsageIcon } from "./ChatUsageIcon";
import type { ChatUsageSummary } from "../../lib/chatUsage";

const mocks = vi.hoisted(() => ({
  fetchChatUsage: vi.fn(),
}));

vi.mock("../../lib/chatUsage", () => ({
  fetchChatUsage: mocks.fetchChatUsage,
}));

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

function summary(over: Partial<ChatUsageSummary> = {}): ChatUsageSummary {
  return {
    perAgent: [],
    llmCallCount: 0,
    totalInput: 0,
    totalOutput: 0,
    totalCacheRead: 0,
    totalCacheWrite: 0,
    totalCost: null,
    modelItems: [],
    lastActiveMs: null,
    ...over,
  };
}

function agentRow(input: number, calls: number): ChatUsageSummary["perAgent"][number] {
  return {
    agentId: "agent-x",
    row: {
      chat_session_id: "c-1",
      chat_kind: "user_group",
      created_at: "2026-11-01T00:00:00Z",
      last_active_at: "2026-11-01T00:00:00Z",
      llm_call_count: calls,
      tokens: {
        last_input: 0,
        last_output: 0,
        total_input: input,
        total_output: 10,
        last_cache_read: 0,
        last_cache_write: 0,
        total_cache_read: 0,
        total_cache_write: 0,
      },
      model_usage: null,
    },
  };
}

describe("ChatUsageIcon", () => {
  beforeEach(() => {
    mocks.fetchChatUsage.mockReset();
    mocks.fetchChatUsage.mockResolvedValue(summary());
  });

  it("renders nothing when the chat has no agent participants", () => {
    const { container } = render(
      <ChatUsageIcon agentIds={[]} chatSessionId="c-1" />,
    );
    expect(container.firstChild).toBeNull();
    expect(mocks.fetchChatUsage).not.toHaveBeenCalled();
  });

  it("shows the merged totals and the model share in the popover", async () => {
    mocks.fetchChatUsage.mockResolvedValue(
      summary({
        llmCallCount: 4,
        totalInput: 12_345,
        totalOutput: 678,
        totalCost: 0.42,
        perAgent: [agentRow(12_345, 4)],
        modelItems: [
          {
            provider: "openai",
            model: "gpt-4o",
            total_input: 12_345,
            total_output: 678,
            total_cache_read: 0,
            total_cache_write: 0,
            total_cost: 0.42,
          },
        ],
      }),
    );
    const { container } = render(
      <ChatUsageIcon agentIds={["agent-x"]} chatSessionId="c-1" />,
    );
    await waitFor(() => expect(mocks.fetchChatUsage).toHaveBeenCalled());

    fireEvent.mouseEnter(container.firstElementChild!);
    expect(screen.getByRole("dialog", { name: "chatUsage.title" })).toBeTruthy();
    expect(screen.getByText("4")).toBeTruthy();
    expect(screen.getByText("12.3K")).toBeTruthy();
    expect(screen.getByText("$0.42")).toBeTruthy();
    // Model share row (single agent → no per-agent section).
    expect(screen.getByText("openai/gpt-4o")).toBeTruthy();
    expect(screen.queryByText("chatUsage.byAgent")).toBeNull();
  });

  it("splits by agent only when more than one participant carries a row", async () => {
    mocks.fetchChatUsage.mockResolvedValue(
      summary({
        llmCallCount: 5,
        perAgent: [
          { ...agentRow(100, 3), agentId: "a-1" },
          { ...agentRow(50, 2), agentId: "a-2" },
        ],
      }),
    );
    const { container } = render(
      <ChatUsageIcon
        agentIds={["a-1", "a-2"]}
        chatSessionId="c-1"
        resolveName={(id) => `Name-${id}`}
      />,
    );
    await waitFor(() => expect(mocks.fetchChatUsage).toHaveBeenCalled());
    fireEvent.mouseEnter(container.firstElementChild!);
    expect(screen.getByText("chatUsage.byAgent")).toBeTruthy();
    expect(screen.getByText("Name-a-1")).toBeTruthy();
    expect(screen.getByText("Name-a-2")).toBeTruthy();
  });

  it("a chat with no recorded calls reads as the empty copy", async () => {
    mocks.fetchChatUsage.mockResolvedValue(summary());
    const { container } = render(
      <ChatUsageIcon agentIds={["agent-x"]} chatSessionId="c-1" />,
    );
    await waitFor(() => expect(mocks.fetchChatUsage).toHaveBeenCalled());
    fireEvent.mouseEnter(container.firstElementChild!);
    expect(screen.getByText("chatUsage.noData")).toBeTruthy();
  });
});
