/**
 * `speakerProvenance` — what a turn discloses about who spoke it (plan D3).
 *
 * The pure rule, tested without mounting React: the badge fires on a
 * substitution and on nothing else. The interesting cases are the silent ones —
 * an ordinary turn and a human speaking in their own voice must produce no
 * output at all, because a badge on every line is how a disclosure turns into
 * noise and then into ignored noise.
 */

import { isVoicedForAnother, speakerDisclosure } from "./speakerProvenance";

describe("speakerDisclosure", () => {
  it("says nothing about an ordinary turn", () => {
    expect(speakerDisclosure(undefined)).toBeNull();
    expect(speakerDisclosure(null)).toBeNull();
  });

  it("discloses a human speaking as the session's initiator", () => {
    // The case the tier was built for: agent-a started the run, a human kept it
    // alive, and the row must not read as the agent's own words.
    expect(
      speakerDisclosure({ origin: "human", author: "nicholas", on_behalf_of: "agent-a" }),
    ).toEqual({ author: "nicholas", standsInFor: "agent-a" });
    expect(isVoicedForAnother({ origin: "human", author: "nicholas", on_behalf_of: "agent-a" })).toBe(
      true,
    );
  });

  it("stays silent when the human joined in their own right", () => {
    // mode 3: `author` set, nobody stood in for. Voicing them as someone else
    // would erase the presence the mode exists to record — and a badge here
    // would appear on every turn they ever write.
    const ownVoice = { origin: "human" as const, author: "nicholas" };
    expect(speakerDisclosure(ownVoice)).toBeNull();
    expect(isVoicedForAnother(ownVoice)).toBe(false);
  });

  it("treats a partial row as nothing to disclose", () => {
    // An unnamed voice is not something a person can act on; `on_behalf_of`
    // with no author is a producer bug, not a disclosure.
    expect(speakerDisclosure({ origin: "human", on_behalf_of: "agent-a" })).toBeNull();
    expect(speakerDisclosure({ origin: "human", author: "  ", on_behalf_of: "agent-a" })).toBeNull();
    // A row claiming somebody stood in for themselves is not a substitution.
    expect(
      speakerDisclosure({ origin: "human", author: "nicholas", on_behalf_of: "nicholas" }),
    ).toBeNull();
  });

  it("normalises whitespace on both sides", () => {
    expect(
      speakerDisclosure({ origin: "human", author: " nicholas ", on_behalf_of: " agent-a " }),
    ).toEqual({ author: "nicholas", standsInFor: "agent-a" });
  });
});
