/**
 * ADR-050 §16.3 — data-driven transcript realignment.
 *
 * The live event path is deliberately role-incomplete: `user` records are
 * never pushed, because the sender renders them optimistically.  These tests
 * pin the compensating rule — the authoritative `message_count` carried by
 * the retained session_state snapshot drives a tail re-fetch whenever the
 * local window believes it is current but the server knows about more
 * records.  That is what makes a remote-origin user message (typed on the
 * phone, persisted by the Runtime, never emitted as an event) visible here.
 *
 * The regression this guards is the duplicate bubble: the re-fetch must merge
 * by message id so an optimistic entry collapses into the server copy.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { useChatStore, handleMessageEvent } from "./chatStore";
import { releaseAdapterSession, useChatAdapterStore } from "../components/chat/chatAdapterStore";
import type { ChatMessage } from "../lib/types";

const AGENT = "com.test.Agent";
const SESSION = "sess-realign";

function seed(
  messages: ChatMessage[],
  cursor: { offset: number; limit: number; total: number },
) {
  useChatStore.setState((s) => ({
    ...s,
    agentStates: {
      ...s.agentStates,
      [AGENT]: {
        ...(s.agentStates[AGENT] ?? {}),
        activeSessionId: SESSION,
        sessionStates: {
          ...(s.agentStates[AGENT]?.sessionStates ?? {}),
          [SESSION]: {
            ...(s.agentStates[AGENT]?.sessionStates?.[SESSION] ?? {}),
            messages,
            messageOffset: cursor.offset,
            messageLimit: cursor.limit,
            messageTotal: cursor.total,
            messagesStale: false,
            isLoadingMore: false,
            isLoadingSession: false,
            loadSequence: 0,
            lastAccessed: Date.now(),
          },
        },
      },
    },
  }));
}

/** Dispatch a session_state snapshot carrying the authoritative count. */
function pushSessionState(messageCount: number, status = "idle") {
  handleMessageEvent(
    {
      type: "session_state",
      session_id: SESSION,
      status: { status },
      message_count: messageCount,
    },
    useChatStore.setState,
    useChatStore.getState,
    AGENT,
  );
}

/** Mock the messages endpoint so any tail fetch returns `entries`. */
function mockTail(entries: Array<Record<string, unknown>>, total: number) {
  const fetchMock = vi.fn((url: string) => {
    if (url.includes("/messages")) {
      return Promise.resolve({
        ok: true,
        status: 200,
        json: () =>
          Promise.resolve({
            messages: entries,
            offset: Math.max(0, total - entries.length),
            limit: entries.length,
            total,
          }),
      });
    }
    return Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve({}) });
  });
  vi.stubGlobal("fetch", fetchMock);
  return fetchMock;
}

const msg = (id: string, type: ChatMessage["type"], content: string, ts: number): ChatMessage => ({
  id,
  type,
  content,
  timestamp: ts,
});

describe("ADR-050 §16.3: session_state count divergence realigns the transcript", () => {
  beforeEach(() => {
    useChatStore.setState((s) => ({ ...s, agentStates: {} }));
    useChatAdapterStore.setState({ sessions: {} });
    releaseAdapterSession(AGENT, SESSION);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    useChatStore.setState((s) => ({ ...s, agentStates: {} }));
    releaseAdapterSession(AGENT, SESSION);
  });

  it("re-fetches the tail when the server count exceeds a window that believes it is current", async () => {
    // Local view: one assistant record, window covers the whole transcript.
    seed([msg("a1", "assistant", "answer", 2000)], { offset: 0, limit: 1, total: 1 });
    // Server knows about the user record that was never pushed as an event.
    const fetchMock = mockTail(
      [
        { id: "u1", ts: "2026-01-01T00:00:00.001Z", role: "user", content: "question" },
        { id: "a1", ts: "2026-01-01T00:00:00.002Z", role: "assistant", content: "answer" },
      ],
      2,
    );

    pushSessionState(2);

    await vi.waitFor(() => {
      const ss = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!;
      expect(ss.messages.some((m) => m.id === "u1")).toBe(true);
    });
    // The tail page, not an offset page: offset is unknown to the client.
    expect(fetchMock.mock.calls[0]![0]).toContain("tail=true");
    // Local count converged on the server's, so the rule self-terminates.
    const ss = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!;
    expect(ss.messageTotal).toBe(2);
  });

  it("does not fetch when there is no divergence", async () => {
    seed([msg("a1", "assistant", "answer", 2000)], { offset: 0, limit: 1, total: 1 });
    const fetchMock = mockTail([], 1);

    pushSessionState(1);
    await new Promise((r) => setTimeout(r, 10));

    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("does not fetch when the local count is transiently ahead (optimistic send not yet persisted)", async () => {
    seed([msg("u1", "user", "hi", 1000)], { offset: 0, limit: 1, total: 1 });
    const fetchMock = mockTail([], 1);

    // Server snapshot still predates the append.
    pushSessionState(0);
    await new Promise((r) => setTimeout(r, 10));

    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("leaves a reader parked mid-history alone instead of yanking them to the tail", async () => {
    // Window [0,2) of a 10-record transcript: legitimately not at tail.
    seed([msg("m0", "user", "old", 1000), msg("m1", "assistant", "old", 1001)], {
      offset: 0,
      limit: 2,
      total: 10,
    });
    const fetchMock = mockTail([], 12);

    pushSessionState(12);
    await new Promise((r) => setTimeout(r, 10));

    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("collapses the sender's optimistic entry instead of doubling it", async () => {
    // The sender rendered its own message optimistically and bumped the local
    // count; the server later reports the same record under the same id.
    const optimistic: ChatMessage = { ...msg("u1", "user", "hi", 1000), _isOptimistic: true };
    seed([optimistic], { offset: 0, limit: 1, total: 1 });
    mockTail(
      [
        { id: "u1", ts: "2026-01-01T00:00:00.001Z", role: "user", content: "hi" },
        { id: "a1", ts: "2026-01-01T00:00:00.002Z", role: "assistant", content: "answer" },
      ],
      2,
    );

    // The assistant reply creates the divergence that triggers the re-fetch.
    pushSessionState(2);

    await vi.waitFor(() => {
      const ss = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!;
      expect(ss.messages.some((m) => m.id === "a1")).toBe(true);
    });
    const ss = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!;
    // Exactly one bubble, and it is the server copy — the optimistic flag is
    // gone, which is what proves the merge deduped by id rather than
    // appending a second entry.
    expect(ss.messages.filter((m) => m.id === "u1")).toHaveLength(1);
    expect(ss.messages.find((m) => m.id === "u1")!._isOptimistic).toBeUndefined();
  });

  it("is single-flight: repeated snapshots for the same divergence fetch once", async () => {
    seed([msg("a1", "assistant", "answer", 2000)], { offset: 0, limit: 1, total: 1 });
    let resolveFetch: ((v: unknown) => void) | undefined;
    const fetchMock = vi.fn(
      () =>
        new Promise((resolve) => {
          resolveFetch = resolve;
        }),
    );
    vi.stubGlobal("fetch", fetchMock);

    // session_state is emitted several times per turn; all carry the same count.
    pushSessionState(2);
    pushSessionState(2);
    pushSessionState(2);
    await new Promise((r) => setTimeout(r, 10));

    expect(fetchMock).toHaveBeenCalledTimes(1);
    resolveFetch?.({
      ok: true,
      status: 200,
      json: () => Promise.resolve({ messages: [], offset: 0, limit: 0, total: 2 }),
    });
  });
});
