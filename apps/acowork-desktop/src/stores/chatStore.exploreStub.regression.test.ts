/**
 * Regression t-01412679 (review C1 / I6) — the `groups=stub` page and the lazy
 * explore-detail read, exercised through the real store.
 *
 * C1: the array and the cursor are in DIFFERENT units. A `groups=stub` page
 * collapses a whole thought/tool run into one record, so `messages.length <
 * limit`. The Runtime used to echo the folded count as `limit`, which made the
 * cache adopt a window that stopped short of the tail: `isAtTail` went false on
 * a tail page, so `record_complete` no longer wrote `messages[]` (live bubbles
 * vanished) and `hasNewer` re-triggered `loadNextPage` forever. The contract
 * pinned here — offset/limit/total count RAW entries and `limit` is the
 * confirmed span — is what makes `windowRawSpan` the correct unit for every
 * window test.
 *
 * I6: detail is fetched on first expand only, deduped while in flight, cached
 * on success, and a FAILED read stays retryable (the error is a state of the
 * same fetch, not a terminal cache entry).
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { useChatStore } from "./chatStore";
import { useChatAdapterStore } from "../components/chat/chatAdapterStore";
import { isAtTail, windowRawSpan } from "../lib/paginationUtils";
import type { ExploreStubMeta } from "../lib/types";

const AGENT = "com.test.Agent";
const SESSION = "sess-explore-stub";
const TOTAL = 1000;
const PAGE_LIMIT = 50;

const STUB_META: ExploreStubMeta = {
  start: TOTAL - PAGE_LIMIT,
  end: TOTAL,
  rawCount: PAGE_LIMIT,
  tools: ["bash"],
  toolsTruncated: false,
};

/** One `groups=stub` tail page: a single collapsed row addressing the whole
 *  window, echoed the way the Runtime now answers (offset/limit/total in raw
 *  entries, `limit` = the span it actually confirmed). */
function stubPages(pageBody?: unknown) {
  const calls: string[] = [];
  const fetchMock = vi.fn((url: string) => {
    calls.push(url);
    if (url.includes("/messages/detail")) {
      return Promise.resolve({
        ok: true,
        status: 200,
        json: () =>
          Promise.resolve({
            messages: [
              { id: "d1", role: "thought", content: "hidden reasoning", ts: new Date(1).toISOString() },
              { id: "d2", role: "tool_call", content: "{}", ts: new Date(2).toISOString(), metadata: { tool_name: "bash", tool_call_id: "c1" } },
            ],
          }),
      });
    }
    return Promise.resolve({
      ok: true,
      status: 200,
      json: () =>
        Promise.resolve(
          pageBody ?? {
            messages: [
              {
                id: "stub-1",
                role: "assistant",
                kind: "explore_stub",
                content: "",
                ts: new Date(TOTAL).toISOString(),
                metadata: {
                  start: STUB_META.start,
                  end: STUB_META.end,
                  raw_count: STUB_META.rawCount,
                  tools: STUB_META.tools,
                  tools_truncated: false,
                },
              },
            ],
            offset: TOTAL - PAGE_LIMIT,
            limit: PAGE_LIMIT,
            total: TOTAL,
          },
        ),
    });
  });
  vi.stubGlobal("fetch", fetchMock);
  return { calls, fetchMock };
}

beforeEach(() => {
  useChatStore.setState((s) => ({ ...s, agentStates: {} }));
  useChatAdapterStore.setState({ sessions: {} });
});
afterEach(() => vi.unstubAllGlobals());

describe("groups=stub page cursor (review C1)", () => {
  it("treats a folded tail page as the tail: the window is the raw span, not the row count", async () => {
    const { calls } = stubPages();
    await useChatStore.getState().loadSessionMessages(AGENT, SESSION, undefined, PAGE_LIMIT);

    const ss = useChatStore.getState().getSessionState(AGENT, SESSION);
    // ONE record, but it addresses the whole 50-entry window.
    expect(ss.messages.length).toBe(1);
    expect(ss.messages[0].type).toBe("explore_stub");
    expect(ss.messageOffset).toBe(TOTAL - PAGE_LIMIT);
    expect(ss.messageLimit).toBe(PAGE_LIMIT);
    expect(ss.messageTotal).toBe(TOTAL);

    expect(windowRawSpan(ss)).toBe(PAGE_LIMIT);
    // The gate that keeps live bubbles flowing and stops `hasNewer` from
    // re-fetching the tail forever.
    expect(isAtTail(ss.messageOffset, windowRawSpan(ss), ss.messageTotal)).toBe(true);

    // Lazy contract: the page load must NOT pull any run body.
    expect(calls.some((u) => u.includes("/messages/detail"))).toBe(false);
  });
});

describe("fetchExploreDetail (review I6)", () => {
  it("dedupes an in-flight read and caches the result by stub id", async () => {
    const { calls } = stubPages();
    const store = useChatStore.getState();

    const [a, b] = await Promise.all([
      store.fetchExploreDetail(AGENT, SESSION, "stub-1", STUB_META),
      store.fetchExploreDetail(AGENT, SESSION, "stub-1", STUB_META),
    ]);
    expect(a?.length).toBe(2);
    expect(b?.length).toBe(2);
    expect(calls.filter((u) => u.includes("/messages/detail")).length).toBe(1);

    // Second expand reads the cache — still one network read.
    const again = await useChatStore.getState().fetchExploreDetail(AGENT, SESSION, "stub-1", STUB_META);
    expect(again?.length).toBe(2);
    expect(calls.filter((u) => u.includes("/messages/detail")).length).toBe(1);

    const entry = useChatStore.getState().getSessionState(AGENT, SESSION).exploreDetails["stub-1"];
    expect(entry?.rows?.length).toBe(2);
    expect(entry?.error).toBeUndefined();
  });

  it("records a failure as retryable state, and a retry recovers", async () => {
    let detailStatus = 500;
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (url.includes("/messages/detail")) {
          return Promise.resolve({
            ok: detailStatus === 200,
            status: detailStatus,
            json: () =>
              Promise.resolve({
                messages: [{ id: "d1", role: "thought", content: "hidden reasoning", ts: new Date(1).toISOString() }],
              }),
          });
        }
        return Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve({ messages: [] }) });
      }),
    );

    const failed = await useChatStore.getState().fetchExploreDetail(AGENT, SESSION, "stub-9", STUB_META);
    expect(failed).toBeNull();
    let entry = useChatStore.getState().getSessionState(AGENT, SESSION).exploreDetails["stub-9"];
    expect(entry?.error).toBe(true);
    expect(entry?.rows).toBeUndefined();

    // A failed read is NOT final — expanding again is the reader asking again.
    detailStatus = 200;
    const ok = await useChatStore.getState().fetchExploreDetail(AGENT, SESSION, "stub-9", STUB_META);
    expect(ok?.length).toBe(1);
    entry = useChatStore.getState().getSessionState(AGENT, SESSION).exploreDetails["stub-9"];
    expect(entry?.rows?.length).toBe(1);
    expect(entry?.error).toBeUndefined();
  });
});