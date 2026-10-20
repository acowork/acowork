/**
 * Regression: the ADR-097 in-conversation locate cursor.
 *
 * The window used to be a "loaded count from the tail" model (`tailLoaded`) —
 * it could only page older from the newest page, never sit in the MIDDLE of a
 * conversation. A search hit now jumps the window to an arbitrary physical
 * line and lets the reader walk BOTH ways. These tests pin the row-position
 * cursor (`firstPos` / `lastPos`) arithmetic, because the whole feature lives
 * or dies on `offset` (counts back from newest) ↔ `line_pos` (0-based
 * ascending) translating cleanly against the backend `read_messages`.
 *
 * `fetch` is the seam: a synthetic conversation of `TOTAL` rows where the row
 * at physical position `p` carries `seq = p`, so a window's coverage and the
 * locate target read straight off `seq`.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { useChatSessionStore, type ChatMessageRow } from "./chatSessionStore";

const TOTAL = 120;

function mkRow(pos: number): ChatMessageRow {
  return { seq: pos, ts: pos, from: "u1", kind: "agent_message", body: `m${pos}`, to: "" };
}

/** The paging contract, mirrored from the backend: `offset` counts BACK from
 *  the newest row; the returned page (ascending) covers physical positions
 *  `[total - offset - limit, total - offset)` — clamped at 0. */
function serveMessages(url: string, total: number) {
  const params = new URL(url).searchParams;
  const offset = Number(params.get("offset") ?? 0);
  const limit = Number(params.get("limit") ?? 50);
  const end = total - offset; // exclusive
  const start = Math.max(0, end - limit);
  const messages: ChatMessageRow[] = [];
  for (let p = start; p < end; p++) messages.push(mkRow(p));
  return { ok: true, status: 200, json: async () => ({ messages, total }) };
}

const fetchMock = vi.fn(async (url: string) => {
  if (String(url).includes("/messages")) return serveMessages(String(url), TOTAL);
  return { ok: false, status: 503, json: async () => ({}), text: async () => "" };
});

const win = (id: string) => useChatSessionStore.getState().windows[id]!;
const seqs = (rows: ChatMessageRow[]) => rows.map((r) => r.seq);

beforeEach(() => {
  useChatSessionStore.setState({ byAgent: {}, windows: {}, live: {}, sending: {}, draft: {} });
  fetchMock.mockClear();
  vi.stubGlobal("fetch", fetchMock);
});
afterEach(() => vi.unstubAllGlobals());

describe("refresh: newest page anchors the tail window", () => {
  it("offset 0 → the newest PAGE rows, firstPos/lastPos flush to the tail", async () => {
    await useChatSessionStore.getState().refresh("cs-1", true);
    const w = win("cs-1");
    // offset=0, limit=50 → positions [70,119].
    expect(w.tail).toHaveLength(50);
    expect(w.firstPos).toBe(70);
    expect(w.lastPos).toBe(119);
    expect(w.total).toBe(TOTAL);
    expect(w.pendingLocateSeq).toBeNull();
  });
});

describe("loadOlder: page back from firstPos (replaces the old tailLoaded math)", () => {
  it("offset = total - firstPos; prepend and pull firstPos down", async () => {
    await useChatSessionStore.getState().refresh("cs-1", true); // firstPos=70
    await useChatSessionStore.getState().loadOlder("cs-1"); // offset=120-70=50 → [20,69]
    const w = win("cs-1");
    expect(w.older).toHaveLength(50);
    expect(seqs(w.older)[0]).toBe(20);
    expect(w.firstPos).toBe(20);
    expect(w.lastPos).toBe(119); // tail untouched
  });

  it("walks to the oldest page then stops (firstPos hits 0, no negative offset)", async () => {
    await useChatSessionStore.getState().refresh("cs-1", true);
    await useChatSessionStore.getState().loadOlder("cs-1"); // → firstPos=20
    await useChatSessionStore.getState().loadOlder("cs-1"); // offset=100 → [0,19], 20 rows
    const after = win("cs-1");
    expect(after.firstPos).toBe(0);
    expect(after.older).toHaveLength(70); // 50 + 20
    expect(seqs(after.older)[0]).toBe(0);

    fetchMock.mockClear();
    await useChatSessionStore.getState().loadOlder("cs-1"); // firstPos<=0 → no-op
    expect(fetchMock.mock.calls.length).toBe(0);
    expect(win("cs-1").firstPos).toBe(0);
  });
});

describe("jumpToMessage: land mid-history around a target line_pos", () => {
  it("centers the target (LEAD above) and parks the cursor over it", async () => {
    await useChatSessionStore.getState().refresh("cs-1", true); // total=120
    // Target line 60: topPos=min(119,60+25)=85, offset=119-85=34 → [36,85].
    await useChatSessionStore.getState().jumpToMessage("cs-1", 60, 60);
    const w = win("cs-1");
    expect(w.tail).toHaveLength(50);
    expect(w.firstPos).toBe(36);
    expect(w.lastPos).toBe(85);
    expect(w.older).toEqual([]); // a jump resets the older segment
    expect(w.pendingLocateSeq).toBe(60);
    // The target row is present in the loaded span so the panel can scroll to it.
    expect(seqs(w.tail)).toContain(60);
  });

  it("clamps a near-tail jump so lastPos never runs past the newest row", async () => {
    await useChatSessionStore.getState().refresh("cs-1", true);
    // Target 118: topPos=min(119,143)=119, offset=0 → [70,119] (fresh tail page).
    await useChatSessionStore.getState().jumpToMessage("cs-1", 118, 118);
    const w = win("cs-1");
    expect(w.lastPos).toBe(119);
    expect(w.firstPos).toBe(70);
    expect(seqs(w.tail)).toContain(118);
    expect(w.pendingLocateSeq).toBe(118);
  });

  it("pulls one page first when the window is unknown (total<=0)", async () => {
    // No prior refresh → jump must fetch to learn total before doing locate math.
    await useChatSessionStore.getState().jumpToMessage("cs-2", 10, 10);
    const w = win("cs-2");
    expect(w.total).toBe(TOTAL);
    // total=120, target 10: topPos=min(119,35)=35, offset=84 → [0,35], 36 rows.
    expect(w.firstPos).toBe(0);
    expect(w.lastPos).toBe(35);
    expect(seqs(w.tail)).toContain(10);
    expect(w.pendingLocateSeq).toBe(10);
  });

  it("clearLocate drops the pending scroll target", async () => {
    await useChatSessionStore.getState().refresh("cs-1", true);
    await useChatSessionStore.getState().jumpToMessage("cs-1", 60, 60);
    expect(win("cs-1").pendingLocateSeq).toBe(60);
    useChatSessionStore.getState().clearLocate("cs-1");
    expect(win("cs-1").pendingLocateSeq).toBeNull();
  });
});

describe("loadNewer: walk back down from a mid-history window", () => {
  it("appends the block above lastPos without disturbing older", async () => {
    await useChatSessionStore.getState().refresh("cs-1", true);
    await useChatSessionStore.getState().jumpToMessage("cs-1", 60, 60); // [36,85]
    const before = win("cs-1");
    expect(before.lastPos).toBe(85);
    expect(before.tail.at(-1)!.seq).toBe(85);

    // topPos=min(119,85+50)=119, take=34, offset=119-119=0 → [86,119].
    await useChatSessionStore.getState().loadNewer("cs-1");
    const w = win("cs-1");
    expect(w.lastPos).toBe(119);
    expect(w.tail.at(-1)!.seq).toBe(119);
    expect(w.tail).toHaveLength(50 + 34);
    expect(w.loadingNewer).toBe(false);
  });

  it("is a no-op once the window reaches the tail", async () => {
    await useChatSessionStore.getState().refresh("cs-1", true);
    fetchMock.mockClear();
    await useChatSessionStore.getState().loadNewer("cs-1"); // already at tail
    expect(fetchMock.mock.calls.length).toBe(0);
  });
});

describe("refresh: a parked mid-history reader keeps its place", () => {
  it("a non-forced wake folds total only — never yanks the tail back", async () => {
    await useChatSessionStore.getState().refresh("cs-1", true);
    await useChatSessionStore.getState().jumpToMessage("cs-1", 60, 60); // [36,85], not at tail

    const parked = win("cs-1");
    expect(parked.lastPos).toBeLessThan(parked.total - 1);

    await useChatSessionStore.getState().refresh("cs-1"); // wake path, force defaults false
    const w = win("cs-1");
    // Cursor is untouched by the wake: still the mid-history span.
    expect(w.firstPos).toBe(36);
    expect(w.lastPos).toBe(85);
    expect(seqs(w.tail)).toContain(60);
  });

  it("force (open / send / tab flip) does reset to the newest page", async () => {
    await useChatSessionStore.getState().refresh("cs-1", true);
    await useChatSessionStore.getState().jumpToMessage("cs-1", 60, 60); // park mid
    await useChatSessionStore.getState().refresh("cs-1", true); // forced
    const w = win("cs-1");
    expect(w.firstPos).toBe(70);
    expect(w.lastPos).toBe(119);
  });
});
