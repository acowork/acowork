/**
 * Regression t-f9a9fbc7: "after the Desktop restarts, the execution session a
 * user opens from the history list shows one 50-record page and cannot scroll
 * up; switching to another session and back fixes it".
 *
 * Root cause pinned here: the store inferred "this cache has no authoritative
 * window yet" from `messages.length === 0`. A running session refills that
 * array through `record_complete` before its history is ever fetched, so the
 * proxy went false while the real fact was unchanged — no HTTP window had
 * landed, and `messageOffset` was 0 because it was never set, not because the
 * cache held the head of the conversation. The load then unioned the server's
 * tail window into `[0, 0)` and reported `[0, total)`: `hasOlder === false`
 * (offset 0) over a 50-row array, so scroll-up was dead. Switching away and
 * back cleared the cache and re-ran the load down a branch that adopted the
 * server cursor verbatim — which is why the symptom healed itself there.
 *
 * The fix is a recorded fact (`windowAuthoritative`) instead of a proxy: no
 * window ⇒ adopt the server's cursor verbatim, never union.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { useChatStore, handleMessageEvent } from "./chatStore";
import { useChatAdapterStore } from "../components/chat/chatAdapterStore";

const AGENT = "com.test.Agent";
const SESSION = "sess-window-authority";
const TOTAL = 214;

/** Serve `TOTAL` raw entries; `tail=true` answers the last `limit` (echoing
 *  the forward offset the caller should have asked for), an explicit `offset`
 *  answers `[offset, offset+limit)` — same contract as the Runtime. */
function stubFetch() {
  const entries = Array.from({ length: TOTAL }, (_, i) => ({
    id: `m-${i}`,
    role: i % 2 === 0 ? "user" : "assistant",
    content: `row ${i}`,
    ts: new Date(1_000 + i).toISOString(),
  }));
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      const params = new URL(url).searchParams;
      const limit = Number(params.get("limit") ?? 50);
      const offset =
        params.get("tail") === "true"
          ? Math.max(0, TOTAL - limit)
          : Number(params.get("offset") ?? 0);
      const slice = entries.slice(offset, offset + limit);
      return Promise.resolve({
        ok: true,
        status: 200,
        json: () => Promise.resolve({ messages: slice, offset, limit: slice.length, total: TOTAL }),
      });
    }),
  );
}

/** Prime the session cache the way a live stream does after a Desktop
 *  restart: `record_complete` rows land before any history fetch, so the
 *  array is non-empty while no HTTP window has ever been adopted. */
function seedFromLiveStream(rows: number) {
  useChatStore.setState((s) => ({ ...s, agentStates: {} }));
  for (let i = 0; i < rows; i++) {
    handleMessageEvent(
      {
        type: "record_complete",
        session_id: SESSION,
        message_id: `live-${i}`,
        role: "assistant",
        content: `live ${i}`,
      },
      useChatStore.setState,
      useChatStore.getState,
      AGENT,
    );
  }
}

beforeEach(() => {
  useChatStore.setState((s) => ({ ...s, agentStates: {} }));
  useChatAdapterStore.setState({ sessions: {} });
});
afterEach(() => vi.unstubAllGlobals());

describe("live-seeded cache without an HTTP window", () => {
  it("keeps the pre-fetch cache marked non-authoritative (live rows carry no coverage claim)", () => {
    seedFromLiveStream(3);
    const ss = useChatStore.getState().getSessionState(AGENT, SESSION);
    expect(ss.messages.length).toBe(3);
    // Live rows are real, so nothing is "stale" — but they say nothing about
    // which part of the conversation the cache covers.
    expect(ss.messagesStale).toBe(false);
    expect(ss.windowAuthoritative).toBe(false);
  });

  it("adopts the server tail cursor verbatim, so hasOlder is true and scroll-up works", async () => {
    seedFromLiveStream(3);
    stubFetch();

    await useChatStore.getState().loadSessionMessages(AGENT, SESSION, undefined, 50);

    const ss = useChatStore.getState().getSessionState(AGENT, SESSION);
    // The window is the server's tail window — NOT a fabricated [0, 214).
    expect(ss.messageOffset).toBe(TOTAL - 50);
    expect(ss.messageLimit).toBe(50);
    expect(ss.messageTotal).toBe(TOTAL);
    expect(ss.windowAuthoritative).toBe(true);
    // hasOlder / hasNewer are derived as `offset > 0` / `offset+limit < total`
    // (chatListAdapter.buildSnapshot), so this is the "can scroll up" contract.
    expect(ss.messageOffset).toBeGreaterThan(0);

    // And the older page is actually fetchable from here.
    await useChatStore.getState().loadSessionMessages(AGENT, SESSION, 0, 50);
    const up = useChatStore.getState().getSessionState(AGENT, SESSION);
    expect(up.messageOffset).toBe(0);
    expect(up.messageLimit).toBe(TOTAL);
    expect(up.messageTotal).toBe(TOTAL);
  });

  it("an authoritative window still unions normally (paging up from the tail)", async () => {
    stubFetch();

    // First load: empty cache → tail window adopted.
    await useChatStore.getState().loadSessionMessages(AGENT, SESSION, undefined, 50);
    // Second load: the scroll-up step `loadPrevPage` issues — one contiguous
    // page back from the current left edge — must union, not reset.
    await useChatStore.getState().loadSessionMessages(AGENT, SESSION, TOTAL - 100, 50);

    const ss = useChatStore.getState().getSessionState(AGENT, SESSION);
    expect(ss.messageOffset).toBe(TOTAL - 100);
    expect(ss.messageOffset + ss.messageLimit).toBe(TOTAL);
    expect(ss.messages.length).toBe(100); // both contiguous pages, no holes
  });

  it("clearSessionMessages revokes the window claim (next load adopts again)", async () => {
    stubFetch();
    await useChatStore.getState().loadSessionMessages(AGENT, SESSION, undefined, 50);
    expect(useChatStore.getState().getSessionState(AGENT, SESSION).windowAuthoritative).toBe(true);

    useChatStore.getState().clearSessionMessages(AGENT, SESSION);
    const cleared = useChatStore.getState().getSessionState(AGENT, SESSION);
    expect(cleared.windowAuthoritative).toBe(false);
    expect(cleared.messagesStale).toBe(true);

    // Live rows land again, then the reload must still adopt the tail cursor.
    seedFromLiveStream(2);
    await useChatStore.getState().loadSessionMessages(AGENT, SESSION, undefined, 50);
    const ss = useChatStore.getState().getSessionState(AGENT, SESSION);
    expect(ss.messageOffset).toBe(TOTAL - 50);
    expect(ss.messageLimit).toBe(50);
    expect(ss.messagesStale).toBe(false);
  });
});