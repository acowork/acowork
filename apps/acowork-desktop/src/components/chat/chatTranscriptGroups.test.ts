/**
 * Transcript layout: the folding contract itself (ADR-106 D3/D4, design 27 §5.5).
 *
 * The surfaces' suites cover what they hand the block; this module is where the
 * reported failure lived — a long-lived execution session drawn as a stack of
 * near-identical cards, because the layout emitted one item per reference row.
 * So the rules that decide grouping are pinned here, off any component:
 *   1. a block is a SESSION, and it sits at that session's newest row;
 *   2. the chat's own question rows fold with it and render exactly once;
 *   3. a row that names no coordinate addresses nothing, so it stays a row and
 *      the surface draws its hole;
 *   4. every folded seq still resolves to the block's key, or a locate hit
 *      (ADR-097) would scroll to nothing.
 */
import { describe, expect, it } from "vitest";
import { buildTranscriptItems } from "./chatTranscriptGroups";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

const A = "agent-a";
const B = "agent-b";
const ME = "user-me";

/** Render key, the way both surfaces compute it: session-qualified row id. */
const keyOf = (row: ChatMessageRow) => `k:${row.meta?.message_id}`;

function ask(seq: number, body: string, to = A): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: ME,
    kind: "user_message",
    body,
    to,
    meta: { message_id: `u${seq}` },
  };
}

function round(
  seq: number,
  instance: string,
  execSid: string,
  roundId: string,
  trigger: string,
  extraMeta?: Record<string, unknown>,
): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: instance,
    kind: "execution_round",
    body: "",
    to: ME,
    meta: {
      message_id: `round:${execSid}|${roundId}`,
      agent_instance_id: instance,
      agent_session_id: execSid,
      round_id: roundId,
      in_reply_to: trigger,
      ...extraMeta,
    },
  };
}

function reply(seq: number, instance: string, body: string): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: instance,
    kind: "agent_message",
    body,
    to: ME,
    meta: { message_id: `a${seq}` },
  };
}

describe("buildTranscriptItems", () => {
  it("folds a session's rounds into ONE item anchored at its newest row", () => {
    const { items } = buildTranscriptItems(
      [
        ask(1, "first", A),
        round(2, A, "s1", "r1", "u1"),
        ask(3, "continue", A),
        round(4, A, "s1", "r2", "u3"),
        ask(5, "and now?", A),
        round(6, A, "s1", "r3", "u5"),
      ],
      keyOf,
    );

    expect(items).toHaveLength(1);
    const block = items[0];
    expect(block?.kind).toBe("run");
    if (block?.kind !== "run") return;
    expect(block.instanceId).toBe(A);
    expect(block.execSessionId).toBe("s1");
    // Chronological, so the block's list reads oldest → newest like the run did.
    expect(block.rounds.map((r) => r.ref)).toEqual([
      `${A}|s1|r1`,
      `${A}|s1|r2`,
      `${A}|s1|r3`,
    ]);
    // The block sits where the session's most recent work is: a fresh round
    // moves the block down instead of stacking a second card under it.
    expect(block.anchorRow.seq).toBe(6);
  });

  it("keeps two execution sessions as two blocks, in transcript order", () => {
    const { items } = buildTranscriptItems(
      [
        ask(1, "to A", A),
        round(2, A, "sA", "r1", "u1"),
        ask(3, "to B", B),
        round(4, B, "sB", "r1", "u3"),
        round(5, A, "sA", "r2", "u1"),
      ],
      keyOf,
    );

    // Each block sits at its OWN newest row: A's later round pulls A's block
    // below B's, so a reader meets the freshest work of every run last.
    expect(
      items
        .filter((i) => i.kind === "run")
        .map((i) => (i.kind === "run" ? i.execSessionId : "")),
    ).toEqual(["sB", "sA"]);
    // A's second round joined its own block, so nothing extra was emitted at 5.
    expect(items).toHaveLength(2);
  });

  it("the question row a block folds renders exactly once — inside the block", () => {
    const { items } = buildTranscriptItems(
      [ask(1, "fix the bug", A), round(2, A, "s1", "r1", "u1")],
      keyOf,
    );

    expect(items).toHaveLength(1);
    if (items[0]?.kind !== "run") return;
    expect(items[0].rounds[0]?.triggerRow?.body).toBe("fix the bug");
    expect(items[0].rounds[0]?.showUserLine).toBe(true);
  });

  it("a fan-out trigger is claimed by one block, so the other shows the answer alone", () => {
    // One human message addressed two agents: the sentence is on the transcript
    // already, so exactly one block may repeat it as its question line.
    const { items } = buildTranscriptItems(
      [
        ask(1, "you both", A),
        round(2, A, "sA", "r1", "u1"),
        round(3, B, "sB", "r1", "u1"),
      ],
      keyOf,
    );

    const first = items[0];
    const second = items[1];
    expect(first?.kind).toBe("run");
    expect(second?.kind).toBe("run");
    if (first?.kind !== "run" || second?.kind !== "run") return;
    expect(first.rounds[0]?.showUserLine).toBe(true);
    expect(second.rounds[0]?.showUserLine).toBe(false);
    expect(second.rounds[0]?.triggerRow?.seq).toBe(1);
  });

  it("a reference row with a missing coordinate stays a row (its hole is the surface's)", () => {
    const broken: ChatMessageRow = {
      seq: 1,
      ts: 1,
      from: A,
      kind: "execution_round",
      body: "",
      to: ME,
      meta: { message_id: "round:broken", agent_instance_id: A, round_id: "r1" },
    };
    const { items } = buildTranscriptItems([broken], keyOf);

    expect(items).toHaveLength(1);
    expect(items[0]?.kind).toBe("row");
  });

  /** ADR-106 presentation-contract amendment: the round that OPENED the
   *  session pairs with its own question bubble — the exchange reads "question,
   *  then block", and the block line leads with the brief instead. */
  it("an opener round keeps its trigger as its own bubble", () => {
    const { items, keyOfSeq } = buildTranscriptItems(
      [
        ask(1, "fix the bug", A),
        round(2, A, "s1", "r1", "u1", { opened_new: true, assignment_brief: "Fix the pagination bug" }),
        ask(3, "also add tests", A),
        round(4, A, "s1", "r2", "u3", { opened_new: false }),
      ],
      keyOf,
    );

    // The opener's question stands alone; the continuation stays folded — one
    // bubble, then the block that answers both.
    expect(items.map((i) => i.kind)).toEqual(["row", "run"]);
    const block = items[1];
    if (block?.kind !== "run") throw new Error("expected a run block");
    const [opener, continuation] = block.rounds;
    expect(opener?.questionOnTranscript).toBe(true);
    expect(opener?.showUserLine).toBe(false);
    // The block still holds the trigger row for the header's title derivation.
    expect(opener?.triggerRow?.body).toBe("fix the bug");
    expect(continuation?.questionOnTranscript).toBeUndefined();
    expect(continuation?.showUserLine).toBe(true);
    // Locate: the opener's seq points at its OWN bubble, the continuation's
    // trigger still points at the block that draws it.
    const anchorKey = keyOf(block.anchorRow);
    expect(keyOfSeq.get(1)).toBe("k:u1");
    expect(keyOfSeq.get(3)).toBe(anchorKey);
  });

  it("an answer that opened no run stays the agent's speech", () => {
    const { items } = buildTranscriptItems(
      [ask(1, "hi", A), reply(2, A, "hello")],
      keyOf,
    );
    expect(items.map((i) => i.kind)).toEqual(["row", "row"]);
  });

  it("maps every folded seq to the block's key, so a locate lands on it", () => {
    const { items, keyOfSeq } = buildTranscriptItems(
      [ask(1, "first", A), round(2, A, "s1", "r1", "u1"), ask(3, "second", A), round(4, A, "s1", "r2", "u3")],
      keyOf,
    );
    const block = items[0];
    if (block?.kind !== "run") throw new Error("expected a run block");
    const anchorKey = keyOf(block.anchorRow);
    // The block's own rows and the question rows it swallowed all resolve to the
    // ONE item that draws them — a search hit on a folded row must scroll to the
    // block, not to a row that is no longer in the DOM.
    for (const seq of [1, 2, 3, 4]) expect(keyOfSeq.get(seq)).toBe(anchorKey);
  });

  it("a locate on a fan-out trigger points at the block that actually draws it", () => {
    const { keyOfSeq } = buildTranscriptItems(
      [
        ask(1, "you both", A),
        round(2, A, "sA", "r1", "u1"),
        round(3, B, "sB", "r1", "u1"),
      ],
      keyOf,
    );
    // seq 1 is rendered by A's block (the claimant), never by B's.
    expect(keyOfSeq.get(1)).toBe(`k:round:sA|r1`);
  });
});
