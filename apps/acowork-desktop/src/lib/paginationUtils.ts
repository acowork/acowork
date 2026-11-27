/**
 * Pagination utility functions shared between chatStore and chatListAdapter.
 *
 * ADR-050: The forward (oldest-end) offset model means `offset=0` is the
 * oldest entry and `offset + limit >= total` is the newest (tail).
 */
import { isExploreBlockRow } from "../components/chat/messageParts";

/**
 * Determine whether the current cache window covers the tail of the
 * conversation.  When true, real-time data (MQTT stream_delta previews
 * and record_complete writes) should be injected into the rendering
 * pipeline.
 *
 * `limit === 0` (fresh session, no HTTP load yet) is considered at-tail
 * so that liveBuffer streaming previews and record_complete writes are
 * active immediately - the user must see their optimistic message and
 * any streaming preview even before the initial HTTP load completes.
 *
 * NOTE: This function answers "should real-time data be injected?".
 * It is NOT the right check for "should we skip loading the tail page?"
 * - use `messageLimit > 0 && ...` for that (scrollToBottom /
 *   ensureLatestInCache), because `limit === 0` means no page is loaded
 *   and an HTTP request IS needed.
 */
export function isAtTail(
  offset: number,
  limit: number,
  total: number,
): boolean {
  return limit === 0 || offset + limit >= total;
}

/**
 * How many RAW entries the cached window covers.
 *
 * `messageOffset` / `messageLimit` / `messageTotal` are all measured in raw
 * JSONL entries, but the array they describe is not: under the `groups=stub`
 * page projection (t-01412679) a contiguous thought/tool run collapses into ONE
 * record addressing `end - start` entries, so `messages.length <= limit`.
 *
 * Every "is this coordinate inside the window?" test must use THIS span, never
 * `messages.length` — mixing the two units is what makes a locate hit look out
 * of bounds and a tail page look mid-history (review C1/I4).
 *
 * `messageLimit` is only meaningful once an HTTP window landed
 * (`windowAuthoritative`); a cache seeded purely by live rows has no cursor and
 * its array length is the best available count.
 */
export function windowRawSpan(state: {
  messageLimit: number;
  messages: readonly unknown[];
  windowAuthoritative: boolean;
}): number {
  return state.windowAuthoritative
    ? state.messageLimit
    : state.messages.length;
}

/**
 * Largest page the Runtime will serve (`messages` clamps `limit` to it). A
 * turn-boundary walk may ask for a page this big on purpose — see
 * `turnBoundaryStep`.
 */
export const SERVER_PAGE_LIMIT_MAX = 500;

/**
 * How far back the "expand the window to the start of the turn" walk should
 * step, or `null` when the window already begins at a bubble and the walk is
 * done.
 *
 * The walk exists because one turn can produce more raw entries than a page: a
 * tail window can fold to a LONE explore block, leaving the user message that
 * opened the turn unloaded (`chatStore.expandWindowToTurnBoundary`).
 *
 * Two rules, both structural:
 *
 *  - The fetched page must stay CONTIGUOUS with the cache. `loadSessionMessages`
 *    unions two windows by their offsets (`[min, max)`), so a page with a hole
 *    above it would silently claim coverage of rows nobody fetched. Stepping
 *    back by exactly one page width keeps `newOffset + limit === messageOffset`.
 *  - The step is the SERVER's page ceiling, not the caller's `limit`. A run
 *    longer than `limit` would otherwise need one iteration per 50 entries and
 *    the walk's iteration cap (`MAX_EXPAND_PAGES`) turned "long run" into "gave
 *    up mid-run, head block still half". Under `groups=stub` these backfill
 *    pages are cheap by construction: every run in them collapses to one row.
 */
export function turnBoundaryStep(
  firstRowType: string | undefined | null,
  messageOffset: number,
): { offset: number; limit: number } | null {
  if (!firstRowType || messageOffset <= 0) return null;
  if (!isExploreBlockRow(firstRowType)) return null;
  const offset = Math.max(0, messageOffset - SERVER_PAGE_LIMIT_MAX);
  // `offset + limit === messageOffset`: the page butts up against the cache,
  // so the union stays exact and no unfetched row is ever claimed as loaded.
  return { offset, limit: messageOffset - offset };
}
