/**
 * The hover meta row on the two execution-surface user bubbles.
 *
 * What the request asked for, and what can break:
 *
 *   1. the cluster is on the USER bubble of the execution surface, in the
 *      order resend → copy → time. An assistant row must not grow one — the
 *      affordance is about re-sending your own turn.
 *   2. the hover resend and the right-click Resend are ONE function, not two
 *      implementations of an idea. Both paths call `resendUserMessageText`,
 *      so the test mocks that helper and asserts the icon reaches it with the
 *      turn's own text. If someone later re-implements the icon inline, this
 *      is the test that says why not.
 *   3. both render paths count: a plain user turn is `MessageBubble`, a turn
 *      carrying attachments or references is `UserWithAttachmentsBubble`.
 *      The with-parts bubble never had a right-click menu, so its hover icon
 *      is its ONLY way to re-send — and it must still behave like the other.
 *
 * Harness shape is `execBubbleSpeaker.test.tsx` (same two components, same
 * avatar stubs).
 */
import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { MessageBubble } from "./MessageBubble";
import { UserWithAttachmentsBubble } from "./UserWithAttachmentsBubble";
import type { ChatMessage } from "../../lib/types";
import { formatBubbleTime } from "../../lib/formatTime";

// Keys instead of one language's wording, so the labels below pin the i18n
// hooks rather than the translation.
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (k: string) => k }),
}));

// The one helper both entry points share. Mocking it is what lets this file
// assert "same function" instead of merely "same effect".
const resend = vi.fn();
vi.mock("../../lib/resendUserMessage", () => ({
  resendUserMessageText: (content: string) => resend(content),
}));

// The cluster asks the write-access hook for itself; the tests answer.
let readOnly = false;
vi.mock("../../lib/session-write-access", () => ({
  useActiveSessionReadOnly: () => readOnly,
}));

vi.mock("../common/AgentAvatar", () => ({ AgentAvatar: () => <span data-testid="agent-avatar" /> }));
vi.mock("../common/UserAvatar", () => ({
  UserAvatar: () => <span data-testid="user-avatar" />,
  BUILTIN_ICONS: {},
}));

const WORDS = "把这条改一下";
const TIMESTAMP = 1_700_000_000_000;

function userMessage(over: Partial<ChatMessage> = {}): ChatMessage {
  return {
    id: "u1",
    type: "user",
    content: WORDS,
    timestamp: TIMESTAMP,
    ...over,
  };
}

const paths = {
  MessageBubble: (content = WORDS) =>
    render(
      <MessageBubble
        message={userMessage({ content })}
        currentSessionId="s1"
        liveUserName="Me"
        speakerKind="user"
        senderAgentId={null}
      />,
    ),
  UserWithAttachmentsBubble: (content = WORDS) =>
    render(
      <UserWithAttachmentsBubble
        userMessage={userMessage({ content })}
        parts={[]}
        currentSessionId="s1"
        liveUserName="Me"
        speakerKind="user"
        senderAgentId={null}
        agentId="inst-sse"
      />,
    ),
};

beforeEach(() => resend.mockReset());

describe.each(Object.entries(paths))("%s — user bubble meta row", (_name, renderPath) => {
  it("offers resend to the left of copy, before the timestamp", () => {
    renderPath();
    const cluster = screen.getByLabelText("chatPanel.copy").parentElement as HTMLElement;

    // Order inside the cluster…
    expect(Array.from(cluster.children).map((c) => c.getAttribute("aria-label"))).toEqual([
      "chatPanel.resend",
      "chatPanel.copy",
    ]);
    // …and the cluster is the thing immediately left of the time text, which
    // is pinned by its own rendered value rather than by a class that other
    // elements in the bubble happen to share.
    expect(cluster.nextElementSibling?.textContent).toBe(formatBubbleTime(TIMESTAMP));
  });

  it("re-sends through the helper the right-click menu calls", () => {
    renderPath();
    fireEvent.click(screen.getByLabelText("chatPanel.resend"));
    expect(resend).toHaveBeenCalledWith(WORDS);
  });

  // An empty turn has nothing to copy and nothing to send, so no dead control
  // may appear. NOTE this passes at the BUBBLE level: both render paths gate
  // the whole column on the text (`userMessage.content &&`), so this pins the
  // outcome, not `BubbleMetaActions`'s own empty-text branch — which the
  // composition tests above cover directly. Parameterized so both bubbles do.
  it("is absent when the turn carries no text", () => {
    renderPath("");
    expect(screen.queryByLabelText("chatPanel.resend")).toBeNull();
    expect(screen.queryByLabelText("chatPanel.copy")).toBeNull();
  });

  // The gate lives in the cluster, so neither bubble has to remember it.
  it("keeps copy but drops resend on a read-only session", () => {
    readOnly = true;
    try {
      renderPath();
      expect(screen.queryByLabelText("chatPanel.resend")).toBeNull();
      expect(screen.getByLabelText("chatPanel.copy")).toBeTruthy();
    } finally {
      readOnly = false;
    }
  });
});

describe("MessageBubble — the other side of the exchange", () => {
  it("gives an assistant row no meta cluster", () => {
    render(
      <MessageBubble
        message={userMessage({ type: "assistant", content: "好的" })}
        currentSessionId="s1"
        liveUserName="Me"
        speakerKind="user"
        senderAgentId={null}
      />,
    );
    expect(screen.queryByLabelText("chatPanel.resend")).toBeNull();
    expect(screen.queryByLabelText("chatPanel.copy")).toBeNull();
  });
});
