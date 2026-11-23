/**
 * One-step send — the client half of the contract.
 *
 * The composer used to park a draft in a local queue while a turn was
 * running and require a second stop click to release it; that queue is gone.
 * A message typed into a running session is now one request carrying
 * `interrupt: true`, and the Runtime owns the ordering (hand the message over,
 * then abort the turn that was running when it arrived).
 *
 * What this file pins, because both failures are silent:
 *   1. `interrupt` reaches the wire when the composer asks for it — dropped
 *      here, the message merely waits for the turn, which looks like the old
 *      behaviour and is easy to mistake for a slow agent;
 *   2. it is ABSENT when it was not asked for — a stray `interrupt: true`
 *      would abort a turn nobody asked to abort (mobile and every older
 *      surface rely on the default `enqueue` semantic);
 *   3. the optimistic bubble still appears immediately in BOTH cases: the
 *      interrupt path must not become a "wait for the server" send.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const sendMessageMock = vi.hoisted(() => vi.fn(async () => {}));

vi.mock("../lib/session-control", () => ({
  sendMessage: sendMessageMock,
  stopSession: vi.fn(async () => {}),
  isAccessDenied: () => false,
}));

const { useChatStore } = await import("./chatStore");

const AGENT = "com.test.Agent";
const SESSION = "sess-one-step";

function seedAgent() {
  useChatStore.setState((s) => ({
    ...s,
    agentStates: {
      ...s.agentStates,
      [AGENT]: {
        activeSessionId: SESSION,
        sessionStates: {},
      } as never,
    },
  }));
}

function sessionMessages() {
  return useChatStore.getState().agentStates[AGENT]!.sessionStates[SESSION]!.messages;
}

describe("sendMessage — one-step send body", () => {
  beforeEach(() => {
    sendMessageMock.mockClear();
    seedAgent();
  });

  afterEach(() => {
    useChatStore.setState((s) => ({ ...s, agentStates: {} }));
  });

  it("carries interrupt: true when the composer was showing a running turn", async () => {
    await useChatStore
      .getState()
      .sendMessage("take this instead", AGENT, undefined, undefined, { interrupt: true });

    expect(sendMessageMock).toHaveBeenCalledTimes(1);
    const body = sendMessageMock.mock.calls[0]![2] as Record<string, unknown>;
    expect(body.interrupt).toBe(true);
    expect(body.content).toBe("take this instead");
    expect(typeof body.message_id).toBe("string");
  });

  it("omits interrupt entirely for an ordinary send (the enqueue default)", async () => {
    await useChatStore.getState().sendMessage("and then this", AGENT);

    const body = sendMessageMock.mock.calls[0]![2] as Record<string, unknown>;
    expect("interrupt" in body).toBe(false);
  });

  it("inserts the optimistic user bubble immediately in both modes", async () => {
    await useChatStore.getState().sendMessage("interrupting", AGENT, undefined, undefined, {
      interrupt: true,
    });
    expect(sessionMessages().filter((m) => m.type === "user")).toHaveLength(1);

    await useChatStore.getState().sendMessage("plain", AGENT);
    expect(sessionMessages().filter((m) => m.type === "user")).toHaveLength(2);
  });
});