/**
 * The speaker badge says NAMES, not ids.
 *
 * The record behind the badge carries ids (a human account uid, an agent
 * instance id) because that is what the write path verified — but a reader
 * cannot tell a person from a UUID, and the complaint that started this was
 * exactly that: "babea202-… 代 31839e13-… 发言" says nothing.
 *
 * The rules pinned here:
 *   1. the AUTHOR resolves against the signed-in account, the agent registry
 *      and the account directory — in that order (own account first: it is
 *      the one the viewer certainly knows);
 *   2. the TARGET (the session's initiator) resolves against the agent
 *      registry and the directory;
 *   3. an id no registry answered for degrades to a SHORT id — never to
 *      nothing, never to the viewer's own name.
 */
import { describe, expect, it } from "vitest";
import { resolveSpeakerNames } from "./speakerNames";

const STAND_IN = {
  origin: "human" as const,
  author: "u-nicholas",
  on_behalf_of: "inst-agent-a",
};

describe("speaker names", () => {
  it("names the signed-in author and the agent initiator", () => {
    const names = resolveSpeakerNames(STAND_IN, {
      me: "u-nicholas",
      myName: "Nicholas",
      agents: { "inst-agent-a": { meta: { name: "Product.Lucy" } } },
      directory: [],
    });
    expect(names?.author).toBe("Nicholas");
    expect(names?.target).toBe("Product.Lucy");
  });

  it("names an author who is not the viewer, from the directory", () => {
    const names = resolveSpeakerNames(STAND_IN, {
      me: "u-other",
      myName: "Other",
      agents: {},
      directory: [
        { user_id: "u-nicholas", username: "nicholas", display_name: "Nicholas" },
      ],
    });
    expect(names?.author).toBe("Nicholas");
  });

  it("names an agent author from the registry", () => {
    const names = resolveSpeakerNames(
      { origin: "agent", author: "inst-agent-b", on_behalf_of: "inst-agent-a" },
      {
        agents: {
          "inst-agent-b": { meta: { name: "SSE.Cassie" } },
          "inst-agent-a": { meta: { name: "Product.Lucy" } },
        },
        directory: [],
      },
    );
    expect(names?.author).toBe("SSE.Cassie");
    expect(names?.target).toBe("Product.Lucy");
  });

  it("degrades an unresolved id to a short id, not to nothing", () => {
    const names = resolveSpeakerNames(
      {
        origin: "human",
        author: "babea202-6daf-48ae-9a64-a7db095ce9f9",
        on_behalf_of: "31839e13-128b-4b51-bd44-721b061b0f4f",
      },
      { agents: {}, directory: [] },
    );
    expect(names?.author).toBe("babea202");
    expect(names?.target).toBe("31839e13");
  });

  it("says nothing when there is no disclosure", () => {
    expect(resolveSpeakerNames(undefined, { agents: {}, directory: [] })).toBeNull();
    // A human speaking in their own right: no substitution, no badge.
    expect(
      resolveSpeakerNames(
        { origin: "human", author: "u-nicholas" },
        { agents: {}, directory: [] },
      ),
    ).toBeNull();
  });
});
