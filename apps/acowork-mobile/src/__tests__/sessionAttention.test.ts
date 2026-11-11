/**
 * The rule behind the inbox red dot, tested without a socket or a DOM.
 *
 * What is worth pinning here is the distinction the badge lives or dies by: a
 * 429 retry wait is `paused` exactly like an iteration-limit pause, but only
 * one of them is waiting for a PERSON. A dot that fires on both teaches the
 * user to ignore dots.
 */
import { describe, it, expect } from 'vitest'
import {
  attentionByAgent,
  attentionLabel,
  isStaleParkedFact,
  attentionReason,
  attentionSessions,
  isWorking,
  needsAttention,
  workingAgentIds,
  type SessionStatusFact,
} from '../lib/sessionAttention'

describe('attentionReason', () => {
  it('flags an open approval / ask-question card', () => {
    expect(attentionReason({ status: 'waiting_approval' })).toBe('approval')
  })

  it('names why a paused turn is waiting for a person', () => {
    expect(attentionReason({ status: 'paused', reason: 'iteration_limit' })).toBe('iteration_limit')
    expect(attentionReason({ status: 'paused', reason: 'loop_detected' })).toBe('loop_detected')
    expect(attentionReason({ status: 'paused', reason: 'debug' })).toBe('debug')
  })

  it('a pause with no reason at all still waits for a person', () => {
    // A Runtime predating `PauseReason`. Hiding its card would be worse than a
    // generic label, because the card expires either way.
    expect(attentionReason({ status: 'paused' })).toBe('paused')
  })

  it('a 429 retry wait is NOT attention — the Runtime resumes it', () => {
    expect(attentionReason({ status: 'paused', retry: true })).toBeNull()
    // A retry that also carries a reason is still a retry: the flag is the fact.
    expect(attentionReason({ status: 'paused', retry: true, reason: 'iteration_limit' })).toBeNull()
  })

  it('a turn working on its own is not attention', () => {
    for (const status of ['llm_awaiting_first_chunk', 'thinking', 'llm_streaming', 'tool_executing', 'idle']) {
      expect(attentionReason({ status })).toBeNull()
    }
  })

  it('unknown is not attention', () => {
    expect(attentionReason(undefined)).toBeNull()
    expect(attentionReason(null)).toBeNull()
    // A status this build has never heard of (newer Runtime) — never guess.
    expect(attentionReason({ status: 'some_future_state' })).toBeNull()
  })
})

describe('isWorking', () => {
  it('counts every mid-turn status, including the ones waiting on a human', () => {
    for (const status of [
      'llm_awaiting_first_chunk',
      'thinking',
      'llm_streaming',
      'tool_executing',
      'waiting_approval',
      'paused',
    ]) {
      expect(isWorking(status)).toBe(true)
    }
  })

  it('does not count idle or errored as work in flight', () => {
    expect(isWorking('idle')).toBe(false)
    expect(isWorking('errored')).toBe(false)
  })
})

describe('isStaleParkedFact (the pull half of the repair)', () => {
  it('calls a parked fact on a non-resident session stale', () => {
    // The retained state topic outlives the Runtime that published it, so the
    // dot it leaves behind after a restart can never be answered away — only
    // the listing can say the session is no longer in memory.
    expect(isStaleParkedFact({ is_active: false }, { status: 'waiting_approval' })).toBe(true)
    expect(isStaleParkedFact({ is_active: false }, { status: 'paused', reason: 'iteration_limit' })).toBe(true)
  })

  it('leaves a resident session alone — the push is the fresher fact', () => {
    expect(isStaleParkedFact({ is_active: true }, { status: 'waiting_approval' })).toBe(false)
  })

  it('an idle or absent fact is never stale', () => {
    expect(isStaleParkedFact({ is_active: false }, { status: 'idle' })).toBe(false)
    expect(isStaleParkedFact({ is_active: false }, undefined)).toBe(false)
  })

  it('a Runtime that does not report residency proves nothing', () => {
    expect(isStaleParkedFact({}, { status: 'waiting_approval' })).toBe(false)
  })
})

describe('aggregation over the whole inbox', () => {
  const all: Record<string, Record<string, SessionStatusFact> | undefined> = {
    'i-1': { 's-1': { status: 'thinking' }, 's-2': { status: 'waiting_approval' } },
    'i-2': { 's-3': { status: 'paused', retry: true } },
    'i-3': { 's-4': { status: 'idle' } },
    'i-4': undefined,
  }

  it('workingAgentIds: everything mid-turn, retry wait included', () => {
    expect([...workingAgentIds(all)].sort()).toEqual(['i-1', 'i-2'])
  })

  it('attentionByAgent: only the agents actually blocked on a person', () => {
    const out = attentionByAgent(all)
    expect(out.get('i-1')).toBe('approval')
    expect(out.has('i-2')).toBe(false) // 429 retry
    expect(out.has('i-3')).toBe(false) // idle
    expect(out.has('i-4')).toBe(false) // no fact
  })

  it('attentionSessions is per session, so one parked session names itself', () => {
    expect([...attentionSessions(all['i-1'])]).toEqual(['s-2'])
    expect([...attentionSessions(undefined)]).toEqual([])
  })
})

describe('needsAttention / attentionLabel', () => {
  it('needsAttention agrees with attentionReason', () => {
    expect(needsAttention({ status: 'waiting_approval' })).toBe(true)
    expect(needsAttention({ status: 'thinking' })).toBe(false)
    expect(needsAttention(undefined)).toBe(false)
  })

  it('every reason has a label, and "not waiting" has none', () => {
    expect(attentionLabel('approval')).toBeTruthy()
    expect(attentionLabel('iteration_limit')).toBeTruthy()
    expect(attentionLabel('loop_detected')).toBeTruthy()
    expect(attentionLabel('debug')).toBeTruthy()
    expect(attentionLabel('paused')).toBeTruthy()
    expect(attentionLabel(null)).toBeUndefined()
  })
})
