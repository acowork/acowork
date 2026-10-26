import { render, screen } from "@testing-library/react";
import { useEffect } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { AgentPage } from "./AgentPage";

/**
 * The page owns no scroll state, but it decides whether the surface that DOES
 * (`AgentChatView`: the `FollowController` from `useStickToBottom` plus the
 * `useVirtualizer` inside `VirtualRowList`) gets a fresh instance per chat
 * session. Every baseline in there is per-view — the scroll offset, the armed
 * jump anchor, the prepared prepend height — and a reused instance applies the
 * previous session's geometry to the new one, which is what parked an
 * agent-switch transcript mid-list while a fresh mount (startup, or the exec
 * surface through its own `key={currentScrollKey}`) always positioned
 * correctly. Cheap and decisive: a stub counts mounts.
 */

let selectedAgentId: string | null = "agent-a";
let byAgent: Record<string, unknown> = {};
const mounts: string[] = [];

vi.mock("../../stores/agentStore", () => ({
  useAgentStore: (selector: (s: unknown) => unknown) =>
    selector({
      selectedAgentId,
      agents: {
        "agent-a": { meta: { alive: true, name: "A" } },
        "agent-b": { meta: { alive: true, name: "B" } },
      },
    }),
}));

vi.mock("../../stores/chatSessionStore", () => ({
  useChatSessionStore: Object.assign(
    (selector: (s: unknown) => unknown) => selector({ byAgent }),
    { getState: () => ({ openAgent: vi.fn(), setMode: vi.fn() }) },
  ),
}));

vi.mock("./AgentChatView", () => ({
  AgentChatView: ({ sessionId }: { sessionId: string }) => {
    // Mount-only (empty deps): a re-render of a reused instance must not count.
    useEffect(() => {
      mounts.push(sessionId);
    }, []);
    return <div data-testid="transcript" data-session={sessionId} />;
  },
}));

vi.mock("./SessionTabBar", () => ({ SessionTabBar: () => <div /> }));
vi.mock("./ChatHistoryDialog", () => ({ ChatHistoryDialog: () => null }));
vi.mock("./ChatPanel", () => ({ ChatPanel: () => <div /> }));

const resolved = (chatSessionId: string) => ({
  resolved: true,
  chatSessionId,
  mode: "chat",
});

describe("AgentPage transcript identity", () => {
  beforeEach(() => {
    selectedAgentId = "agent-a";
    byAgent = { "agent-a": resolved("chat-a"), "agent-b": resolved("chat-b") };
    mounts.length = 0;
  });

  it("an agent switch mounts a NEW transcript surface, never reuses the instance", () => {
    const { rerender } = render(<AgentPage />);
    const first = screen.getByTestId("transcript");
    expect(first.dataset.session).toBe("chat-a");

    selectedAgentId = "agent-b";
    rerender(<AgentPage />);

    // Reused state is the bug: a second mount is the observable half of the
    // contract that the per-view baselines depend on.
    expect(mounts).toEqual(["chat-a", "chat-b"]);
    const second = screen.getByTestId("transcript");
    expect(second.dataset.session).toBe("chat-b");
    expect(second).not.toBe(first); // different node: the old subtree is gone
  });

  it("re-rendering the SAME agent keeps the surface mounted (no scroll reset per keystroke)", () => {
    const { rerender } = render(<AgentPage />);
    rerender(<AgentPage />);
    rerender(<AgentPage />);
    expect(mounts).toEqual(["chat-a"]);
  });
});