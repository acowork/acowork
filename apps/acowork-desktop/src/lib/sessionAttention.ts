/**
 * "This session is holding a turn open for a human" — the one rule behind every
 * attention badge (sidebar row, session tab, mobile inbox row).
 *
 * Three interactive cards stop a running turn and then expire on their own: a
 * tool approval and an `ask_user_question` (both 5-minute-ish, then the Runtime
 * answers for the human), and a pause the human must continue or stop
 * (iteration limit / loop detection / debug). None of them carries a message
 * the transcript would announce, so an unselected agent that is parked on one
 * looks exactly like an agent that is thinking — and the card times out while
 * nobody is looking at it. That is the bug this module exists to fix, and it is
 * fixed by NAMING the state rather than by adding a parallel notification
 * channel: the fact is already on the wire (`session_state`), it just used to
 * be read as one undifferentiated "busy".
 *
 * The source of truth is `SessionStatus`, which the Runtime owns and pushes to
 * every client (ADR-014 / ADR-043). Two consequences are deliberate:
 *
 * - **`paused` is not one thing.** A 429 retry wait is also `paused`, but the
 *   Runtime resumes it by itself — a red dot there would teach the user to
 *   ignore red dots. Only a pause with no `retry_info` is a wait for a person.
 * - **`waiting_approval` does not say approval vs question.** The status only
 *   carries a `request_id`; both cards are the same obligation to the reader
 *   ("answer me before this expires"), so they share one reason rather than
 *   being guessed apart from a field that cannot distinguish them.
 *
 * A session status of `null`/absent means "unknown" (never pushed, or pushed by
 * an older Runtime), and unknown is never attention: a badge that appears from
 * an absence is a badge nobody trusts.
 */
import type { SessionStatus } from "./types";

/**
 * Why a session is waiting on a human. The distinction exists for the badge's
 * accessible label (a tooltip that says *what* is being asked for is worth more
 * than one that says "something"), never for the dot's colour: one obligation,
 * one red dot.
 */
export type AttentionReason =
  /** A tool approval or an `ask_user_question` card is open. */
  | "approval"
  /** Iteration limit reached — continue or stop is the human's call. */
  | "iteration_limit"
  /** Loop detector tripped — continue or stop is the human's call. */
  | "loop_detected"
  /** The debugger parked the session (DevMode). */
  | "debug"
  /** A pause we cannot attribute (Runtime predating `PauseReason`) that is
   *  nonetheless not an automatic retry — so it is waiting for someone. */
  | "paused";

/**
 * The obligation this status represents, or `null` when the turn is working on
 * its own. Exhaustive over `SessionStatus`: a new lifecycle state fails
 * compilation here until somebody decides whether it waits for a human.
 */
export function attentionReason(
  status: SessionStatus | null | undefined,
): AttentionReason | null {
  if (!status) return null;
  switch (status.status) {
    case "idle":
    case "llm_awaiting_first_chunk":
    case "thinking":
    case "llm_streaming":
    case "tool_executing":
      return null;
    case "waiting_approval":
      return "approval";
    case "paused": {
      const detail = status.detail;
      // 429 retry: the Runtime is waiting on the provider, not on the human.
      if (detail?.retry_info) return null;
      switch (detail?.reason) {
        case "iteration_limit":
          return "iteration_limit";
        case "loop_detected":
          return "loop_detected";
        case "debug":
          return "debug";
        // No reason at all: a Runtime that predates `PauseReason`. Every pause
        // that is not a retry wait is a wait for a decision, so it counts.
        case undefined:
        default:
          return "paused";
      }
    }
  }
}

/** `true` when this status is blocked on the reader. */
export function needsAttention(
  status: SessionStatus | null | undefined,
): boolean {
  return attentionReason(status) !== null;
}

/**
 * The structural minimum this module needs from a cached session. Kept
 * structural (not `SessionChatState`) so the rule can be used by any surface
 * holding a status, without importing the store.
 */
export interface SessionAttentionSource {
  sessionStatus?: SessionStatus | null;
}

/** The structural minimum this module needs from a cached agent. */
export interface AgentAttentionSource {
  sessionStates: Record<string, SessionAttentionSource | undefined>;
}

/** Sessions of one agent that are waiting on a human. */
export function attentionSessions(
  sessionStates: Record<string, SessionAttentionSource | undefined>,
): Set<string> {
  const out = new Set<string>();
  for (const [sessionId, state] of Object.entries(sessionStates)) {
    if (needsAttention(state?.sessionStatus)) out.add(sessionId);
  }
  return out;
}

/**
 * Is the status the frontend holds for this session one the Runtime can no
 * longer back up?
 *
 * The listing answers two questions about one row, and the second is the
 * staleness check the first cannot do alone: `status` says what is true *if the
 * session is resident*, `is_active` says whether it is resident at all (ADR-038
 * — the key is present in the in-memory session map, and `close_session`
 * removes it). A turn parked on an approval or an iteration-limit pause is a
 * loop *blocked mid-iteration*, so it is necessarily resident. Therefore a row
 * reporting `is_active: false` while the client still shows a non-idle status
 * is reporting a leftover: the push that set it came from a Runtime process
 * that has since died or closed the session, and the retained topic keeps
 * replaying it to every subscriber on every reconnect.
 *
 * That is not cosmetic. The card it advertises no longer exists, so the badge
 * can never be cleared by answering it — which is exactly the badge nobody
 * trusts. This is the pull half of the fact the push half delivers.
 *
 * `is_active === undefined` (a Runtime predating the field) is deliberately
 * "not stale": absence proves nothing, and clearing live state on a guess would
 * hide a real card.
 */
export function isStaleParkedStatus(
  row: { is_active?: boolean },
  held: SessionStatus | null | undefined,
): boolean {
  if (row.is_active !== false) return false;
  return held != null && held.status !== "idle";
}

/**
 * Agent instance id → why it needs the reader, for the sidebar rows.
 *
 * `visible` is the suppression rule, and it is the only thing that makes the
 * badge readable: once the agent's own pending card is on screen the dot has
 * nothing left to tell you — it would just sit there while you look at the
 * dialog. Suppression is per (agent, session), not per agent: an agent whose
 * *other* session is parked still gets the dot, because that card is somewhere
 * the reader is not.
 */
export function attentionByAgent(
  agentStates: Record<string, AgentAttentionSource | undefined>,
  visible?: { agentId: string; sessionId: string | null },
): Map<string, AttentionReason> {
  const out = new Map<string, AttentionReason>();
  for (const [agentId, agent] of Object.entries(agentStates)) {
    if (!agent) continue;
    for (const [sessionId, state] of Object.entries(agent.sessionStates)) {
      const reason = attentionReason(state?.sessionStatus);
      if (!reason) continue;
      if (
        visible &&
        visible.agentId === agentId &&
        visible.sessionId === sessionId
      ) {
        continue;
      }
      // One dot per agent: the first parked session speaks for the agent.
      out.set(agentId, reason);
      break;
    }
  }
  return out;
}
