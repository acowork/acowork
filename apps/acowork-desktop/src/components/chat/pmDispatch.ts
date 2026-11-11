/**
 * The PM's dispatch — words the PM generated, spoken in a human's name.
 *
 * A PM task dispatch writes its task body (the title and the description, both
 * written as Markdown by the PM service) into the Chat as a `user_message` row
 * and opens the execution session with the same words as its first turn. The
 * row's voice is the task's creator — a person — but its words are NOT the
 * person's: nobody typed `## 背景` in the composer, and rendering them as
 * literal text paints the Markdown's own syntax as characters (the whole
 * `**bold**` / `##` / `|table|` source on screen).
 *
 * So the two facts are kept apart, and each store carries its own marker for
 * the same one:
 *
 * - the **Chat log** row carries `meta.task_id` — the dispatch's own thread
 *   coordinate, written by the Gateway's dispatch pipeline and declared in the
 *   shared `IntentMeta` registry. The human send path never sets it: a row
 *   somebody typed has no task.
 * - the **session** turn carries the origin the Runtime resolved
 *   (`turn_origin: "pm_task"`). The Runtime knows the turn's origin; a client
 *   that had to read the row's metadata itself would own a copy of the key
 *   name per surface, forever.
 *
 * Absence means "a person typed this" in both stores — never "unknown".
 */

/** The wire value of a PM task dispatch's turn origin. */
export const PM_DISPATCH_ORIGIN = "pm_task";

/**
 * A Chat-log row the PM wrote — the dispatch's `user_message` in the project's
 * group chat.
 *
 * `task_id` is the marker: it is the dispatch's thread coordinate (ADR-106
 * D15) and the shared `IntentMeta` registry declares it, so it survives the
 * storage boundary. The human send path builds its meta from `message_id` /
 * `mentions` / `repo` only — a row somebody typed has no task on it.
 */
export function isPmDispatchChatRow(row: {
  kind: string;
  meta?: { task_id?: string };
}): boolean {
  return row.kind === "user_message" && !!row.meta?.task_id;
}

/**
 * A session turn the PM wrote — the dispatch's first turn in the execution
 * session, which the Runtime marks with the turn's origin.
 */
export function isPmDispatchTurn(turn: { turnOrigin?: string }): boolean {
  return turn.turnOrigin === PM_DISPATCH_ORIGIN;
}
