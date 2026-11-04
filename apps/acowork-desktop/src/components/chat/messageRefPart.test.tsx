// @vitest-environment jsdom
/**
 * ADR-107 — the reference part of a user turn.
 *
 * Why this test exists:
 *   A Chat-Router-dispatched turn used to reach the execution session as ONE
 *   string, `{brief}\n\n[user message]\n{words}`. The transcript therefore
 *   showed the router's framing as the human's message, and auto-title (which
 *   reads the first `user` row) produced near-identical titles, because every
 *   such row began with the same sentence.
 *
 *   The fix splits the turn into parts: the `user` row keeps the human's words
 *   verbatim, and the runtime's background rides beside it as a `context_ref`
 *   system row — a *part*, parallel to an attachment. The contract pinned here
 *   is the rendering half of that decision:
 *
 *     1. a `context_ref` row folds into the user block above it (it is never a
 *        message of its own), and fold order keeps the words first;
 *     2. the human's words render LITERALLY — markdown syntax survives as
 *        characters, no `.prose` shell — the same speaker-based split
 *        `GroupChatPanel.userLiteral.test.tsx` pins;
 *     3. the reference body DOES render as markdown, and only once the block is
 *        expanded: the whole point of moving it out of the words is that it can
 *        be rich text without dragging rich text into the bubble;
 *     4. a turn with no words still renders its reference, below nothing.
 */
import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { UserWithAttachmentsBubble } from "./UserWithAttachmentsBubble";
import { foldMessages } from "./messageFolder";
import { convertConversationEntry } from "../../stores/chatStore";
import type { ChatMessage, ConversationEntry } from "../../lib/types";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
vi.mock("../../lib/logger", () => ({
  log: { warn: vi.fn(), debug: vi.fn(), info: vi.fn(), error: vi.fn() },
  setLevel: vi.fn(),
}));

const USER_WORDS = "**把这段** 放到后面去 — 不要动我的原文";
const REF_BODY = "**The user** asked for a rename.\n\n- first turn\n- second turn";

function userRow(content = USER_WORDS, ts = 1_700_000_000_000): ChatMessage {
  return { id: "u1", type: "user", content, timestamp: ts } as ChatMessage;
}

function refRow(ts = 1_700_000_000_100): ChatMessage {
  return {
    id: "r1",
    type: "system",
    content: REF_BODY,
    timestamp: ts,
    metadata: { type: "context_ref", kind: "chat_background" },
  } as unknown as ChatMessage;
}

function renderTurn(parts: ChatMessage[], content = USER_WORDS) {
  return render(
    <UserWithAttachmentsBubble
      userMessage={userRow(content)}
      parts={parts}
      liveUserName={null}
      liveUserAvatarUrl={null}
      liveUserBuiltinAvatarId={null}
      currentSessionId="s1"
      agentId="a1"
    />,
  );
}

describe("context_ref part — folding", () => {
  it("folds the reference into the user block instead of emitting its own", () => {
    const blocks = foldMessages([userRow(), refRow()]);
    expect(blocks).toHaveLength(1);
    expect(blocks[0].type).toBe("user_with_parts");
    // words first, reference second — the order the turn reads in
    expect(blocks[0].items.map((m) => m.id)).toEqual(["u1", "r1"]);
  });

  it("does not fold a reference that is not adjacent to a user row", () => {
    const blocks = foldMessages([{ id: "a0", type: "assistant", content: "hi", timestamp: 1 } as ChatMessage, refRow()]);
    expect(blocks.map((b) => b.type)).not.toContain("user_with_parts");
  });
});

describe("context_ref part — rendering", () => {
  it("keeps the human's words literal and collapses the reference by default", () => {
    const { container } = renderTurn([refRow()]);
    // 1. words are characters, not markup
    expect(container.textContent).toContain(USER_WORDS);
    expect(container.innerHTML).not.toContain("<strong>");
    // 2. the reference body is not in the DOM until asked for
    expect(container.textContent).not.toContain("first turn");
  });

  it("renders the reference body as markdown once expanded", () => {
    const { container } = renderTurn([refRow()]);
    fireEvent.click(screen.getByRole("button", { name: /messageRef.chatBackground/ }));

    expect(container.textContent).toContain("first turn");
    // markdown, not literal text: the ref body goes through the prose shell
    expect(container.querySelector(".prose strong")?.textContent).toBe("The user");
    // and the bubble still holds the human's own characters verbatim
    expect(container.textContent).toContain(USER_WORDS);
  });

  it("renders the reference for a turn with no words", () => {
    const { container } = renderTurn([refRow()], "");
    expect(screen.getByRole("button", { name: /messageRef.chatBackground/ })).toBeTruthy();
    expect(container.textContent).not.toContain("**把这段**");
  });
});

describe("context_ref part — the JSONL load path", () => {
  /**
   * The regression this pins is the one that shipped: the JSONL loader
   * (`chatStore.convertConversationEntry`) kept `metadata` only for the five
   * attachment tags, so a reference row reached `foldMessages` as a
   * metadata-less `system` message, was never folded, and painted as a
   * full-width system bubble (`ChatInputBar`'s sibling defect). A test that
   * starts from a hand-built `ChatMessage` cannot see it — these entries are
   * the exact wire shape the Runtime writes.
   */
  const wireEntries = [
    {
      id: "u1",
      ts: "2026-11-04T10:36:37.600Z",
      role: "user",
      content: USER_WORDS,
    },
    {
      id: "r1",
      ts: "2026-11-04T10:36:37.600Z",
      role: "system",
      content: REF_BODY,
      metadata: { type: "context_ref", kind: "chat_background" },
    },
  ] as unknown as ConversationEntry[];

  it("keeps the reference's metadata so it can be folded, not rendered as a system row", () => {
    const messages = wireEntries
      .map((e) => convertConversationEntry(e, "a1"))
      .filter((m): m is ChatMessage => m !== null);

    expect(messages).toHaveLength(2);
    expect(messages[1].metadata).toMatchObject({
      type: "context_ref",
      kind: "chat_background",
    });

    const blocks = foldMessages(messages);
    expect(blocks.map((b) => b.type)).toEqual(["user_with_parts"]);
    expect(blocks[0].items.map((m) => m.id)).toEqual(["u1", "r1"]);
  });

  it("still folds an attachment row through the same loader path", () => {
    const withAttachment = [
      wireEntries[0],
      {
        id: "a1",
        ts: "2026-11-04T10:36:37.600Z",
        role: "system",
        content: "Attached file: notes.md",
        metadata: { type: "attached_file", abs_path: "/tmp/notes.md", name: "notes.md" },
      },
    ] as unknown as ConversationEntry[];

    const messages = withAttachment
      .map((e) => convertConversationEntry(e, "a1"))
      .filter((m): m is ChatMessage => m !== null);
    expect(foldMessages(messages).map((b) => b.type)).toEqual(["user_with_parts"]);
  });
});
