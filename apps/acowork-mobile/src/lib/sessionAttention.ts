/**
 * "This session is holding a turn open for a human" — the mobile twin of
 * Desktop's `lib/sessionAttention.ts`, and the rule behind the inbox's red dot.
 *
 * Three interactive cards stop a running turn and then expire on their own: a
 * tool approval and an `ask_user_question` (the Runtime answers for the human
 * once the timeout elapses), and a pause the human must continue or stop
 * (iteration limit / loop detection / debug). None of them writes a message, so
 * on a 390pt screen a parked agent and a working agent looked identical — and
 * the card timed out while the phone sat in someone's pocket.
 *
 * The source is the fleet-wide `acowork/agents/+/sessions/+/state` push
 * (`lib/presence.ts`), which already carries the fact. Two rules keep the dot
 * honest, both of them the same ones Desktop applies:
 *
 * - **`paused` is not one thing.** A 429 retry wait is also `paused` but the
 *   Runtime resumes it by itself; only a pause that is not a retry is a wait for
 *   a person.
 * - **Unknown is not attention.** A session we have no status for (never
 *   opened, agent offline, push missed) gets no dot — a badge that appears from
 *   an absence is a badge nobody trusts.
 */

/** The lifecycle fact one `.../sessions/{sid}/state` publish carries for a
 *  session: its status name, plus — for `paused` — why it paused and whether
 *  the pause is a provider retry. Same shape and same rule as Desktop's
 *  `SessionStatus`, so the two inboxes never disagree about what a dot means. */
export interface SessionStatusFact {
  status: string
  reason?: string
  /** `true` for a 429 retry wait — the Runtime resumes it by itself. */
  retry?: boolean
}

/** Why a session is waiting on a human. */
export type AttentionReason =
  /** A tool approval or an `ask_user_question` card is open. */
  | 'approval'
  /** Iteration limit reached — continue or stop is the human's call. */
  | 'iteration_limit'
  /** Loop detector tripped — continue or stop is the human's call. */
  | 'loop_detected'
  /** The debugger parked the session (DevMode). */
  | 'debug'
  /** A pause we cannot attribute (Runtime predating `PauseReason`) that is
   *  nonetheless not an automatic retry — so it is waiting for someone. */
  | 'paused'

/** Statuses that mean "the agent is working on its own" (the inbox's breathing
 *  green dot). The execution surface also gates its composer on this set, so it
 *  is the one place the working vocabulary may be written. */
const WORKING_STATUSES = new Set([
  'llm_awaiting_first_chunk',
  'thinking',
  'llm_streaming',
  'tool_executing',
  'waiting_approval',
  'paused',
])

/** `true` when the session is doing something (including waiting on a human). */
export function isWorking(status: string): boolean {
  return WORKING_STATUSES.has(status)
}

/**
 * The obligation this status represents, or `null` when nothing is owed.
 *
 * A 429 retry wait is `paused` too, but with `retry: true` and no reason — it is
 * the Runtime waiting on a provider, not on a person, so it stays silent. A
 * pause with neither a reason nor `retry` is a Runtime from before `PauseReason`
 * and is still a wait for a decision, so it badges (with a generic label) rather
 * than hiding a card that will expire.
 */
export function attentionReason(fact: SessionStatusFact | null | undefined): AttentionReason | null {
  if (!fact) return null
  switch (fact.status) {
    case 'waiting_approval':
      return 'approval'
    case 'paused':
      if (fact.retry) return null
      switch (fact.reason) {
        case 'iteration_limit':
          return 'iteration_limit'
        case 'loop_detected':
          return 'loop_detected'
        case 'debug':
          return 'debug'
        // No reason at all: a Runtime that predates `PauseReason`. Every pause
        // that is not a retry (already returned above) waits for a decision.
        default:
          return 'paused'
      }
    default:
      return null
  }
}

/**
 * The sentence a badge shows to a screen reader and to a long-press tooltip.
 *
 * It lives beside the rule, not in the screens, because it is the same fact
 * stated once: two surfaces phrasing the same obligation differently is how a
 * user learns to distrust both. Mobile has no i18n layer yet, so the strings
 * are here; Desktop keeps the same set as `agentList.*` / `sessionTabBar.*`
 * keys in its locale files.
 */
export function attentionLabel(reason: AttentionReason | null | undefined): string | undefined {
  switch (reason) {
    case 'approval':
      return '等待你确认'
    case 'iteration_limit':
      return '已到迭代上限，等待你决定'
    case 'loop_detected':
      return '检测到循环，等待你决定'
    case 'debug':
      return '调试暂停，等待你继续'
    case 'paused':
      return '已暂停，等待你继续'
    default:
      return undefined
  }
}

/** `true` when this status is blocked on the reader. */
export function needsAttention(fact: SessionStatusFact | null | undefined): boolean {
  return attentionReason(fact) !== null
}

/** Per-agent map of session id → fact, as the store keeps it. */
export type AgentSessionStatuses = Record<string, Record<string, SessionStatusFact> | undefined>

/** Agents with at least one session the agent is working on (any status in the
 *  working set), for the inbox's breathing dot. */
export function workingAgentIds(all: AgentSessionStatuses): Set<string> {
  const out = new Set<string>()
  for (const [agentId, sessions] of Object.entries(all)) {
    if (!sessions) continue
    for (const fact of Object.values(sessions)) {
      if (isWorking(fact.status)) {
        out.add(agentId)
        break
      }
    }
  }
  return out
}

/** Sessions of one agent that are waiting on a human. */
export function attentionSessions(sessions: Record<string, SessionStatusFact> | undefined): Set<string> {
  const out = new Set<string>()
  for (const [sessionId, fact] of Object.entries(sessions ?? {})) {
    if (needsAttention(fact)) out.add(sessionId)
  }
  return out
}

/**
 * Is the fact the inbox holds for this session one the Runtime can no longer
 * back up?
 *
 * The listing answers two questions about one row, and the second is the
 * staleness check the push cannot do alone. `.../sessions/+/state` is a RETAINED
 * topic: the last publish outlives the process that made it, and every
 * reconnect replays it. `is_active` says whether the session is resident at all
 * (ADR-038). A turn parked on an approval card is a loop blocked mid-iteration,
 * so it is necessarily resident — therefore a row reporting `is_active: false`
 * while the inbox still shows a dot for it is reporting a leftover, and that dot
 * can never be cleared by answering it, because the card no longer exists.
 *
 * `is_active === undefined` (a Runtime predating the field) is deliberately
 * "not stale": absence proves nothing, and clearing live state on a guess would
 * hide a real card.
 */
export function isStaleParkedFact(
  row: { is_active?: boolean },
  fact: SessionStatusFact | null | undefined,
): boolean {
  if (row.is_active !== false) return false
  return fact != null && fact.status !== 'idle'
}

/** Agent instance id → why it needs the reader, for the inbox rows. One dot per
 *  agent; the first parked session speaks for it. */
export function attentionByAgent(all: AgentSessionStatuses): Map<string, AttentionReason> {
  const out = new Map<string, AttentionReason>()
  for (const [agentId, sessions] of Object.entries(all)) {
    for (const fact of Object.values(sessions ?? {})) {
      const reason = attentionReason(fact)
      if (reason) {
        out.set(agentId, reason)
        break
      }
    }
  }
  return out
}
