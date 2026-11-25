/**
 * A refused rename must not survive on screen.
 *
 * `agentStore.renameSession` used to `try { PUT /config } catch { log.error }`
 * — it swallowed the failure, so the optimistic title it had just written to
 * three places (the `sessions[]` row, the sidebar's derived `sessionTitle`,
 * and the `chatStore` live overlay) stayed visible. The user saw "renamed",
 * the backend still held the old title, and the next refresh showed the old
 * name back with no error anywhere. That is the visible half of the
 * "renaming never sticks" report (the invisible half was the Runtime
 * re-persisting its automatic title — see the runtime regression test
 * `a_human_rename_survives_the_next_title_checkpoint`).
 *
 * The contract asserted here, mirroring `setSessionVisibility`:
 *   • every optimistically patched surface rolls back to the value the
 *     backend last reported, and
 *   • the error is rethrown, because the caller (`SessionTabBar.commitRename`)
 *     owns the user-visible message.
 *
 * The overlay case is the subtle one: the tab reads the overlay FIRST, so
 * restoring `sessions[]` alone is invisible. When the session had no overlay
 * title before the rename the rollback has to remove it again, letting the
 * list snapshot (the backend's value) win.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";

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

// Only the rename transport is exercised here; the rest of the module is
// never called by these tests.
vi.mock("../lib/session-control", () => ({
    patchSessionConfig: vi.fn(),
}));

import * as sessionControl from "../lib/session-control";

import { useAgentStore } from "./agentStore";
import { useChatStore } from "./chatStore";
import type { SessionInfo } from "../lib/types";

const AGENT = "c1f2a3b4-5d6e-4f70-8a9b-0c1d2e3f4a5b";
const SID = "20261124_104538_2cf37c";
/** The automatic title the runtime generated from the first user message. */
const SERVER_TITLE = "第一条消息";

const patchMock = vi.mocked(sessionControl.patchSessionConfig);

function makeSession(id: string, title: string | undefined): SessionInfo {
    return {
        session_id: id,
        created_at: "2026-01-01T00:00:00Z",
        last_active_at: "2026-01-01T00:00:00Z",
        message_count: 3,
        title,
        can_write: true,
    };
}

/** Seed the store as the backend last reported it. `overlay` is the live
 *  per-session title (`undefined` = the tab falls through to the snapshot). */
function seed(overlay: string | undefined) {
    useAgentStore.setState({
        agents: {
            [AGENT]: {
                meta: null,
                profile: {},
                sessions: [makeSession(SID, SERVER_TITLE)],
                // `sessions[0]` is the most recent session, so the sidebar
                // mirrors its title.
                sessionTitle: SERVER_TITLE,
                pagination: {
                    currentPage: 1,
                    totalPages: 1,
                    totalCount: 1,
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
                sessionStates: {
                    [SID]: {
                        ...(state.agentStates[AGENT]?.sessionStates[SID] ?? {}),
                        ...(overlay === undefined ? {} : { title: overlay }),
                    },
                },
                activeSessionId: SID,
                openSessionIds: [SID],
                lastLoadedSessionId: SID,
                isSessionInitLoading: false,
                preferredModel: null,
                preferredProvider: null,
                llmAvailability: "unspecified",
            },
        },
    }) as never);
}

const rowTitle = () =>
    useAgentStore.getState().agents[AGENT]?.sessions.find((s) => s.session_id === SID)?.title;
const sidebarTitle = () => useAgentStore.getState().agents[AGENT]?.sessionTitle;
const overlayTitle = () =>
    useChatStore.getState().agentStates[AGENT]?.sessionStates[SID]?.title;

beforeEach(() => {
    patchMock.mockReset();
    useChatStore.setState({ agentStates: {} } as never);
});

describe("renameSession — a refused rename rolls back", () => {
    it("restores the list row, the sidebar title and the live overlay", async () => {
        seed(SERVER_TITLE);
        patchMock.mockRejectedValue(new Error("500 config error"));

        await expect(
            useAgentStore.getState().renameSession(AGENT, SID, "人工标题"),
        ).rejects.toThrow(/500/);

        expect(rowTitle()).toBe(SERVER_TITLE);
        expect(sidebarTitle()).toBe(SERVER_TITLE);
        expect(overlayTitle()).toBe(SERVER_TITLE);
    });

    it("clears an overlay it created, so the authoritative snapshot shows again", async () => {
        // The session had no overlay title before the rename: the rollback
        // must REMOVE the optimistic one rather than leave it shadowing the
        // (correct) `sessions[]` value with the rejected title.
        seed(undefined);
        expect(overlayTitle()).toBeUndefined();
        patchMock.mockRejectedValue(new Error("403 forbidden"));

        await expect(
            useAgentStore.getState().renameSession(AGENT, SID, "人工标题"),
        ).rejects.toThrow(/403/);

        expect(overlayTitle()).toBeUndefined();
        expect(rowTitle()).toBe(SERVER_TITLE);
    });

    it("keeps the new title when the backend accepts it", async () => {
        seed(SERVER_TITLE);
        patchMock.mockResolvedValue(undefined);

        await useAgentStore.getState().renameSession(AGENT, SID, "人工标题");

        expect(patchMock).toHaveBeenCalledWith(AGENT, SID, { title: "人工标题" });
        expect(rowTitle()).toBe("人工标题");
        expect(sidebarTitle()).toBe("人工标题");
        expect(overlayTitle()).toBe("人工标题");
    });
});