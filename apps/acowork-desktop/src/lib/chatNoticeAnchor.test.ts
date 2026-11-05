/**
 * The column a channel receipt lands in (ADR-106 D14, design 31 §6's
 * presentation join). Pure: a by-coordinate read of one known row decides a
 * column, and nothing here derives delivery state.
 */
import { describe, expect, it } from "vitest";
import { indexByMessageId, noticeAnchorOf } from "./chatNoticeAnchor";
import type { ChatMessageRow } from "../stores/chatSessionStore";

function row(seq: number, from: string, messageId?: string): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from,
    to: "",
    kind: "user_message",
    body: `m${seq}`,
    meta: messageId ? { message_id: messageId } : {},
  };
}

function notice(inReplyTo?: string): ChatMessageRow {
  return {
    seq: 99,
    ts: 99,
    from: "ag",
    to: "ag",
    kind: "system_notice",
    body: "",
    meta: { message_id: "n1", notice_type: "delivery_failed", in_reply_to: inReplyTo },
  };
}

describe("noticeAnchorOf", () => {
  const rows = [row(1, "me", "m-1"), row(2, "ag", "m-2")];
  const index = indexByMessageId(rows);

  it("places a receipt about my message in my column", () => {
    expect(noticeAnchorOf(notice("m-1"), index, "me")).toBe("self");
  });

  it("places a receipt about someone else's message in their column", () => {
    expect(noticeAnchorOf(notice("m-2"), index, "me")).toBe("peer");
  });

  it("resolves nothing when the subject has been paged out", () => {
    // The window is a window: a receipt whose message is above it must stay
    // centered rather than guess a column.
    expect(noticeAnchorOf(notice("m-0"), index, "me")).toBeUndefined();
  });

  it("resolves nothing for a row with no coordinate", () => {
    expect(noticeAnchorOf(notice(undefined), index, "me")).toBeUndefined();
  });

  it("skips rows with no message_id when indexing", () => {
    const partial = indexByMessageId([row(1, "me"), row(2, "me", "m-2")]);
    expect(partial.has("m-2")).toBe(true);
    expect(partial.size).toBe(1);
  });
});
