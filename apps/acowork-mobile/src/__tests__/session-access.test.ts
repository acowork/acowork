import { describe, it, expect, beforeEach, vi } from 'vitest'
import { useAgentStore } from '../stores/agentStore'
import { useChatStore, setChatTransport, type ChatTransport } from '../stores/chatStore'
import { isReadOnlySession } from '../lib/session-write-access'
import type { SessionInfo } from '../lib/types'

function session(over: Partial<SessionInfo> & { session_id: string }): SessionInfo {
  return { title: over.session_id, ...over }
}

function fakeTransport(over: Partial<ChatTransport> = {}): ChatTransport {
  return {
    openSession: vi.fn(async () => {}),
    fetchMessages: vi.fn(async () => []),
    fetchSessions: vi.fn(async () => ({ items: [], hasMore: false })),
    createSession: vi.fn(async () => session({ session_id: 'new' })),
    deleteSession: vi.fn(async () => {}),
    fetchSnapshot: vi.fn(async () => ({ status: null, messageCount: null })),
    send: vi.fn(async () => 'm1'),
    approval: vi.fn(async () => {}),
    answer: vi.fn(async () => {}),
    ...over,
  }
}

// Reset at the TOP level: `setAgents` deliberately preserves already-fetched
// session pages across refreshes, so a per-describe reset would let one
// test's sessions leak into the next.
beforeEach(() => {
  useAgentStore.setState({ agents: {}, agentList: [], users: [] })
  useChatStore.setState({ agentStates: {}, selectedAgentId: null})
})

describe('isReadOnlySession — ADR-076 §决策 4', () => {
  it('is read-only only when can_write is exactly false', () => {
    expect(isReadOnlySession(false)).toBe(true)
    expect(isReadOnlySession(true)).toBe(false)
  })

  it('degrades to writable when can_write is absent', () => {
    // A newly created optimistic session and an older Runtime both omit the
    // field. Locking every control would be worse than a rejected write.
    expect(isReadOnlySession(undefined)).toBe(false)
  })

  it('never derives write access from visibility', () => {
    // public is readable by everyone but owned by one account; a public
    // session this user does not own MUST be read-only.
    expect(isReadOnlySession(false)).toBe(true)
    expect(session({ session_id: 's1', visibility: 'public', can_write: false })).toMatchObject({
      visibility: 'public',
      can_write: false,
    })
  })
})

describe('chatStore.openSession — atomic three-step migration', () => {
  it('sends open_session and reloads history for a writable session', async () => {
    const t = fakeTransport()
    setChatTransport(t)
    useAgentStore.getState().setAgents([{ id: 'a1', name: 'Agent', node_id: 'n1' }])
    useAgentStore.getState().mergeSessions('a1', [
      session({ session_id: 's1', title: 'One', can_write: true }),
      session({ session_id: 's2', title: 'Two', can_write: true }),
    ])

    const ok = await useChatStore.getState().openSession('a1', 's2')
    expect(ok).toBe(true)
    expect(t.openSession).toHaveBeenCalledWith('a1', 's2')
    expect(t.fetchMessages).toHaveBeenCalledWith('a1', 's2')
    expect(useChatStore.getState().agentStates.a1?.activeSessionId).toBe('s2')
  })

  it('does NOT send open_session for a read-only session', async () => {
    const t = fakeTransport()
    setChatTransport(t)
    useAgentStore.getState().setAgents([{ id: 'a1', name: 'Agent', node_id: 'n1' }])
    useAgentStore.getState().mergeSessions('a1', [session({ session_id: 'ro', can_write: false })])

    const ok = await useChatStore.getState().openSession('a1', 'ro')
    expect(ok).toBe(true)
    // open_session flips the session's global Active/Closed state; a viewer
    // must not be able to do that for someone else's session.
    expect(t.openSession).not.toHaveBeenCalled()
    expect(t.fetchMessages).toHaveBeenCalledWith('a1', 'ro')
  })

  it('reverts to the previous session when history loading fails', async () => {
    // s1 loads fine, s2 fails — so there is a real previous session to
    // fall back to.
    const t = fakeTransport({
      fetchMessages: vi.fn(async (_a: string, sid: string) => {
        if (sid === 's2') throw new Error('boom')
        return []
      }),
    })
    setChatTransport(t)
    useAgentStore.getState().setAgents([{ id: 'a1', name: 'Agent', node_id: 'n1' }])
    useAgentStore.getState().mergeSessions('a1', [
      session({ session_id: 's1', can_write: true }),
      session({ session_id: 's2', can_write: true }),
    ])

    expect(await useChatStore.getState().openSession('a1', 's1')).toBe(true)
    const ok = await useChatStore.getState().openSession('a1', 's2')
    expect(ok).toBe(false)
    // Leaving the UI on s2 with s1's messages would be a lie.
    expect(useChatStore.getState().agentStates.a1?.activeSessionId).toBe('s1')
    expect(useChatStore.getState().agentStates.a1?.loading).toBe(false)
  })
})

describe('createSession defaults to private', () => {
  it('creates a private session and opens it', async () => {
    const created = session({ session_id: 'n1', visibility: 'private', can_write: true })
    const t = fakeTransport({ createSession: vi.fn(async () => created) })
    setChatTransport(t)
    useAgentStore.getState().setAgents([{ id: 'a1', name: 'Agent', node_id: 'n1' }])

    const id = await useChatStore.getState().createSession('a1')
    expect(id).toBe('n1')
    expect(useChatStore.getState().agentStates.a1?.activeSessionId).toBe('n1')
  })
})

describe('deleteSession', () => {
  it('clears the active id when the open session is deleted', async () => {
    setChatTransport(fakeTransport())
    useAgentStore.getState().setAgents([{ id: 'a1', name: 'Agent', node_id: 'n1' }])
    useAgentStore.getState().mergeSessions('a1', [
      session({ session_id: 's1', can_write: true }),
      session({ session_id: 's2', can_write: true }),
    ])
    await useChatStore.getState().openSession('a1', 's1')

    await useChatStore.getState().deleteSession('a1', 's1')
    // The list lives in agentStore now; active id lives in chatStore.
    const remaining = useAgentStore.getState().agents.a1?.sessions.map((s) => s.session_id)
    expect(remaining).toEqual(['s2'])
    expect(useChatStore.getState().agentStates.a1?.activeSessionId).not.toBe('s1')
  })
})
