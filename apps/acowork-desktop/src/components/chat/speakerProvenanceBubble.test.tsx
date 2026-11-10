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
import { beforeEach, describe, expect, it, vi } from "vitest";
import { MessageBubble } from "./MessageBubble";
import { UserWithAttachmentsBubble } from "./UserWithAttachmentsBubble";
import { useAgentStore } from "../../stores/agentStore";
import { useAuthStore } from "../../stores/authStore";
import { useUserProfileStore } from "../../stores/userProfileStore";
import type { ChatMessage, TurnSpeaker } from "../../lib/types";

// The account directory is a NETWORK fetch; this suite is about what the
// badge SAYS, not about how the directory arrives. Stubbed with the real
// resolver kept intact — the badge calls `resolveInitiatorIdentity`.
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
  author: "u-nicholas",
  on_behalf_of: "agent-a",
};

/**
 * The registries the badge reads for NAMES: the signed-in account and the
 * agent registry. Both are keyed the way the wire keys them — the account by
 * its user id, the agent by its INSTANCE id.
 */
function seedRegistries() {
  // The ID comes from the account (it never changes); the NAME comes from the
  // profile store, because that is the one a rename updates live
  // (`authStore.account.display_name` is written once, at login).
  useAuthStore.setState({
    account: { user_id: "u-nicholas" } as never,
  });
  useUserProfileStore.setState({
    profile: { ...useUserProfileStore.getState().profile, displayName: "Nicholas" },
  });
  useAgentStore.setState({
    agents: {
      "agent-a": { meta: { instance_id: "agent-a", name: "Product.Lucy" } },
    },
  } as never);
}

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
  beforeEach(seedRegistries);

  // The badge names the two sides — the PERSON and the AGENT — not the ids
  // the record carries. Pinning the names (not a translated sentence) keeps
  // the suite failing on a behaviour change rather than on a translation edit.
  const AUTHOR_NAME = "Nicholas";
  const TARGET_NAME = "Product.Lucy";

  it("discloses a human standing in for the run's initiator, by NAME", () => {
    const { container } = renderPath(STAND_IN);
    const text = container.textContent ?? "";
    // Both sides of the fact: who typed it, and whose voice it is in.
    expect(text).toContain(AUTHOR_NAME);
    expect(text).toContain(TARGET_NAME);
    // The ids stay out of the picture — a reader cannot tell a UUID from a
    // person, which is the whole point of the change.
    expect(text).not.toContain("u-nicholas");
    expect(text).not.toContain("agent-a");
    // The words themselves are untouched — provenance is a label, never a prefix
    // the model would read back as input.
    expect(text).toContain("换成 sonnet 再试一次");
  });

  it("says nothing when the human speaks in their own right", () => {
    // mode 3: a participant of the bound Chat. `author` is set, nobody stood in.
    const { container } = renderPath({ origin: "human", author: "u-nicholas" });
    // The live user name still renders (`Nicholas`), but no badge sentence
    // does — the "stood in for" fact is absent.
    expect(container.textContent ?? "").not.toContain(TARGET_NAME);
    expect(container.textContent ?? "").toContain("换成 sonnet 再试一次");
  });

  it("says nothing on an ordinary turn", () => {
    const { container } = renderPath(undefined);
    expect(container.textContent ?? "").not.toContain(TARGET_NAME);
    expect(container.textContent ?? "").toContain("换成 sonnet 再试一次");
  });
});
