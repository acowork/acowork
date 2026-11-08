import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ContextUsageIcon } from "./ContextUsageIcon";

const mocks = vi.hoisted(() => ({
  chatState: {
    agentStates: {
      "agent-1": {
        sessionStates: {
          "session-1": {
            contextUsage: {
              context_window: 262_100,
              input_tokens: 140_000,
              output_tokens: 4_900,
              total_tokens: 200_000,
              usable_context: 262_100,
              usage_percent: 55.3,
              // ADR-067: sections now sourced from chatStore so the
              // popover works without DevMode. Same byte values the
              // old debugState fixture provided.
              sections: [
                { key: "system_prompt", size_bytes: 11_010 },
                { key: "tool_definitions", size_bytes: 29_910 },
                { key: "messages", size_bytes: 391_020 },
                { key: "skill_instructions", size_bytes: 3_150 },
              ],
            },
            isCompacting: false,
            provider: "openai" as const,
            sessionStatus: { status: "idle" } as const,
          },
        },
      },
    },
    sendCompressAction: vi.fn(),
    cancelCompressAction: vi.fn(),
    fetchSessionState: vi.fn(async () => {}),
  },
  agentState: {
    agents: {
      "agent-1": {
        meta: {
          // DevMode off by default — the per-turn cache-hit comparison
          // row is a debug-only diagnostic and must stay hidden unless
          // a test explicitly flips this on.
          debug_state: "disabled" as const,
          alive: true,
        },
      },
    },
  },
  t: (key: string): string => {
    const translations: Record<string, string> = {
      "contextUsage.title": "上下文用量",
      "contextUsage.close": "关闭",
      "contextUsage.used": "已使用",
      "contextUsage.usageBarLabel": "分类用量占比",
      "contextUsage.categories.systemPrompt": "系统提示词",
      "contextUsage.categories.tools": "工具定义",
      "contextUsage.categories.messages": "对话消息",
      "contextUsage.categories.connectors": "连接器",
      "contextUsage.categories.skills": "技能",
      "contextUsage.compressing": "压缩中...",
      "contextUsage.compressSummary": "压缩整个上下文",
      "contextUsage.cancelCompact": "取消压缩",
      "contextUsage.cacheHitLabel": "缓存命中率（累计）",
      "contextUsage.cacheHitPerTurnLabel": "缓存命中率（实时）",
    };
    return translations[key] ?? key;
  },
}));

vi.mock("../../stores/chatStore", () => ({
  useChatStore: Object.assign(
    (selector: (state: unknown) => unknown) => selector(mocks.chatState),
    // The component's `refreshOnOpen` path reads the store imperatively.
    { getState: () => mocks.chatState },
  ),
}));

vi.mock("../../stores/agentStore", () => ({
  useAgentStore: (selector: (state: unknown) => unknown) => selector(mocks.agentState),
}));

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: mocks.t }),
}));

describe("ContextUsageIcon", () => {
  beforeEach(() => {
    mocks.chatState.sendCompressAction.mockReset();
    mocks.chatState.cancelCompressAction.mockReset();
    mocks.chatState.fetchSessionState.mockReset();
    mocks.chatState.agentStates["agent-1"].sessionStates["session-1"].isCompacting = false;
    mocks.chatState.agentStates["agent-1"].sessionStates["session-1"].sessionStatus =
      { status: "idle" } as const;
  });

  it("renders categorized usage from the latest debug snapshot and can be closed", () => {
    const { container } = render(
      <ContextUsageIcon agentId="agent-1" sessionId="session-1" />,
    );

    fireEvent.mouseEnter(container.firstElementChild!);

    expect(screen.getByRole("dialog", { name: "上下文用量" })).toBeTruthy();
    expect(screen.getByText("55.3%")).toBeTruthy();
    expect(screen.getByText("系统提示词")).toBeTruthy();
    expect(screen.getByText("工具定义")).toBeTruthy();
    expect(screen.getByText("对话消息")).toBeTruthy();
    expect(screen.getByText("连接器")).toBeTruthy();
    expect(screen.getByText("技能")).toBeTruthy();
    expect(screen.getByText("1.4%")).toBeTruthy();
    expect(screen.getByText("3.8%")).toBeTruthy();
    expect(screen.getByText("49.7%")).toBeTruthy();
    expect(screen.getByText("0.4%")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "关闭" }));
    expect(screen.queryByRole("dialog", { name: "上下文用量" })).toBeNull();
  });

  it("uses the shared dropdown surface tokens so light/dark mode both work", () => {
    const { container } = render(
      <ContextUsageIcon agentId="agent-1" sessionId="session-1" />,
    );
    fireEvent.mouseEnter(container.firstElementChild!);

    const dialog = screen.getByRole("dialog", { name: "上下文用量" });
    // Theme-aware palette, NOT a hardcoded black background, and the
    // same width / radius / border as sibling toolbar dropdowns.
    expect(dialog.className).toContain("bg-modal-surface");
    expect(dialog.className).toContain("rounded-md");
    expect(dialog.className).toContain("border-border-outer");
    expect(dialog.className).not.toContain("bg-[#202126]");
    expect(dialog.className).not.toContain("rounded-[22px]");
  });

  it("shows the cache-hit row when provider cache accounting is present", () => {
    // The input-box usage popup shows the session-lifetime (cumulative)
    // window — the same window the right-hand agent-status panel and
    // the bottom status bar use. We feed cumulative totals and assert
    // the ratio is computed from those, not from the per-turn fields.
    mocks.chatState.agentStates["agent-1"].sessionStates["session-1"].contextUsage = {
      ...mocks.chatState.agentStates["agent-1"].sessionStates["session-1"].contextUsage,
      cache_read_tokens: 5_000, // per-turn — must be ignored
      input_tokens: 12_345, // per-turn — must be ignored
      total_cache_read_tokens: 90_000,
      total_input_tokens: 144_900,
    };

    const { container } = render(
      <ContextUsageIcon agentId="agent-1" sessionId="session-1" />,
    );
    fireEvent.mouseEnter(container.firstElementChild!);

    // OpenAI protocol, cumulative: 90K / 144.9K ≈ 0.6211 → "62%".
    expect(screen.getByText("62%")).toBeTruthy();
    expect(screen.getByText("缓存命中率（累计）")).toBeTruthy();
    expect(screen.getByText("90.0K")).toBeTruthy();
    expect(screen.getByText("144.9K")).toBeTruthy();
  });

  it("hides the per-turn cache-hit row while DevMode is off", () => {
    // Even with per-turn cache accounting present, the real-time row is
    // a debug-only diagnostic — it must NOT render unless the agent is
    // in DevMode (`debug_state === "enabled"` && alive).
    mocks.chatState.agentStates["agent-1"].sessionStates["session-1"].contextUsage = {
      ...mocks.chatState.agentStates["agent-1"].sessionStates["session-1"].contextUsage,
      cache_read_tokens: 5_000,
      input_tokens: 12_345,
      total_cache_read_tokens: 90_000,
      total_input_tokens: 144_900,
    };

    const { container } = render(
      <ContextUsageIcon agentId="agent-1" sessionId="session-1" />,
    );
    fireEvent.mouseEnter(container.firstElementChild!);

    // Cumulative row still rendered...
    expect(screen.getByText("缓存命中率（累计）")).toBeTruthy();
    // ...but the real-time comparison row is absent.
    expect(screen.queryByText("缓存命中率（实时）")).toBeNull();
  });

  it("shows the real-time per-turn cache-hit row below the cumulative row in DevMode", () => {
    // DevMode on: the popover renders BOTH windows side by side —
    // cumulative (session-lifetime) on top, per-turn (last LLM call)
    // below it, for direct comparison.
    mocks.agentState.agents["agent-1"].meta.debug_state = "enabled";
    mocks.chatState.agentStates["agent-1"].sessionStates["session-1"].contextUsage = {
      ...mocks.chatState.agentStates["agent-1"].sessionStates["session-1"].contextUsage,
      cache_read_tokens: 5_000,
      input_tokens: 12_345,
      total_cache_read_tokens: 90_000,
      total_input_tokens: 144_900,
    };

    const { container } = render(
      <ContextUsageIcon agentId="agent-1" sessionId="session-1" />,
    );
    fireEvent.mouseEnter(container.firstElementChild!);

    // Cumulative row: 90K / 144.9K → "62%".
    expect(screen.getByText("缓存命中率（累计）")).toBeTruthy();
    expect(screen.getByText("62%")).toBeTruthy();
    expect(screen.getByText("90.0K")).toBeTruthy();
    expect(screen.getByText("144.9K")).toBeTruthy();
    // Per-turn row: OpenAI protocol, 5K / 12.345K → floor(40.5) = "40%".
    expect(screen.getByText("缓存命中率（实时）")).toBeTruthy();
    expect(screen.getByText("40%")).toBeTruthy();
    expect(screen.getByText("5.0K")).toBeTruthy();
    expect(screen.getByText("12.3K")).toBeTruthy();
  });

  it("dispatches a Summary(1) compress action when the button is clicked", () => {
    const { container } = render(
      <ContextUsageIcon agentId="agent-1" sessionId="session-1" />,
    );
    fireEvent.mouseEnter(container.firstElementChild!);

    fireEvent.click(screen.getByRole("button", { name: "压缩整个上下文" }));
    expect(mocks.chatState.sendCompressAction).toHaveBeenCalledWith(
      "agent-1",
      "session-1",
      1, // CompressType::SUMMARY
    );
  });

  it("disables the compress button while the session is busy", () => {
    mocks.chatState.agentStates["agent-1"].sessionStates["session-1"].sessionStatus =
      { status: "running" } as const;

    const { container } = render(
      <ContextUsageIcon agentId="agent-1" sessionId="session-1" />,
    );
    fireEvent.mouseEnter(container.firstElementChild!);

    const btn = screen.getByRole("button", { name: "压缩整个上下文" });
    expect((btn as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(btn);
    expect(mocks.chatState.sendCompressAction).not.toHaveBeenCalled();

    // Restore so other tests aren't affected.
    mocks.chatState.agentStates["agent-1"].sessionStates["session-1"].sessionStatus =
      { status: "idle" } as const;
  });

  it("ADR-083: turns into a clickable cancel button while compacting", () => {
    mocks.chatState.agentStates["agent-1"].sessionStates["session-1"].isCompacting = true;
    const { container } = render(
      <ContextUsageIcon agentId="agent-1" sessionId="session-1" />,
    );
    fireEvent.mouseEnter(container.firstElementChild!);

    const btn = screen.getByRole("button", { name: "取消压缩" });
    // Enabled — this is the whole point of ADR-083 (it used to be disabled).
    expect((btn as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(btn);
    expect(mocks.chatState.cancelCompressAction).toHaveBeenCalledWith(
      "agent-1",
      "session-1",
    );
    expect(mocks.chatState.sendCompressAction).not.toHaveBeenCalled();

    mocks.chatState.agentStates["agent-1"].sessionStates["session-1"].isCompacting = false;
  });

  it("popover breakdown is populated from chatStore (ADR-067) without needing DevMode", () => {
    // No debugState mock, no useDebugStore selector — DevMode is off.
    // The percentages must still come from `contextUsage.sections` on
    // chatStore; before ADR-067 they read from `debugStore` and were
    // all 0 when DevMode was disabled.
    const { container } = render(
      <ContextUsageIcon agentId="agent-1" sessionId="session-1" />,
    );
    fireEvent.mouseEnter(container.firstElementChild!);

    // Same expectations as the primary test: with chatState-provided
    // sections the breakdown percentages must be non-zero.
    expect(screen.getByText("1.4%")).toBeTruthy();
    expect(screen.getByText("3.8%")).toBeTruthy();
    expect(screen.getByText("49.7%")).toBeTruthy();
    expect(screen.getByText("0.4%")).toBeTruthy();
  });

  describe("refreshOnOpen (the Chat block's snapshot contract)", () => {
    it("pulls the session state on each closed→open edge, never while open", async () => {
      const { container } = render(
        <ContextUsageIcon agentId="agent-1" sessionId="session-1" refreshOnOpen />,
      );
      const host = container.firstElementChild!;

      fireEvent.mouseEnter(host);
      expect(mocks.chatState.fetchSessionState).toHaveBeenCalledTimes(1);
      expect(mocks.chatState.fetchSessionState).toHaveBeenCalledWith("agent-1", "session-1");

      // Pointer wobble (leave then re-enter inside the 150ms close grace) does
      // NOT re-fetch: the popover never left the open state.
      fireEvent.mouseLeave(host);
      fireEvent.mouseEnter(host);
      expect(mocks.chatState.fetchSessionState).toHaveBeenCalledTimes(1);

      // A real close (grace timer elapsed) followed by a fresh display DOES.
      fireEvent.mouseLeave(host);
      await new Promise((r) => setTimeout(r, 200));
      fireEvent.mouseEnter(host);
      expect(mocks.chatState.fetchSessionState).toHaveBeenCalledTimes(2);
    });

    it("stays silent without the flag — the exec composer's ring is stream-fed", () => {
      const { container } = render(
        <ContextUsageIcon agentId="agent-1" sessionId="session-1" />,
      );
      fireEvent.mouseEnter(container.firstElementChild!);
      expect(mocks.chatState.fetchSessionState).not.toHaveBeenCalled();
    });
  });

  describe("click trigger (the Chat block: hover-open was too jumpy)", () => {
    it("hover does not open; click opens with a fresh pull; outside click and Escape close", () => {
      const { container } = render(
        <ContextUsageIcon
          agentId="agent-1"
          sessionId="session-1"
          trigger="click"
          refreshOnOpen
        />,
      );
      const host = container.firstElementChild!;
      const button = screen.getByRole("button", { name: "contextUsage.ariaLabel" });

      fireEvent.mouseEnter(host);
      expect(screen.queryByRole("dialog")).toBeNull();
      expect(mocks.chatState.fetchSessionState).not.toHaveBeenCalled();

      fireEvent.click(button);
      expect(screen.getByRole("dialog", { name: "上下文用量" })).toBeTruthy();
      expect(mocks.chatState.fetchSessionState).toHaveBeenCalledTimes(1);

      // A mousedown INSIDE the portalled panel must not close it — the
      // containment test has to cover the portal, not just the wrapper.
      fireEvent.mouseDown(screen.getByRole("dialog"));
      expect(screen.getByRole("dialog", { name: "上下文用量" })).toBeTruthy();

      fireEvent.mouseDown(document.body);
      expect(screen.queryByRole("dialog")).toBeNull();

      // Re-display pulls fresh again.
      fireEvent.click(button);
      expect(mocks.chatState.fetchSessionState).toHaveBeenCalledTimes(2);

      fireEvent.keyDown(document, { key: "Escape" });
      expect(screen.queryByRole("dialog")).toBeNull();
    });
  });
});
