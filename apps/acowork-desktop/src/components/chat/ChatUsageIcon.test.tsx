// @vitest-environment jsdom
/**
 * ChatUsageIcon — the per-chat usage button on the Chat composers.
 *
 * Locked behaviors:
 * - an empty agent roster (human-only DM / group) renders NOTHING —
 *   there is no router spend to show and a dead button reads as a bug;
 * - the popover opens on hover and shows the merged totals, the
 *   per-model share, and — only for a multi-agent ledger — the
 *   per-agent split (合计 + per-agent 明细, per the confirmed口径);
 * - the headline reads the router's HISTORY-BUDGET occupancy
 *   (`history_tokens` / `history_max_tokens`) — never the
 *   provider-reported prompt count, which also carries the system
 *   prompt and the tool schemas and would render as >100%;
 * - an unmeasured occupancy renders a dash, not a fabricated 0%;
 * - the context-window editor writes the SAME wire field the Agent
 *   Setup panel's "Chat Session Router" card writes, and only on the
 *   surfaces that name a single editor agent;
 * - a fresh chat with no recorded calls falls back to the empty copy.
 */
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ChatUsageIcon } from "./ChatUsageIcon";
import type { ChatUsageSummary } from "../../lib/chatUsage";

const mocks = vi.hoisted(() => ({
  fetchChatUsage: vi.fn(),
  fetchRouterWindow: vi.fn(),
  saveRouterWindow: vi.fn(),
}));

vi.mock("../../lib/chatUsage", () => ({
  fetchChatUsage: mocks.fetchChatUsage,
}));

vi.mock("../../lib/chatRouterConfig", async () => {
  const actual = await vi.importActual<
    typeof import("../../lib/chatRouterConfig")
  >("../../lib/chatRouterConfig");
  return {
    ...actual,
    fetchRouterWindow: mocks.fetchRouterWindow,
    saveRouterWindow: mocks.saveRouterWindow,
  };
});

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
    historyTokens: null,
    ...over,
  };
}

function agentRow(
  input: number,
  calls: number,
  historyTokens: number | null = null,
): ChatUsageSummary["perAgent"][number] {
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
      history_tokens: historyTokens,
    },
  };
}

/**
 * The popover's editor input. The pencil button and the input share the
 * `contextUsage.editWindow` label (same as the exec surface), so the
 * input is selected by its tag, not by the label.
 */
function windowInput(): HTMLInputElement {
  const input = document.querySelector<HTMLInputElement>(
    'input[type="number"]',
  );
  if (!input) throw new Error("window editor input not found");
  return input;
}

describe("ChatUsageIcon", () => {
  beforeEach(() => {
    mocks.fetchChatUsage.mockReset();
    mocks.fetchChatUsage.mockResolvedValue(summary());
    mocks.fetchRouterWindow.mockReset();
    mocks.fetchRouterWindow.mockResolvedValue(null);
    mocks.saveRouterWindow.mockReset();
    mocks.saveRouterWindow.mockResolvedValue(true);
  });

  it("renders nothing when the chat has no agent participants", () => {
    const { container } = render(
      <ChatUsageIcon agentIds={[]} chatSessionId="c-1" />,
    );
    expect(container.firstChild).toBeNull();
    expect(mocks.fetchChatUsage).not.toHaveBeenCalled();
  });

  it("shows the model share in the popover and drops the ledger counters", async () => {
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
    // Model share row (single agent → no per-agent section). The name
    // appears EXACTLY once: the by-model breakdown is the only place the
    // serving model is named (the old headline line that duplicated it is
    // gone by request).
    expect(screen.getAllByText("openai/gpt-4o").length).toBe(1);
    // The share row renders the model's WHOLE-CHAT cumulative tokens. A
    // single-model chat shows no percentage: "100%" is not information and
    // reads like it belongs to the headline's per-turn percent.
    const dialogText = screen.getByRole("dialog", { name: "chatUsage.title" })
      .textContent;
    expect(dialogText).toMatch(/13\.0K/);
    expect(dialogText).not.toMatch(/100\s*%/);
    // The cumulative measure is spelled out in the section header itself, so
    // the two numbers cannot be read as the headline's per-turn figure.
    expect(dialogText).toContain("chatUsage.byModel");
    expect(screen.getByText("chatUsage.usedThisTurn")).toBeTruthy();
    // The cumulative ledger counters are deliberately absent.
    expect(screen.queryByText("chatUsage.llmCalls")).toBeNull();
    expect(screen.queryByText("chatUsage.totalInput")).toBeNull();
    expect(screen.queryByText("chatUsage.totalOutput")).toBeNull();
    expect(screen.queryByText("chatUsage.cacheRead")).toBeNull();
    expect(screen.queryByText("chatUsage.byAgent")).toBeNull();
  });

  it("shows the model share only when there is more than one model to compare", async () => {
    const item = (
      provider: string,
      model: string,
      input: number,
    ): ChatUsageSummary["modelItems"][number] => ({
      provider,
      model,
      total_input: input,
      total_output: 0,
      total_cache_read: 0,
      total_cache_write: 0,
      total_cost: 0.1,
    });
    mocks.fetchChatUsage.mockResolvedValue(
      summary({
        llmCallCount: 2,
        totalInput: 3_000,
        perAgent: [agentRow(3_000, 2)],
        modelItems: [item("openai", "gpt-4o", 2_000), item("anthropic", "claude-sonnet-5", 1_000)],
      }),
    );
    const { container } = render(
      <ChatUsageIcon agentIds={["agent-x"]} chatSessionId="c-1" />,
    );
    await waitFor(() => expect(mocks.fetchChatUsage).toHaveBeenCalled());
    fireEvent.mouseEnter(container.firstElementChild!);
    const dialogText = screen.getByRole("dialog", { name: "chatUsage.title" })
      .textContent;
    // 2,000 / 3,000 = 66.7%, 1,000 / 3,000 = 33.3%.
    expect(dialogText).toMatch(/2\.0K\s*·\s*66\.7%/);
    expect(dialogText).toMatch(/1\.0K\s*·\s*33\.3%/);
  });

  it("names the serving model once, in the by-model breakdown only", async () => {
    mocks.fetchChatUsage.mockResolvedValue(
      summary({
        llmCallCount: 1,
        perAgent: [agentRow(10, 1, 2_000)],
        modelItems: [
          {
            provider: "anthropic",
            model: "claude-sonnet-5",
            total_input: 10,
            total_output: 2,
            total_cache_read: 0,
            total_cache_write: 0,
            total_cost: 0.1,
          },
        ],
      }),
    );
    const { container } = render(
      <ChatUsageIcon agentIds={["agent-x"]} chatSessionId="c-1" />,
    );
    await waitFor(() => expect(mocks.fetchChatUsage).toHaveBeenCalled());
    fireEvent.mouseEnter(container.firstElementChild!);
    // Exactly one occurrence — a second one means the removed headline
    // line (or anything else) crept back in and re-introduced the
    // duplication the user flagged.
    expect(screen.getAllByText("anthropic/claude-sonnet-5").length).toBe(1);
    expect(screen.getByText("chatUsage.byModel")).toBeTruthy();
  });

  it("the headline reads the history-budget occupancy, not the prompt count", async () => {
    mocks.fetchRouterWindow.mockResolvedValue(8_000);
    mocks.fetchChatUsage.mockResolvedValue(
      summary({
        llmCallCount: 1,
        // 2,000 of the 8,000-token budget = 25%. The provider-reported
        // prompt count (last_input) is deliberately absent from the
        // headline — it carries the system prompt + tool schemas.
        historyTokens: 2_000,
        perAgent: [agentRow(10, 1, 2_000)],
      }),
    );
    const { container } = render(
      <ChatUsageIcon
        agentIds={["agent-x"]}
        chatSessionId="c-1"
        editableAgentId="agent-x"
      />,
    );
    await waitFor(() => expect(mocks.fetchChatUsage).toHaveBeenCalled());
    fireEvent.mouseEnter(container.firstElementChild!);
    expect(screen.getByText("25.0%")).toBeTruthy();
    expect(screen.getByText("2.0K")).toBeTruthy();
    expect(screen.getByText("8.0K")).toBeTruthy();
  });

  it("an unmeasured occupancy renders a dash, never a fabricated 0%", async () => {
    mocks.fetchChatUsage.mockResolvedValue(
      summary({
        llmCallCount: 1,
        historyTokens: null,
        perAgent: [agentRow(10, 1, null)],
      }),
    );
    const { container } = render(
      <ChatUsageIcon agentIds={["agent-x"]} chatSessionId="c-1" />,
    );
    await waitFor(() => expect(mocks.fetchChatUsage).toHaveBeenCalled());
    fireEvent.mouseEnter(container.firstElementChild!);
    expect(screen.queryByText("0.0%")).toBeNull();
    // ...and the dash explains itself instead of leaving the reader to
    // guess why a ledger with recorded calls has no occupancy.
    expect(
      screen.getByTitle("chatUsage.usedNotMeasured"),
    ).toBeTruthy();
  });

  it("keeps one type scale: the section headers all sit on the same step", async () => {
    mocks.fetchChatUsage.mockResolvedValue(
      summary({
        llmCallCount: 5,
        totalCacheRead: 400,
        totalInput: 1_000,
        totalOutput: 100,
        perAgent: [
          { ...agentRow(600, 3), agentId: "a-1" },
          { ...agentRow(500, 2), agentId: "a-2" },
        ],
        modelItems: [
          {
            provider: "openai",
            model: "gpt-4o",
            total_input: 1_100,
            total_output: 0,
            total_cache_read: 400,
            total_cache_write: 0,
            total_cost: 0.2,
          },
        ],
      }),
    );
    const { container } = render(
      <ChatUsageIcon agentIds={["a-1", "a-2"]} chatSessionId="c-1" />,
    );
    await waitFor(() => expect(mocks.fetchChatUsage).toHaveBeenCalled());
    fireEvent.mouseEnter(container.firstElementChild!);
    const dialog = screen.getByRole("dialog", { name: "chatUsage.title" });

    // The popover owns a fixed set of steps — the exec surface's grammar:
    // section header / row / helper / button + the headline figure. Anything
    // else is an accident (the "why is `By model` bigger than its own column
    // header" report).
    const ALLOWED = ["text-sm", "text-xs", "text-10", "text-11", "text-[clamp(1.125rem,4.5vw,1.375rem)]"];
    // Only size-looking utilities (text-sm / text-10 / text-[13px] /
    // text-xl …) — colors (`text-red-600`), alignment (`text-center`) and
    // the `text-text-*` tokens are not sizes.
    const SIZE_RE = /^text-(\[.*\]|base|lg|sm|xs|\d+|\d?xl)$/;
    const steps = [...dialog.querySelectorAll<HTMLElement>("*")].flatMap((el) =>
      (el.getAttribute("class") ?? "").split(/\s+/).filter((c) => SIZE_RE.test(c)),
    );
    expect(steps.length).toBeGreaterThan(0);
    for (const step of steps) expect(ALLOWED).toContain(step);

    // All three section headers are on the ONE header step.
    const headerSize = (key: string) =>
      screen.getByText(key).getAttribute("class")?.match(/\btext-(sm|xs|10|11)\b/)?.[0];
    expect(headerSize("contextUsage.cacheHitLabel")).toBe("text-sm");
    expect(headerSize("chatUsage.byModel")).toBe("text-sm");
    expect(headerSize("chatUsage.byAgent")).toBe("text-sm");
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

  it("offers the window editor only when a single editor agent is named", async () => {
    const { container: groupContainer } = render(
      <ChatUsageIcon agentIds={["a-1", "a-2"]} chatSessionId="c-1" />,
    );
    await waitFor(() => expect(mocks.fetchChatUsage).toHaveBeenCalled());
    fireEvent.mouseEnter(groupContainer.firstElementChild!);
    expect(screen.queryByLabelText("contextUsage.editWindow")).toBeNull();
  });

  it("saves the edited window through the router-config writer", async () => {
    mocks.fetchRouterWindow.mockResolvedValue(8_000);
    const { container } = render(
      <ChatUsageIcon
        agentIds={["agent-x"]}
        chatSessionId="c-1"
        editableAgentId="agent-x"
      />,
    );
    await waitFor(() => expect(mocks.fetchChatUsage).toHaveBeenCalled());
    fireEvent.mouseEnter(container.firstElementChild!);

    fireEvent.click(screen.getByLabelText("contextUsage.editWindow"));
    const input = windowInput();
    fireEvent.change(input, { target: { value: "12" } });
    fireEvent.click(screen.getByText("contextUsage.save"));

    await waitFor(() =>
      expect(mocks.saveRouterWindow).toHaveBeenCalledWith("agent-x", 12_000),
    );
  });

  it("rejects a window outside the router's accepted range", async () => {
    mocks.fetchRouterWindow.mockResolvedValue(8_000);
    const { container } = render(
      <ChatUsageIcon
        agentIds={["agent-x"]}
        chatSessionId="c-1"
        editableAgentId="agent-x"
      />,
    );
    await waitFor(() => expect(mocks.fetchChatUsage).toHaveBeenCalled());
    fireEvent.mouseEnter(container.firstElementChild!);

    fireEvent.click(screen.getByLabelText("contextUsage.editWindow"));
    const input = windowInput();
    fireEvent.change(input, { target: { value: "999" } });
    fireEvent.click(screen.getByText("contextUsage.save"));

    expect(await screen.findByText("contextUsage.windowRangeError")).toBeTruthy();
    expect(mocks.saveRouterWindow).not.toHaveBeenCalled();
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
