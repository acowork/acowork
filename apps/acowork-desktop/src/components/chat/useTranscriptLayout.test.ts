// @vitest-environment jsdom
/**
 * `useTranscriptLayout` — the adapters' key discipline, which is where the
 * "it jumps right after loading a page" report came from.
 *
 * A folded block's anchor row is the execution session's OLDEST row *in the
 * loaded window* (`anchorOf` in chatTranscriptGroups), so `loadOlder` moves it
 * for any session whose older rounds the new page brings in. Keying the virtual
 * item by that row handed the list a brand-new item at that position: the
 * block's measured height was gone, its estimate took over, and everything
 * under it shifted. The key is the session now, and the two sides of the locate
 * path (`keyOfSeq` and the item's own key) must agree, or a search hit scrolls
 * to nothing.
 */
import { renderHook } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { useTranscriptLayout } from "./useTranscriptLayout";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

const A = "agent-a";
const ME = "user-me";

function ask(seq: number, body: string): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: ME,
    kind: "user_message",
    body,
    to: A,
    meta: { message_id: `u${seq}` },
  };
}

function round(seq: number, roundId: string, trigger: string): ChatMessageRow {
  return {
    seq,
    ts: seq,
    from: A,
    kind: "execution_round",
    body: "",
    to: ME,
    meta: {
      message_id: `round:s1|${roundId}`,
      agent_instance_id: A,
      agent_session_id: "s1",
      round_id: roundId,
      in_reply_to: trigger,
    },
  };
}

/** The one run block's key, after checking that every seq it owns resolves to
 *  it — the contract `useLocateMessage` relies on. */
function blockKeyIn(rows: ChatMessageRow[], scope = "chat-1") {
  const { result } = renderHook(() => useTranscriptLayout(rows, scope));
  const block = result.current.items.find((i) => i.kind === "run");
  if (block?.kind !== "run") throw new Error("expected one run block");
  const key = result.current.virtualKeyOf(block);
  const owned = [
    block.anchorRow.seq,
    ...block.rounds.map((r) => r.roundRow.seq),
  ];
  for (const seq of owned) {
    expect(result.current.layout.keyOfSeq.get(seq)).toBe(key);
  }
  return { key, anchorSeq: block.anchorRow.seq };
}

describe("useTranscriptLayout", () => {
  it("keeps a block's virtual key when a page is prepended above it", () => {
    const tail: ChatMessageRow[] = [
      ask(5, "继续修"),
      round(6, "r2", "u5"),
    ];
    const paged: ChatMessageRow[] = [
      ask(1, "看下这个 bug"),
      round(2, "r1", "u1"),
      ...tail,
    ];

    const before = blockKeyIn(tail);
    const after = blockKeyIn(paged);

    // The prepended page DID move the anchor (the block now starts further up).
    expect(after.anchorSeq).toBe(2);
    expect(before.anchorSeq).toBe(6);
    // …and the identity the virtual list caches a height under did not move.
    expect(after.key).toBe(before.key);
  });

  it("keys two sessions apart even though both are blocks", () => {
    const otherRound: ChatMessageRow = {
      ...round(3, "x1", "u1"),
      meta: {
        message_id: "round:s2|x1",
        agent_instance_id: A,
        agent_session_id: "s2",
        round_id: "x1",
        in_reply_to: "u1",
      },
    };
    const { result } = renderHook(() =>
      useTranscriptLayout([ask(1, "both"), round(2, "r1", "u1"), otherRound], "chat-1"),
    );
    const keys = result.current.items
      .filter((i) => i.kind === "run")
      .map((i) => result.current.virtualKeyOf(i));
    expect(new Set(keys).size).toBe(keys.length);
  });
});