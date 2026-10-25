/**
 * The routing reaction row (ADR-100 D11) on the mobile chat surface.
 *
 * The reaction is a `system_notice` by producer but an agent bubble by
 * rendering — the predicate and the wait rule are the mobile twins of the
 * desktop `chatTurnState` tests, pinned here so the two apps cannot drift on
 * the interaction-feel invariant: a user message gets its agent answer within
 * seconds, and the live line keeps covering the run until the terminal row.
 */
import { describe, expect, it } from 'vitest'
import type { ChatMessageRow } from '../lib/types'

import { isTurnRoutedNotice, opensAgentTurn, pendingTurn } from '../screens/chat/AgentChatScreen'

let seq = 0
function row(kind: string, meta: ChatMessageRow['meta'] = {}, body = ''): ChatMessageRow {
  seq += 1
  return { seq, ts: seq, from: 'ag', kind, body, to: 'u', meta } as ChatMessageRow
}

const user = (id = 'u1') => row('user_message', { message_id: id }, 'review the repo')
const reaction = (trigger: string, mode: 'new' | 'continue' = 'new') =>
  row('system_notice', {
    message_id: `tr:${trigger}`,
    notice_type: 'turn_routed',
    in_reply_to: trigger,
    agent_session_id: 'exec-1',
    turn_mode: mode,
  })
const reply = (inReplyTo: string, status = 'completed') =>
  row('agent_message', { message_id: `a:${inReplyTo}`, in_reply_to: inReplyTo, status }, 'done')

describe('isTurnRoutedNotice', () => {
  it('matches only the turn_routed notice', () => {
    expect(isTurnRoutedNotice(reaction('u1'))).toBe(true)
    expect(isTurnRoutedNotice(row('system_notice', { notice_type: 'delivery_failed' }))).toBe(false)
    expect(isTurnRoutedNotice(row('agent_message', { notice_type: 'turn_routed' }))).toBe(false)
  })
})

describe('pendingTurn with the routing reaction', () => {
  const none = new Set<string>()

  it('keeps waiting after the reaction — the run is still in flight', () => {
    expect(pendingTurn([user(), reaction('u1')], false, none)).toBe(true)
  })

  it('ends at the terminal reply', () => {
    expect(pendingTurn([user(), reaction('u1'), reply('u1')], false, none)).toBe(false)
  })
})

describe('pendingTurn only waits on a row that woke an agent', () => {
  const none = new Set<string>()
  /** As the archive-only chat write entry files it: no recipient, no mentions —
   *  a `user_user` DM and an un-@'d group remark. Same rule as the desktop. */
  const plain = (id = 'u1') => ({ ...row('user_message', { message_id: id }, 'hi'), to: '' }) as ChatMessageRow

  it('never waits on a row that asked nothing of an agent', () => {
    expect(pendingTurn([plain()], false, none)).toBe(false)
    // Skipped, not an answer: the turn that did ask keeps its wait.
    expect(pendingTurn([user('u1'), plain('u2')], false, none)).toBe(true)
  })

  it('waits on the fan-out row, whose broadcast copy names its agents in meta', () => {
    const fanout = { ...plain(), meta: { message_id: 'u1', mentions: ['ag'] } } as ChatMessageRow
    expect(pendingTurn([fanout], false, none)).toBe(true)
    expect(opensAgentTurn(plain())).toBe(false)
    expect(opensAgentTurn({ ...plain(), to: ' ' } as ChatMessageRow)).toBe(false)
  })
})
