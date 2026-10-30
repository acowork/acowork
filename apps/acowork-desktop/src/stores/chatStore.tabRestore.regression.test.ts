/**
 * Regression t-f9a9fbc7 (recovery half): "quit the Desktop while an execution
 * session is running, relaunch — the session is gone from the tab strip".
 *
 * Root cause pinned here: the Desktop owns no session state, but the tab strip
 * was rebuilt from a single pointer (`/latest-session`) and, when that pointer
 * was unreachable during the boot window, from nothing at all. The sessions a
 * user has open are Runtime truth — `open_session` adds to the `Active` set,
 * `close_session` removes from it, `GET /sessions?active=true` reports it — so
 * the recovery is a projection of that set, not a client-side memory.
 *
 * `adoptRuntimeTabs` is that projection. These tests pin its contract:
 *   - cold start only (empty strip), once per app launch per agent;
 *   - only sessions this caller may write (`can_write !== false`), because a
 *     session resident only because someone else has it open is not ours;
 *   - the newest one goes through the full open path, the rest are strip
 *     entries only (they are already Active server-side);
 *   - a FAILED query restores nothing and does not latch, so a later attempt
 *     still works.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";

const mockInvoke = vi.fn().mockResolvedValue(null);
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => mockInvoke(...args),
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
import type { SessionInfo } from "../lib/types";

const AGENT = "com.acowork.engineer";

function row(
  sessionId: string,
  opts: { active?: boolean; canWrite?: boolean; lastActive?: string } = {},
): SessionInfo {
  return {
    session_id: sessionId,
    created_at: "2026-10-01T00:00:00Z",
    last_active_at: opts.lastActive ?? "2026-10-01T00:00:00Z",
    message_count: 0,
    title: sessionId,
    is_active: opts.active ?? true,
    can_write: opts.canWrite ?? true,
  } as SessionInfo;
}

function agentState() {
  return useChatStore.getState().agentStates[AGENT];
}

beforeEach(() => {
  useChatStore.setState({ agentStates: {} });
  // openSession owns the backend call + history load; the projection under
  // test is what decides WHICH session that is, so stub it and assert.
  useChatStore.setState({
    openSession: vi.fn(async () => {}),
  } as never);
});

describe("cold-start tab restore from the Runtime Active set", () => {
  it("is offered only while the strip is empty and nothing has been restored yet", () => {
    expect(useChatStore.getState().needsTabRestore(AGENT)).toBe(true);

    useChatStore.setState((s) => ({
      agentStates: {
        ...s.agentStates,
        [AGENT]: {
          sessionStates: {},
          activeSessionId: "s-open",
          openSessionIds: ["s-open"],
          lastLoadedSessionId: null,
          isSessionInitLoading: false,
          preferredModel: null,
          preferredProvider: null,
          llmAvailability: "unspecified",
          tabsRestoredFromRuntime: false,
        },
      },
    }) as never);

    // A non-empty strip means something already decided what to show — the
    // restore must never stomp it.
    expect(useChatStore.getState().needsTabRestore(AGENT)).toBe(false);
  });

  it("projects every writable Active session, opening only the newest for real", async () => {
    const openSpy = vi.fn(async () => {});
    useChatStore.setState({ openSession: openSpy } as never);

    const foreground = await useChatStore.getState().adoptRuntimeTabs(AGENT, [
      row("s-old", { lastActive: "2026-10-01T08:00:00Z" }),
      row("s-running", { lastActive: "2026-10-01T12:00:00Z" }),
      row("s-mid", { lastActive: "2026-10-01T10:00:00Z" }),
    ]);

    expect(foreground).toBe("s-running");
    expect(openSpy).toHaveBeenCalledTimes(1);
    expect(openSpy).toHaveBeenCalledWith(AGENT, "s-running");

    const agent = agentState();
    // Background tabs are strip entries only: they are already `Active`
    // server-side, so re-opening them would cost N history loads to paint
    // one tab bar.
    expect(agent.openSessionIds).toEqual(["s-old", "s-mid"]);
    expect(agent.tabsRestoredFromRuntime).toBe(true);
    // Titles are NOT copied into the live overlay: that tier outranks the
    // `sessions[]` snapshot, so a client copy would shadow a later backend
    // rename. A restored tab therefore has no session state of its own.
    expect(agent.sessionStates["s-mid"]).toBeUndefined();
  });

  it("skips sessions the caller may not write (someone else has them open)", async () => {
    const openSpy = vi.fn(async () => {});
    useChatStore.setState({ openSession: openSpy } as never);

    const foreground = await useChatStore.getState().adoptRuntimeTabs(AGENT, [
      row("s-shared-with-me", { canWrite: false, lastActive: "2026-10-01T12:00:00Z" }),
      row("s-mine", { lastActive: "2026-10-01T08:00:00Z" }),
    ]);

    expect(foreground).toBe("s-mine");
    expect(openSpy).toHaveBeenCalledWith(AGENT, "s-mine");
    expect(agentState().openSessionIds).toEqual([]);
  });

  it("treats an older Runtime without `can_write` as ours (no silent lock-out)", async () => {
    const openSpy = vi.fn(async () => {});
    useChatStore.setState({ openSession: openSpy } as never);

    const legacy = row("s-legacy", {});
    delete (legacy as { can_write?: boolean }).can_write;

    expect(await useChatStore.getState().adoptRuntimeTabs(AGENT, [legacy])).toBe("s-legacy");
  });

  it("does not restore twice in one launch (once per agent, then the strip is user-owned)", async () => {
    const openSpy = vi.fn(async () => {});
    useChatStore.setState({ openSession: openSpy } as never);

    await useChatStore.getState().adoptRuntimeTabs(AGENT, [row("s-1")]);
    // The stubbed openSession does not touch the strip, so this second call is
    // blocked by the latch alone — exactly the "agent stop/start later in the
    // same launch must not resurrect closed tabs" rule.
    const second = await useChatStore.getState().adoptRuntimeTabs(AGENT, [row("s-2")]);

    expect(second).toBeNull();
    expect(openSpy).toHaveBeenCalledTimes(1);
  });

  it("a failed Active query restores nothing AND does not latch", async () => {
    const openSpy = vi.fn(async () => {});
    useChatStore.setState({ openSession: openSpy } as never);

    expect(await useChatStore.getState().adoptRuntimeTabs(AGENT, null)).toBeNull();
    expect(openSpy).not.toHaveBeenCalled();
    // Backend truth was never learned, so the restore is still owed.
    expect(useChatStore.getState().needsTabRestore(AGENT)).toBe(true);
  });

  it("an empty Active set is a real answer: latch, open nothing", async () => {
    const openSpy = vi.fn(async () => {});
    useChatStore.setState({ openSession: openSpy } as never);

    expect(await useChatStore.getState().adoptRuntimeTabs(AGENT, [])).toBeNull();
    expect(openSpy).not.toHaveBeenCalled();
    expect(useChatStore.getState().needsTabRestore(AGENT)).toBe(false);
  });
});