/**
 * The wait-this-turn claim has exactly two ways to be wrong (ADR-106 D9) — the
 * mobile cases of `lib/chatTurnInflight`.
 *
 * It can be asserted from the transcript's shape — "the newest human row has no
 * answer" — which is how a dead node went on looking busy forever after a
 * reload; or it can be dropped as soon as the POST returns, which is how the
 * surface showed nothing at all for the seconds (measured: up to 81) between
 * delivery and the row the turn actually produces.
 *
 * What changed with design 31 §6: the surface no longer decides which rows
 * count as an answer. The service projects `delivery.answered` onto every row
 * it serves, so this module asks one question — was my row answered? — and the
 * policy (round, router reply, live receipt, addressing card; retired
 * milestones never) lives in `acowork-user`'s read model, where Desktop reads
 * the same field.
 */
import { describe, expect, it } from 'vitest'
import { turnInflight, turnSettled, type InflightTurn } from '../lib/chatTurnInflight'
import type { ChatDeliveryView, ChatMessageRow } from '../lib/types'

/** The send this device made. `messageId` is the id the Gateway gave it: it
 *  addresses the row whose `delivery` closes the claim. */
const TURN: InflightTurn = { messageId: 'm-1', at: 1 }

function row(delivery?: ChatDeliveryView, over: Partial<ChatMessageRow> = {}): ChatMessageRow {
  return {
    seq: 2,
    ts: 1,
    from: 'u1',
    kind: 'user_message',
    body: '把分页修一下',
    to: 'agent-a',
    meta: { message_id: 'm-1' },
    ...(delivery ? { delivery } : {}),
    ...over,
  } as ChatMessageRow
}

const UNANSWERED: ChatDeliveryView = { answered: false }
const ANSWERED: ChatDeliveryView = {
  answered: true,
  answered_by: { message_id: 'round:m-1', kind: 'execution_round' },
  targets: [{ agent_instance_id: 'agent-a', agent_session_id: 's-1', round_id: 'r:m-1' }],
}

describe('chatTurnInflight', () => {
  it('waits while the service still says the row is unanswered (D9)', () => {
    // The POST returned and the row is durable, but nothing has answered it:
    // the window that used to be blank, so it MUST read as in flight.
    expect(turnInflight([row(UNANSWERED)], TURN)).toBe(true)
    expect(turnSettled([row(UNANSWERED)], TURN)).toBe(false)
  })

  it('a row the service has not projected yet keeps the claim', () => {
    // The optimistic row this device inserted before the write's response came
    // back carries no `delivery`. Absent is NOT "answered": the send is still
    // owed an outcome, and the deadline is what bounds the wait.
    expect(turnInflight([row(undefined)], TURN)).toBe(true)
  })

  it('the service’s verdict ends the turn, whatever produced the answer', () => {
    // A round, the router's own reply, a receipt and an addressing question are
    // the same fact here: `answered`. Which kinds count is the read model's
    // rule, not a second copy of it in this app.
    expect(turnInflight([row(ANSWERED)], TURN)).toBe(false)
    expect(turnSettled([row(ANSWERED)], TURN)).toBe(true)
  })

  it('another row’s answer never settles this send', () => {
    // Two sends in the same chat: the claim is keyed by this device's own id,
    // so the other turn's projection is not evidence about this one.
    const otherRound = row(ANSWERED, { seq: 1, meta: { message_id: 'm-0' } })
    expect(turnInflight([otherRound, row(UNANSWERED)], TURN)).toBe(true)
  })

  it('with no send of our own there is no claim to make', () => {
    // This is the reload case: a transcript full of unanswered rows is NOT
    // evidence of liveness (D9) — a dead node would look busy forever.
    expect(turnInflight([row(UNANSWERED)], undefined)).toBe(false)
    expect(turnSettled([row(UNANSWERED)], undefined)).toBe(true)
  })
})
