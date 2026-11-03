// @vitest-environment jsdom
/**
 * The block read's re-read lifecycle (ADR-106 D5/D9 + the 2026-11-03 revision).
 *
 * Three properties, all of them silent when they break:
 *
 *   1. a `pending` block keeps re-reading. The reported bug was a block stuck at
 *      "running" until the surface was unmounted and remounted: the poll was a
 *      one-shot `setTimeout` armed by an effect whose dependencies cannot change
 *      while a round stays pending (D3's zero-write means the transcript gains no
 *      row), so it fired once and was never re-armed;
 *   2. a terminal block is NEVER re-read (D5 — a finished round is history);
 *   3. the round-state signal reaches the mounted block it names, and only that
 *      one. It is the notification face D3 removed, and it is what makes the flip
 *      prompt instead of waiting on the 15 s backstop.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { act, renderHook } from "@testing-library/react";
import {
  noteRoundState,
  useChatBlockStore,
  useChatBlocks,
} from "./chatBlockStore";
import { handleMessageEvent, useChatStore } from "./chatStore";
import type { ChatBlockEntry } from "../lib/chatBlocks";

vi.mock("../lib/chatBlocks", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../lib/chatBlocks")>();
  return { ...actual, fetchChatBlocks: vi.fn() };
});

import { fetchChatBlocks } from "../lib/chatBlocks";

const fetchMock = vi.mocked(fetchChatBlocks);

const CHAT = "chat-1";
const REF = "i-1|s-1|r:1";
const POLL_MS = 15_000;
/** The store batches refs for one frame before it issues a read. */
const BATCH_SLACK = 100;

function entryOf(
  status: ChatBlockEntry["status"],
  projection?: ChatBlockEntry["projection"],
): ChatBlockEntry {
  const [agent_instance_id, agent_session_id, round_id] = REF.split("|");
  return { ref: REF, agent_instance_id, agent_session_id, round_id, status, projection };
}

/** A terminal projection — what a finished round's read returns. */
const done = () =>
  entryOf("ok", {
    round_id: "r:1",
    chat_session_id: CHAT,
    terminal: true,
    status: "completed",
    answer: { body: "shipped", ts: "2026-11-03T09:02:43.378Z" },
  });

function seedPending() {
  useChatBlockStore.setState({ blocks: { [REF]: entryOf("pending") } });
}

beforeEach(() => {
  vi.useFakeTimers();
  // The store is a module singleton and the mount registry lives beside it:
  // leftovers from one test would decide the next one's fetch counts.
  useChatBlockStore.setState({ blocks: {}, pending: {}, inFlight: {} });
  fetchMock.mockReset();
  fetchMock.mockResolvedValue([entryOf("pending")]);
});

afterEach(async () => {
  // Drain what the test queued before the clock is thrown away: the store is a
  // module singleton, and a timer scheduled under a discarded fake clock never
  // fires again — its queue would sit there and decide the next test's counts.
  await act(async () => {
    await vi.advanceTimersByTimeAsync(POLL_MS * 6);
  });
  vi.useRealTimers();
});

describe("the pending backstop", () => {
  it("reads a ref it does not hold, batched per chat", async () => {
    renderHook(() => useChatBlocks(CHAT, [REF]));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(POLL_MS);
    });
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(fetchMock).toHaveBeenCalledWith(CHAT, [REF]);
  });

  it("re-reads a still-pending block on EVERY tick, not once", async () => {
    // The regression. A held `pending` ref is not re-queued by `ensure`, so the
    // first fetch here can only be the poll — and one fetch in two intervals is
    // exactly the block that looked frozen until the view was remounted. The
    // `+ BATCH_SLACK` is the 16 ms batch window the reload has to ride.
    seedPending();
    renderHook(() => useChatBlocks(CHAT, [REF]));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(POLL_MS + BATCH_SLACK);
    });
    expect(fetchMock).toHaveBeenCalledTimes(1);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(POLL_MS);
    });
    expect(fetchMock).toHaveBeenCalledTimes(2);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(POLL_MS);
    });
    expect(fetchMock).toHaveBeenCalledTimes(3);
  });

  it("never re-reads a terminal block (D5)", async () => {
    useChatBlockStore.setState({ blocks: { [REF]: done() } });
    renderHook(() => useChatBlocks(CHAT, [REF]));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(POLL_MS * 4);
    });
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("stops re-reading the moment the block reaches terminal", async () => {
    seedPending();
    renderHook(() => useChatBlocks(CHAT, [REF]));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(POLL_MS + BATCH_SLACK);
    });
    expect(fetchMock).toHaveBeenCalledTimes(1);
    // The read lands as history: the poll's dependency flips and the interval is
    // torn down — a settled block that keeps polling does not believe history.
    fetchMock.mockResolvedValue([done()]);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(POLL_MS * 3);
    });
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });

  it("stops polling on unmount", async () => {
    seedPending();
    const { unmount } = renderHook(() => useChatBlocks(CHAT, [REF]));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(POLL_MS + BATCH_SLACK);
    });
    expect(fetchMock).toHaveBeenCalledTimes(1);
    unmount();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(POLL_MS * 3);
    });
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });
});

describe("noteRoundState (ADR-106 revision 2026-11-03)", () => {
  it("re-reads the ref a mounted block holds", async () => {
    seedPending();
    renderHook(() => useChatBlocks(CHAT, [REF]));
    await act(async () => {
      expect(noteRoundState(CHAT, REF)).toBe(true);
      await vi.advanceTimersByTimeAsync(50);
    });
    expect(fetchMock).toHaveBeenCalledWith(CHAT, [REF]);
  });

  it("re-reads once more when the first answer still says pending", async () => {
    // The signal is published at the turn boundary while the round's terminal
    // record is appended by the session's single writer (ADR-092), so the first
    // projection read can legitimately arrive too early.
    seedPending();
    renderHook(() => useChatBlocks(CHAT, [REF]));
    await act(async () => {
      noteRoundState(CHAT, REF);
      await vi.advanceTimersByTimeAsync(50);
    });
    expect(fetchMock).toHaveBeenCalledTimes(1);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(600);
    });
    expect(fetchMock).toHaveBeenCalledTimes(2);
    // Bounded: a third re-read would be a poll under another name.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5_000);
    });
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });

  it("does not fetch for a Chat or a round nobody is showing", async () => {
    seedPending();
    renderHook(() => useChatBlocks(CHAT, [REF]));
    await act(async () => {
      expect(noteRoundState("other-chat", REF)).toBe(false);
      expect(noteRoundState(CHAT, "i-2|s-2|r:9")).toBe(false);
      await vi.advanceTimersByTimeAsync(1_000);
    });
    expect(fetchMock).not.toHaveBeenCalled();
  });
});

describe("the round_state event reaches the block store", () => {
  const payload = {
    type: "round_state",
    instance_id: "i-1",
    session_id: "s-1",
    round_id: "r:1",
    chat_session_id: CHAT,
    status: "completed",
    terminal: true,
    ts: "2026-11-03T09:02:43.378Z",
  };

  it("re-reads the block the signal names, by the ref it carries", async () => {
    seedPending();
    renderHook(() => useChatBlocks(CHAT, [REF]));
    await act(async () => {
      handleMessageEvent(
        payload,
        useChatStore.setState,
        useChatStore.getState,
        "i-1",
      );
      await vi.advanceTimersByTimeAsync(50);
    });
    expect(fetchMock).toHaveBeenCalledWith(CHAT, [REF]);
  });

  it("is not a content event: it never touches the transcript", async () => {
    // `session_id` here is the EXECUTION session. Reading it as content would
    // write another agent's run into this account's message window.
    const before = useChatStore.getState();
    handleMessageEvent(
      { ...payload, session_id: "s-foreign", chat_session_id: "chat-foreign" },
      useChatStore.setState,
      useChatStore.getState,
      "i-1",
    );
    const after = useChatStore.getState();
    expect(after).toBe(before);
  });
});
