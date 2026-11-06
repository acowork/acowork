/**
 * Answering an addressing card is a RE-DELIVERY (ADR-106 D6).
 *
 * These are the rules that make that sentence true rather than aspirational:
 * the human's own words come back, they come back ONCE (the original row's id
 * rides as `message_id`, which the store dedupes on), and the address belongs
 * only to the agent that asked. The last one is the failure this file exists to
 * keep: a fan-out that refuses continuation for two agents because the human
 * answered one of them is the same silent re-routing D6 was written to remove.
 *
 * WHERE the words come from is the other half: the service resolves them from
 * the log (design 31 §6), so a card whose message has scrolled out of the
 * loaded window still answers. This module only folds in the click.
 */
import { describe, expect, it } from "vitest";
import {
  answerWithChoice,
  directCardAddress,
  groupCardAddress,
  mayAnswerCard,
} from "./chatCardAnswer";
import type { CardAnswerFacts, ChatMessageRow } from "../stores/chatSessionStore";

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

/** What `fetchCardAnswer` returns for a card about `m-1`. */
function facts(over: Partial<CardAnswerFacts> = {}): CardAnswerFacts {
  return {
    agentId: "agent-a",
    messageId: "m-1",
    text: "go on with the release note",
    attachmentIds: [],
    refs: [],
    ...over,
  };
}

describe("answerWithChoice", () => {
  it("keeps the human's own words and the row they wrote", () => {
    const answer = answerWithChoice(facts(), { sessionId: "s-1" });
    expect(answer).toEqual({
      text: "go on with the release note",
      attachmentIds: [],
      agentId: "agent-a",
      messageId: "m-1",
      sessionId: "s-1",
      forceNew: false,
    });
  });

  it("refuses a click that names neither a session nor new work", () => {
    expect(answerWithChoice(facts(), {})).toBeNull();
  });

  it("carries the attachments the original message had", () => {
    const answer = answerWithChoice(
      facts({ attachmentIds: ["att-1"] }),
      { forceNew: true },
    );
    expect(answer?.attachmentIds).toEqual(["att-1"]);
  });

  it("echoes the original row's pinned material back verbatim", () => {
    // ADR-106 revision 2026-11-04: a re-delivery is the SAME utterance, so the
    // `@session` material it carried travels with its RESOLVED coordinates —
    // the Gateway passes the echo through rather than re-pinning a sentence
    // already written. Which refs are "the answering agent's own" is decided
    // where the log is read (`chat_card::refs_for`), not here.
    const pinned = [
      {
        agentInstanceId: "agent-a",
        sessionId: "s-9",
        roundId: "r-1",
        terminal: true,
      },
    ];
    const answer = answerWithChoice(facts({ refs: pinned }), {
      sessionId: "s-1",
    })!;
    expect(answer.refs).toEqual(pinned);
    expect(groupCardAddress(answer)).toEqual({
      targetSessions: { "agent-a": "s-1" },
      messageId: "m-1",
      refs: pinned,
    });
    expect(directCardAddress(answer)).toEqual({
      targetSession: "s-1",
      messageId: "m-1",
      refs: pinned,
    });
  });

  it("omits an empty material list — absence is not `[]`-noise", () => {
    const answer = answerWithChoice(facts(), { sessionId: "s-1" })!;
    expect("refs" in answer).toBe(false);
  });

  it("refuses an answer the service could not attribute to an agent", () => {
    // The card's author IS the agent whose runs are on offer; without it there
    // is no address to name, so a re-delivery would be a message to nobody.
    expect(answerWithChoice(facts({ agentId: "" }), { sessionId: "s-1" })).toBeNull();
  });
});

describe("the answer's address, per surface", () => {
  it("names the asking agent alone on the group channel", () => {
    const answer = answerWithChoice(facts(), { sessionId: "s-1" })!;
    expect(groupCardAddress(answer)).toEqual({
      targetSessions: { "agent-a": "s-1" },
      messageId: "m-1",
    });
  });

  it("and refuses continuation for that agent alone", () => {
    // The leak this pins: a blanket flag would make `@A @B` start a new session
    // for B because the human answered A's question.
    const answer = answerWithChoice(facts(), { forceNew: true })!;
    expect(groupCardAddress(answer)).toEqual({
      forceNewFor: ["agent-a"],
      messageId: "m-1",
    });
  });

  it("is the degenerate one-session form on the 1:1 channel", () => {
    const picked = answerWithChoice(facts(), { sessionId: "s-1" })!;
    expect(directCardAddress(picked)).toEqual({
      targetSession: "s-1",
      messageId: "m-1",
    });
    const fresh = answerWithChoice(facts(), { forceNew: true })!;
    expect(directCardAddress(fresh)).toEqual({
      forceNew: true,
      messageId: "m-1",
    });
  });

  it("is a `confirm_continue` card answered the same way", () => {
    // Same re-delivery, different question wording — the address shape must not
    // drift between the two notice types that share it.
    const answer = answerWithChoice(facts(), { sessionId: "s-2" })!;
    expect(groupCardAddress(answer).targetSessions).toEqual({ "agent-a": "s-2" });
  });
});

describe("mayAnswerCard", () => {
  it("is the speaker's decision, not a bystander's", () => {
    expect(mayAnswerCard(card(), "u1")).toBe(true);
    expect(mayAnswerCard(card(), "someone-else")).toBe(false);
  });

  it("stays visible in local mode, where no id can be resolved", () => {
    expect(mayAnswerCard(card({ to: "" }), "")).toBe(true);
    expect(mayAnswerCard(card(), "")).toBe(true);
  });
});
