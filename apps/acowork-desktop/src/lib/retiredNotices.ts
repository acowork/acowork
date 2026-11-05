/**
 * The `system_notice` types the transcript renders as **nothing** (ADR-106 D14,
 * design 27 §5.5.5).
 *
 * v3.1 removed the writers, not the rows: an old transcript still carries
 * "queued" milestones, the routing reaction and the card-resolution receipt, and
 * the log keeps recording whatever it was given. History has to stay readable
 * *and* honest, so these rows stay on disk and are hidden at render time —
 * resurrecting them would narrate a routing decision and a decision card that
 * the reference row + the round's block now own (D8/D12), and would show a
 * milestone as if it were speech.
 *
 * One spelling, two readers: [ChatNoticeEventRow.tsx](../components/chat/ChatNoticeEventRow.tsx)
 * paints them as nothing, and [chatTurnInflight.ts](chatTurnInflight.ts) must not
 * treat them as the outcome that closed a turn — a row nobody can see cannot be
 * the fact that made the human's message visibly handled. `interaction_request`
 * is absent on purpose: it is a *kind*, not a notice type, and both surfaces drop
 * that kind before they reach this set.
 *
 * Desktop and Mobile keep the same set (design 27 §5.5.4: "Desktop 两视图 +
 * Mobile 同一套隐藏集合"); the mobile copy is
 * [../../acowork-mobile/src/lib/retiredNotices.ts](../../acowork-mobile/src/lib/retiredNotices.ts).
 */
export const RETIRED_NOTICES: ReadonlySet<string> = new Set([
  "turn_dispatched",
  "turn_started",
  "turn_routed",
  "router_fallback",
  "interaction_resolved",
  // Not a milestone but the same class: a receipt nobody retracts. `delivery_failed`
  // was written as a durable row and left there, so a failure that a retry later
  // resolved still told every reader the message had not been delivered. The live
  // alert on the human's row (`delivery.alert`) replaced it (ADR-105 D5, revision
  // 2026-11-06); the rows history holds are hidden here.
  "delivery_failed",
]);