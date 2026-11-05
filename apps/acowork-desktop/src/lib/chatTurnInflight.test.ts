/**
 * The wait-this-turn claim has exactly two ways to be wrong (ADR-106 D9).
 *
 * It can be asserted from the transcript's shape — "the newest human row has no
 * answer" — which is how a dead node went on looking busy forever after a
 * reload; or it can be dropped as soon as the POST returns, which is how the
 * surface showed nothing at all for the seconds (measured: up to 81) between
 * delivery and the row the turn actually produces. Both are pinned here, in the
 * direction the fix has to keep: the claim is about this device's own send and
 * dies at the first row that answers that send's id.
 */
import { describe, expect, it } from "vitest";
import { turnInflight, turnSettled, type InflightTurn } from "./chatTurnInflight";
import type { ChatMessageRow } from "../stores/chatSessionStore";

/** The send this device made. `messageId` is the id the Gateway gave it — every
 *  outcome row in the Chat threads itself to that id under `in_reply_to`. */
const TURN: InflightTurn = { messageId: "m-1", at: 1 };

function row(over: Partial<ChatMessageRow>): ChatMessageRow {
  return {
    seq: 2,
    ts: 1,
    from: "u1",
    kind: "user_message",
    body: "fix the pagination",
    to: "agent-a",
    meta: { message_id: "m-1" },
    ...over,
  };
}

const HUMAN = row({});

describe("chatTurnInflight", () => {
  it("waits from the send until the round's reference row lands (D3/D9)", () => {
    // The POST has returned and the human's row is durable — the only two facts
    // the surface actually owns. A router still deciding, or a Runtime still
    // starting, has written nothing yet: this is the window that used to be
    // blank, so it MUST read as in flight.
    expect(turnInflight([HUMAN], TURN)).toBe(true);
    // The Runtime publishes this row before spending a token on the round, so
    // its arrival is positive evidence the turn was picked up — and from here
    // the block narrates the run, not a line under the transcript.
    expect(
      turnInflight(
        [HUMAN, row({ seq: 3, kind: "execution_round", body: "", from: "agent-a", meta: { round_id: "r:m-1", in_reply_to: "m-1" } })],
        TURN,
      ),
    ).toBe(false);
  });

  it("the router's own answer settles the turn: no round was opened (D12)", () => {
    const reply = row({
      seq: 3,
      kind: "agent_message",
      from: "agent-a",
      body: "The release note is already in `docs/`.",
      meta: { message_id: "reply:m-1", in_reply_to: "m-1" },
    });
    expect(turnSettled([HUMAN, reply], TURN)).toBe(true);
  });

  it("a receipt settles the turn — it is the answer the human gets to see", () => {
    const receipt = row({
      seq: 3,
      kind: "system_notice",
      from: "agent-a",
      body: "",
      meta: {
        message_id: "dlv-failed:m-1:agent-a",
        notice_type: "delivery_failed",
        in_reply_to: "m-1",
      },
    });
    expect(turnInflight([HUMAN, receipt], TURN)).toBe(false);
  });

  it("an addressing question settles it too, because the human has to act", () => {
    // A `session_choice` is a `system_notice` answering the same row. Treating
    // it as unresolved would lock the composer on the one turn whose whole
    // point is that the human should now choose.
    const card = row({
      seq: 3,
      kind: "system_notice",
      from: "agent-a",
      body: "",
      meta: { message_id: "session_choice:m-1", notice_type: "session_choice", in_reply_to: "m-1" },
    });
    expect(turnInflight([HUMAN, card], TURN)).toBe(false);
  });

  it("a milestone the transcript hides settles nothing (Rev.2)", () => {
    // `turn_dispatched` / `turn_started` render as nothing (RETIRED_NOTICES), so
    // an old row of that shape leaves the human staring at a blank stream while
    // the surface believes it has an answer.
    const hidden = row({
      seq: 3,
      kind: "system_notice",
      from: "agent-a",
      body: "",
      meta: { message_id: "ts:m-1", notice_type: "turn_started", in_reply_to: "m-1" },
    });
    expect(turnInflight([HUMAN, hidden], TURN)).toBe(true);
  });

  it("counts rows only for THIS send", () => {
    // Somebody else's turn settling is not this device's turn settling, and a
    // round belonging to an older message must not unlock the composer while a
    // fresh send is still unanswered.
    const otherRound = row({
      seq: 3,
      kind: "execution_round",
      from: "agent-a",
      body: "",
      meta: { round_id: "r:m-0", in_reply_to: "m-0" },
    });
    expect(turnInflight([otherRound, HUMAN], TURN)).toBe(true);
  });

  it("ignores a later human row that merely quotes a round", () => {
    // A `user_message` carries `in_reply_to` when it addresses a block; it is
    // speech, not an outcome, and must never settle the turn above it.
    const quoted = row({
      seq: 4,
      body: "continue",
      meta: { message_id: "m-2", in_reply_to: "m-1" },
    });
    expect(turnInflight([HUMAN, quoted], TURN)).toBe(true);
  });

  it("with no send of our own there is no claim to make", () => {
    // This is the reload case: a newest human row with no answer is NOT
    // evidence of liveness (D9) — a dead node would look busy forever.
    expect(turnInflight([HUMAN], undefined)).toBe(false);
    expect(turnSettled([HUMAN], undefined)).toBe(true);
  });
});
