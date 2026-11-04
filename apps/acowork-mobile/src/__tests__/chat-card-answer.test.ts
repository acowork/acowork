/**
 * Answering an addressing card is a RE-DELIVERY (ADR-106 D6) — the mobile cases
 * of `lib/chatCardAnswer`.
 *
 * The rule these hold is the one the decision was written for: the answer to
 * "which task did you mean?" is the human's OWN message, sent again with the
 * address they just chose. So a surface must refuse to build an answer out of
 * anything else — a card whose original row is unreadable, or a click that named
 * neither a session nor new work, would put words in the human's mouth or deliver
 * an unaddressed turn as if it were answered.
 *
 * Mobile is 1:1 (design 27 §5.4.4), so the address takes the single-session form
 * and the group's per-agent shape does not exist here.
 */
import { describe, expect, it } from 'vitest'
import {
  buildCardAnswer,
  directCardAddress,
  mayAnswerCard,
} from '../lib/chatCardAnswer'
import type { ChatMessageRow } from '../lib/types'

function card(meta: ChatMessageRow['meta'] = {}, from = 'ag-1'): ChatMessageRow {
  return {
    seq: 2,
    ts: 2,
    from,
    kind: 'system_notice',
    body: '',
    to: 'u1',
    meta: { message_id: 'n-1', notice_type: 'session_choice', in_reply_to: 'm-1', ...meta },
  } as ChatMessageRow
}

function original(over: Partial<ChatMessageRow> = {}): ChatMessageRow {
  return {
    seq: 1,
    ts: 1,
    from: 'u1',
    kind: 'user_message',
    body: '把季度报表整理一下',
    to: 'ag-1',
    meta: { message_id: 'm-1' },
    ...over,
  } as ChatMessageRow
}

describe('buildCardAnswer', () => {
  it('re-delivers the original words, addressed, keyed by the original row id', () => {
    const answer = buildCardAnswer(card(), original(), { sessionId: 's-9' })
    expect(answer).toEqual({
      text: '把季度报表整理一下',
      attachmentIds: [],
      agentId: 'ag-1',
      messageId: 'm-1',
      sessionId: 's-9',
      forceNew: false,
    })
  })

  it('keeps the files the original message had — resolved meta first', () => {
    const withMeta = original({
      attachments: [
        { id: 'a-1', filename: 'shot.png', mime: 'image/png', size: 1 },
        { id: 'a-2', filename: 'log.txt', mime: 'text/plain', size: 2 },
      ],
    } as Partial<ChatMessageRow>)
    expect(buildCardAnswer(card(), withMeta, { forceNew: true })?.attachmentIds).toEqual(['a-1', 'a-2'])

    const withIds = original({ attachment_ids: ['a-3'] } as Partial<ChatMessageRow>)
    expect(buildCardAnswer(card(), withIds, { forceNew: true })?.attachmentIds).toEqual(['a-3'])
  })

  it('refuses when the question is not a faithful re-delivery', () => {
    // The card points at nothing the surface can read.
    expect(buildCardAnswer(card(), undefined, { sessionId: 's-9' })).toBeNull()
    // A card with no message behind it (a hand-written notice, an old row).
    expect(buildCardAnswer(card({ in_reply_to: undefined }), original(), { sessionId: 's-9' })).toBeNull()
    // A question with no author: whose runs are on offer is unknown.
    expect(buildCardAnswer(card({}, ''), original(), { sessionId: 's-9' })).toBeNull()
    // A click that named neither a session nor new work.
    expect(buildCardAnswer(card(), original(), {})).toBeNull()
  })

  it('treats a blank session id as no session named', () => {
    expect(buildCardAnswer(card(), original(), { sessionId: '  ' })).toBeNull()
    expect(buildCardAnswer(card(), original(), { sessionId: '  ', forceNew: true })?.forceNew).toBe(true)
  })

  it('re-delivers the ORIGINAL message quoted material, pinned coords included (ADR-106 rev 2)', () => {
    // A card answer is the same sentence, so the reference it first carried must
    // ride back verbatim — the pre-pinned {round, terminal} are NOT re-resolved
    // here (the Gateway passes an already-pinned ref straight through).
    const withRefs = original({
      meta: {
        message_id: 'm-1',
        refs: [
          {
            agent_instance_id: 'ag-1',
            session_id: 's-7',
            round_id: 'r-3',
            terminal: true,
          },
        ],
      },
    } as Partial<ChatMessageRow>)
    const answer = buildCardAnswer(card(), withRefs, { sessionId: 's-9' })!
    expect(answer.refs).toEqual([
      { agentInstanceId: 'ag-1', sessionId: 's-7', roundId: 'r-3', terminal: true },
    ])
  })

  it('drops references that belong to another agent, and omits the key when nothing survives', () => {
    const foreign = original({
      meta: {
        message_id: 'm-1',
        refs: [
          {
            agent_instance_id: 'ag-OTHER',
            session_id: 's-7',
            round_id: 'r-3',
            terminal: true,
          },
        ],
      },
    } as Partial<ChatMessageRow>)
    // The answering card was authored by ag-1; another instance's material is
    // not this re-delivery's to carry.
    expect(buildCardAnswer(card(), foreign, { sessionId: 's-9' })).not.toHaveProperty('refs')
  })
})

describe('directCardAddress', () => {
  it('names the session when one was picked, and carries the dedup id either way', () => {
    const answer = buildCardAnswer(card(), original(), { sessionId: 's-9' })!
    expect(directCardAddress(answer)).toEqual({ targetSession: 's-9', messageId: 'm-1' })
    const fresh = buildCardAnswer(card(), original(), { forceNew: true })!
    expect(directCardAddress(fresh)).toEqual({ forceNew: true, messageId: 'm-1' })
  })

  it('carries the echoed references onto the address so the send wires them (ADR-106 rev 2)', () => {
    const withRefs = original({
      meta: {
        message_id: 'm-1',
        refs: [
          {
            agent_instance_id: 'ag-1',
            session_id: 's-7',
            round_id: 'r-3',
            terminal: true,
          },
        ],
      },
    } as Partial<ChatMessageRow>)
    const answer = buildCardAnswer(card(), withRefs, { sessionId: 's-9' })!
    expect(directCardAddress(answer)).toEqual({
      targetSession: 's-9',
      messageId: 'm-1',
      refs: [{ agentInstanceId: 'ag-1', sessionId: 's-7', roundId: 'r-3', terminal: true }],
    })
  })
})

describe('mayAnswerCard (D10)', () => {
  it('only the person the card asked may answer it', () => {
    expect(mayAnswerCard(card(), 'u1')).toBe(true)
    expect(mayAnswerCard(card(), 'u2')).toBe(false)
  })

  it('local mode has one person and no id, so it never hides the card', () => {
    expect(mayAnswerCard(card(), '')).toBe(true)
    expect(mayAnswerCard(card({}, ''), '')).toBe(true)
  })

  it('a card that named no addressee is answerable by whoever is reading', () => {
    // `to` is the human the card asked; a notice broadcast to the whole channel
    // leaves it empty, and gateing on an absent addressee would hide the card
    // from the only people who can see it at all.
    expect(mayAnswerCard({ ...card(), to: '' }, 'u2')).toBe(true)
  })
})
