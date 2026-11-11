/**
 * The group list — the mobile twin of Desktop's projectGroupStore read path
 * (design 27 §4.4). What must hold, exactly as on Desktop:
 *   1. `GET /api/chat-sessions/{me}` is the ONE source; the list keeps only
 *      the group kinds (`project_group` / `user_group`) — the 1:1 pairs and
 *      DM twins belong to other inbox sections and must not appear twice;
 *   2. most-recently-active first;
 *   3. a failed refresh keeps the last list — an unreachable Gateway must
 *      not empty the inbox of the groups you were just reading;
 *   4. no account (local mode) means no groups.
 */
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { ChatSessionInfo } from '../lib/types'

const api = vi.hoisted(() => ({
  fetchMyChatSessions: vi.fn(),
}))

vi.mock('../lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../lib/api')>()
  return { ...actual, fetchMyChatSessions: api.fetchMyChatSessions }
})

import { useAuthStore } from '../stores/authStore'
import { useProjectGroupStore, wakeGroupChat } from '../stores/projectGroupStore'

function group(sessionId: string, over: Partial<ChatSessionInfo> = {}): ChatSessionInfo {
  return {
    session_id: sessionId,
    kind: 'project_group',
    title: sessionId,
    last_active_at: 1,
    message_count: 0,
    participants: [],
    ...over,
  } as ChatSessionInfo
}

beforeEach(() => {
  vi.clearAllMocks()
  useAuthStore.setState({ me: { user_id: 'u1', username: 'me', display_name: 'Me', role: 'user', language: 'zh', timezone: 'Asia/Shanghai' } })
  useProjectGroupStore.setState({ groups: [], groupsLoaded: false, activeGroupId: null })
})

describe('fetchGroups', () => {
  it('keeps only the group kinds, most recently active first', async () => {
    api.fetchMyChatSessions.mockResolvedValue([
      group('g-old', { last_active_at: 100 }),
      group('g-new', { last_active_at: 300 }),
      group('g-ug', { last_active_at: 200, kind: 'user_group' }),
      group('dm-1', { kind: 'user_user' }),
      group('a-1', { kind: 'agent_user' }),
    ])
    await useProjectGroupStore.getState().fetchGroups()
    expect(useProjectGroupStore.getState().groups.map((g) => g.session_id)).toEqual(['g-new', 'g-ug', 'g-old'])
  })

  it('a failed refresh keeps the last list', async () => {
    api.fetchMyChatSessions.mockResolvedValue([group('g-keep')])
    await useProjectGroupStore.getState().fetchGroups()
    api.fetchMyChatSessions.mockRejectedValue(new Error('gateway down'))
    await wakeGroupChat()
    expect(useProjectGroupStore.getState().groups.map((g) => g.session_id)).toEqual(['g-keep'])
  })

  it('no account means no groups, and no call', async () => {
    useAuthStore.setState({ me: null })
    await useProjectGroupStore.getState().fetchGroups()
    expect(api.fetchMyChatSessions).not.toHaveBeenCalled()
    expect(useProjectGroupStore.getState().groups).toEqual([])
  })
})

describe('selectGroup', () => {
  it('remembers the group the screen shows, and drops it on unmount', () => {
    useProjectGroupStore.getState().selectGroup('g-1')
    expect(useProjectGroupStore.getState().activeGroupId).toBe('g-1')
    useProjectGroupStore.getState().clearSelection()
    expect(useProjectGroupStore.getState().activeGroupId).toBeNull()
  })
})
