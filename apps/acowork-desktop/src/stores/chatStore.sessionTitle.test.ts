/**
 * Store-level coverage for the unified per-session `title` path.
 *
 * The regression this pins: a Chat-Router exec session has no row in
 * `agentStore.sessions[]` until `fetchSessions`, so the old bespoke
 * `updateSessionTitle` silently dropped the MQTT-pushed title (no row → no-op)
 * and refused to refresh (already titled → skip). Title now lands in
 * `chatStore.sessionStates[sid].title` like model/provider, with two writers:
 *
 *   - `seedOptimisticTitle` — provisional, first-message derived; must land
 *     even with no list row, and must NOT clobber an existing authoritative
 *     title (live overlay OR the `fetchSessions` snapshot, since the Runtime
 *     generates a session's title once and never re-pushes it);
 *   - `setSessionTitleLocal` — manual rename; ignores blank input so the
 *     overlay never shadows the snapshot with the "Untitled" fallback.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: () => Promise.reject(new Error("not used in this test")),
}));

vi.mock("../lib/logger", () => ({
  log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
  setLevel: () => {},
  getLevel: () => "off" as const,
}));

vi.mock("../lib/profileStore", () => ({
  loadAllProfiles: () => ({}),
  loadProfile: () => null,
  saveProfile: () => {},
  DEFAULT_PROFILE: {
    language: "zh-CN",
    timezone: "Asia/Shanghai",
    model_override: null,
    reasoning_effort: null,
    auto_approve_tools: [],
    yolo_mode: false,
  },
}));

import { useChatStore } from "./chatStore";
import { useAgentStore } from "./agentStore";

const AGENT = "com.test.Agent";
const SESSION = "sess-title-1";

const readTitle = () =>
  useChatStore.getState().getSessionState(AGENT, SESSION).title;

beforeEach(() => {
  // Reset both stores: title reads span chatStore (overlay) and agentStore
  // (list snapshot).
  useChatStore.setState({ agentStates: {} });
  useAgentStore.setState({ agents: {}, selectedAgentId: null });
});

describe("seedOptimisticTitle", () => {
  it("lands a derived title even with no `sessions[]` row (the original drop bug)", () => {
    useChatStore.getState().seedOptimisticTitle(AGENT, SESSION, "提交代码。承接刚才 review 的结论");
    // The session had no list row and no prior state — updateSessionState must
    // create it and the title must be present, not silently discarded.
    expect(readTitle()).toBe("提交代码。承接刚才 review 的结论");
  });

  it("caps long messages by code points (CJK-safe)", () => {
    useChatStore.getState().seedOptimisticTitle(AGENT, SESSION, "字".repeat(50));
    expect(Array.from(readTitle() ?? "")).toHaveLength(25); // 24 + ellipsis
  });

  it("no-ops when the live overlay already holds a title", () => {
    useChatStore.getState().seedOptimisticTitle(AGENT, SESSION, "第一个标题");
    useChatStore.getState().seedOptimisticTitle(AGENT, SESSION, "第二个用户消息");
    // The second turn must not overwrite the first optimistic seed.
    expect(readTitle()).toBe("第一个标题");
  });

  it("no-ops when the `sessions[]` snapshot already has a title (backend generated once)", () => {
    // Simulate a restart: overlay empty (wiped by clearAgentSessions) but the
    // list snapshot still carries the authoritative title. Re-seeding would
    // shadow it with a truncated latest message and no later push would heal
    // it, because the Runtime only generates a title on the first turn.
    useAgentStore.setState((s) => ({
      agents: {
        ...s.agents,
        [AGENT]: {
          sessions: [{ session_id: SESSION, title: "已有的正式标题" }],
        },
      } as never,
    }));

    useChatStore.getState().seedOptimisticTitle(AGENT, SESSION, "随便一条新消息");
    expect(readTitle()).toBeNull();
  });
});

describe("setSessionTitleLocal", () => {
  it("writes a non-empty title to the overlay", () => {
    useChatStore.getState().setSessionTitleLocal(AGENT, SESSION, "用户重命名");
    expect(readTitle()).toBe("用户重命名");
  });

  it("ignores blank input so it can never shadow the snapshot", () => {
    useChatStore.getState().setSessionTitleLocal(AGENT, SESSION, "真实标题");
    useChatStore.getState().setSessionTitleLocal(AGENT, SESSION, "   ");
    // Blank write is a no-op — the prior value must survive.
    expect(readTitle()).toBe("真实标题");
  });
});
