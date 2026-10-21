/**
 * Group-surface store — the classification + user-side member actions that
 * carry ADR-098's `user_group` onto the shared group pipeline (P1/P2).
 *
 * Two behaviours are pinned here, both store rules rather than pixels:
 *
 *   1. `fetchGroups` classification — a `user_user` row becomes a DM, while
 *      `project_group` AND the new `user_group` land in the single `groups`
 *      bucket (the `kind` tag drives the row icon / member-panel path).
 *   2. the user-side member actions — `createGroup` (self is always added and
 *      deduped, the caller opens the fresh row), `addMembers` / `removeMember`
 *      (sync the local roster from the response; removing self drops the row
 *      and releases the pane; removing the owner throws so the panel can
 *      surface "transfer first"), and the ADR-098 governance writes
 *      `renameGroup` / `transferOwner` (backfill `title` / `owner`).
 *
 * `fetch` is the seam (the store hits the Gateway directly); the message
 * pipeline (`chatSessionStore.openGroup`) is stubbed because selecting a group
 * fans into it and those calls are irrelevant to these store rules.
 */
import { describe, expect, it, vi, beforeEach } from "vitest";

vi.mock("../lib/config", () => ({ getGatewayUrl: () => "http://gw.test" }));

const openGroup = vi.fn(async () => {});
vi.mock("./chatSessionStore", () => ({
  useChatSessionStore: { getState: () => ({ openGroup }) },
}));

import { useProjectGroupStore, dmPeerOf } from "./projectGroupStore";
import { useAuthStore } from "./authStore";
import type { ChatSessionInfo } from "./chatSessionStore";

const ME = "u-me";

function session(over: Partial<ChatSessionInfo> & { session_id: string; kind: string }): ChatSessionInfo {
  return {
    title: "",
    created_at: 0,
    last_active_at: 0,
    message_count: 0,
    ...over,
  } as ChatSessionInfo;
}

function jsonResp(body: unknown, status = 200): Response {
  return { ok: status >= 200 && status < 300, status, json: async () => body } as unknown as Response;
}

/** Route-aware fetch stub keyed by method + path suffix; records every call. */
function stubFetch(routes: Record<string, (url: string, init?: RequestInit) => Response | Promise<Response>>) {
  const spy = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === "string" ? input : input.toString();
    const method = (init?.method ?? "GET").toUpperCase();
    const key = `${method} ${url}`;
    for (const [pattern, make] of Object.entries(routes)) {
      const [m, suffix] = pattern.split(" ");
      if (method === m && url.includes(suffix!)) return make(url, init);
    }
    void key;
    throw new Error(`unexpected fetch: ${key}`);
  });
  vi.stubGlobal("fetch", spy);
  return spy;
}

beforeEach(() => {
  useAuthStore.setState({ mode: "multi_user", status: "logged_in", account: {
    user_id: ME, username: "me", display_name: "Me", role: "user",
    language: "en", timezone: "UTC", created_at: "", updated_at: "",
  }, accessToken: "token" });
  useProjectGroupStore.setState({ groups: [], dms: [], activeGroupId: null });
  openGroup.mockClear();
});

describe("fetchGroups classification (ADR-098)", () => {
  it("splits user_user into dms and folds project_group + user_group into groups", async () => {
    const rows = [
      session({ session_id: "dm-1", kind: "user_user", last_active_at: 5, participants: [{ id: ME }, { id: "u-peer" }] }),
      session({ session_id: "pg-1", kind: "project_group", project_id: "p-1", last_active_at: 10 }),
      session({ session_id: "ug-1", kind: "user_group", title: "Design sync", last_active_at: 8, participants: [{ id: ME }, { id: "u-peer" }] }),
    ];
    stubFetch({ "GET /api/chat-sessions/": () => jsonResp(rows) });
    await useProjectGroupStore.getState().fetchGroups();
    const { groups, dms } = useProjectGroupStore.getState();
    expect(dms.map((d) => d.session_id)).toEqual(["dm-1"]);
    // Both group kinds share one bucket, newest activity first (pg 10 > ug 8).
    expect(groups.map((g) => g.session_id)).toEqual(["pg-1", "ug-1"]);
  });

  it("clears the lists in local mode (no account)", async () => {
    useAuthStore.setState({ account: null });
    useProjectGroupStore.setState({ groups: [session({ session_id: "x", kind: "user_group" })], dms: [] });
    await useProjectGroupStore.getState().fetchGroups();
    expect(useProjectGroupStore.getState().groups).toEqual([]);
  });
});

describe("dmPeerOf", () => {
  it("returns the participant that is not the caller", () => {
    const dm = session({ session_id: "dm-1", kind: "user_user", participants: [{ id: ME }, { id: "u-peer" }] });
    expect(dmPeerOf(dm, ME)).toBe("u-peer");
  });
});

describe("createGroup", () => {
  it("posts a user_group with self added + deduped, then opens the fresh row", async () => {
    const created = session({ session_id: "ug-new", kind: "user_group", title: "Design sync", participants: [{ id: ME }, { id: "u-bob" }, { id: "ag-1" }] });
    let seenBody: any;
    const spy = stubFetch({
      "POST /api/chat-sessions": (_url, init) => {
        seenBody = JSON.parse(String(init!.body));
        return jsonResp(created);
      },
    });
    // `me` is passed and re-picked: the store must dedup it against the forced self.
    const sid = await useProjectGroupStore.getState().createGroup("Design sync", ["u-bob", ME, "ag-1", "u-bob"]);
    expect(sid).toBe("ug-new");
    expect(seenBody).toMatchObject({ kind: "user_group", title: "Design sync" });
    expect(seenBody.participants).toEqual([ME, "u-bob", "ag-1"]);
    expect(useProjectGroupStore.getState().groups.map((g) => g.session_id)).toContain("ug-new");
    expect(useProjectGroupStore.getState().activeGroupId).toBe("ug-new");
    expect(spy).toHaveBeenCalledTimes(1);
  });
});

describe("addMembers", () => {
  it("posts the ids and syncs the local roster from the response", async () => {
    useProjectGroupStore.setState({
      groups: [session({ session_id: "ug-1", kind: "user_group", participants: [{ id: ME }] })],
    });
    const after = session({ session_id: "ug-1", kind: "user_group", participants: [{ id: ME }, { id: "u-bob" }] });
    const spy = stubFetch({
      "POST /api/chat-sessions/ug-1/participants": (_url, init) => {
        expect(JSON.parse(String(init!.body))).toEqual({ participant_ids: ["u-bob"] });
        return jsonResp(after);
      },
    });
    await useProjectGroupStore.getState().addMembers("ug-1", ["u-bob"]);
    expect(useProjectGroupStore.getState().groups[0].participants).toEqual([{ id: ME }, { id: "u-bob" }]);
    expect(spy).toHaveBeenCalledTimes(1);
  });

  it("is a no-op for an empty id list", async () => {
    const spy = stubFetch({});
    await useProjectGroupStore.getState().addMembers("ug-1", []);
    expect(spy).not.toHaveBeenCalled();
  });
});

describe("removeMember", () => {
  it("removing another member syncs the roster from the response", async () => {
    useProjectGroupStore.setState({
      groups: [session({ session_id: "ug-1", kind: "user_group", participants: [{ id: ME }, { id: "u-bob" }] })],
    });
    const after = session({ session_id: "ug-1", kind: "user_group", participants: [{ id: ME }] });
    stubFetch({
      "DELETE /api/chat-sessions/ug-1/participants/u-bob": () => jsonResp(after),
    });
    await useProjectGroupStore.getState().removeMember("ug-1", "u-bob");
    expect(useProjectGroupStore.getState().groups[0].participants).toEqual([{ id: ME }]);
  });

  it("removing self drops the row and releases the pane if it was active", async () => {
    useProjectGroupStore.setState({
      groups: [session({ session_id: "ug-1", kind: "user_group", participants: [{ id: ME }, { id: "u-bob" }] })],
      activeGroupId: "ug-1",
    });
    const after = session({ session_id: "ug-1", kind: "user_group", participants: [{ id: "u-bob" }] });
    stubFetch({
      "DELETE /api/chat-sessions/ug-1/participants/u-me": () => jsonResp(after),
    });
    await useProjectGroupStore.getState().removeMember("ug-1", ME);
    expect(useProjectGroupStore.getState().groups).toEqual([]);
    expect(useProjectGroupStore.getState().activeGroupId).toBeNull();
  });

  it("refusing to remove the owner throws and leaves the roster untouched", async () => {
    // The backend refuses an owner removal before the ≥2 floor (ADR-098);
    // the store surfaces the failure instead of patching local state.
    useProjectGroupStore.setState({
      groups: [session({ session_id: "ug-1", kind: "user_group", owner: "u-bob", participants: [{ id: ME }, { id: "u-bob" }] })],
    });
    stubFetch({
      "DELETE /api/chat-sessions/ug-1/participants/u-bob": () =>
        jsonResp({ error: "a group must keep an owner" }, 422),
    });
    await expect(useProjectGroupStore.getState().removeMember("ug-1", "u-bob")).rejects.toThrow(
      /422/,
    );
    expect(useProjectGroupStore.getState().groups[0].participants).toEqual([{ id: ME }, { id: "u-bob" }]);
  });
});

describe("renameGroup / transferOwner (ADR-098 governance)", () => {
  it("rename PATCHes the title route and backfills the local row", async () => {
    useProjectGroupStore.setState({
      groups: [session({ session_id: "ug-1", kind: "user_group", title: "old" })],
    });
    stubFetch({
      "PATCH /api/chat-sessions/ug-1/title": () =>
        jsonResp(session({ session_id: "ug-1", kind: "user_group", title: "new" })),
    });
    await useProjectGroupStore.getState().renameGroup("ug-1", "new");
    expect(useProjectGroupStore.getState().groups[0].title).toBe("new");
  });

  it("transfer POSTs the owner route and backfills the local owner", async () => {
    useProjectGroupStore.setState({
      groups: [session({ session_id: "ug-1", kind: "user_group", owner: ME })],
    });
    stubFetch({
      "POST /api/chat-sessions/ug-1/owner": () =>
        jsonResp(session({ session_id: "ug-1", kind: "user_group", owner: "u-bob" })),
    });
    await useProjectGroupStore.getState().transferOwner("ug-1", "u-bob");
    expect(useProjectGroupStore.getState().groups[0].owner).toBe("u-bob");
  });
});
