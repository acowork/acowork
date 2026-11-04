/**
 * Answering an addressing card is a RE-DELIVERY (ADR-106 D6).
 *
 * These are the rules that make that sentence true rather than aspirational:
 * the human's own words come back, they come back ONCE (the original row's id
 * rides as `message_id`, which the store dedupes on), and the address belongs
 * only to the agent that asked. The last one is the failure this file exists to
 * keep: a fan-out that refuses continuation for two agents because the human
 * answered one of them is the same silent re-routing D6 was written to remove.
 */
import { describe, expect, it } from "vitest";
import {
  buildCardAnswer,
  directCardAddress,
  groupCardAddress,
  mayAnswerCard,
} from "./chatCardAnswer";
import type { ChatMessageRow } from "../stores/chatSessionStore";

function row(over: Partial<ChatMessageRow>): ChatMessageRow {
  return {
    seq: 1,
    ts: 1,
    from: "u1",
    kind: "user_message",
    body: "go on with the release note",
    to: "",
    ...over,
  };
}

/** The card the Runtime writes when it will not guess (D6 levels 3 and 5):
 *  bodyless, authored by the agent, pointing at the row it asks about. */
function card(over: Partial<ChatMessageRow> = {}): ChatMessageRow {
  return row({
    from: "agent-a",
    kind: "system_notice",
    body: "",
    to: "u1",
    meta: {
      message_id: "session_choice:m-1",
      notice_type: "session_choice",
      in_reply_to: "m-1",
      options: [{ session_id: "s-1", label: "Draft the note" }],
      allow_new_task: true,
    },
    ...over,
  });
}

const original = row({ meta: { message_id: "m-1" } });

describe("buildCardAnswer", () => {
  it("re-delivers the human's own words against the row they wrote", () => {
    const answer = buildCardAnswer(card(), original, { sessionId: "s-1" });
    expect(answer).toEqual({
      text: "go on with the release note",
      attachmentIds: [],
      agentId: "agent-a",
      messageId: "m-1",
      sessionId: "s-1",
      forceNew: false,
    });
  });

  it("refuses to invent words when the asked-about row is not readable", () => {
    // A card whose original row is outside the window (or was never written)
    // cannot be answered with a paraphrase — that would be putting sentences in
    // the human's mouth.
    expect(buildCardAnswer(card(), undefined, { sessionId: "s-1" })).toBeNull();
    const orphan = card({ meta: { ...card().meta, in_reply_to: undefined } });
    expect(buildCardAnswer(orphan, original, { sessionId: "s-1" })).toBeNull();
  });

  it("refuses a click that names neither a session nor new work", () => {
    expect(buildCardAnswer(card(), original, {})).toBeNull();
  });

  it("carries the attachments the original message had", () => {
    const withFiles = row({
      meta: { message_id: "m-1" },
      attachments: [{ id: "att-1", filename: "a.png", mime: "image/png", size: 1 }],
    });
    const answer = buildCardAnswer(card(), withFiles, { forceNew: true });
    expect(answer?.attachmentIds).toEqual(["att-1"]);
  });

  it("echoes the original row's pinned material back verbatim", () => {
    // ADR-106 revision 2026-11-04: a re-delivery is the SAME utterance, so the
    // `@session` material it carried travels with its RESOLVED coordinates —
    // the Gateway passes the echo through rather than re-pinning a sentence
    // already written. And only the answering agent's own refs may ride: a
    // card is that agent's question about ITS runs.
    const quoted = row({
      meta: {
        message_id: "m-1",
        refs: [
          {
            agent_instance_id: "agent-a",
            session_id: "s-9",
            round_id: "r-1",
            terminal: true,
          },
          {
            agent_instance_id: "agent-b",
            session_id: "s-8",
            round_id: "r-2",
            terminal: false,
          },
        ],
      },
    });
    const answer = buildCardAnswer(card(), quoted, { sessionId: "s-1" })!;
    expect(answer.refs).toEqual([
      {
        agentInstanceId: "agent-a",
        sessionId: "s-9",
        roundId: "r-1",
        terminal: true,
      },
    ]);
    expect(groupCardAddress(answer)).toEqual({
      targetSessions: { "agent-a": "s-1" },
      messageId: "m-1",
      refs: [
        {
          agentInstanceId: "agent-a",
          sessionId: "s-9",
          roundId: "r-1",
          terminal: true,
        },
      ],
    });
    expect(directCardAddress(answer)).toEqual({
      targetSession: "s-1",
      messageId: "m-1",
      refs: [
        {
          agentInstanceId: "agent-a",
          sessionId: "s-9",
          roundId: "r-1",
          terminal: true,
        },
      ],
    });
  });

  it("omits an empty material list — absence is not `[]`-noise", () => {
    const answer = buildCardAnswer(card(), original, { sessionId: "s-1" })!;
    expect("refs" in answer).toBe(false);
  });
});

describe("the answer's address, per surface", () => {
  it("names the asking agent alone on the group channel", () => {
    const answer = buildCardAnswer(card(), original, { sessionId: "s-1" })!;
    expect(groupCardAddress(answer)).toEqual({
      targetSessions: { "agent-a": "s-1" },
      messageId: "m-1",
    });
  });

  it("and refuses continuation for that agent alone", () => {
    // The leak this pins: a blanket flag would make `@A @B` start a new session
    // for B because the human answered A's question.
    const answer = buildCardAnswer(card(), original, { forceNew: true })!;
    expect(groupCardAddress(answer)).toEqual({
      forceNewFor: ["agent-a"],
      messageId: "m-1",
    });
  });

  it("collapses to the single-session form on the 1:1 channel", () => {
    const picked = buildCardAnswer(card(), original, { sessionId: "s-1" })!;
    expect(directCardAddress(picked)).toEqual({
      targetSession: "s-1",
      messageId: "m-1",
    });
    const fresh = buildCardAnswer(card(), original, { forceNew: true })!;
    expect(directCardAddress(fresh)).toEqual({
      forceNew: true,
      messageId: "m-1",
    });
  });

  it("is a `confirm_continue` card answered the same way", () => {
    // Same re-delivery, different question wording — the address shape must not
    // drift between the two notice types that share it.
    const ask = card({
      meta: { ...card().meta, notice_type: "confirm_continue" },
    });
    const answer = buildCardAnswer(ask, original, { sessionId: "s-2" })!;
    expect(groupCardAddress(answer).targetSessions).toEqual({ "agent-a": "s-2" });
  });
});

describe("mayAnswerCard", () => {
  it("is the speaker's decision, not a bystander's", () => {
    expect(mayAnswerCard(card(), "u1")).toBe(true);
    expect(mayAnswerCard(card(), "someone-else")).toBe(false);
  });

  it("allows it in local mode, where the surface has no id to compare", () => {
    // One person exists; refusing because their id is unresolvable would hide
    // the only answerable card in the app.
    expect(mayAnswerCard(card(), "")).toBe(true);
    expect(mayAnswerCard(card({ to: "" }), "u1")).toBe(true);
  });
});
