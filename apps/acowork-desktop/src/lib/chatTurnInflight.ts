/**
 * The turn this device just sent, and whether anything has answered it yet
 * (ADR-106 D9, restoring ADR-100 Rev.2's in-flight semantics).
 *
 * A Chat surface may only claim liveness about its OWN send. It used to derive
 * the wait line from the transcript's shape ("the newest human row has no
 * answer"), which D9 retired for a real reason: after a reload, a chat whose
 * node died would go on looking busy forever. What D9 did not allow for was the
 * window between the send's POST and the row that turn produces. The POST only
 * waits for delivery (`wake_and_deliver`), and the next durable fact is written
 * by the Runtime AFTER the Chat Router has routed and the execution session has
 * opened its round — measured at 0.1 s to 81 s of Gateway latency. With nothing
 * in the transcript and the composer unlocked again after ~150 ms, that window
 * reads as "my message went nowhere", and a second send races a router that is
 * still deciding the first one. There is no Chat-side queue to absorb that.
 *
 * So the claim stays first-person and self-limiting:
 * - it starts from the response of this device's own POST (the id the Gateway
 *   gave the human's row), never from a shape read off the newest rows;
 * - it ends at the first row that answers that id — the round's reference row
 *   (from then on the block narrates the run, ADR-106 D3/D9), the router's own
 *   reply, or a receipt (§6.5 budget), or at a **live** delivery alert on that
 *   row (ADR-105 D5 revision 2026-11-06: a failed send is no longer a row of its
 *   own, so nothing would ever close the claim if the alert did not);
 * - and it ends on a deadline, so a client that never sees any of those rows
 *   stops asserting the turn is in flight instead of locking the composer for
 *   good. `TURN_INFLIGHT_DEADLINE_MS` is deliberately the same order as the
 *   sweeper's own tick: the turn gets its fallback receipt at about the moment
 *   the client gives up claiming it.
 */

import type { ChatMessageRow } from "../stores/chatSessionStore";

/** One send this device made and has not seen an outcome for. */
export interface InflightTurn {
  /**
   * The durable `message_id` the Gateway assigned to the human's row, taken
   * from the send's own response. It addresses the row whose `delivery` closes
   * the claim — a locally invented id would wait for a row that cannot exist.
   */
  messageId: string;
  /** Wall clock of the send. Carried for the expiry timer's diagnostics; the
   *  deadline itself is enforced by the store, not by a render-time clock read
   *  (a `Date.now()` inside a memo would make the line's presence depend on
   *  when a re-render happened to occur). */
  at: number;
}

/** Backstop for a turn whose outcome rows never arrive (a Runtime that died
 *  between delivery and round open, a lost receipt). Matches the sweeper's
 *  300 s pass, which is what eventually writes the missing receipt. */
export const TURN_INFLIGHT_DEADLINE_MS = 300_000;

/**
 * Did anything answer the human's row? The service says so: every served row
 * carries `delivery.answered`, computed from the log's `in_reply_to` reverse
 * edge (design 31 §6). This module used to answer the question itself by
 * scanning rows for an outcome kind — that scan had to re-know which kinds
 * count (and which retired milestones do not, ADR-106 D14), and a row whose
 * reference was empty (the card bug) could never close it.
 *
 * A row with no `delivery` is one this device inserted optimistically before
 * the write's response came back: not projected yet, so the claim stands.
 */
export function turnSettled(
  rows: readonly ChatMessageRow[],
  turn?: InflightTurn,
): boolean {
  if (!turn) return true;
  const row = rows.find((r) => r.meta?.message_id === turn.messageId);
  if (!row?.delivery) return false;
  // A live alert IS an outcome: the send did not land, and saying so is what the
  // line was waiting for. Without this the composer would stay locked on a
  // delivery that already failed, which is how a bounced send used to read as a
  // turn still in flight.
  return row.delivery.answered || Boolean(row.delivery.alert);
}

/**
 * Is this surface's own send still owed a first visible outcome? Drives BOTH
 * the centered wait line and the composer lock: one state, so the line can
 * never outlive the lock it explains.
 */
export function turnInflight(
  rows: readonly ChatMessageRow[],
  turn?: InflightTurn,
): boolean {
  return !!turn && !turnSettled(rows, turn);
}
