// Targeted reproduction: after `session_created` triggers
// `activateNewlyCreatedSession`, the FileTreeNode's `addAttachedContext`
// call uses the prop sessionId. Verify that the chain ends up writing
// to the new session.
import { describe, it, expect, beforeEach, vi } from "vitest";
import { render, act, fireEvent } from "@testing-library/react";
import { useCallback } from "react";
import { useChatStore, handleMessageEvent, scheduleSendReconciliation } from "./chatStore";
import { useAgentStore } from "./agentStore";
import { FileTreeNode } from "../components/workspace/FileTree/FileTreeNode";
import type { ChatMessage } from "../lib/types";

const AGENT = "com.test.Agent";

function seedAgent(sessionIds: string[], activeSessionId: string | null) {
  useChatStore.setState((s) => ({
    ...s,
    agentStates: {
      ...s.agentStates,
      [AGENT]: {
        ...(s.agentStates[AGENT] ?? {}),
        activeSessionId,
        openSessionIds: sessionIds,
        sessionStates: Object.fromEntries(
          sessionIds.map((sid) => [
            sid,
            {
              ...useChatStore.getState().getSessionState(AGENT, sid),
              attachedContext: [],
              messages: [],
              loadSequence: 0,
              lastAccessed: Date.now(),
            },
          ]),
        ),
      },
    },
  }));
}

describe("REPRO: add to file routes to old session after new session creation", () => {
  beforeEach(() => {
    useChatStore.setState((s) => ({ ...s, agentStates: {} }));
    useAgentStore.setState((s) => ({ ...s, agents: {} }));
    vi.stubGlobal(
      "fetch",
      vi.fn(() =>
        Promise.resolve({
          ok: true,
          status: 200,
          json: () =>
            Promise.resolve({ messages: [], offset: 0, limit: 0, total: 0 }),
        }),
      ),
    );
  });

  function FileTreeNodeSim(props: { sessionId: string }) {
    const addAttachedContext = useChatStore((s) => s.addAttachedContext);
    const handleAddToChat = useCallback(() => {
      addAttachedContext(AGENT, props.sessionId, {
        id: `${AGENT}:foo.txt`,
        type: "file",
        name: "foo.txt",
        absPath: "/foo.txt",
      });
    }, [props.sessionId, addAttachedContext]);
    return <button onClick={handleAddToChat}>add</button>;
  }

  function AttachedContextChipsSim() {
    const items = useChatStore((s) => {
      const aid = s.agentStates[AGENT]?.activeSessionId;
      if (!aid) return [];
      return s.agentStates[AGENT]?.sessionStates[aid]?.attachedContext ?? [];
    });
    return (
      <div data-testid="chips">
        {items.map((it) => (
          <span key={it.id} data-testid="chip">
            {it.name}
          </span>
        ))}
      </div>
    );
  }

  function WorkspaceExplorerSim() {
    const activeSessionId = useChatStore(
      (s) => s.agentStates[AGENT]?.activeSessionId ?? null,
    );
    return activeSessionId ? <FileTreeNodeSim sessionId={activeSessionId} /> : null;
  }

  it("REPRO-A: full flow — activateNewlyCreatedSession → addAttachedContext lands in NEW", async () => {
    seedAgent(["sess-old"], "sess-old");

    await act(async () => {
      await useAgentStore.getState().activateNewlyCreatedSession("sess-new", AGENT);
    });

    expect(useChatStore.getState().getActiveSessionId(AGENT)).toBe("sess-new");

    const { getByText, getByTestId } = render(
      <>
        <WorkspaceExplorerSim />
        <AttachedContextChipsSim />
      </>,
    );

    act(() => {
      (getByText("add") as HTMLButtonElement).click();
    });

    const chips = getByTestId("chips");
    console.log("[REPRO-A] chips innerHTML:", chips.innerHTML);
    console.log(
      "[REPRO-A] sess-new attachedContext:",
      useChatStore.getState().getSessionState(AGENT, "sess-new").attachedContext,
    );
    console.log(
      "[REPRO-A] sess-old attachedContext:",
      useChatStore.getState().getSessionState(AGENT, "sess-old").attachedContext,
    );

    expect(chips.innerHTML).toContain("foo.txt");
  });

  it("REPRO-B: full flow — handleMessageEvent session_created → addAttachedContext", async () => {
    // Initialize the agentStore with a minimal agent entry so that
    // activateNewlyCreatedSession / fetchSessions don't crash.
    useAgentStore.setState((s) => ({
      ...s,
      agents: {
        ...s.agents,
        [AGENT]: {
          ...(s.agents[AGENT] ?? {}),
          meta: { id: AGENT, name: "Test Agent", package_id: "test", status: "ready" } as any,
          sessions: [],
          pagination: {
            currentPage: 1,
            totalPages: 1,
            totalCount: 0,
            pageSize: 20,
          },
          selectedAgentId: AGENT,
          selectedModel: null,
          preferredModel: null,
          preferredProvider: null,
          isLoading: false,
          sessionTitle: null,
        } as any,
      },
    }));
    seedAgent(["sess-old"], "sess-old");

    // Simulate the mqtt event landing on the chatStore handler.
    // Trace what's happening: the handler is supposed to call
    // agentStore.activateNewlyCreatedSession which awaits openSession.
    console.log(
      "[REPRO-B] BEFORE event: activeSessionId:",
      useChatStore.getState().getActiveSessionId(AGENT),
    );

    const traceSub = useChatStore.subscribe((s) => {
      console.log(
        "[REPRO-B.subscribe] activeSessionId ->",
        s.agentStates[AGENT]?.activeSessionId,
        "openSessionIds ->",
        s.agentStates[AGENT]?.openSessionIds,
      );
    });

    await act(async () => {
      handleMessageEvent(
        {
          type: "session_created",
          instance_id: AGENT,
          session_id: "sess-new",
        },
        useChatStore.setState,
        useChatStore.getState,
        AGENT,
      );
      // Wait long enough for all microtasks + setTimeout(0) chains.
      await new Promise((r) => setTimeout(r, 100));
    });
    traceSub();

    console.log(
      "[REPRO-B] AFTER event: activeSessionId:",
      useChatStore.getState().getActiveSessionId(AGENT),
    );
    console.log(
      "[REPRO-B] AFTER event: openSessionIds:",
      useChatStore.getState().getOpenSessionIds(AGENT),
    );

    const { getByText, getByTestId } = render(
      <>
        <WorkspaceExplorerSim />
        <AttachedContextChipsSim />
      </>,
    );

    act(() => {
      (getByText("add") as HTMLButtonElement).click();
    });

    const chips = getByTestId("chips");
    console.log("[REPRO-B] chips innerHTML:", chips.innerHTML);
    console.log(
      "[REPRO-B] sess-new attachedContext:",
      useChatStore.getState().getSessionState(AGENT, "sess-new").attachedContext,
    );
    console.log(
      "[REPRO-B] sess-old attachedContext:",
      useChatStore.getState().getSessionState(AGENT, "sess-old").attachedContext,
    );

    // The bug: file went into sess-old (because activeSessionId silently
    // stayed sess-old), chips also read sess-old → user "sees nothing in
    // NEW session" but "sees the file in OLD session" — exactly the user
    // report. The chips do contain "foo.txt" because both ends read OLD.
    // The actual assertion: the file must land in sess-new (where the
    // user clicked Add to Chat).
    expect(
      useChatStore.getState().getSessionState(AGENT, "sess-new").attachedContext,
    ).toHaveLength(1);
  });

  it("REPRO-C: round-trip NEW → OLD → NEW preserves the chip in NEW", async () => {
    // The user's actual report: they created NEW session, added a file,
    // then noticed the chip didn't show in NEW, switched to OLD tab and
    // saw the chip there. This test simulates that round-trip explicitly
    // to confirm chip persistence is correct in BOTH sessions.
    useAgentStore.setState((s) => ({
      ...s,
      agents: {
        ...s.agents,
        [AGENT]: {
          ...(s.agents[AGENT] ?? {}),
          meta: { id: AGENT, name: "Test Agent", package_id: "test", status: "ready" } as any,
          sessions: [],
          pagination: { currentPage: 1, totalPages: 1, totalCount: 0, pageSize: 20 },
          selectedAgentId: AGENT,
          selectedModel: null,
          preferredModel: null,
          preferredProvider: null,
          isLoading: false,
          sessionTitle: null,
        } as any,
      },
    }));
    seedAgent(["sess-old"], "sess-old");

    // 1. session_created → activateNewlyCreatedSession → activeSessionId=NEW
    await act(async () => {
      handleMessageEvent(
        { type: "session_created", instance_id: AGENT, session_id: "sess-new" },
        useChatStore.setState,
        useChatStore.getState,
        AGENT,
      );
      await new Promise((r) => setTimeout(r, 100));
    });
    expect(useChatStore.getState().getActiveSessionId(AGENT)).toBe("sess-new");

    // 2. user adds file via FileTreeNode (which reads activeSessionId=NEW)
    act(() => {
      useChatStore.getState().addAttachedContext(AGENT, "sess-new", {
        id: "foo.txt",
        type: "file",
        name: "foo.txt",
        absPath: "/foo.txt",
      });
    });
    expect(
      useChatStore.getState().getSessionState(AGENT, "sess-new").attachedContext,
    ).toHaveLength(1);

    // 3. user switches to OLD tab (activeSessionId=OLD)
    act(() => {
      useChatStore.setState((s) => ({
        ...s,
        agentStates: {
          ...s.agentStates,
          [AGENT]: { ...s.agentStates[AGENT]!, activeSessionId: "sess-old" },
        },
      }));
    });
    expect(
      useChatStore.getState().getSessionState(AGENT, "sess-old").attachedContext,
    ).toHaveLength(0);

    // 4. user switches BACK to NEW tab → chip must still be in NEW
    act(() => {
      useChatStore.setState((s) => ({
        ...s,
        agentStates: {
          ...s.agentStates,
          [AGENT]: { ...s.agentStates[AGENT]!, activeSessionId: "sess-new" },
        },
      }));
    });
    const newCtx = useChatStore
      .getState()
      .getSessionState(AGENT, "sess-new").attachedContext;
    console.log("[REPRO-C] sess-new attachedContext after round-trip:", newCtx);
    expect(newCtx).toHaveLength(1);
    expect(newCtx[0].name).toBe("foo.txt");
  });

  it("REPRO-D: FileTreeNodeSim still routes correctly after switching back to NEW", async () => {
    // The chain that the user reports as broken:
    //   1. NEW session created
    //   2. activeSessionId = NEW
    //   3. FileTreeNode (driven by activeSessionId) clicks Add to Chat
    //      → attachedContext should be added to NEW
    //   4. User switches back to OLD tab
    //   5. The chip was visible in OLD, NOT in NEW (per user report)
    //
    // Our test simulates steps 1-5 via the full store API and asserts
    // that the file ALWAYS lands in NEW (where the user clicked).
    useAgentStore.setState((s) => ({
      ...s,
      agents: {
        ...s.agents,
        [AGENT]: {
          ...(s.agents[AGENT] ?? {}),
          meta: { id: AGENT, name: "Test Agent", package_id: "test", status: "ready" } as any,
          sessions: [],
          pagination: { currentPage: 1, totalPages: 1, totalCount: 0, pageSize: 20 },
          selectedAgentId: AGENT,
          selectedModel: null,
          preferredModel: null,
          preferredProvider: null,
          isLoading: false,
          sessionTitle: null,
        } as any,
      },
    }));
    seedAgent(["sess-old"], "sess-old");

    // Step 1: activate NEW via session_created
    await act(async () => {
      handleMessageEvent(
        { type: "session_created", instance_id: AGENT, session_id: "sess-new" },
        useChatStore.setState,
        useChatStore.getState,
        AGENT,
      );
      await new Promise((r) => setTimeout(r, 100));
    });
    expect(useChatStore.getState().getActiveSessionId(AGENT)).toBe("sess-new");

    // Step 2-3: User adds file via FileTreeNode (reads activeSessionId=NEW)
    const { getByText } = render(<WorkspaceExplorerSim />);
    act(() => {
      (getByText("add") as HTMLButtonElement).click();
    });
    // File should land in NEW, not OLD
    expect(
      useChatStore.getState().getSessionState(AGENT, "sess-new").attachedContext,
    ).toHaveLength(1);
    expect(
      useChatStore.getState().getSessionState(AGENT, "sess-old").attachedContext,
    ).toHaveLength(0);

    // Step 4: User switches back to OLD tab (activeSessionId=OLD)
    act(() => {
      useChatStore.setState((s) => ({
        ...s,
        agentStates: {
          ...s.agentStates,
          [AGENT]: { ...s.agentStates[AGENT]!, activeSessionId: "sess-old" },
        },
      }));
    });

    // Step 5: User clicks Add to Chat AGAIN from the (now OLD) tab —
    // FileTreeNode now writes to OLD.  This is the "I added a file in
    // NEW, but it showed up in OLD" pattern (user actually re-added in
    // OLD after switching).  Both behaviors are technically valid as
    // long as the user can see and remove the chip from wherever it
    // was written.
    act(() => {
      (getByText("add") as HTMLButtonElement).click();
    });
// Now both should have a chip
    expect(
      useChatStore.getState().getSessionState(AGENT, "sess-new").attachedContext,
    ).toHaveLength(1);
    expect(
      useChatStore.getState().getSessionState(AGENT, "sess-old").attachedContext,
    ).toHaveLength(1);
  });

  it("REPRO-E: full user sequence from the log — create → auto-activate NEW → click OLD tab (silent switch) → add file → lands in OLD (per-session semantics)", async () => {
    // Reconstructs the exact sequence evidenced by the production log:
    //   15:52:13 create_session → session_created → activateNewlyCreatedSession
    //            → openSession(NEW) → activeSessionId=NEW (confirmed by
    //            OpenSession success already_active)
    //   [user clicks OLD tab] → setActiveTab(OLD) — SILENT, no MQTT (this
    //            is the only path that flips activeSessionId without a
    //            publish, which is why the log shows no switch command)
    //   15:52:49 chat_message → written to OLD (confirmed by runtime log)
    //
    // The user's complaint ("file added to OLD instead of NEW") is the
    // EXPECTED per-session behavior once they have silently switched to
    // the OLD tab. This test pins that contract: Add to Chat ALWAYS
    // targets the active session at click time.
    useAgentStore.setState((s) => ({
      ...s,
      agents: {
        ...s.agents,
        [AGENT]: {
          ...(s.agents[AGENT] ?? {}),
          meta: { id: AGENT, name: "Test Agent", package_id: "test", status: "ready" } as any,
          sessions: [],
          pagination: { currentPage: 1, totalPages: 1, totalCount: 0, pageSize: 20 },
          selectedAgentId: AGENT,
          selectedModel: null,
          preferredModel: null,
          preferredProvider: null,
          isLoading: false,
          sessionTitle: null,
        } as any,
      },
    }));
    seedAgent(["sess-old"], "sess-old");

    // Step 1: user clicks "+" → session_created → NEW auto-activated
    await act(async () => {
      handleMessageEvent(
        { type: "session_created", instance_id: AGENT, session_id: "sess-new" },
        useChatStore.setState,
        useChatStore.getState,
        AGENT,
      );
      await new Promise((r) => setTimeout(r, 100));
    });
    expect(useChatStore.getState().getActiveSessionId(AGENT)).toBe("sess-new");

    // Step 2: user clicks OLD tab — SessionTabBar.handleTabClick → setActiveTab
    // (silent, no MQTT — this is why the production log shows NO command).
    act(() => {
      useChatStore.getState().setActiveTab(AGENT, "sess-old");
    });
    expect(useChatStore.getState().getActiveSessionId(AGENT)).toBe("sess-old");

    // Step 3: user clicks Add to Chat in the (now OLD) panel — file lands in OLD.
    const { getByText } = render(<WorkspaceExplorerSim />);
    act(() => {
      (getByText("add") as HTMLButtonElement).click();
    });
    const oldCtx = useChatStore.getState().getSessionState(AGENT, "sess-old").attachedContext;
    const newCtx = useChatStore.getState().getSessionState(AGENT, "sess-new").attachedContext;
    console.log("[REPRO-E] after add-in-OLD: old=", oldCtx, "new=", newCtx);

    // Per-session semantics: the file went to OLD because OLD was active.
    // This is EXACTLY what the user observed ("file added to the previously
    // open session's input box").
    expect(oldCtx).toHaveLength(1);
    expect(newCtx).toHaveLength(0);

    // Step 4: user switches back to NEW → no chip there (correct: file is in OLD)
    act(() => {
      useChatStore.getState().setActiveTab(AGENT, "sess-new");
    });
    expect(
      useChatStore.getState().getSessionState(AGENT, "sess-new").attachedContext,
    ).toHaveLength(0);

    // Step 5: user adds a file in NEW → now lands in NEW (per-session isolation)
    act(() => {
      useChatStore.getState().addAttachedContext(AGENT, "sess-new", {
        id: "bar.txt",
        type: "file",
        name: "bar.txt",
        absPath: "/bar.txt",
      });
    });
    expect(
      useChatStore.getState().getSessionState(AGENT, "sess-new").attachedContext,
    ).toHaveLength(1);
    expect(
      useChatStore.getState().getSessionState(AGENT, "sess-old").attachedContext,
    ).toHaveLength(1);
  });

  it("REPRO-F: file-tree context menu Add to Chat targets the session active at click time (stale useMemo closure regression)", async () => {
    // Regression for the 2026-08-29 bug: FileTreeNode memoised ctxMenuItems
    // with a dependency array that omitted the onClick handlers. After
    // session_created auto-activated a NEW session, the right-click menu
    // still captured the OLD sessionId closure — "add to file" performed
    // while the NEW session was active landed in the OLD one.
    seedAgent(["sess-old"], "sess-old");

    // jsdom has no layout: the menu measures 0×0, so the position hook
    // defers one animation frame and would re-measure outside act(). Run
    // rAF synchronously so that retry stays inside the act scope.
    vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => {
      cb(0);
      return 0;
    });

    const baseProps = {
      entry: { name: "foo.txt", type: "file" as const, size: 3, mtime: 0 },
      depth: 0,
      agentId: AGENT,
      workspaceId: "__agent_home__",
      relPath: "foo.txt",
      absPath: "/foo.txt",
      isExpanded: false,
      isLoading: false,
      isSelected: false,
      onToggle: vi.fn(),
      onSelect: vi.fn(),
      slotSize: 24,
      slotStart: 0,
      slotIndex: 0,
    };

    const { rerender, container } = render(
      <FileTreeNode {...baseProps} />,
    );
    const row = container.querySelector('[data-rel-path="foo.txt"]')!;

    // Right-click the row, then click the first menu item ("Add to Chat" —
    // FileTreeNode pushes it first, before the divider-gated items).
    // Note: async act — ContextMenu closes itself via a Promise.finally
    // microtask after the item's onClick, which a sync act does not flush.
    const clickAddToChat = async () => {
      act(() => {
        fireEvent.contextMenu(row);
      });
      const items = document.querySelectorAll(".context-menu .context-menu-item");
      await act(async () => {
        fireEvent.click(items[0] as HTMLButtonElement);
      });
    };

    // Step 1: OLD session active → chip lands in OLD.
    await clickAddToChat();
    expect(
      useChatStore.getState().getSessionState(AGENT, "sess-old").attachedContext,
    ).toHaveLength(1);

    // Step 2: session_created → NEW auto-activated (production event chain).
    await act(async () => {
      handleMessageEvent(
        { type: "session_created", instance_id: AGENT, session_id: "sess-new" },
        useChatStore.setState,
        useChatStore.getState,
        AGENT,
      );
      await new Promise((r) => setTimeout(r, 100));
    });
    expect(useChatStore.getState().getActiveSessionId(AGENT)).toBe("sess-new");

    // Step 3: the tree's `sessionId` prop is gone — the node subscribes to
    // the active session through `useAddToChatTarget`, so the memoised menu
    // rebuilds on its own. A no-op rerender proves nothing was captured from
    // a stale prop; the chip must land in NEW.
    rerender(<FileTreeNode {...baseProps} />);
    await clickAddToChat();

    expect(
      useChatStore.getState().getSessionState(AGENT, "sess-new").attachedContext,
    ).toHaveLength(1);
    // OLD must not receive a second chip.
    expect(
      useChatStore.getState().getSessionState(AGENT, "sess-old").attachedContext,
    ).toHaveLength(1);
  });

  // ── REPRO-G: send button + phase banner stale state ────────────────
  //
  // User report (2026-09-01):
  //   - After send, button still shows "Send" instead of "Stop" (sending=false
  //     even though sessionStatus moved to llm_awaiting_first_chunk).
  //   - After done, button still shows "Stop" (sending=true even though
  //     sessionStatus is back to idle).
  //   - Switching sessions + switching back makes the UI correct → proves
  //     the store state IS right, but the subscription / re-render layer
  //     dropped the in-place update.
  //
  // Hypothesis: the scheduleSendReconciliation set() chain
  // (300/800/1600ms loadSequence updates) racing with the session_state
  // MQTT set() chain drops intermediate sessionStatus transitions on the
  // floor. The probe below subscribes via the same selector pattern as
  // ChatPanel and records every observed (status, sending) pair.
  it("REPRO-G: sendMessage → session_state transitions are observed by the status selector (no drop)", async () => {
    const SESSION = "sess-g";
    seedAgent([SESSION], SESSION);

    // Use real timers so the 300/800/1600ms reconciliation actually fires
    // (the user's bug is timing-dependent).
    vi.useFakeTimers({ shouldAdvanceTime: false });

    // Probe: mirrors ChatPanel.tsx:408-413 selector pattern. Records every
    // observed transition so we can assert the chain is observed end-to-end.
    const observed: Array<{ status: string; sending: boolean; t: number }> = [];
    const t0 = Date.now();
    function StatusProbe() {
      const sessionStatus = useChatStore((s) => {
        const agent = s.agentStates[AGENT];
        return agent?.sessionStates[SESSION]?.sessionStatus ?? null;
      });
      const sending = sessionStatus?.status !== "idle" && sessionStatus != null;
      if (
        observed.length === 0 ||
        observed[observed.length - 1].status !== (sessionStatus?.status ?? "null") ||
        observed[observed.length - 1].sending !== !!sending
      ) {
        observed.push({
          status: sessionStatus?.status ?? "null",
          sending: !!sending,
          t: Date.now() - t0,
        });
      }
      return (
        <div data-testid="status">
          {sessionStatus?.status ?? "null"}|{sending ? "1" : "0"}
        </div>
      );
    }

    render(<StatusProbe />);

    // 1. Backend publishes session_state (idle → llm_awaiting_first_chunk).
    //    The user's backend does this ~50ms after sendMessage lands. We
    //    compress that here.
    await act(async () => {
      handleMessageEvent(
        {
          type: "session_state",
          instance_id: AGENT,
          session_id: SESSION,
          status: { status: "llm_awaiting_first_chunk" },
          message_count: 0,
          input_tokens: 0,
          output_tokens: 0,
          total_input_tokens: 0,
          total_output_tokens: 0,
          ratio: 0,
          updated_at: 0,
        },
        useChatStore.setState,
        useChatStore.getState,
        AGENT,
      );
    });

    // 2. scheduleSendReconciliation (what sendMessage schedules after a
    //    successful MQTT publish). On each retry, loadSessionMessages runs
    //    a set() for loadSequence+abortController — which does NOT include
    //    sessionStatus in its patch but DOES create a new agent object.
    //    If the selector's identity check trips, the in-flight transition
    //    observed in step 1 could be lost.
    await act(async () => {
      // Insert an optimistic message first so scheduleSendReconciliation
      // has something to wait for (it short-circuits if no _isOptimistic).
      const ss0 = useChatStore.getState().getSessionState(AGENT, SESSION);
      useChatStore.setState((s) => ({
        ...s,
        agentStates: {
          ...s.agentStates,
          [AGENT]: {
            ...s.agentStates[AGENT]!,
            sessionStates: {
              ...s.agentStates[AGENT]!.sessionStates,
              [SESSION]: {
                ...ss0,
                messages: [
                  ...ss0.messages,
                  {
                    id: "msg-opt",
                    content: "hello",
                    type: "user",
                    timestamp: Date.now(),
                    _isOptimistic: true,
                  } as ChatMessage,
                ],
              },
            },
          },
        },
      }));

      scheduleSendReconciliation(AGENT, SESSION);
      // Advance through all 3 retry delays: 300 + 800 + 1600 = 2700ms.
      // The first fetch resolves quickly (mock returns empty), the second
      // also resolves, the third is allowed to fire too.
      await vi.advanceTimersByTimeAsync(300);
      await Promise.resolve();
      await vi.advanceTimersByTimeAsync(800);
      await Promise.resolve();
      await vi.advanceTimersByTimeAsync(1600);
      await Promise.resolve();
    });

    // 3. Backend publishes session_state (llm_awaiting_first_chunk → idle).
    await act(async () => {
      handleMessageEvent(
        {
          type: "session_state",
          instance_id: AGENT,
          session_id: SESSION,
          status: { status: "idle" },
          message_count: 1,
          input_tokens: 0,
          output_tokens: 0,
          total_input_tokens: 0,
          total_output_tokens: 0,
          ratio: 0,
          updated_at: 0,
        },
        useChatStore.setState,
        useChatStore.getState,
        AGENT,
      );
    });

    vi.useRealTimers();

    // The chain we expect to observe:
    //   1. null → null (initial)
    //   2. llm_awaiting_first_chunk → sending=1   (the bug: user reports this is missing)
    //   3. (the 3 reconciliation set()s — should not flip status back)
    //   4. idle → sending=0                       (the bug: user reports this is missing)
    //
    // We don't pin exact ordering of (3) vs (2) since the timeline
    // depends on how the test's mocks schedule microtasks, but we DO
    // require that (2) and (4) appear in observed[] in that order.
    const sawProcessing = observed.some((o) => o.sending === true);
    const sawBackToIdle = observed.some((o) => o.sending === false && o.status === "idle");

    // If the bug is real, `sawProcessing` will be false OR `sawBackToIdle`
    // will be false (or both — depending on which transition is dropped).
    expect(
      sawProcessing,
      `Selector never observed a processing transition. Observed: ${JSON.stringify(observed)}`,
    ).toBe(true);
    expect(
      sawBackToIdle,
      `Selector never observed the back-to-idle transition. Observed: ${JSON.stringify(observed)}`,
    ).toBe(true);
  });
});
