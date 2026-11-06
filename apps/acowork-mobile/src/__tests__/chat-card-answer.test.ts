/**
 * Answering an addressing card is a RE-DELIVERY (ADR-106 D6) — the mobile cases
 * of `lib/chatCardAnswer`.
 *
 * The rule these hold is the one the decision was written for: the answer to
 * "which task did you mean?" is the human's OWN message, sent again with the
 * address they just chose. So a surface must refuse to build an answer out of
 * anything else — a click that named neither a session nor new work would
 * deliver an unaddressed turn as if it were answered.
 *
 * Where the words come FROM moved out of this module (design 31 §6): the
 * service resolves them from the whole log, so a card whose message sits
 * outside the loaded window still answers. `lib/api#fetchCardAnswer` carries
 * that half; here we only fold in the click.
 *
 * Mobile is 1:1 (design 27 §5.4.4), so the address takes the single-session form
 * and the group's per-agent shape does not exist here.
 */
import { describe, expect, it } from 'vitest'
import { answerWithChoice, directCardAddress, mayAnswerCard } from '../lib/chatCardAnswer'
import type { CardAnswerFacts } from '../lib/api'
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

/** What `fetchCardAnswer` returns for a card about `m-1`. */
function facts(over: Partial<CardAnswerFacts> = {}): CardAnswerFacts {
  return {
    agentId: 'ag-1',
    messageId: 'm-1',
    text: '把季度报表整理一下',
    attachmentIds: [],
    refs: [],
    ...over,
  }
}

describe('answerWithChoice', () => {
  it('re-delivers the original words, addressed, keyed by the original row id', () => {
    const answer = answerWithChoice(facts(), { sessionId: 's-9' })
    expect(answer).toEqual({
      text: '把季度报表整理一下',
      attachmentIds: [],
      agentId: 'ag-1',
      messageId: 'm-1',
      sessionId: 's-9',
      forceNew: false,
    })
  })

  it('keeps the files the original message had', () => {
    // The ids are resolved where the log is read (`chat_card::attachment_ids_of`),
    // so a re-delivery cannot quietly drop them.
    expect(answerWithChoice(facts({ attachmentIds: ['a-1', 'a-2'] }), { forceNew: true })?.attachmentIds).toEqual([
      'a-1',
      'a-2',
    ])
  })

  it('refuses a question with no author — whose runs are on offer would be unknown', () => {
    expect(answerWithChoice(facts({ agentId: '' }), { sessionId: 's-9' })).toBeNull()
  })

  it('refuses a click that named neither a session nor new work', () => {
    expect(answerWithChoice(facts(), {})).toBeNull()
  })

  it('treats a blank session id as no session named', () => {
    expect(answerWithChoice(facts(), { sessionId: '  ' })).toBeNull()
    expect(answerWithChoice(facts(), { sessionId: '  ', forceNew: true })?.forceNew).toBe(true)
  })

  it('re-delivers the ORIGINAL message quoted material, pinned coords included (ADR-106 rev 2)', () => {
    // A card answer is the same sentence, so the reference it first carried must
    // ride back verbatim — the pre-pinned {round, terminal} are NOT re-resolved
    // here (the Gateway passes an already-pinned ref straight through). Which
    // refs are this agent's own is decided where the log is read.
    const refs = [{ agentInstanceId: 'ag-1', sessionId: 's-7', roundId: 'r-3', terminal: true }]
    const answer = answerWithChoice(facts({ refs }), { sessionId: 's-9' })!
    expect(answer.refs).toEqual(refs)
  })

  it('omits the refs key when the original utterance carried no material', () => {
    expect(answerWithChoice(facts(), { sessionId: 's-9' })).not.toHaveProperty('refs')
  })
})

describe('directCardAddress', () => {
  it('names the session when one was picked, and carries the dedup id either way', () => {
    const answer = answerWithChoice(facts(), { sessionId: 's-9' })!
    expect(directCardAddress(answer)).toEqual({ targetSession: 's-9', messageId: 'm-1' })
    const fresh = answerWithChoice(facts(), { forceNew: true })!
    expect(directCardAddress(fresh)).toEqual({ forceNew: true, messageId: 'm-1' })
  })

  it('carries the echoed references onto the address so the send wires them (ADR-106 rev 2)', () => {
    const refs = [{ agentInstanceId: 'ag-1', sessionId: 's-7', roundId: 'r-3', terminal: true }]
    const answer = answerWithChoice(facts({ refs }), { sessionId: 's-9' })!
    expect(directCardAddress(answer)).toEqual({
      targetSession: 's-9',
      messageId: 'm-1',
      refs,
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
