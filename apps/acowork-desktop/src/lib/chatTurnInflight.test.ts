/**
 * The wait-this-turn claim has exactly two ways to be wrong (ADR-106 D9).
 *
 * It can be asserted from the transcript's shape — "the newest human row has no
 * answer" — which is how a dead node went on looking busy forever after a
 * reload; or it can be dropped as soon as the POST returns, which is how the
 * surface showed nothing at all for the seconds (measured: up to 81) between
 * delivery and the row the turn actually produces.
 *
 * What changed with design 31 §6: the surface no longer decides which rows
 * count as an answer. The service projects `delivery.answered` onto every row
 * it serves, so this module asks one question — was my row answered? — and the
 * policy (round, router reply, live receipt, addressing card; retired
 * milestones never) lives in `acowork-user`'s read model, tested there.
 */
import { describe, expect, it } from "vitest";
import {
  turnInflight,
  turnSettled,
  resolveExpectedResponders,
  type InflightTurn,
} from "./chatTurnInflight";
import type { ChatDeliveryView, ChatMessageRow } from "../stores/chatSessionStore";

/** The send this device made. `messageId` is the id the Gateway gave it: it
 *  addresses the row whose `delivery` closes the claim. */
const TURN: InflightTurn = { messageId: "m-1", at: 1 };

/** A round's reference row — the only kind of replied row that addresses a
 *  round for the Gateway (`addressed_execution_round` needs the kind AND both
 *  ids). */
function roundRow(agent: string): ChatMessageRow {
  return {
    seq: 3,
    ts: 1,
    from: agent,
    kind: "execution_round",
    body: "",
    to: "u1",
    meta: { message_id: "round:1", agent_instance_id: agent, agent_session_id: "s-1" },
  };
}

function row(delivery: ChatDeliveryView | undefined, over: Partial<ChatMessageRow> = {}): ChatMessageRow {
  return {
    seq: 2,
    ts: 1,
    from: "u1",
    kind: "user_message",
    body: "fix the pagination",
    to: "agent-a",
    meta: { message_id: "m-1" },
    ...(delivery ? { delivery } : {}),
    ...over,
  };
}

const UNANSWERED: ChatDeliveryView = { answered: false };
const ANSWERED: ChatDeliveryView = {
  answered: true,
  answered_by: { message_id: "round:m-1", kind: "execution_round" },
  target: {
    agent_instance_id: "agent-a",
    agent_session_id: "s-1",
    round_id: "r:m-1",
  },
};

describe("chatTurnInflight", () => {
  it("waits while the service still says the row is unanswered (D9)", () => {
    // The POST returned and the row is durable, but nothing has answered it:
    // the window that used to be blank, so it MUST read as in flight.
    expect(turnInflight([row(UNANSWERED)], TURN)).toBe(true);
    expect(turnSettled([row(UNANSWERED)], TURN)).toBe(false);
  });

  it("a row the service has not projected yet keeps the claim", () => {
    // The optimistic row this device inserted before the write's response came
    // back carries no `delivery`. Absent is NOT "answered": the send is still
    // owed an outcome, and the deadline is what bounds the wait.
    expect(turnInflight([row(undefined)], TURN)).toBe(true);
  });

  it("the service's verdict ends the turn, whatever produced the answer", () => {
    // The surface no longer re-derives which kinds settle a turn — a round, the
    // router's own reply, a receipt and an addressing card are the same fact
    // here: `answered`.
    expect(turnInflight([row(ANSWERED)], TURN)).toBe(false);
    expect(turnSettled([row(ANSWERED)], TURN)).toBe(true);
  });

  it("another row's answer never settles this send", () => {
    // Two sends in the same chat: the claim is keyed by this device's own id,
    // so the other turn's projection is not evidence about this one.
    const otherRound = row(ANSWERED, { seq: 1, meta: { message_id: "m-0" } });
    expect(turnInflight([otherRound, row(UNANSWERED)], TURN)).toBe(true);
  });

  it("with no send of our own there is no claim to make", () => {
    // This is the reload case: a transcript full of unanswered rows is NOT
    // evidence of liveness (D9) — a dead node would look busy forever.
    expect(turnInflight([row(UNANSWERED)], undefined)).toBe(false);
    expect(turnSettled([row(UNANSWERED)], undefined)).toBe(true);
  });
});

// The turn header's naming basis (ADR-106 revision 2026-11-08): the delivery
// decision's OWN inputs, in the order the wire honours them. This is the one
// place the ladder is written down, so the surfaces cannot each re-derive it
// (and drift). The cases mirror `deliver_mentions` /
// `addressed_execution_round` in the Gateway, rung for rung.
describe("resolveExpectedResponders", () => {
  it("a 1:1 send names its peer, and nothing else", () => {
    expect(resolveExpectedResponders({ peer: "p" })).toEqual(["p"]);
  });

  it("a level-1 reply reaches the round's agent, whoever else the body names", () => {
    // The inReplyTo channel ignores the mention list server-side — naming the
    // mentions here would put a face on an agent that never sees the send.
    expect(
      resolveExpectedResponders({
        mentions: ["a", "b"],
        repliedRow: roundRow("c"),
      }),
    ).toEqual(["c"]);
  });

  it("a reply to a row that names no round falls through, as the server's does", () => {
    // `addressed_execution_round` only accepts an `execution_round` row with
    // both ids; a reply to a human's own row is an ordinary turn, so the
    // mentions are the targets again.
    expect(
      resolveExpectedResponders({
        mentions: ["a"],
        repliedRow: { seq: 1, ts: 1, from: "u1", kind: "user_message", body: "", to: "a" },
      }),
    ).toEqual(["a"]);
  });

  it("a reply whose row this surface has not loaded names nobody", () => {
    // Unresolved is not the same as absent: the Gateway resolves `in_reply_to`
    // against the whole log and delivers to the round's agent alone, so the
    // mentions would be a guess about a send they never receive. The centered
    // line carries that wait until the closing row lands.
    expect(
      resolveExpectedResponders({ mentions: ["a", "b"], repliedRow: null }),
    ).toEqual([]);
  });

  it("without a reply, the mentions are the targets", () => {
    expect(resolveExpectedResponders({ mentions: ["a", "b"] })).toEqual(["a", "b"]);
  });

  it("an un-addressed send goes to the group's default responder", () => {
    expect(
      resolveExpectedResponders({ mentions: [], defaultResponder: "z" }),
    ).toEqual(["z"]);
  });

  it("none of those: the row archives, and no claim — so no names", () => {
    expect(resolveExpectedResponders({ mentions: [] })).toEqual([]);
  });
});
