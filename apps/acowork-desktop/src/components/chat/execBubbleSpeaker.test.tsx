/**
 * Execution-surface regression tests: the initiator-side bubble renders by
 * SPEAKER, not by role.
 *
 * Why this test exists (the reported defect): an execution session writes
 * EVERY inbound turn — a human's message and an Agent's intent alike — as a
 * `user` conversation entry, so both render paths of the execution view
 * (`MessageBubble`'s user branch, `UserWithAttachmentsBubble`) treated the
 * right-hand side as a person and printed it as literal text. In an
 * agent-to-agent run the initiator is an Agent and its body is markdown (a
 * PM dispatch brief with `##` headings and numbered steps — verified against
 * a real session transcript), so the reader saw the syntax characters
 * instead of the document.
 *
 * The contract pinned here is DOM shape, not CSS (jsdom computes no styles):
 *   1. an Agent initiator's bubble goes through the `.prose` markdown shell,
 *      and takes the agent skin — `bg-chat-bubble`, NOT `bg-chat-user`. The
 *      pairing is the point: markdown inside the accent bubble is the
 *      "user bubble text turned black" defect (`speakerBubble.ts`), so a
 *      test that only checked the parser would let the skin drift.
 *   2. a human initiator's bubble is still literal: no `.prose`, the `**`
 *      characters survive. This is what `GroupChatPanel.userLiteral` pins for
 *      the chat surfaces; the execution view must keep it.
 *   3. an unresolved initiator (`"unknown"` — registry or directory has not
 *      answered, or the agent was uninstalled) keeps the literal rendering.
 *      Guessing "agent" would parse a person's typed words.
 *   4. the bubble stays on the initiator's side and keeps its tail corner in
 *      every case — only colors follow the speaker.
 */
import { render } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { MessageBubble } from "./MessageBubble";
import { UserWithAttachmentsBubble } from "./UserWithAttachmentsBubble";
import type { ChatMessage } from "../../lib/types";
import type { SpeakerKind } from "./speakerBubble";

// The avatars are not under test, and `AgentAvatar` resolves its image through
// the gateway's avatar-file endpoint, which no test network serves.
vi.mock("../common/AgentAvatar", () => ({
  AgentAvatar: () => <span data-testid="agent-avatar" />,
}));
vi.mock("../common/UserAvatar", () => ({
  UserAvatar: () => <span data-testid="user-avatar" />,
  BUILTIN_ICONS: {},
}));

/** A dispatch brief in the shape the real thing arrives in. */
const MARKDOWN_BRIEF = `## 需要做的事
1. 定位组件。
2. 改成**点按钮即选中**。`;

const PLAIN_WORDS = "先写个plan文档留档吧";

function userMessage(content: string): ChatMessage {
  return {
    id: "u1",
    type: "user",
    content,
    timestamp: 1_700_000_000_000,
    senderDisplayName: "PM",
  };
}

function renderBubble(speakerKind: SpeakerKind, content = MARKDOWN_BRIEF) {
  return render(
    <MessageBubble
      message={userMessage(content)}
      currentSessionId="s1"
      liveUserName="PM"
      speakerKind={speakerKind}
      senderAgentId={speakerKind === "agent" ? "inst-pm" : null}
    />,
  );
}

function renderPartsBubble(speakerKind: SpeakerKind, content = MARKDOWN_BRIEF) {
  return render(
    <UserWithAttachmentsBubble
      userMessage={userMessage(content)}
      parts={[]}
      currentSessionId="s1"
      liveUserName="PM"
      speakerKind={speakerKind}
      senderAgentId={speakerKind === "agent" ? "inst-pm" : null}
      agentId="inst-sse"
    />,
  );
}

describe.each([
  ["MessageBubble (execution view, plain user turn)", renderBubble],
  [
    "UserWithAttachmentsBubble (execution view, user turn with parts)",
    renderPartsBubble,
  ],
])("%s", (_name, renderPath) => {
  it("renders an Agent initiator's speech as markdown, in the agent skin", () => {
    const { container } = renderPath("agent");

    // Parsed: the shell is there and the syntax became structure.
    expect(container.querySelector(".prose")).not.toBeNull();
    expect(container.querySelector("h2")?.textContent).toContain("需要做的事");
    expect(container.querySelector("ol li")).not.toBeNull();
    expect(container.querySelector("strong")?.textContent).toContain("点按钮即选中");
    // The raw markers are gone — that is what "parsed" means.
    expect(container.textContent).not.toContain("## 需要做的事");

    // Skinned: agent bubble, and NOT the accent human bubble. The pairing of
    // parser and skin is the whole contract (see the file header).
    const bubble = container.querySelector<HTMLElement>(".bg-chat-bubble");
    expect(bubble).not.toBeNull();
    expect(container.querySelector(".bg-chat-user")).toBeNull();
    // Still the initiator's side: right-aligned column with the `br` tail.
    expect(bubble!.className).toContain("rounded-br-sm");
  });

  it("renders a human initiator's words literally, in the accent skin", () => {
    const { container } = renderPath("user");

    expect(container.querySelector(".prose")).toBeNull();
    expect(container.querySelector("h2")).toBeNull();
    expect(container.querySelector("strong")).toBeNull();
    // Every marker survives as a character.
    expect(container.textContent).toContain("## 需要做的事");
    expect(container.textContent).toContain("**点按钮即选中**");

    expect(container.querySelector(".bg-chat-bubble")).toBeNull();
    expect(container.querySelector(".bg-chat-user")).not.toBeNull();
  });

  it("keeps an unresolved initiator literal — no guessing it is an Agent", () => {
    const { container } = renderPath("unknown");

    expect(container.querySelector(".prose")).toBeNull();
    expect(container.textContent).toContain("## 需要做的事");
    expect(container.querySelector(".bg-chat-user")).not.toBeNull();
  });

  it("draws the initiator with the agent avatar only when the speaker is an Agent", () => {
    const agent = renderPath("agent");
    expect(agent.container.querySelector('[data-testid="agent-avatar"]')).not.toBeNull();
    expect(agent.container.querySelector('[data-testid="user-avatar"]')).toBeNull();

    const human = renderPath("user");
    expect(human.container.querySelector('[data-testid="user-avatar"]')).not.toBeNull();
    expect(human.container.querySelector('[data-testid="agent-avatar"]')).toBeNull();
  });

  it("renders short plain words unchanged in both skins", () => {
    const human = renderPath("user", PLAIN_WORDS);
    expect(human.container.textContent).toContain(PLAIN_WORDS);

    const agent = renderPath("agent", PLAIN_WORDS);
    expect(agent.container.textContent).toContain(PLAIN_WORDS);
    expect(agent.container.querySelector(".prose")).not.toBeNull();
  });
});
