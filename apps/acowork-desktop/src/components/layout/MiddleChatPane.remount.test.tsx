import { render, screen } from "@testing-library/react";
import { useEffect } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { MiddleChatPane } from "./MiddleChatPane";

/**
 * The remount contract of the middle pane's view selection. Both real panes own
 * per-view scroll state (a `FollowController` plus the `useVirtualizer` inside
 * `VirtualRowList`), so a switch between views must mount a new instance rather
 * than hand the new view the previous view's geometry baselines — the defect
 * that parked a switched agent transcript mid-list while a fresh mount always
 * positioned correctly. Stubs count mounts; node identity is the other half.
 */
const { mounts } = vi.hoisted(() => ({ mounts: [] as string[] }));

vi.mock("../chat/AgentPage", () => ({
  AgentPage: () => {
    useEffect(() => {
      mounts.push("agent");
    }, []);
    return <div data-testid="pane" data-kind="agent" />;
  },
}));
vi.mock("../chat/GroupChatPanel", () => ({
  GroupChatPanel: () => {
    useEffect(() => {
      mounts.push("group");
    }, []);
    return <div data-testid="pane" data-kind="group" />;
  },
}));
vi.mock("../chat/EmptyChatPane", () => ({
  EmptyChatPane: () => {
    useEffect(() => {
      mounts.push("empty");
    }, []);
    return <div data-testid="pane" data-kind="empty" />;
  },
}));

describe("MiddleChatPane view identity", () => {
  beforeEach(() => {
    mounts.length = 0;
  });

  it("switching agent or group mounts a NEW pane; the same view never remounts", () => {
    const { rerender } = render(<MiddleChatPane agentId="agent-1" groupId={null} />);
    const agent1 = screen.getByTestId("pane");
    expect(agent1.dataset.kind).toBe("agent");
    expect(mounts).toEqual(["agent"]);

    // Re-rendering the same view (a keystroke, a poll tick) must keep it.
    rerender(<MiddleChatPane agentId="agent-1" groupId={null} />);
    expect(mounts).toEqual(["agent"]);
    expect(screen.getByTestId("pane")).toBe(agent1);

    // Another agent: a new instance, not the previous agent's baselines.
    rerender(<MiddleChatPane agentId="agent-2" groupId={null} />);
    expect(mounts).toEqual(["agent", "agent"]);
    const agent2 = screen.getByTestId("pane");
    expect(agent2).not.toBe(agent1);

    // An open group replaces the agent page.
    rerender(<MiddleChatPane agentId={null} groupId="group-1" />);
    expect(mounts).toEqual(["agent", "agent", "group"]);
    const group1 = screen.getByTestId("pane");
    expect(group1.dataset.kind).toBe("group");
    expect(group1).not.toBe(agent2);

    // Another group (or DM) — the sibling of the agent-switch case.
    rerender(<MiddleChatPane agentId={null} groupId="group-2" />);
    expect(mounts).toEqual(["agent", "agent", "group", "group"]);
    expect(screen.getByTestId("pane")).not.toBe(group1);
  });

  it("the empty state is the only pane with neither view selected", () => {
    const { rerender } = render(<MiddleChatPane agentId={null} groupId={null} />);
    expect(screen.getByTestId("pane").dataset.kind).toBe("empty");

    rerender(<MiddleChatPane agentId={null} groupId="group-1" />);
    expect(screen.getByTestId("pane").dataset.kind).toBe("group");

    rerender(<MiddleChatPane agentId={null} groupId={null} />);
    expect(screen.getByTestId("pane").dataset.kind).toBe("empty");
  });
});