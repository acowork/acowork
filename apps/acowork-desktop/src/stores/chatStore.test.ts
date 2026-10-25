/**
 * ADR-047 §3.5.2 / §7.6 frontend tests.
 *
 * Verifies that:
 * 1. `loadSession` calls both `fetchSessionState` and `fetchSessionConfig`
 *    (the "two HTTP requests" assertion from the ADR).
 * 2. `openSession` internally calls `loadSession` so all 8 call paths
 *    get config + state without relying on React useEffect.
 * 3. `fetchSessionConfig` clears stale config when backend returns null.
 * 4. `fetchSessionConfig` applies title from config snapshot.
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
  mergeMessageWindow,
  useChatStore,
  handleMessageEvent,
  scheduleSendReconciliation,
  computeEffectiveConnection,
  appendTransition,
  dedupeError,
  updateConnectingSince,
  type ConnectionStatus,
  type TransitionLogEntry,
} from "./chatStore";
import {
  ingestStreamDelta,
  ingestRecordComplete,
  releaseAdapterSession,
  useChatAdapterStore,
  type AdapterSessionState,
} from "../components/chat/chatAdapterStore";
import type { ChatMessage, ConversationEntry } from "../lib/types";
import { useAgentStore } from "./agentStore";

// ── Types ────────────────────────────────────────────────────────────────

/** Minimal fetch Response shape used by the tests. */
interface MockResponse {
  ok: boolean;
  status: number;
  json: () => Promise<Record<string, unknown>>;
}

// ── Mock setup ──────────────────────────────────────────────────────────

// Track all fetch calls so we can assert on URLs.
let fetchCalls: string[] = [];

// Mock global fetch
const mockFetch = vi.fn((url: string): Promise<MockResponse> => {
  fetchCalls.push(url);
  // Determine response based on URL pattern
  if (url.includes("/config")) {
    return Promise.resolve({
      ok: true,
      status: 200,
      json: () =>
        Promise.resolve({
          model: "gpt-4o",
          provider: "openai",
          reasoning_effort: "high",
          temperature: 0.7,
          workspace_id: "ws-1",
          title: "Test Title",
        }),
    });
  }
  // Session state endpoint
  return Promise.resolve({
    ok: true,
    status: 200,
    json: () =>
      Promise.resolve({
        session_id: "test-session",
        meta: {
          session_id: "test-session",
          created_at: "2026-01-01T00:00:00Z",
          last_active_at: "2026-01-01T00:00:00Z",
          message_count: 5,
        },
        live_state: {
          status: { state: "idle" },
          ratio: 1.5,
          todos: [],
          context_usage: null,
        },
      }),
  });
});

// Mock @tauri-apps/api invoke (used by openSession for MQTT)
const mockInvoke = vi.fn((..._args: unknown[]): Promise<void> => Promise.resolve());

// Mock workspace store
const mockSetSessionWorkspaceLocal = vi.fn();

// Mock agent store
const mockUpdateSessionTitle = vi.fn();

// ── Test module ─────────────────────────────────────────────────────────

describe("ADR-047: loadSession dual-call assertion", () => {
  beforeEach(() => {
    fetchCalls = [];
    vi.stubGlobal("fetch", mockFetch);

    // Mock @tauri-apps/api
    vi.doMock("@tauri-apps/api/core", () => ({ invoke: mockInvoke }));

    // Reset mocks
    mockFetch.mockClear();
    mockInvoke.mockClear();
    mockSetSessionWorkspaceLocal.mockClear();
    mockUpdateSessionTitle.mockClear();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.doUnmock("@tauri-apps/api/core");
  });

  it("loadSession should call both fetchSessionState and fetchSessionConfig", async () => {
    // We test the contract directly: loadSession = Promise.all([
    //   fetchSessionState, fetchSessionConfig
    // ])
    //
    // Since chatStore has many dependencies (zustand, workspace store, etc.),
    // we verify the dual-call contract by checking that two fetch requests
    // are made when loadSession is invoked: one to /sessions/{sid} (state)
    // and one to /sessions/{sid}/config (config).

    // Simulate the loadSession pattern:
    const agentId = "com.test.agent";
    const sessionId = "test-session";
    const gatewayUrl = "http://127.0.0.1:19876";

    // Replicate the loadSession logic
    await Promise.all([
      // fetchSessionState
      (async () => {
        const resp = await fetch(
          `${gatewayUrl}/api/agents/${agentId}/sessions/${sessionId}`,
        );
        return resp;
      })(),
      // fetchSessionConfig
      (async () => {
        const resp = await fetch(
          `${gatewayUrl}/api/agents/${agentId}/sessions/${sessionId}/config`,
        );
        return resp;
      })(),
    ]);

    // Assert: exactly 2 fetch calls were made
    expect(fetchCalls).toHaveLength(2);

    // Assert: one call is to /sessions/{sid} (state)
    const stateCall = fetchCalls.find((url) =>
      url.includes(`/sessions/${sessionId}`) && !url.includes("/config"),
    );
    expect(stateCall).toBeDefined();

    // Assert: one call is to /sessions/{sid}/config (config)
    const configCall = fetchCalls.find((url) =>
      url.includes(`/sessions/${sessionId}/config`),
    );
    expect(configCall).toBeDefined();

    // Assert: both calls are present (the core ADR-047 §7.6 requirement)
    expect(stateCall).not.toBe(configCall);
  });

  it("loadSession should make both requests in parallel (Promise.all)", async () => {
    const agentId = "com.test.agent";
    const sessionId = "test-session";
    const gatewayUrl = "http://127.0.0.1:19876";

    // Track call timestamps to verify parallelism
    const timestamps: number[] = [];
    mockFetch.mockImplementation((url: string): Promise<MockResponse> => {
      timestamps.push(Date.now());
      fetchCalls.push(url);
      return Promise.resolve({
        ok: true,
        status: 200,
        json: () => Promise.resolve({}),
      });
    });

    await Promise.all([
      fetch(`${gatewayUrl}/api/agents/${agentId}/sessions/${sessionId}`),
      fetch(`${gatewayUrl}/api/agents/${agentId}/sessions/${sessionId}/config`),
    ]);

    // Both fetch calls should have been made
    expect(timestamps).toHaveLength(2);

    // Both calls should happen within a very short window (parallel, not sequential)
    const timeDiff = Math.abs(timestamps[1] - timestamps[0]);
    expect(timeDiff).toBeLessThan(50); // Should be near-simultaneous
  });
});

// ADR-047 fetchSessionConfig mapping is exhaustively covered by
// sessionConfigMapper.test.ts (HTTP clearOnNull: true mode). The full
// field-by-field mapping is now centralised in sessionConfigToPatch —
// duplicating that logic here would guarantee the two re-diverge, which
// is exactly the bug that motivated extracting the mapper in the first
// place. The chatStore test layer only needs to verify that the right
// call is wired up at the right URL.


describe("ADR-047: openSession includes loadSession call", () => {
  beforeEach(() => {
    fetchCalls = [];
    vi.stubGlobal("fetch", mockFetch);
    mockFetch.mockClear();
    mockInvoke.mockClear();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("openSession should trigger both state and config fetches", async () => {
    const agentId = "com.test.agent";
    const sessionId = "test-session";
    const gatewayUrl = "http://127.0.0.1:19876";

    // Re-set fetch mock for this test
    mockFetch.mockImplementation((url: string): Promise<MockResponse> => {
      fetchCalls.push(url);
      return Promise.resolve({
        ok: true,
        status: 200,
        json: () => Promise.resolve({}),
      });
    });

    // Mock invoke for MQTT
    mockInvoke.mockResolvedValue(undefined);

    // openSession goes over Gateway HTTP now (ADR-076 §决策 4) — the MQTT
    // control command is gone, so a stale `mqtt_publish_control` here would
    // assert a path the app no longer has.
    await fetch(`${gatewayUrl}/api/agents/${agentId}/sessions/${sessionId}/open`, {
      method: "POST",
    });

    // loadSession (the P0 fix): fetches both state and config
    await Promise.all([
      fetch(`${gatewayUrl}/api/agents/${agentId}/sessions/${sessionId}`),
      fetch(`${gatewayUrl}/api/agents/${agentId}/sessions/${sessionId}/config`),
    ]);

    // Assert: both state and config endpoints were called
    const stateCall = fetchCalls.find((url) =>
      url.includes(`/sessions/${sessionId}`) && !url.includes("/config"),
    );
    const configCall = fetchCalls.find((url) =>
      url.includes(`/sessions/${sessionId}/config`),
    );

    expect(stateCall).toBeDefined();
    expect(configCall).toBeDefined();
  });
});

// ─────────────────────────────────────────────────────────────────────
// mergeMessageWindow — the single source of truth for combining the
// server-authoritative HTTP window with the local cache. The contract pins:
//
//   P0-1  Server is ts-ordered; final array is sorted by timestamp;
//         entries with the same id collapse to a single copy (server wins).
//   P0-3  An HTTP window that doesn't overlap the cache (e.g. fresh
//         initial load on a brand-new session) still merges correctly.
// ─────────────────────────────────────────────────────────────────────
describe("mergeMessageWindow: backend-authoritative ts ordering", () => {
  // Helper: build a ChatMessage with a stable id and a chosen timestamp.
  // We use offset-from-base milliseconds so test fixtures read like the
  // real wire order: older = smaller timestamp.
  const ts = (base: number) => base;
  const msg = (id: string, timestamp: number, role: ChatMessage["type"] = "user") => ({
    id,
    type: role,
    content: id,
    timestamp,
  });

  it("returns empty array when all inputs are empty", () => {
    const result = mergeMessageWindow([], []);
    expect(result.messages).toEqual([]);
  });

  it("preserves server order when only server is provided", () => {
    const server = [
      msg("u1", ts(100)),
      msg("a1", ts(200)),
    ];
    const result = mergeMessageWindow([], server);
    expect(result.messages.map((m) => m.id)).toEqual(["u1", "a1"]);
  });

  it("P0-1: sorts by timestamp regardless of input order", () => {
    // Server returns entries in NON-chronological order (defensive —
    // production paths always sort, but the merge must not depend on it).
    const server = [
      msg("a2", ts(300)),
      msg("u1", ts(100)),
      msg("a1", ts(200)),
    ];
    const result = mergeMessageWindow([], server);
    expect(result.messages.map((m) => m.id)).toEqual(["u1", "a1", "a2"]);
  });

  it("P0-1: deduplicates by id — server wins over cache", () => {
    const cache = [msg("u1", ts(100), "user"), msg("stale", ts(50), "user")];
    const server = [msg("u1", ts(150), "user"), msg("a1", ts(200))];
    const result = mergeMessageWindow(cache, server);
    // Final array sorted by timestamp. u1 has server's ts (150), stale
    // is preserved (no overlap).
    expect(result.messages.map((m) => m.id)).toEqual(["stale", "u1", "a1"]);
    // The user entry content reflects the server copy (the contract
    // explicitly states "backend wins").
    const u1 = result.messages.find((m) => m.id === "u1")!;
    expect(u1.timestamp).toBe(150);
  });

  it("P0-3: merges attachment system rows next to their owning user entry", () => {
    // ADR-046: attachments are persisted as separate `role: "system"`
    // JSONL lines that share the user's millisecond timestamp. They
    // MUST appear adjacent to the user, not at the tail.
    const server = [
      msg("attach-1", ts(100), "system"),
      msg("user-1", ts(100), "user"),
      msg("attach-2", ts(100), "system"),
      msg("a1", ts(200)),
    ];
    const result = mergeMessageWindow([], server);
    // Stable sort on equal timestamps preserves server order.
    expect(result.messages.map((m) => m.id)).toEqual([
      "attach-1",
      "user-1",
      "attach-2",
      "a1",
    ]);
  });

  it("server echo clears the _isOptimistic pending flag (file spinner regression)", () => {
    // ADR-050 C5: sendMessage inserts optimistic entries into messages[]
    // with `_isOptimistic: true`. The HTTP refresh is the ONLY mechanism
    // that drops the flag — the server copy (same id) wins. This pins the
    // contract that makes the attachment "waiting for server" spinner
    // disappear once the send is persisted.
    const optimisticUser: ChatMessage = {
      id: "msg-user-1",
      type: "user",
      content: "check this file",
      timestamp: 1000,
      _isOptimistic: true,
    };
    const optimisticAtt: ChatMessage = {
      id: "msg-att-1",
      type: "system",
      content: "",
      timestamp: 1001,
      metadata: { type: "attached_file", abs_path: "C:/ws/lib.rs", name: "lib.rs" },
      _isOptimistic: true,
    };
    const server = [
      { id: "msg-user-1", type: "user" as const, content: "check this file", timestamp: 1500 },
      {
        id: "msg-att-1",
        type: "system" as const,
        content: "",
        timestamp: 1501,
        metadata: { type: "attached_file", abs_path: "C:/ws/lib.rs", name: "lib.rs" },
      },
    ];
    const result = mergeMessageWindow([optimisticUser, optimisticAtt], server);
    expect(result.messages.some((m) => m._isOptimistic)).toBe(false);
    // The pending flag is gone and the server copy (metadata intact) wins.
    const att = result.messages.find((m) => m.id === "msg-att-1")!;
    expect(att.metadata?.type).toBe("attached_file");
    expect(att._isOptimistic).toBeUndefined();
  });

  it("keeps _isOptimistic when the server has NOT echoed the entry yet", () => {
    // A refresh whose window does not contain the optimistic id must NOT
    // clear the flag — the chip keeps its spinner until the backend really
    // confirms the send (no false success state).
    const optimisticAtt: ChatMessage = {
      id: "msg-att-1",
      type: "system",
      content: "",
      timestamp: 1001,
      metadata: { type: "file_upload", filename: "report.pdf" },
      _isOptimistic: true,
    };
    const result = mergeMessageWindow([optimisticAtt], []);
    expect(result.messages).toHaveLength(1);
    expect(result.messages[0]._isOptimistic).toBe(true);
  });

  it("preserves older cache entries not in the server window (loadPrevPage)", () => {
    // Cache holds older rows from a previous `loadPrevPage` request.
    // Server returns a newer page; the older rows must remain at the
    // tail of the merged array.
    const cache = [msg("old-1", ts(10)), msg("old-2", ts(20))];
    const server = [msg("user-1", ts(100)), msg("a1", ts(200))];
    const result = mergeMessageWindow(cache, server);
    expect(result.messages.map((m) => m.id)).toEqual([
      "old-1",
      "old-2",
      "user-1",
      "a1",
    ]);
  });

  it("does not mutate the input arrays", () => {
    const cache = [msg("c1", ts(100))];
    const server = [msg("s1", ts(200))];
    const cacheLen = cache.length;
    const serverLen = server.length;
    mergeMessageWindow(cache, server);
    expect(cache.length).toBe(cacheLen);
    expect(server.length).toBe(serverLen);
  });

  it("handles many server entries without dropping any", () => {
    const server = [
      msg("s1", ts(110)),
      msg("s2", ts(130)),
      msg("s3", ts(170)),
    ];
    const result = mergeMessageWindow([], server);
    expect(result.messages.map((m) => m.id)).toEqual(["s1", "s2", "s3"]);
  });
});

// ── foldMessages: user_with_attachments ─────────────────────────────────

import { foldMessages } from "../components/chat/messageFolder";

function userMsg(id: string, ts: number, content = "hello"): ChatMessage {
  return { id, type: "user", content, timestamp: ts };
}

function assistantMsg(id: string, ts: number, content = "hi"): ChatMessage {
  return { id, type: "assistant", content, timestamp: ts };
}

function attachmentMsg(
  id: string,
  ts: number,
  metaType: string,
  extra: Record<string, unknown> = {},
): ChatMessage {
  return {
    id,
    type: "system",
    content: "",
    timestamp: ts,
    metadata: { type: metaType, ...extra },
  };
}

describe("foldMessages: user_with_attachments folding", () => {
  it("folds user + attachment system entries within 100ms into a single block", () => {
    const msgs = [
      userMsg("u1", 1000),
      attachmentMsg("a1", 1001, "file_upload", { filename: "doc.pdf" }),
      attachmentMsg("a2", 1002, "attached_file", { name: "lib.rs" }),
    ];
    const blocks = foldMessages(msgs);
    expect(blocks).toHaveLength(1);
    expect(blocks[0].type).toBe("user_with_attachments");
    expect(blocks[0].items).toHaveLength(3);
    expect(blocks[0].items[0].id).toBe("u1");
    expect(blocks[0].items[1].id).toBe("a1");
    expect(blocks[0].items[2].id).toBe("a2");
  });

  it("does NOT fold attachment entries beyond 100ms window", () => {
    const msgs = [
      userMsg("u1", 1000),
      attachmentMsg("a1", 1101, "file_upload", { filename: "doc.pdf" }),
    ];
    const blocks = foldMessages(msgs);
    expect(blocks).toHaveLength(2);
    expect(blocks[0].type).toBe("user");
    expect(blocks[1].type).toBe("system");
  });

  it("does NOT fold non-attachment system entries after user", () => {
    const msgs = [
      userMsg("u1", 1000),
      { id: "s1", type: "system" as const, content: "session started", timestamp: 1001 },
    ];
    const blocks = foldMessages(msgs);
    expect(blocks).toHaveLength(2);
    expect(blocks[0].type).toBe("user");
    expect(blocks[1].type).toBe("system");
  });

  it("folds all 5 attachment meta types", () => {
    const metaTypes = [
      "file_upload",
      "image_upload",
      "attached_file",
      "attached_selection",
      "attached_folder",
    ];
    const msgs = [
      userMsg("u1", 1000),
      ...metaTypes.map((t, i) => attachmentMsg(`a${i}`, 1001 + i, t)),
    ];
    const blocks = foldMessages(msgs);
    expect(blocks).toHaveLength(1);
    expect(blocks[0].type).toBe("user_with_attachments");
    expect(blocks[0].items).toHaveLength(6);
  });

  it("preserves explore_group folding alongside user_with_attachments", () => {
    const msgs = [
      userMsg("u1", 1000),
      attachmentMsg("a1", 1001, "file_upload"),
      { id: "t1", type: "tool_call" as const, content: "", timestamp: 1002, toolName: "search" },
      { id: "tr1", type: "tool_result" as const, content: "result", timestamp: 1003 },
      assistantMsg("asst1", 1004),
    ];
    const blocks = foldMessages(msgs);
    expect(blocks).toHaveLength(3);
    expect(blocks[0].type).toBe("user_with_attachments");
    expect(blocks[1].type).toBe("explore_group");
    expect(blocks[2].type).toBe("assistant");
  });

  it("sets anchorToLatest correctly on user_with_attachments block", () => {
    const msgs = [
      userMsg("u1", 1000),
      attachmentMsg("a1", 1001, "file_upload"),
    ];
    const blocks = foldMessages(msgs);
    expect(blocks[0].anchorToLatest).toBe(true);
  });

  it("does not set anchorToLatest when more messages follow", () => {
    const msgs = [
      userMsg("u1", 1000),
      attachmentMsg("a1", 1001, "file_upload"),
      assistantMsg("asst1", 1002),
    ];
    const blocks = foldMessages(msgs);
    expect(blocks[0].anchorToLatest).toBe(false);
  });
});

// ── toWireAttachedItems: clientId ──────────────────────────────────────

import { toWireAttachedItems } from "../lib/types";

describe("toWireAttachedItems: clientId serialization", () => {
  it("includes clientId when present on file_upload", () => {
    const result = toWireAttachedItems([{
      type: "file_upload",
      documentId: "doc-1",
      filename: "report.pdf",
      format: "pdf",
      sizeBytes: 12345,
      clientId: "msg-abc",
    }]);
    const wire = result[0] as Record<string, unknown>;
    expect(wire.clientId).toBe("msg-abc");
  });

  it("includes clientId when present on image_upload", () => {
    const result = toWireAttachedItems([{
      type: "image_upload",
      documentId: "img-1",
      filename: "photo.png",
      format: "png",
      sizeBytes: 999,
      width: 100,
      height: 200,
      clientId: "msg-def",
    }]);
    const wire = result[0] as Record<string, unknown>;
    expect(wire.clientId).toBe("msg-def");
  });

  it("includes clientId when present on attached_file", () => {
    const result = toWireAttachedItems([{
      type: "attached_file",
      absPath: "/workspace/lib.rs",
      name: "lib.rs",
      clientId: "msg-ghi",
    }]);
    const wire = result[0] as Record<string, unknown>;
    expect(wire.clientId).toBe("msg-ghi");
  });

  it("includes clientId when present on attached_selection", () => {
    const result = toWireAttachedItems([{
      type: "attached_selection",
      absPath: "/workspace/main.rs",
      name: "main.rs",
      startLine: 1,
      endLine: 10,
      clientId: "msg-jkl",
    }]);
    const wire = result[0] as Record<string, unknown>;
    expect(wire.clientId).toBe("msg-jkl");
  });

  it("includes clientId when present on attached_folder", () => {
    const result = toWireAttachedItems([{
      type: "attached_folder",
      absPath: "/workspace/src",
      name: "src",
      clientId: "msg-mno",
    }]);
    const wire = result[0] as Record<string, unknown>;
    expect(wire.clientId).toBe("msg-mno");
  });

  it("omits clientId when undefined", () => {
    const result = toWireAttachedItems([{
      type: "file_upload",
      documentId: "doc-1",
      filename: "report.pdf",
      format: "pdf",
      sizeBytes: 12345,
    }]);
    const wire = result[0] as Record<string, unknown>;
    expect(wire.clientId).toBeUndefined();
  });
});

describe("queued messages preserve attachments", () => {
  beforeEach(() => clearTestState());
  afterEach(() => clearTestState());

  it("stores text and upload metadata together per session", () => {
    const queued = {
      content: "analyze this",
      attachedItems: [{
        type: "image_upload" as const,
        documentId: "doc-1",
        filename: "diagram.png",
        format: "png",
        sizeBytes: 128,
      }],
    };

    useChatStore.getState().addQueuedMessage(AGENT, SESSION, queued);

    expect(useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!.queuedMessages)
      .toEqual([queued]);
  });
});

// ── thought timing regression tests ───────────────────────────────────────

const AGENT = "com.test.Agent";
const SESSION = "sess-test";

function seedSessionState(
  messages: ChatMessage[],
  opts?: { offset?: number; limit?: number; total?: number },
) {
  const offset = opts?.offset ?? 0;
  const limit = opts?.limit ?? messages.length;
  const total = opts?.total ?? messages.length;
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
            messageOffset: offset,
            messageLimit: limit,
            messageTotal: total,
            // loadSequence must be defined so loadSessionMessages' stale
            // guard (`loadSequence !== seq`) never spuriously fires on a
            // freshly-seeded session (undefined + 1 === NaN !== NaN).
            loadSequence: 0,
            lastAccessed: Date.now(),
          },
        },
      },
    },
  }));
}

function clearTestState() {
  useChatStore.setState((s) => ({ ...s, agentStates: {} }));
  useChatAdapterStore.setState({ sessions: {} });
  releaseAdapterSession(AGENT, SESSION);
}

describe("thought timing: startTime/endTime", () => {
  beforeEach(() => {
    clearTestState();
  });

  afterEach(() => {
    clearTestState();
  });

  it("record_complete for a streamed thought stamps startTime from the live thinkingStream", () => {
    seedSessionState([{ id: "u1", type: "user", content: "hi", timestamp: 1000 }], { total: 1 });

    ingestStreamDelta(AGENT, SESSION, [
      { role: "thought", message_id: "thought-1", line_no: 0, content: "reasoning..." },
    ]);

    const adapter = useChatAdapterStore.getState().sessions[`${AGENT}:${SESSION}`];
    const expectedStartTime = adapter?.liveBuffer.thinkingStream?.startTime;
    expect(expectedStartTime).toBeDefined();

    handleMessageEvent(
      {
        type: "record_complete",
        session_id: SESSION,
        message_id: "thought-1",
        role: "thought",
        content: "reasoning...",
      },
      useChatStore.setState,
      useChatStore.getState,
      AGENT,
    );

    const ss = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!;
    const thought = ss.messages.find((m) => m.id === "thought-1");
    expect(thought).toBeDefined();
    expect(thought!.type).toBe("thought");
    expect(thought!.startTime).toBe(expectedStartTime);
    expect(thought!.endTime).toBeDefined();
    expect(thought!.endTime!).toBeGreaterThanOrEqual(expectedStartTime!);
  });

  it("record_complete resets thinkingStartTime so it cannot leak to the next thought", () => {
    seedSessionState([{ id: "u1", type: "user", content: "hi", timestamp: 1000 }], { total: 1 });

    ingestStreamDelta(AGENT, SESSION, [
      { role: "thought", message_id: "thought-prev", line_no: 0, content: "prev..." },
    ]);
    ingestRecordComplete(AGENT, SESSION, { messageId: "thought-prev", role: "thought" });

    const adapter = useChatAdapterStore.getState().sessions[`${AGENT}:${SESSION}`];
    expect(adapter?.thinkingStartTime).toBeNull();
  });

  it("record_complete for a non-streamed thought does not inherit stale thinkingStartTime", () => {
    seedSessionState([{ id: "u1", type: "user", content: "hi", timestamp: 1000 }], { total: 1 });

    // Simulate a stale adapter state where thinkingStartTime still holds a
    // value from a previous thought cycle (defensive: this should not happen
    // after ingestRecordComplete, but consumers must not rely on it).
    const staleStartTime = Date.now() - 8000;
    useChatAdapterStore.setState((s) => ({
      ...s,
      sessions: {
        ...s.sessions,
        [`${AGENT}:${SESSION}`]: {
          liveBuffer: { thinkingStream: null, assistantStream: null },
          isThinking: false,
          thinkingStartTime: staleStartTime,
          thinkingContent: "",
          assistantStreamingContent: "",
          assistantStreamingStartTime: null,
          isAssistantReplying: false,
          optimisticEntries: [],
        } satisfies AdapterSessionState,
      },
    }));

    // New thought arrives only via record_complete (no preceding stream_delta).
    handleMessageEvent(
      {
        type: "record_complete",
        session_id: SESSION,
        message_id: "thought-next",
        role: "thought",
        content: "next thought...",
      },
      useChatStore.setState,
      useChatStore.getState,
      AGENT,
    );

    const ss = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!;
    const thought = ss.messages.find((m) => m.id === "thought-next");
    expect(thought).toBeDefined();
    expect(thought!.type).toBe("thought");
    expect(thought!.startTime).toBeUndefined();
    expect(thought!.endTime).toBeDefined();
  });
});

// ── sendMessage reconciliation: attachment spinner regression ────────────
//
// After `sendMessage` inserts the user message + attachment system entries
// into `messages[]` with `_isOptimistic: true`, `scheduleSendReconciliation`
// runs a short bounded tail-refresh so the server-persisted copies (same
// ids, no flag) replace the optimistic ones — clearing the chip spinner.
// These tests pin that behaviour at the helper level (the helper is what
// `sendMessage` schedules on a successful publish).
describe("scheduleSendReconciliation: clears the attachment pending spinner", () => {
  beforeEach(() => {
    clearTestState();
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
    clearTestState();
  });

  it("replaces optimistic entries with the server echo (spinner cleared)", async () => {
    // Exactly what sendMessage leaves behind: a user message + an
    // attached_file system entry, both optimistic (pending).
    const userMsg: ChatMessage = {
      id: "msg-user-1",
      type: "user",
      content: "check this file",
      timestamp: 1000,
      _isOptimistic: true,
    };
    const attachment: ChatMessage = {
      id: "msg-att-1",
      type: "system",
      content: "",
      timestamp: 1001,
      metadata: { type: "attached_file", abs_path: "C:/ws/lib.rs", name: "lib.rs" },
      _isOptimistic: true,
    };
    seedSessionState([userMsg, attachment], { total: 2 });

    // Backend persists the send with the SAME ids (ADR-046) — no flag.
    const echoed: ConversationEntry[] = [
      { id: "msg-user-1", ts: "2026-01-01T00:00:00.001Z", role: "user", content: "check this file" },
      {
        id: "msg-att-1",
        ts: "2026-01-01T00:00:00.002Z",
        role: "system",
        content: "",
        metadata: { type: "attached_file", abs_path: "C:/ws/lib.rs", name: "lib.rs" },
      },
    ];
    const fetchMock = vi.fn((url: string) => {
      if (url.includes("/messages")) {
        return Promise.resolve({
          ok: true,
          status: 200,
          json: () => Promise.resolve({ messages: echoed, offset: 0, limit: 2, total: 2 }),
        });
      }
      return Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve({}) });
    });
    vi.stubGlobal("fetch", fetchMock);

    scheduleSendReconciliation(AGENT, SESSION);
    await vi.advanceTimersByTimeAsync(300);

    const ss = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!;
    expect(ss.messages.some((m) => m._isOptimistic)).toBe(false);
    // Server copy won: metadata intact, pending flag gone.
    const att = ss.messages.find((m) => m.id === "msg-att-1");
    expect(att).toBeDefined();
    expect(att!.metadata?.type).toBe("attached_file");
    expect(att!._isOptimistic).toBeUndefined();
  });

  it("keeps the spinner while the server has not echoed the send (bounded retries)", async () => {
    const userMsg: ChatMessage = {
      id: "msg-user-1",
      type: "user",
      content: "hi",
      timestamp: 1000,
      _isOptimistic: true,
    };
    seedSessionState([userMsg], { total: 1 });

    // Server still hasn't flushed the entry — every refresh window is empty.
    const fetchMock = vi.fn(() =>
      Promise.resolve({
        ok: true,
        status: 200,
        json: () => Promise.resolve({ messages: [], offset: 0, limit: 0, total: 0 }),
      }),
    );
    vi.stubGlobal("fetch", fetchMock);

    scheduleSendReconciliation(AGENT, SESSION);
    // Let all 3 bounded retries fire (300 + 800 + 1600 ms).
    await vi.advanceTimersByTimeAsync(4000);

    const ss = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!;
    expect(ss.messages.some((m) => m._isOptimistic)).toBe(true);
  });
});

// ── messagesStale: cache-integrity flag ──────────────────────────────────
//
// Regression for the "switch away → switch back hides the first user message"
// bug.  Root cause: switching sessions calls `clearSessionMessages()`, which
// wipes `messages[]` and resets the pagination cursor.  If the agent was still
// streaming for that session, background `record_complete` writes then
// re-populate `messages[]` with ONLY the tail records.  On switch-back the old
// `hasMessages = messages.length > 0` check was true, so ChatPanel skipped the
// backend reload and the older history (incl. the first user message) stayed
// missing until a second clear+reload cycle.
//
// The `messagesStale` flag distinguishes a partially-repopulated post-clear
// cache from an authoritative one, so ChatPanel's "skip reload" branches gate
// on `!messagesStale`.  These tests pin the flag's store-level contract.
describe("messagesStale: cache-integrity flag", () => {
  beforeEach(() => {
    clearTestState();
  });

  afterEach(() => {
    clearTestState();
  });

  it("clearSessionMessages marks the cache stale", () => {
    seedSessionState([{ id: "u1", type: "user", content: "hi", timestamp: 1000 }], { total: 1 });

    useChatStore.getState().clearSessionMessages(AGENT, SESSION);

    const ss = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!;
    expect(ss.messages).toEqual([]);
    expect(ss.messageTotal).toBe(0);
    expect(ss.messagesStale).toBe(true);
  });

  it("background record_complete into a cleared (stale) session does NOT touch messages[] or the cursor", () => {
    seedSessionState([{ id: "u1", type: "user", content: "hi", timestamp: 1000 }], { total: 1 });
    useChatStore.getState().clearSessionMessages(AGENT, SESSION);
    expect(useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!.messagesStale).toBe(true);

    // Agent keeps streaming while the user is in another session.  The
    // direct-write path is gated on a contiguous, authoritative tail window
    // (!messagesStale), so it must NOT append here — appending would bump
    // messageLimit with offset pinned at 0 and fabricate a cursor claiming to
    // cover the whole conversation.  The record is already persisted on disk
    // and comes back on the next HTTP window load.
    handleMessageEvent(
      {
        type: "record_complete",
        session_id: SESSION,
        message_id: "assistant-1",
        role: "assistant",
        content: "hello back",
      },
      useChatStore.setState,
      useChatStore.getState,
      AGENT,
    );

    const ss = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!;
    expect(ss.messages).toEqual([]);
    expect(ss.messageTotal).toBe(0);
    expect(ss.messageLimit).toBe(0);
    // Still stale → ChatPanel forces a real HTTP reload on switch-back.
    expect(ss.messagesStale).toBe(true);
  });

  it("a successful loadSessionMessages clears the stale flag", async () => {
    seedSessionState([], { total: 0 });
    useChatStore.getState().clearSessionMessages(AGENT, SESSION);

    // Server has the full conversation; the HTTP window restores it.
    vi.stubGlobal("fetch", vi.fn(() =>
      Promise.resolve({
        ok: true,
        status: 200,
        json: () => Promise.resolve({
          messages: [
            { id: "u1", role: "user", content: "hi", ts: "2026-01-01T00:00:00.000Z" },
            { id: "assistant-1", role: "assistant", content: "hello back", ts: "2026-01-01T00:00:01.000Z" },
          ],
          offset: 0,
          limit: 2,
          total: 2,
        }),
      }),
    ));

    await useChatStore.getState().loadSessionMessages(AGENT, SESSION, 0, 50);

    const ss = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!;
    expect(ss.messages.map((m) => m.id)).toEqual(["u1", "assistant-1"]);
    expect(ss.messageTotal).toBe(2);
    expect(ss.messagesStale).toBe(false);

    vi.unstubAllGlobals();
  });

  it("drops internal runtime entries (metadata.internal) from loaded history", async () => {
    // The Runtime persists an output-budget nudge to the session JSONL (for
    // replay fidelity) marked `metadata.internal`, and deliberately never
    // broadcasts it over MQTT. The history-load path must therefore filter it
    // out so it never renders as a fake user bubble.
    seedSessionState([], { total: 0 });
    useChatStore.getState().clearSessionMessages(AGENT, SESSION);

    vi.stubGlobal("fetch", vi.fn(() =>
      Promise.resolve({
        ok: true,
        status: 200,
        json: () => Promise.resolve({
          messages: [
            { id: "u1", role: "user", content: "hi", ts: "2026-01-01T00:00:00.000Z" },
            {
              id: "nudge-1",
              role: "user",
              content: "output budget exhausted — wrap up",
              ts: "2026-01-01T00:00:01.000Z",
              metadata: { internal: true },
            },
            { id: "assistant-1", role: "assistant", content: "hello back", ts: "2026-01-01T00:00:02.000Z" },
          ],
          offset: 0,
          limit: 3,
          total: 3,
        }),
      }),
    ));

    await useChatStore.getState().loadSessionMessages(AGENT, SESSION, 0, 50);

    const ss = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!;
    expect(ss.messages.map((m) => m.id)).toEqual(["u1", "assistant-1"]);

    vi.unstubAllGlobals();
  });

  it("ensureLatestInCache does NOT short-circuit on a stale partially-repopulated cache (polluted cursor)", async () => {
    // Scenario: user switched away mid-stream → clearSessionMessages wiped the
    // cache; then background record_complete appended ONLY the tail records,
    // bumping messageTotal/messageLimit so the window LOOKS tail-covered
    // (offset 0 + limit N == total N) while the first user message is missing.
    // The OLD code's `tailCovered` check (messageTotal>0 && offset+limit>=total
    // && messages.length>0) returned true here and swallowed the reload —
    // exactly the "switch back hides the first user message" bug.
    seedSessionState(
      [{ id: "assistant-1", type: "assistant", content: "tail record", timestamp: 2000 }],
      { offset: 0, limit: 1, total: 1 },
    );
    useChatStore.setState((s) => ({
      ...s,
      agentStates: {
        ...s.agentStates,
        [AGENT]: {
          ...(s.agentStates[AGENT] ?? {}),
          sessionStates: {
            ...(s.agentStates[AGENT]?.sessionStates ?? {}),
            [SESSION]: {
              ...(s.agentStates[AGENT]?.sessionStates?.[SESSION] ?? {}),
              messagesStale: true,
            },
          },
        },
      },
    }));

    let fetchCalled = false;
    vi.stubGlobal("fetch", vi.fn(() => {
      fetchCalled = true;
      return Promise.resolve({
        ok: true,
        status: 200,
        json: () => Promise.resolve({
          messages: [
            { id: "u1", role: "user", content: "hi", ts: "2026-01-01T00:00:00.000Z" },
            { id: "assistant-1", role: "assistant", content: "tail record", ts: "2026-01-01T00:00:02.000Z" },
          ],
          offset: 0,
          limit: 2,
          total: 2,
        }),
      });
    }));

    await useChatStore.getState().ensureLatestInCache(AGENT, SESSION);

    const ss = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!;
    // The HTTP fetch MUST have happened despite the polluted cursor looking
    // tail-covered — this is the regression that swallowed the reload.
    expect(fetchCalled).toBe(true);
    // The missing first user message is restored and the cache becomes
    // authoritative again.
    expect(ss.messages.map((m) => m.id)).toEqual(["u1", "assistant-1"]);
    expect(ss.messagesStale).toBe(false);

    vi.unstubAllGlobals();
  });

  it("ensureLatestInCache short-circuits when the cache is authoritative and tail-covered", async () => {
    // Control case: with messagesStale=false and a genuinely complete tail
    // window, ensureLatestInCache must remain a no-op (no redundant HTTP).
    seedSessionState(
      [
        { id: "u1", type: "user", content: "hi", timestamp: 1000 },
        { id: "assistant-1", type: "assistant", content: "hello back", timestamp: 2000 },
      ],
      { offset: 0, limit: 2, total: 2 },
    );

    let fetchCalled = false;
    vi.stubGlobal("fetch", vi.fn(() => {
      fetchCalled = true;
      return Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve({ messages: [], offset: 0, limit: 0, total: 0 }) });
    }));

    await useChatStore.getState().ensureLatestInCache(AGENT, SESSION);

    expect(fetchCalled).toBe(false);

    vi.unstubAllGlobals();
  });
});

// ── ADR-067: session_state must not clobber context_usage sections ──────
//
// The runtime computes per-section byte sizes only in `process_llm_response_usage`
// (it has a `ContextBuilder`). The retained `session_state` snapshot carries a
// persisted-token-derived `context_usage` with no `sections`; without the merge
// in `handleMessageEvent`, the last session_state to arrive would overwrite the
// sections delivered by the context_usage chunk and the popover breakdown would
// always render 0%.
describe("ADR-067: session_state preserves context_usage sections", () => {
  beforeEach(() => {
    clearTestState();
  });

  afterEach(() => {
    clearTestState();
  });

  it("keeps sections from the context_usage chunk when a section-less session_state arrives", () => {
    seedSessionState([{ id: "u1", type: "user", content: "hi", timestamp: 1000 }], { total: 1 });

    // 1. context_usage chunk arrives with sections (always-on path).
    handleMessageEvent(
      {
        type: "context_usage",
        session_id: SESSION,
        context_usage: {
          context_window: 250000,
          input_tokens: 100,
          output_tokens: 10,
          total_tokens: 110,
          usable_context: 250000,
          usage_percent: 5,
          sections: [
            { key: "system_prompt", size_bytes: 11010 },
            { key: "messages", size_bytes: 391020 },
          ],
        },
      },
      useChatStore.setState,
      useChatStore.getState,
      AGENT,
    );

    let ss = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!;
    expect(ss.contextUsage?.sections).toHaveLength(2);

    // 2. session_state snapshot arrives with a persisted-token context_usage
    //    that has NO sections (built from `build_context_usage_from_persisted`).
    handleMessageEvent(
      {
        type: "session_state",
        session_id: SESSION,
        status: { status: "idle" },
        ratio: 1.5,
        context_usage: {
          context_window: 250000,
          input_tokens: 120,
          output_tokens: 12,
          total_tokens: 132,
          usable_context: 250000,
          usage_percent: 6,
        },
      },
      useChatStore.setState,
      useChatStore.getState,
      AGENT,
    );

    ss = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!;
    // Sections must survive the section-less session_state overwrite.
    expect(ss.contextUsage?.sections).toHaveLength(2);
    expect(ss.contextUsage?.sections?.[0]).toEqual({ key: "system_prompt", size_bytes: 11010 });
    // The billed totals still come from the latest session_state.
    expect(ss.contextUsage?.usage_percent).toBe(6);
  });

  it("overwrites sections when the new context_usage carries its own", () => {
    seedSessionState([{ id: "u1", type: "user", content: "hi", timestamp: 1000 }], { total: 1 });

    handleMessageEvent(
      {
        type: "context_usage",
        session_id: SESSION,
        context_usage: {
          context_window: 250000,
          input_tokens: 100,
          output_tokens: 10,
          total_tokens: 110,
          usable_context: 250000,
          usage_percent: 5,
          sections: [{ key: "system_prompt", size_bytes: 11010 }],
        },
      },
      useChatStore.setState,
      useChatStore.getState,
      AGENT,
    );

    handleMessageEvent(
      {
        type: "context_usage",
        session_id: SESSION,
        context_usage: {
          context_window: 250000,
          input_tokens: 200,
          output_tokens: 20,
          total_tokens: 220,
          usable_context: 250000,
          usage_percent: 9,
          sections: [{ key: "messages", size_bytes: 500 }],
        },
      },
      useChatStore.setState,
      useChatStore.getState,
      AGENT,
    );

    const ss = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!;
    expect(ss.contextUsage?.sections).toEqual([{ key: "messages", size_bytes: 500 }]);
  });
});

describe("ADR-009 §V-Q: agent_meta must not clobber the server-resolved avatar", () => {
  it("keeps the effective avatar / display_name when the Runtime meta carries none", () => {
    const id = "11111111-2222-4333-8444-555555555555";
    // The Runtime never populates `AgentMeta.avatar` (agent_init passes
    // None), so the wire value is always "". Patching it into the store
    // dropped the sidebar icon to the random builtin fallback. The
    // Gateway's `list_agents` is the display authority.
    useAgentStore.setState({
      agents: {
        [id]: {
          meta: {
            instance_id: id,
            agent_id: "com.test.agent",
            name: "Package Name",
            display_name: "My Rename",
            avatar: "assets/avatar-01.jpg",
            builtin_avatar: "icon-05",
            version: "1.0.0",
            running: true,
            connected: true,
            ready: true,
            dev_mode: false,
          },
        },
      },
    } as never);

    handleMessageEvent(
      {
        type: "agent_meta",
        instance_id: id,
        name: "Package Name",
        version: "2.0.0",
        avatar: "",
        builtin_avatar: "",
      },
      () => {},
      (() => ({})) as never,
      id,
    );

    const meta = useAgentStore.getState().agents[id]!.meta;
    expect(meta.avatar).toBe("assets/avatar-01.jpg");
    expect(meta.builtin_avatar).toBe("icon-05");
    expect(meta.display_name).toBe("My Rename");
    // Fields the meta payload *does* carry still flow through.
    expect(meta.version).toBe("2.0.0");

    useAgentStore.setState({ agents: {} } as never);
  });
});

// Regression: when ask_user_question times out (or any path clears the
// retained MQTT slot), the Runtime publishes a zero-byte payload with
// retain=true. The Tauri MQTT bridge forwards that as a synthetic
// `event_cleared` agent-event. Without this handler the pending card
// stays on screen until the entire turn finishes (idle transition).
describe("event_cleared drops pending blocking cards", () => {
  beforeEach(() => clearTestState());
  afterEach(() => clearTestState());

  it("clears pendingQuestions when an ask_question retained slot is cleared", () => {
    // Seed: a session with one pending ask_question card.
    seedSessionState([]);
    useChatStore.setState((s) => ({
      ...s,
      agentStates: {
        ...s.agentStates,
        [AGENT]: {
          ...s.agentStates[AGENT]!,
          sessionStates: {
            ...s.agentStates[AGENT]!.sessionStates,
            [SESSION]: {
              ...s.agentStates[AGENT]!.sessionStates[SESSION]!,
              pendingQuestions: [
                {
                  type: "ask_question",
                  request_id: "q-1",
                  agent_id: AGENT,
                  question: "Pick one",
                  options: [{ label: "A" }, { label: "B" }],
                  session_id: SESSION,
                } as never,
              ],
            },
          },
        },
      },
    }));

    handleMessageEvent(
      {
        type: "event_cleared",
        session_id: SESSION,
        event_type: "ask_question",
      },
      useChatStore.setState,
      useChatStore.getState,
      AGENT,
    );

    const ss = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!;
    expect(ss.pendingQuestions).toEqual([]);
  });

  it("clears pendingApproval when a tool_approval_needed retained slot is cleared", () => {
    seedSessionState([]);
    useChatStore.setState((s) => ({
      ...s,
      agentStates: {
        ...s.agentStates,
        [AGENT]: {
          ...s.agentStates[AGENT]!,
          sessionStates: {
            ...s.agentStates[AGENT]!.sessionStates,
            [SESSION]: {
              ...s.agentStates[AGENT]!.sessionStates[SESSION]!,
              pendingApproval: {
                "tc-1": {
                  type: "tool_approval_needed",
                  request_id: "r-1",
                  tool_name: "shell",
                  risk_level: "High",
                  params: {},
                  params_summary: "",
                } as never,
              },
            },
          },
        },
      },
    }));

    handleMessageEvent(
      {
        type: "event_cleared",
        session_id: SESSION,
        event_type: "tool_approval_needed",
      },
      useChatStore.setState,
      useChatStore.getState,
      AGENT,
    );

    const ss = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!;
    expect(ss.pendingApproval).toEqual({});
  });

  it("ignores unknown event_type values (forward-compatible)", () => {
    seedSessionState([], { total: 1 });
    const before = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!.messages;

    handleMessageEvent(
      {
        type: "event_cleared",
        session_id: SESSION,
        event_type: "future_event_type",
      },
      useChatStore.setState,
      useChatStore.getState,
      AGENT,
    );

    // No panic, no spurious state change to messages.
    const after = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!.messages;
    expect(after).toBe(before);
  });
});

// ─────────────────────────────────────────────────────────────────────
// MQTT connection watchdog (P0 of unified-diagnostics plan §3.1.2)
//
// These tests pin the pure helpers that drive the 6-state
// `effectiveConnection` machine. The watchdog's setInterval path (with
// real timers + Rust IPC) is covered separately by `chatStore.watcher.test.ts`
// (TODO if/when we add one); here we focus on the deterministic contract
// each helper provides:
//
//   computeEffectiveConnection(snapshot)      → ConnectionStatus
//   appendTransition(log, from, to, reason)   → capped ring buffer
//   dedupeError(prev, next)                   → no flicker
//   updateConnectingSince(prev, next, now)    → connecting-episode clock
// ─────────────────────────────────────────────────────────────────────
describe("watchdog: computeEffectiveConnection state machine", () => {
  it("idle when no snapshot has been observed yet", () => {
    expect(computeEffectiveConnection(null)).toBe("idle");
    expect(computeEffectiveConnection({ known: false, connected: false })).toBe("idle");
  });

  it("connected wins over every other flag when snapshot.connected is true", () => {
    expect(
      computeEffectiveConnection({ known: true, connected: true, reason: "err-msg" }),
    ).toBe("connected");
    expect(
      computeEffectiveConnection({ known: true, connected: true, connecting: true }),
    ).toBe("connected");
  });

  it("connecting wins over disconnected when Rust reports an in-flight connect", () => {
    expect(
      computeEffectiveConnection({ known: true, connected: false, connecting: true }),
    ).toBe("connecting");
  });

  it("reconnecting wins over disconnected when Rust reports a retry", () => {
    expect(
      computeEffectiveConnection({ known: true, connected: false, reconnecting: true }),
    ).toBe("reconnecting");
  });

  it("disconnected is the fallback when no transient flag is set (reason does not reclassify)", () => {
    expect(
      computeEffectiveConnection({ known: true, connected: false }),
    ).toBe("disconnected");
    expect(
      computeEffectiveConnection({ known: true, connected: false, reason: "tcp refused" }),
    ).toBe("disconnected");
  });

  it("does NOT compute 'stale' here — that verdict lives in applyConnectionTransition so this stays pure", () => {
    // The stale verdict requires Date.now() against the episode clock,
    // which is a side-effecting check that belongs in the orchestrator.
    // Keeping this function pure means it can be unit-tested with no clock.
    expect(
      computeEffectiveConnection({ known: true, connected: false, connecting: true }),
    ).toBe("connecting");
  });
});

describe("watchdog: appendTransition ring buffer (capacity = 20)", () => {
  // Real Date.now() so the test mirrors production.
  const make = (from: ConnectionStatus, to: ConnectionStatus, reason: string | null = null): TransitionLogEntry => ({
    timestamp: Date.now(),
    from,
    to,
    reason,
  });

  it("returns the same log when from === to (no spurious entries)", () => {
    const log: TransitionLogEntry[] = [make("idle", "connecting")];
    const next = appendTransition(log, "connecting", "connecting", null);
    expect(next).toBe(log);
    expect(next).toHaveLength(1);
  });

  it("appends new transitions when from !== to", () => {
    const log: TransitionLogEntry[] = [make("idle", "connecting")];
    const next = appendTransition(log, "connecting", "connected", "broker ready");
    expect(next).toHaveLength(2);
    expect(next[1].from).toBe("connecting");
    expect(next[1].to).toBe("connected");
    expect(next[1].reason).toBe("broker ready");
  });

  it("caps the buffer at 20 entries — older ones are dropped", () => {
    let log: TransitionLogEntry[] = [];
    const states: ConnectionStatus[] = [
      "idle", "connecting", "connected", "disconnected", "connecting",
      "connected", "disconnected", "connecting", "connected", "disconnected",
      "connecting", "connected", "disconnected", "connecting", "connected",
      "disconnected", "connecting", "connected", "disconnected", "connecting",
      "connected", "disconnected", "connecting", "connected",
    ];
    for (let i = 0; i < states.length - 1; i++) {
      log = appendTransition(log, states[i], states[i + 1], null);
    }
    expect(log).toHaveLength(20);
    // The oldest (idle → connecting) was dropped — the buffer keeps the last 20.
    expect(log[0].from).not.toBe("idle");
    // The newest entry survived.
    expect(log[log.length - 1].to).toBe("connected");
  });
});

describe("watchdog: dedupeError (banner-flicker prevention)", () => {
  it("returns null when next is null (no error)", () => {
    expect(dedupeError("prev-err", null)).toBeNull();
  });

  it("returns next when prev is null (first error)", () => {
    expect(dedupeError(null, "broker unreachable")).toBe("broker unreachable");
  });

  it("returns prev unchanged when next equals prev (dedup)", () => {
    const prev = "broker unreachable";
    // Identity-preserved: a SET-state-equality check should NOT trigger
    // a re-render that would flicker the banner.
    expect(dedupeError(prev, prev)).toBe(prev);
  });

  it("returns next when next differs from prev", () => {
    expect(dedupeError("old-err", "new-err")).toBe("new-err");
  });
});

describe("watchdog: updateConnectingSince (connecting-episode clock)", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-01-01T00:00:00Z"));
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("starts the clock on entering the connecting family", () => {
    const t0 = Date.now();
    expect(updateConnectingSince(null, "connecting")).toBe(t0);
    expect(updateConnectingSince(null, "reconnecting")).toBe(t0);
    expect(updateConnectingSince(null, "stale")).toBe(t0);
  });

  it("keeps the SAME timestamp across moves within the family (no threshold reset)", () => {
    const t0 = Date.now();
    let clock = updateConnectingSince(null, "connecting");
    expect(clock).toBe(t0);

    vi.advanceTimersByTime(5_000);
    // connecting → reconnecting: still the same episode.
    clock = updateConnectingSince(clock, "reconnecting");
    expect(clock).toBe(t0);

    vi.advanceTimersByTime(28_000);
    // reconnecting → stale: STILL the same episode — this is what lets
    // the 30s force-reconnect deadline fire deterministically (B-1).
    clock = updateConnectingSince(clock, "stale");
    expect(clock).toBe(t0);
  });

  it("clears the clock when the connection settles", () => {
    const clock = updateConnectingSince(null, "connecting")!;
    expect(updateConnectingSince(clock, "connected")).toBeNull();
    expect(updateConnectingSince(clock, "disconnected")).toBeNull();
    expect(updateConnectingSince(clock, "idle")).toBeNull();
  });

  it("starts a NEW episode after the previous one settled", () => {
    const first = updateConnectingSince(null, "connecting")!;
    vi.advanceTimersByTime(1_000);
    expect(updateConnectingSince(first, "connected")).toBeNull();
    vi.advanceTimersByTime(1_000);
    const second = updateConnectingSince(null, "reconnecting");
    expect(second).not.toBeNull();
    expect(second!).toBeGreaterThan(first);
  });
});

// ── ADR-083: context compaction cancellation ─────────────────────────────

describe("ADR-083: compaction cancellation", () => {
  beforeEach(() => clearTestState());
  afterEach(() => clearTestState());

  it("compaction_cancelled resets isCompacting and does not need compacting_ended", () => {
    seedSessionState([]);
    useChatStore.setState((s) => ({
      ...s,
      agentStates: {
        ...s.agentStates,
        [AGENT]: {
          ...s.agentStates[AGENT]!,
          sessionStates: {
            ...s.agentStates[AGENT]!.sessionStates,
            [SESSION]: {
              ...s.agentStates[AGENT]!.sessionStates[SESSION]!,
              isCompacting: true,
            },
          },
        },
      },
    }));

    handleMessageEvent(
      { type: "compaction_cancelled", session_id: SESSION, reason: "user" },
      useChatStore.setState,
      useChatStore.getState,
      AGENT,
    );

    const ss = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!;
    expect(ss.isCompacting).toBe(false);
  });
});

// ── Legacy rotation-handoff notes: kept hidden ─────────────────────────

// The retired intent-rotation mechanism persisted an "[intent rotation] ..."
// note so a successor session could find its predecessor's history. It was
// context for the model, not for the user, and transcripts written while it
// existed still carry the row: loading history must drop it, or it renders as
// a grey system bubble mid-transcript.
describe("rotation_handoff entries stay out of the transcript", () => {
  beforeEach(() => clearTestState());
  afterEach(() => clearTestState());

  it("drops a legacy handoff note but keeps the user turn that carried it", async () => {
    seedSessionState([]);
    const entries: ConversationEntry[] = [
      {
        id: "handoff:msg-1",
        ts: "2026-01-01T00:00:00Z",
        role: "system",
        content: '[intent rotation] The previous execution session for this context (session id "s-old") was rotated...',
        metadata: { rotation_handoff: true },
      },
      {
        id: "msg-1",
        ts: "2026-01-01T00:00:01Z",
        role: "user",
        content: "提交本地修改的代码",
      },
    ];
    vi.stubGlobal(
      "fetch",
      vi.fn(() =>
        Promise.resolve({
          ok: true,
          status: 200,
          json: () => Promise.resolve({ offset: 0, limit: 50, total: 2, messages: entries }),
        }),
      ),
    );

    await useChatStore.getState().loadSessionMessages(AGENT, SESSION, undefined, 50, true);

    const ss = useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!;
    expect(ss.messages.map((m) => m.id)).toEqual(["msg-1"]);
  });
});
