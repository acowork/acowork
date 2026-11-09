/**
 * Execution-surface regression tests: a turn voiced as somebody else says so.
 *
 * Why this test exists (plan D3): once a human may append turns to an
 * agent-to-agent execution session, the transcript contains rows whose words are
 * a person's but whose voice is the agent's task. Without a disclosure on the
 * row, "a human kept this run alive" renders identically to "agent-a wrote
 * this" — including the initiator's own agent bubble, because `speakerKind` is
 * still `agent` for the session.
 *
 * The three cases are the whole rule (`speakerProvenance.ts`):
 *   1. a substitution is disclosed — the person's name and whose voice it is;
 *   2. a human speaking in their own right says nothing (mode 3);
 *   3. an ordinary turn (no speaker at all) says nothing.
 *
 * The silent cases are the ones worth pinning: a badge that fires on every line
 * is how a disclosure becomes noise and then stops being read.
 */
import { render } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { MessageBubble } from "./MessageBubble";
import { UserWithAttachmentsBubble } from "./UserWithAttachmentsBubble";
import type { ChatMessage, TurnSpeaker } from "../../lib/types";

vi.mock("../common/AgentAvatar", () => ({
  AgentAvatar: () => <span data-testid="agent-avatar" />,
}));
vi.mock("../common/UserAvatar", () => ({
  UserAvatar: () => <span data-testid="user-avatar" />,
  BUILTIN_ICONS: {},
}));

function userMessage(speaker?: TurnSpeaker): ChatMessage {
  return {
    id: "u1",
    type: "user",
    content: "换成 sonnet 再试一次",
    timestamp: 1_700_000_000_000,
    senderDisplayName: "Nicholas",
    ...(speaker ? { speaker } : {}),
  };
}

const STAND_IN: TurnSpeaker = {
  origin: "human",
  author: "nicholas",
  on_behalf_of: "agent-a",
};

function renderPlain(speaker?: TurnSpeaker) {
  return render(
    <MessageBubble
      message={userMessage(speaker)}
      currentSessionId="s1"
      liveUserName="Nicholas"
      // The session's initiator is still the agent: that is exactly why the
      // badge has to carry the correction.
      speakerKind="agent"
      senderAgentId="agent-a"
    />,
  );
}

function renderParts(speaker?: TurnSpeaker) {
  return render(
    <UserWithAttachmentsBubble
      userMessage={userMessage(speaker)}
      parts={[]}
      currentSessionId="s1"
      liveUserName="Nicholas"
      speakerKind="agent"
      senderAgentId="agent-a"
      agentId="agent-b"
    />,
  );
}

describe.each([
  ["MessageBubble (execution view)", renderPlain],
  ["UserWithAttachmentsBubble (execution view, with parts)", renderParts],
])("%s — speaker provenance", (_name, renderPath) => {
  // The badge's marker is the AUTHOR as recorded (`nicholas`, lowercase), not
  // the translated sentence: the test environment's locale is `en`, and pinning
  // a string from one locale file would make this suite fail on a translation
  // edit rather than on a behaviour change. The live user name renders as
  // `Nicholas` (case differs), so the lowercase id appears only in the badge.
  const DISCLOSURE = "nicholas";

  it("discloses a human standing in for the run's initiator", () => {
    const { container } = renderPath(STAND_IN);
    const text = container.textContent ?? "";
    // Both sides of the fact: who typed it, and whose voice it is in.
    expect(text).toContain(DISCLOSURE);
    expect(text).toContain("agent-a");
    // The words themselves are untouched — provenance is a label, never a prefix
    // the model would read back as input.
    expect(text).toContain("换成 sonnet 再试一次");
  });

  it("says nothing when the human speaks in their own right", () => {
    // mode 3: a participant of the bound Chat. `author` is set, nobody stood in.
    const { container } = renderPath({ origin: "human", author: "nicholas" });
    expect(container.textContent ?? "").not.toContain(DISCLOSURE);
    expect(container.textContent ?? "").toContain("换成 sonnet 再试一次");
  });

  it("says nothing on an ordinary turn", () => {
    const { container } = renderPath(undefined);
    expect(container.textContent ?? "").not.toContain(DISCLOSURE);
    expect(container.textContent ?? "").toContain("换成 sonnet 再试一次");
  });
});
