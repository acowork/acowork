/**
 * `fetchCardAnswer` — the wire half of "the service owns the words".
 *
 * The rule under test is the status mapping, because that is where the surfaces
 * get their behaviour from: a spent card (`409`) must NOT be reported as an
 * error (the projection's next read closes it), and every "this can never be
 * re-delivered" answer (`404` hidden-not-found, `422` not-a-card / origin-gone)
 * must collapse to one honest outcome instead of a dead button.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fetchCardAnswer } from "./chatSessionStore";

/** A `Response`-shaped stub: the store only reads `status`, `ok`, `json()`. */
function reply(status: number, body?: unknown) {
  return {
    status,
    ok: status >= 200 && status < 300,
    json: async () => body,
  } as unknown as Response;
}

describe("fetchCardAnswer", () => {
  beforeEach(() => {
    vi.stubGlobal("fetch", vi.fn());
  });
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("hands back the original words, coordinates and pinned material", async () => {
    vi.mocked(fetch).mockResolvedValue(
      reply(200, {
        agent_id: "agent-a",
        message_id: "m-1",
        text: "run the migration on staging",
        attachment_ids: ["att-1"],
        refs: [
          {
            agent_instance_id: "agent-a",
            session_id: "s-9",
            round_id: "r-1",
            terminal: true,
          },
          // A quadruple without coordinates is not a reference; it must not
          // become a half-empty picker entry.
          { agent_instance_id: "agent-b" },
        ],
      }),
    );
    const lookup = await fetchCardAnswer("chat-1", "card-1");
    expect(lookup).toEqual({
      ok: true,
      facts: {
        agentId: "agent-a",
        messageId: "m-1",
        text: "run the migration on staging",
        attachmentIds: ["att-1"],
        refs: [
          {
            agentInstanceId: "agent-a",
            sessionId: "s-9",
            roundId: "r-1",
            terminal: true,
          },
        ],
      },
    });
    // The card's own row is the address — not the message it asks about.
    expect(String(vi.mocked(fetch).mock.calls[0][0])).toContain(
      "/api/chat-sessions/chat-1/messages/card-1/answer",
    );
  });

  it("treats a spent card as settled, not as a failure", async () => {
    vi.mocked(fetch).mockResolvedValue(reply(409));
    expect(await fetchCardAnswer("chat-1", "card-1")).toEqual({
      ok: false,
      reason: "already_answered",
    });
  });

  it("collapses every unanswerable card to one outcome", async () => {
    for (const status of [404, 422]) {
      vi.mocked(fetch).mockResolvedValue(reply(status));
      expect(await fetchCardAnswer("chat-1", "card-1")).toEqual({
        ok: false,
        reason: "gone",
      });
    }
  });

  it("surfaces a real transport failure instead of pretending the card is gone", async () => {
    vi.mocked(fetch).mockResolvedValue(reply(500));
    await expect(fetchCardAnswer("chat-1", "card-1")).rejects.toThrow(
      "card answer → 500",
    );
  });
});
