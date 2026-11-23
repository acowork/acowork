/**
 * Self-check for `chatStore.clearAgentSessions`.
 *
 * Used by both the agent-stop path (`agentStore.stopAgent`) and the
 * Gateway-disconnect path (AppLayout's gateway-transition effect).
 *
 * GC contract: every object/array field on the session must be reset to
 * `null` / empty so the previous Runtime's payload — especially the
 * upload-file blobs held inside `messages[].attachment` and
 * `attachedContext` — loses its store-side reference and can be
 * collected. Half-cleaning (e.g. `messages: []` but
 * `attachedContext: [...]` still populated) would defeat the whole
 * point and leave attachment memory pinned until the next full reload.
 */
import { describe, it, expect, beforeEach } from "vitest";
import { useChatStore } from "./chatStore";

const AGENT = "agent-1";
const SESSION_A = "session-a";
const SESSION_B = "session-b";

/** Build a session state filled with non-default / non-empty values for
 *  every object/array field we care about. If `clearAgentSessions`
 *  misses any of these, the assertions below will fail. */
function seedSession(agentId: string, sessionId: string) {
    const bigBlob = new Array(1024).fill("x").join(""); // pretend attachment
    useChatStore.setState((s) => ({
        ...s,
        agentStates: {
            ...s.agentStates,
            [agentId]: {
                ...(s.agentStates[agentId] ?? {}),
                activeSessionId: sessionId,
                openSessionIds: [sessionId],
                sessionStates: {
                    ...(s.agentStates[agentId]?.sessionStates ?? {}),
                    [sessionId]: {
                        // object/array fields — must be reset
                        messages: [{ id: "m1", content: bigBlob } as never],
                        tokenUsage: { input: 1, output: 2 } as never,
                        contextUsage: { usage_percent: 50 } as never,
                        pendingApproval: { req1: { tool_call_id: "t1" } } as never,
                        pendingQuestions: [{ request_id: "q1" }] as never,
                        todos: [{ id: "todo1" }] as never,
                        treeExpandedPaths: ["/foo"] as never,
                        attachedContext: [{ type: "file", content: bigBlob }] as never,
                        toolProgress: { t1: { elapsedMs: 100, timeoutMs: 1000 } } as never,
                        // primitive fields — must be reset too
                        messageOffset: 50,
                        messageLimit: 100,
                        messageTotal: 500,
                        messagesStale: true,
                        loadError: "previous load failed",
                        sessionStatus: { status: "streaming" } as never,
                        model: "gpt-4",
                        provider: "openai",
                        ratio: 0.5,
                        reasoningEffort: "medium",
                        temperature: 0.7,
                        isCompacting: true,
                        hasMoreIncremental: true,
                        abortController: new AbortController(),
                        loadSequence: 7,
                        isReasoning: true,
                        isSessionReady: true,
                        isLoadingSession: true,
                        isLoadingMore: true,
                        serverError: { content: "boom", timestamp: Date.now() } as never,
                        lastAccessed: 12345,
                    },
                },
            },
        },
    }));
}

function getSession(agentId: string, sessionId: string) {
    return useChatStore.getState().getSessionState(agentId, sessionId);
}

describe("chatStore.clearAgentSessions", () => {
    beforeEach(() => {
        useChatStore.setState({ agentStates: {} } as never);
    });

    it("resets every object/array field so attachment refs are released", () => {
        seedSession(AGENT, SESSION_A);
        // Sanity: seed worked.
        expect(getSession(AGENT, SESSION_A).messages.length).toBe(1);
        expect(getSession(AGENT, SESSION_A).attachedContext.length).toBe(1);

        useChatStore.getState().clearAgentSessions(AGENT);

        const ss = getSession(AGENT, SESSION_A);
        // Object/array fields — must all be null/empty.
        expect(ss.messages).toEqual([]);
        expect(ss.tokenUsage).toBeNull();
        expect(ss.contextUsage).toBeNull();
        expect(ss.pendingApproval).toEqual({});
        expect(ss.pendingQuestions).toEqual([]);
        expect(ss.todos).toEqual([]);
        expect(ss.treeExpandedPaths).toEqual([]);
        expect(ss.attachedContext).toEqual([]);
        expect(ss.toolProgress).toEqual({});
        // Primitives that must also be reset.
        expect(ss.sessionStatus).toBeNull();
        expect(ss.serverError).toBeNull();
        expect(ss.loadError).toBeNull();
        expect(ss.abortController).toBeNull();
        expect(ss.model).toBeNull();
        expect(ss.provider).toBeNull();
        expect(ss.isSessionReady).toBe(false);
        expect(ss.isLoadingSession).toBe(false);
        expect(ss.isLoadingMore).toBe(false);
        expect(ss.isReasoning).toBe(false);
        expect(ss.isCompacting).toBe(false);
    });

    it("does NOT touch activeSessionId / openSessionIds (place-keeping for reconnect)", () => {
        seedSession(AGENT, SESSION_A);
        seedSession(AGENT, SESSION_B);
        useChatStore.setState((s) => ({
            ...s,
            agentStates: {
                ...s.agentStates,
                [AGENT]: {
                    ...s.agentStates[AGENT],
                    activeSessionId: SESSION_B,
                    openSessionIds: [SESSION_A, SESSION_B],
                },
            },
        }));

        useChatStore.getState().clearAgentSessions(AGENT);

        const agent = useChatStore.getState().agentStates[AGENT];
        expect(agent.activeSessionId).toBe(SESSION_B);
        expect(agent.openSessionIds).toEqual([SESSION_A, SESSION_B]);
    });

    it("clears every session under the agent, not just the active one", () => {
        seedSession(AGENT, SESSION_A);
        seedSession(AGENT, SESSION_B);
        // BOTH should have messages before, neither after.
        expect(getSession(AGENT, SESSION_A).messages.length).toBe(1);
        expect(getSession(AGENT, SESSION_B).messages.length).toBe(1);

        useChatStore.getState().clearAgentSessions(AGENT);

        expect(getSession(AGENT, SESSION_A).messages).toEqual([]);
        expect(getSession(AGENT, SESSION_B).messages).toEqual([]);
    });

    it("is a safe no-op for an agent that does not exist in chatStore", () => {
        // Should not throw, should not write anything.
        expect(() =>
            useChatStore.getState().clearAgentSessions("nonexistent-agent"),
        ).not.toThrow();
    });

    it("does not touch OTHER agents' session state", () => {
        seedSession("agent-x", SESSION_A);
        seedSession("agent-y", SESSION_A);
        useChatStore.getState().clearAgentSessions("agent-x");

        expect(getSession("agent-x", SESSION_A).messages).toEqual([]);
        expect(getSession("agent-y", SESSION_A).messages.length).toBe(1);
    });
});

// (intentionally does not import agentStore — clearAgentSessions is a
// pure chat-side helper; cross-store tests live in
// gatewayTransition.test.ts.)