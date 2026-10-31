import { describe, expect, it } from "vitest";
import {
  isFailureStatus,
  isEmptyTerminalReply,
  isSystemFallback,
  isTerminalReply,
  isTurnRoutedNotice,
  opensAgentTurn,
  pendingTurn,
  terminalNoticeKey,
} from "./chatTurnState";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

let seq = 0;
function row(kind: string, meta: ChatMessageRow["meta"] = {}, body = ""): ChatMessageRow {
  seq += 1;
  return { seq, ts: seq, from: "ag", kind, body, to: "u", meta };
}

const user = (id = "u1") => row("user_message", { message_id: id }, "hello");
/** A row as the archive-only write entry files it: no recipient, no mentions —
 *  what a `user_user` DM and an un-@'d group message look like on disk. */
const dm = (id = "u1") => ({ ...row("user_message", { message_id: id }, "hi"), to: "" });
const reply = (inReplyTo: string, status = "completed", body = "done") =>
  row("agent_message", { message_id: `a:${inReplyTo}`, in_reply_to: inReplyTo, status }, body);
const card = (inReplyTo: string, id = "card:1") =>
  row("interaction_request", { message_id: id, in_reply_to: inReplyTo });

describe("isTerminalReply / isEmptyTerminalReply", () => {
  it("only agent rows with a §7.1 outcome are terminal", () => {
    expect(isTerminalReply(reply("u1"))).toBe(true);
    expect(isTerminalReply(reply("u1", "aborted"))).toBe(true);
    // A row without a status predates the outcome field.
    expect(isTerminalReply(row("agent_message", { message_id: "a:1" }, "hi"))).toBe(false);
    expect(isTerminalReply(user())).toBe(false);
  });

  it("an empty terminal row IS its outcome; a body or attachment is not", () => {
    expect(isEmptyTerminalReply(reply("u1", "aborted", ""))).toBe(true);
    expect(isEmptyTerminalReply(reply("u1", "failed", "boom"))).toBe(false);
    const withFile = reply("u1", "completed", "");
    withFile.attachments = [{ id: "f1", filename: "a.txt", mime: "text/plain", size: 1 }];
    expect(isEmptyTerminalReply(withFile)).toBe(false);
  });
});

describe("isFailureStatus / terminalNoticeKey", () => {
  it("treats a user stop as a fact, not an error", () => {
    expect(isFailureStatus("aborted")).toBe(false);
    expect(isFailureStatus("failed")).toBe(true);
    expect(isFailureStatus("timeout")).toBe(true);
  });

  it("maps each outcome to its own line", () => {
    expect(terminalNoticeKey("aborted")).toBe("chatView.notice.interrupted");
    expect(terminalNoticeKey("timeout")).toBe("chatView.notice.timeout");
    expect(terminalNoticeKey("failed")).toBe("chatView.notice.failed");
    expect(terminalNoticeKey("waiting")).toBe("chatView.notice.waiting");
    expect(terminalNoticeKey("completed")).toBe("chatView.notice.noAnswer");
    expect(terminalNoticeKey(undefined)).toBe("chatView.notice.noAnswer");
  });
});

describe("isTurnRoutedNotice", () => {
  // ADR-100 D11: the routing reaction is a `system_notice` by producer but an
  // agent bubble by rendering — the predicate is the only thing the surfaces
  // key on, so a wrong match silently centers a reply or bubbles a receipt.
  it("matches only the turn_routed notice", () => {
    expect(
      isTurnRoutedNotice(
        row("system_notice", { notice_type: "turn_routed", message_id: "tr:u1", in_reply_to: "u1" }),
      ),
    ).toBe(true);
    expect(isTurnRoutedNotice(row("system_notice", { notice_type: "delivery_failed" }))).toBe(false);
    expect(isTurnRoutedNotice(row("system_notice", {}))).toBe(false);
    expect(isTurnRoutedNotice(row("agent_message", { notice_type: "turn_routed" }))).toBe(false);
  });
});

describe("pendingTurn", () => {
  const none = new Set<string>();

  it("is true while the HTTP send is still in flight", () => {
    expect(pendingTurn([], true, none)).toBe(true);
  });

  it("is true for a human message with no answer yet", () => {
    expect(pendingTurn([user()], false, none)).toBe(true);
  });

  it("is false once the terminal reply lands", () => {
    expect(pendingTurn([user(), reply("u1")], false, none)).toBe(false);
  });

  it("keeps waiting after the routing reaction — the run is still in flight", () => {
    // ADR-100 D11: the reaction answers "where did my message go" within
    // seconds, but the live line below it is the only thing covering a run
    // that may take hours. Closing the wait here would silence the chat
    // between the reaction and the answer.
    const reaction = row("system_notice", {
      message_id: "tr:u1",
      notice_type: "turn_routed",
      in_reply_to: "u1",
      agent_session_id: "exec-1",
      turn_mode: "new",
    });
    expect(pendingTurn([user(), reaction], false, none)).toBe(true);
  });

  it("ignores a reply that answers an earlier turn", () => {
    expect(pendingTurn([user("u2"), reply("u1")], false, none)).toBe(true);
  });

  it("counts a non-terminal reply as still waiting", () => {
    const pending = row("agent_message", { message_id: "a:1", in_reply_to: "u1" }, "partial");
    expect(pendingTurn([user(), pending], false, none)).toBe(true);
  });

  it("stops waiting while a decision card is open — the ball is human", () => {
    expect(pendingTurn([user(), card("u1")], false, none)).toBe(false);
    // A closed card is not an answer: the turn is either resumed in place (the
    // human row it produced becomes the newest trigger) or ended by a terminal
    // row. With neither present the wait is still owed.
    expect(pendingTurn([user(), card("u1")], false, new Set(["card:1"]))).toBe(true);
  });

  it("ignores a card that belongs to a previous turn", () => {
    expect(pendingTurn([user("u2"), card("u1")], false, none)).toBe(true);
  });

  it("waits on the newest human message only", () => {
    expect(pendingTurn([user("u1"), reply("u1"), user("u2")], false, none)).toBe(true);
  });

  it("does not wait on a human row with no id (nothing can answer it)", () => {
    expect(pendingTurn([row("user_message", {}, "hi")], false, none)).toBe(false);
  });

  it("does not wait when the window has no human message at all", () => {
    expect(pendingTurn([], false, none)).toBe(false);
  });

  it("does not wait on a row that woke nobody — a DM or an un-@'d remark", () => {
    // The archive-only chat write entry stores `to: ""` and no mentions. Keying
    // the line on it held a `user_user` DM at "working on it" forever: with no
    // agent in the conversation, no `agent_message` can ever answer it.
    expect(pendingTurn([dm("u1")], false, none)).toBe(false);
    // Skipped, not treated as an answer: the turn that DID ask something keeps
    // its wait through a plain remark that landed after it.
    expect(pendingTurn([user("u1"), dm("u2")], false, none)).toBe(true);
  });

  it("waits on the fan-out row, whose single broadcast copy names its agents in meta", () => {
    // §5.3.1: one `user_message` with `to` empty and `mentions` set.
    const fanout = { ...dm("u1"), meta: { message_id: "u1", mentions: ["ag"] } };
    expect(pendingTurn([fanout], false, none)).toBe(true);
  });

  it("agentReachable=false silences the line even while the send is in flight", () => {
    // A chat with no Agent participant has nobody working, so asserting a wait
    // is a lie at every moment, not only after the row lands.
    expect(pendingTurn([], true, none, false)).toBe(false);
    expect(pendingTurn([user()], false, none, false)).toBe(false);
    expect(pendingTurn([user()], false, none, true)).toBe(true);
  });
});

describe("opensAgentTurn", () => {
  it("reads the delivered-to agent off the chat-message row", () => {
    expect(opensAgentTurn(user())).toBe(true);
  });

  it("reads the mentioned agents off the fan-out row's meta", () => {
    expect(opensAgentTurn({ ...dm("u1"), meta: { message_id: "u1", mentions: ["ag"] } })).toBe(true);
    // An empty mention list is a plain send that the client resolved to nobody.
    expect(opensAgentTurn({ ...dm("u1"), meta: { message_id: "u1", mentions: [] } })).toBe(false);
  });

  it("treats a whitespace-only recipient as no recipient", () => {
    expect(opensAgentTurn({ ...dm("u1"), to: "  " })).toBe(false);
  });
});

describe("isSystemFallback", () => {
  // The row shape the Gateway sweeper appends (§5.5.1): a terminal
  // `agent_message` written ON the responder's behalf, carrying the fixed
  // fallback wording as its body.
  const fallbackRow = () =>
    row(
      "agent_message",
      { message_id: "fallback-req1", in_reply_to: "req1", status: "failed", fallback: true },
      "No answer received before the response deadline",
    );

  it("recognises the sweeper's receipt", () => {
    expect(isSystemFallback(fallbackRow())).toBe(true);
  });

  it("does not claim an agent's own failure is a system receipt", () => {
    // Same `status=failed`, no `fallback` stamp — this is the responder
    // reporting its own outcome, and must keep painting red.
    expect(isSystemFallback(row("agent_message", { status: "failed" }, "boom"))).toBe(false);
  });

  it("is not fooled by a non-agent row that carries the flag", () => {
    expect(isSystemFallback(row("system_notice", { fallback: true }))).toBe(false);
  });

  it("needs its own branch because the receipt is not an empty terminal row", () => {
    // Documents WHY the renderers cannot rely on `isEmptyTerminalReply`: the
    // receipt has a body, so without an explicit branch it renders as an
    // agent bubble.
    expect(isEmptyTerminalReply(fallbackRow())).toBe(false);
    expect(isTerminalReply(fallbackRow())).toBe(true);
  });
});
