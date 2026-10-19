/**
 * Design 27 (v2.1) — closing/deleting the LAST open exec tab must not
 * resurrect a background session.
 *
 * The pre-chat rule "the exec surface always needs an open session" made
 * `agentStore.closeSession` fall back to `openSession(remaining[0])` when
 * no open tabs were left — so closing the last exec tab instantly opened
 * another one ("close one, get one back, never close them all"). With the
 * pinned Chat tab as the landing surface that fallback is a bug: the only
 * sessions that may auto-activate after a close/delete are tabs that are
 * STILL OPEN (closeTab's neighbour), never closed background rows.
 *
 * These tests drive the real stores with `chatStore.openSession` swapped
 * for a spy and assert: last open tab closed/deleted → zero openSession
 * calls, active becomes null; a still-open neighbour → exactly one call.
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({
    invoke: () => Promise.reject(new Error("not used in this test")),
}));

vi.mock("@tauri-apps/api/event", () => ({
    listen: () => Promise.resolve(() => {}),
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

import { useAgentStore } from "./agentStore";
import { useChatStore } from "./chatStore";
import type { SessionInfo } from "../lib/types";

const AGENT = "b7f0c6c2-4f10-4f5e-9a1e-3d8e9f2a1c33";
const ACTIVE = "sess-active";
const BACKGROUND = "sess-background";
const OTHER_OPEN = "sess-other-open";

const mockOpenSession = vi.fn();
const originalOpenSession = useChatStore.getState().openSession;

function makeSession(id: string): SessionInfo {
    return {
        session_id: id,
        created_at: "2026-01-01T00:00:00Z",
        last_active_at: "2026-01-01T00:00:00Z",
        message_count: 0,
        title: id,
        can_write: true,
    };
}

/** Seed the agent's history list + the chat-store tab strip state. */
function seed(allSessions: SessionInfo[], openIds: string[], activeId: string | null) {
    useAgentStore.setState({
        agents: {
            [AGENT]: {
                meta: null,
                profile: {},
                sessions: allSessions,
                sessionTitle: undefined,
                pagination: {
                    currentPage: 1,
                    totalPages: 1,
                    totalCount: allSessions.length,
                    pageSize: 20,
                },
                isLoading: false,
            },
        },
    } as never);
    useChatStore.setState((state) => ({
        agentStates: {
            ...state.agentStates,
            [AGENT]: {
                sessionStates: {},
                activeSessionId: activeId,
                openSessionIds: openIds,
                lastLoadedSessionId: null,
                isSessionInitLoading: false,
                preferredModel: null,
                preferredProvider: null,
                llmAvailability: "unspecified",
            },
        },
    }) as never);
}

const strip = () => useChatStore.getState().agentStates[AGENT];

beforeEach(() => {
    mockOpenSession.mockReset().mockResolvedValue(undefined);
    useChatStore.setState({ openSession: mockOpenSession } as never);
    // chatStore.closeTab POSTs /close (can_write=true rows) — stub it.
    vi.stubGlobal(
        "fetch",
        vi.fn(() =>
            Promise.resolve({
                ok: true,
                status: 200,
                text: () => Promise.resolve(""),
                json: () => Promise.resolve({}),
            }),
        ),
    );
});

afterEach(() => {
    vi.unstubAllGlobals();
    useChatStore.setState({ openSession: originalOpenSession } as never);
    useAgentStore.setState({ agents: {} } as never);
    useChatStore.setState({ agentStates: {} } as never);
});

describe("agentStore.closeSession — last exec tab must not auto-reopen", () => {
    it("closing the only open tab does NOT open a background session", async () => {
        // History still has a closed background row — the old fallback
        // would resurrect it, which is the "close one, get one back" loop.
        seed(
            [makeSession(ACTIVE), makeSession(BACKGROUND)],
            [ACTIVE],
            ACTIVE,
        );

        await useAgentStore.getState().closeSession(AGENT, ACTIVE);

        expect(mockOpenSession).not.toHaveBeenCalled();
        const st = strip();
        expect(st.openSessionIds).toEqual([]);
        expect(st.activeSessionId).toBeNull();
    });

    it("closing the only open tab with no history rows is equally quiet", async () => {
        seed([makeSession(ACTIVE)], [ACTIVE], ACTIVE);

        await useAgentStore.getState().closeSession(AGENT, ACTIVE);

        expect(mockOpenSession).not.toHaveBeenCalled();
        expect(strip().activeSessionId).toBeNull();
    });

    it("a still-open neighbour becomes the new active (unchanged behaviour)", async () => {
        seed(
            [makeSession(ACTIVE), makeSession(OTHER_OPEN), makeSession(BACKGROUND)],
            [ACTIVE, OTHER_OPEN],
            ACTIVE,
        );

        await useAgentStore.getState().closeSession(AGENT, ACTIVE);

        // closeTab activates the neighbour; agentStore re-opens it
        // idempotently — the ONLY allowed auto-open.
        expect(mockOpenSession).toHaveBeenCalledTimes(1);
        expect(mockOpenSession).toHaveBeenCalledWith(AGENT, OTHER_OPEN);
        expect(strip().activeSessionId).toBe(OTHER_OPEN);
    });
});

describe("agentStore.deleteSession — same no-resurrect rule", () => {
    it("deleting the current (only open) session does NOT open a background row", async () => {
        seed(
            [makeSession(ACTIVE), makeSession(BACKGROUND)],
            [ACTIVE],
            ACTIVE,
        );

        await useAgentStore.getState().deleteSession(AGENT, ACTIVE);

        expect(mockOpenSession).not.toHaveBeenCalled();
        const st = strip();
        expect(st.openSessionIds).toEqual([]);
        expect(st.activeSessionId).toBeNull();
    });
});
