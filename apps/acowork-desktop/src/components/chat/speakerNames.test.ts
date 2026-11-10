/**
 * The speaker badge says NAMES, not ids.
 *
 * The record behind the badge carries ids (a human account uid, an agent
 * instance id) because that is what the write path verified — but a reader
 * cannot tell a UUID from a person, and the complaint that started this was
 * exactly that: "babea202-… 代 31839e13-… 发言" says nothing.
 *
 * The rules pinned here:
 *   1. the AUTHOR resolves against the signed-in account, the agent registry
 *      and the account directory — the signed-in account first, because the
 *      account directory EXCLUDES the viewer (`list_directory` filters the
 *      caller out), so it can never answer for "me";
 *   2. the TARGET (the session's initiator) resolves through the SAME
 *      resolver the initiator surfaces use (`resolveInitiatorIdentity`):
 *      agent registry → account directory → short id;
 *   3. an id no registry answers for degrades to a SHORT id — never to
 *      nothing, never to the viewer's own name.
 */
import { describe, expect, it } from "vitest";
import { resolveSpeakerNames } from "./speakerNames";
import type { DirectoryUser } from "../../lib/types";

const STAND_IN = {
  origin: "human" as const,
  author: "u-nicholas",
  on_behalf_of: "inst-agent-a",
};

const DIR: DirectoryUser[] = [
  {
    user_id: "u-other",
    username: "other",
    display_name: "Other Person",
  },
];

describe("speaker names", () => {
  it("names the signed-in author and the agent initiator", () => {
    const names = resolveSpeakerNames(STAND_IN, {
      me: "u-nicholas",
      myName: "Nicholas",
      agents: { "inst-agent-a": { meta: { name: "Product.Lucy" } } },
      directory: DIR,
    });
    expect(names?.author).toBe("Nicholas");
    expect(names?.target).toBe("Product.Lucy");
  });

  it("names an author who is not the viewer, from the directory", () => {
    const names = resolveSpeakerNames(
      { origin: "human", author: "u-other", on_behalf_of: "inst-agent-a" },
      {
        me: "u-nicholas",
        myName: "Nicholas",
        agents: { "inst-agent-a": { meta: { name: "Product.Lucy" } } },
        directory: DIR,
      },
    );
    expect(names?.author).toBe("Other Person");
  });

  it("names an agent author from the registry", () => {
    const names = resolveSpeakerNames(
      { origin: "agent", author: "inst-agent-b", on_behalf_of: "inst-agent-a" },
      {
        agents: {
          "inst-agent-b": { meta: { name: "SSE.Cassie" } },
          "inst-agent-a": { meta: { name: "Product.Lucy" } },
        },
        directory: DIR,
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
      { agents: {}, directory: DIR },
    );
    expect(names?.author).toBe("babea202");
    expect(names?.target).toBe("31839e13");
  });

  it("falls back to the short id when the signed-in account has no name yet", () => {
    // The profile store's name is the live one (a rename updates it), but it
    // can still be empty before onboarding finishes. The viewer must not be
    // painted as a bare id when the DIRECTORY could answer — but the
    // directory excludes the viewer, so there is nothing to fall back to.
    const names = resolveSpeakerNames(STAND_IN, {
      me: "u-nicholas",
      myName: "",
      agents: { "inst-agent-a": { meta: { name: "Product.Lucy" } } },
      directory: DIR,
    });
    expect(names?.author).toBe("u-nichol");
  });

  it("says nothing when there is no disclosure", () => {
    expect(resolveSpeakerNames(undefined, { agents: {}, directory: DIR })).toBeNull();
    // A human speaking in their own right: no substitution, no badge.
    expect(
      resolveSpeakerNames(
        { origin: "human", author: "u-nicholas" },
        { agents: {}, directory: DIR },
      ),
    ).toBeNull();
  });
});
