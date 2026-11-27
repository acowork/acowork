/**
 * `windowRawSpan` / `turnBoundaryStep` — the two unit-discipline helpers of the
 * paginated transcript (ADR-050 cursors, t-01412679 `groups=stub` projection).
 *
 * The invariant both exist for: `messageOffset` / `messageLimit` / `messageTotal`
 * and a stub's `start` / `end` are RAW JSONL entry coordinates, while
 * `messages[]` holds RECORDS — under `groups=stub` one record covers a whole
 * thought/tool run. Any window test or step that measures with `messages.length`
 * is off by the number of folded runs.
 */
import { describe, expect, it } from "vitest";
import {
  SERVER_PAGE_LIMIT_MAX,
  isAtTail,
  turnBoundaryStep,
  windowRawSpan,
} from "./paginationUtils";

describe("windowRawSpan", () => {
  it("uses the server-confirmed span once a window landed, not the record count", () => {
    // 50 raw entries behind ONE collapsed stub row.
    expect(windowRawSpan({ messageLimit: 50, messages: [{}], windowAuthoritative: true })).toBe(50);
    // The same reading the store's at-tail gate takes: a folded tail page IS
    // the tail (review C1 — with the record count this was false).
    expect(isAtTail(950, windowRawSpan({ messageLimit: 50, messages: [{}], windowAuthoritative: true }), 1000)).toBe(true);
  });

  it("falls back to the array length when live rows seeded the cache (no cursor yet)", () => {
    expect(windowRawSpan({ messageLimit: 0, messages: [{}, {}, {}], windowAuthoritative: false })).toBe(3);
  });
});

describe("turnBoundaryStep", () => {
  it("stops on a bubble and at the head of the log", () => {
    expect(turnBoundaryStep("assistant", 120)).toBeNull();
    expect(turnBoundaryStep("user", 120)).toBeNull();
    expect(turnBoundaryStep("explore_stub", 0)).toBeNull();
    expect(turnBoundaryStep(undefined, 120)).toBeNull();
  });

  it("steps back a full server page for a raw explore head row", () => {
    expect(turnBoundaryStep("thought", 1000)).toEqual({ offset: 500, limit: 500 });
    expect(turnBoundaryStep("tool_result", 1000)).toEqual({ offset: 500, limit: 500 });
  });

  it("treats a compressed stub head as an explore head (it is a run that began earlier)", () => {
    expect(turnBoundaryStep("explore_stub", 1000)).toEqual({ offset: 500, limit: 500 });
  });

  it("clamps at the oldest entry instead of going negative", () => {
    expect(turnBoundaryStep("thought", 120)).toEqual({ offset: 0, limit: 120 });
  });

  it("every step is CONTIGUOUS with the cache — the union math depends on it", () => {
    for (const offset of [1, 120, 499, 500, 501, 100_000]) {
      const step = turnBoundaryStep("tool_call", offset);
      expect(step).not.toBeNull();
      // `offset + limit === messageOffset`: no hole is ever claimed as loaded.
      expect(step!.offset + step!.limit).toBe(offset);
      expect(step!.limit).toBeLessThanOrEqual(SERVER_PAGE_LIMIT_MAX);
    }
  });
});