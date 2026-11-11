/**
 * The group mention grammar — the mobile twin of Desktop's
 * `GroupChatPanel.mention` cases, pinned against the pure functions in
 * `lib/groupMentions` (the client resolves `@Name`, the Gateway never parses
 * free text — design 27 §5.3).
 *
 * What must hold, exactly as on Desktop:
 *   1. longest name wins, and a name inside another word still matches
 *      (WeChat-style, cheap and predictable);
 *   2. only agents become delivery targets — the caller passes the agent
 *      subset, and a human `@name` stays text;
 *   3. the caret rule for the autocomplete: `@` must open a word, whitespace
 *      closes the token, so `a@b` never triggers;
 *   4. an address rides ONLY the targets still mentioned in the text — a chip
 *      whose `@` the human deleted stops being an instruction (and stops
 *      being sent);
 *   5. the `→` chip reads the row's own address fields, never a guess.
 */
import { describe, expect, it } from 'vitest'
import {
  applyMention,
  buildChatAddress,
  detectMention,
  filterMentionCandidates,
  resolveMentions,
  rowToTargets,
} from '../lib/groupMentions'
import type { ChatMessageRow } from '../lib/types'

const A = { id: 'ag-1', name: '小助手' }
const B = { id: 'ag-2', name: '小助手Pro' }
const AGENTS = [A, B]

function row(over: Partial<ChatMessageRow>): ChatMessageRow {
  return { seq: 1, ts: 1, from: 'u1', kind: 'user_message', body: '', to: '', ...over } as ChatMessageRow
}

describe('resolveMentions', () => {
  it('matches the longest name and does not double-count its prefix', () => {
    expect(resolveMentions('让 @小助手Pro 看一下', AGENTS)).toEqual(['ag-2'])
  })

  it('collects every distinct agent mentioned', () => {
    expect(resolveMentions('@小助手 和 @小助手Pro 都看下', AGENTS).sort()).toEqual(['ag-1', 'ag-2'])
  })

  it('a name inside another word still matches (the WeChat rule)', () => {
    expect(resolveMentions('请@小助手一下', AGENTS)).toEqual(['ag-1'])
  })

  it('no mention, no targets — the send stays un-addressed (the ADR-105 instruction)', () => {
    expect(resolveMentions('随便说一句', AGENTS)).toEqual([])
  })
})

describe('detectMention', () => {
  it('finds the open token at the caret', () => {
    const v = '看 @小助'
    expect(detectMention(v, v.length)).toEqual({ query: '小助', start: 2 })
  })

  it('an `@` mid-word never triggers (a@b is an email, not a mention)', () => {
    const v = 'a@b'
    expect(detectMention(v, v.length)).toBeNull()
  })

  it('whitespace closes the token', () => {
    const v = '@foo bar'
    expect(detectMention(v, v.length)).toBeNull()
  })

  it('the caret outside the token means no picker', () => {
    const v = '@小助 已经打完了'
    expect(detectMention(v, v.length)).toBeNull()
  })
})

describe('filterMentionCandidates / applyMention', () => {
  const cands = [
    { id: '1', name: 'Alice' },
    { id: '2', name: 'Alicia' },
    { id: '3', name: 'Bob' },
  ]
  it('filters case-insensitively, substring-style', () => {
    expect(filterMentionCandidates(cands, 'ali').map((c) => c.id)).toEqual(['1', '2'])
    expect(filterMentionCandidates(cands, '')).toEqual(cands)
  })

  it('splices @name over the token and parks the caret past the space', () => {
    const value = 'call @Al'
    const next = applyMention(value, value.length, 5, 'Alice')
    expect(next.value).toBe('call @Alice ')
    expect(next.caret).toBe('call @Alice '.length)
  })
})

describe('buildChatAddress', () => {
  it('an address rides only the still-mentioned targets', () => {
    const addresses = {
      'ag-1': { forceNew: true },
      'ag-2': { refs: [{ agentInstanceId: 'ag-2', sessionId: 's-1' }] },
    }
    // Both mentioned: both ride.
    expect(buildChatAddress(['ag-1', 'ag-2'], addresses)).toEqual({
      refs: [{ agentInstanceId: 'ag-2', sessionId: 's-1' }],
      forceNewFor: ['ag-1'],
    })
    // The `@ag-2` was deleted: its reference is no longer an instruction.
    expect(buildChatAddress(['ag-1'], addresses)).toEqual({ forceNewFor: ['ag-1'] })
  })

  it('nothing chosen, or nothing mentioned, is ABSENCE — not an empty object', () => {
    expect(buildChatAddress([], { 'ag-1': { forceNew: true } })).toBeUndefined()
    expect(buildChatAddress(['ag-1'], {})).toBeUndefined()
  })
})

describe('rowToTargets', () => {
  it('a `to` on the row is the whole answer', () => {
    expect(rowToTargets(row({ to: 'u2', meta: { mentions: ['ag-1'] } }))).toEqual(['u2'])
  })

  it('a human row with no `to` reads its stored mentions', () => {
    expect(rowToTargets(row({ meta: { mentions: ['ag-1', 'u3'] } }))).toEqual(['ag-1', 'u3'])
  })

  it('an echo row answers the sender of the row it continues', () => {
    const echo = row({ kind: 'agent_message', from: 'ag-1' })
    expect(rowToTargets(echo, 'u1')).toEqual(['u1'])
    // And no trigger, no chip: the row carries no address.
    expect(rowToTargets(echo)).toEqual([])
  })
})
