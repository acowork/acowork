/**
 * The attribution rule for an agent-origin turn: does a PEER agent's speech sit
 * on this row, and whose?
 *
 * The defect this pins: an `intent_response` row is written as a `user` entry
 * (every inbound turn is), so the transcript painted a peer agent's answer on
 * the initiator's side under the initiator's name — in a human-run session, the
 * human's own name over somebody else's words.
 *
 * The rule has one temptation worth pinning explicitly: guessing. A row with no
 * display projection, no parseable envelope, or an envelope naming the
 * initiator itself is NOT a peer — it keeps the rendering it always had.
 *
 * And the render: the peer's bubble is on the left, wears the agent skin, and
 * shows the peer's name — never the initiator's.
 */
import { render } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { MessageBubble } from "./MessageBubble";
import { peerSpeakerId } from "./speakerBubble";
import { useAgentStore } from "../../stores/agentStore";
import { useAuthStore } from "../../stores/authStore";
import type { ChatMessage, IntentDisplay } from "../../lib/types";

// Neither the directory (a network fetch) nor the avatars are under test.
vi.mock("../../lib/sessionInitiator", async (importOriginal) => {
  const actual =
    await importOriginal<typeof import("../../lib/sessionInitiator")>();
  return { ...actual, useUserDirectory: () => [] };
});
vi.mock("../common/AgentAvatar", () => ({
  AgentAvatar: () => <span data-testid="agent-avatar" />,
}));
vi.mock("../common/UserAvatar", () => ({
  UserAvatar: () => <span data-testid="user-avatar" />,
  BUILTIN_ICONS: {},
}));

const PEER = "8984ef0f-0cc6-4920-b938-4cd7480fe933";
const OTHER_PEER = "bd68e6a7-7dd4-4ca7-82a2-e65745c4d198";

/** The shape a real `intent_response` row has on disk (verified against a live
 *  session transcript): machine envelope in `content`, human projection in
 *  `display`, nothing about the sender but the envelope's `from`. */
const DISPLAY: IntentDisplay = {
  origin: "response",
  action: "schedule",
  summary: "评审任务已创建。",
  body: "### 评审任务已创建。\n\n任务 ID `t-5f425b32`，已指派 SSE.Cassie。",
};

function peerRow(from = PEER): ChatMessage {
  return {
    id: `resp-${from}`,
    type: "user",
    content: `[intent_response:${from}:schedule] {"message":"评审任务已创建。"}`,
    timestamp: 1_700_000_000_000,
    display: DISPLAY,
    senderDisplayName: "Nicholas",
  };
}

describe("peerSpeakerId", () => {
  it("names the sender of an agent-origin turn in a human's session", () => {
    expect(peerSpeakerId(peerRow(), null)).toBe(PEER);
  });

  it("keeps the initiator's own turn on the initiator's side", () => {
    // An agent-initiated session: its own request row names the initiator.
    const own = { ...peerRow(OTHER_PEER) };
    expect(peerSpeakerId(own, OTHER_PEER)).toBeNull();
  });

  it("is silent for a turn with no display projection", () => {
    // A human's own words, and a legacy agent row written before the
    // projection existed. Both keep the rendering they always had.
    const human: ChatMessage = {
      id: "u1",
      type: "user",
      content: "先写个plan文档留档吧",
      timestamp: 1_700_000_000_000,
    };
    expect(peerSpeakerId(human, null)).toBeNull();
    expect(peerSpeakerId({ ...peerRow(), display: undefined }, null)).toBeNull();
  });

  it("is silent when the envelope names nobody", () => {
    expect(
      peerSpeakerId({ ...peerRow(), content: "not an envelope" }, null),
    ).toBeNull();
  });
});

describe("MessageBubble — a peer agent's turn", () => {
  beforeEach(() => {
    useAuthStore.setState({ account: { user_id: "u-nicholas" } as never });
    useAgentStore.setState({ agents: {} });
  });

  function renderRow(from = PEER, initiatorAgentId: string | null = null) {
    return render(
      <MessageBubble
        message={peerRow(from)}
        currentSessionId="s1"
        liveUserName="Nicholas"
        speakerKind={initiatorAgentId ? "agent" : "user"}
        senderAgentId={initiatorAgentId}
      />,
    );
  }

  it("draws the bubble on the left, in the agent skin, as the peer", () => {
    const { container } = renderRow();

    // Left side: the tail corner is the left one, and the human's accent skin
    // (`bg-chat-user`) is nowhere — that bubble is the initiator's.
    const bubble = container.querySelector<HTMLElement>(".bg-chat-bubble");
    expect(bubble).not.toBeNull();
    expect(bubble!.className).toContain("rounded-bl-sm");
    expect(bubble!.className).not.toContain("rounded-br-sm");
    expect(container.querySelector(".bg-chat-user")).toBeNull();
    expect(container.querySelector('[data-testid="agent-avatar"]')).not.toBeNull();

    // The words are the peer's, written for a person: the projection is
    // parsed as markdown and the machine envelope is folded, not painted.
    expect(container.querySelector("h3")?.textContent).toContain("评审任务已创建");
    expect(container.querySelector("details pre")?.textContent).toContain(
      "[intent_response:",
    );
    // Nobody signed it as the human.
    expect(container.textContent).not.toContain("Nicholas");
  });

  it("names the sending agent once the registry has it", () => {
    useAgentStore.setState({
      agents: { [PEER]: { meta: { name: "Docs", display_name: "Docs" } } } as never,
    });
    const { container } = renderRow();
    expect(container.textContent).toContain("Docs");
  });

  it("leaves the initiator's own turn on the initiator's side", () => {
    const { container } = renderRow(OTHER_PEER, OTHER_PEER);
    // The agent initiator's own turn: right side, `br` tail, its own skin.
    const bubble = container.querySelector<HTMLElement>(".bg-chat-bubble");
    expect(bubble!.className).toContain("rounded-br-sm");
    expect(container.textContent).toContain("Nicholas");
  });
});