/**
 * The group transcript and composer (design 27 §5.3) — the mobile twin of
 * Desktop's GroupChatPanel render cases.
 *
 * What must hold on the surface, not just in the pure library:
 *   1. rows carry NAMES — every participant id resolves through the local
 *      registry (agents first, then the directory's users, then `me`), and
 *      the transcript never paints a raw UUID;
 *   2. the `→` chip under a bubble reads the row's own address, the same
 *      rule as Desktop's `rowToTargets`;
 *   3. typing `@` opens the picker over the roster, and picking inserts
 *      `@Name ` at the token;
 *   4. sending an `@小助手` line POSTs `/api/intents/mentions` with the
 *      agent's instance id as the resolved target — the client resolves,
 *      the Gateway routes.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { GroupChatScreen } from '../screens/chat/GroupChatScreen'
import { setAuthBridge } from '../lib/api'
import { useAuthStore } from '../stores/authStore'
import { useAgentStore } from '../stores/agentStore'
import { useProjectGroupStore } from '../stores/projectGroupStore'
import { useChatSessionStore } from '../stores/chatSessionStore'
import type { ChatMessageRow } from '../lib/types'

;(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true

const GROUP = 'g-1'
const AGENT = 'ag-1'
const ME = 'u1'
const PEER = 'u2'

let posts: Array<{ url: string; body: Record<string, unknown> }> = []
let host: HTMLDivElement
let root: Root

function row(over: Partial<ChatMessageRow>): ChatMessageRow {
  return { seq: 1, ts: 1, from: ME, kind: 'user_message', body: '', to: '', meta: {}, ...over } as ChatMessageRow
}

const ROWS: ChatMessageRow[] = [
  row({ seq: 1, from: PEER, body: '进度怎么样了', meta: { message_id: 'r1', mentions: [AGENT] } }),
  row({ seq: 2, from: AGENT, kind: 'agent_message', body: '快好了', to: PEER, meta: { message_id: 'r2' } }),
]

beforeEach(() => {
  posts = []
  vi.resetModules()
  setAuthBridge({
    baseUrl: () => 'http://gw.test',
    accessToken: () => 'tok',
    refresh: async () => null,
    sessionExpired: () => {},
  })
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input)
      const body = init?.body ? (JSON.parse(String(init.body)) as Record<string, unknown>) : {}
      if ((init?.method ?? 'GET').toUpperCase() === 'POST') posts.push({ url, body })
      const res = url.includes('/messages?')
        ? { messages: ROWS, total: ROWS.length, offset: 0, limit: 50 }
        : { message_id: 'm-new', status: 'delivered', results: [{ status: 'delivered' }] }
      return { ok: true, status: 200, json: async () => res, text: async () => JSON.stringify(res) }
    }),
  )
  useAuthStore.setState({
    me: { user_id: ME, username: 'me', display_name: '我', role: 'user', language: 'zh', timezone: 'Asia/Shanghai' },
    baseUrl: 'http://gw.test',
    accessToken: 'tok',
  })
  useAgentStore.setState({
    agentList: [{ id: AGENT, name: '小助手', node_id: 'n1', status: 'idle' }],
    users: [{ id: PEER, display_name: 'Bob' }],
  })
  useProjectGroupStore.setState({
    groups: [
      {
        session_id: GROUP,
        kind: 'project_group',
        title: '项目群',
        created_at: 1,
        last_active_at: 2,
        message_count: 2,
        participants: [
          { id: ME, identity: 'human' },
          { id: PEER, identity: 'human' },
          { id: AGENT, identity: 'agent' },
        ],
      },
    ],
    activeGroupId: GROUP,
  })
  useChatSessionStore.setState({ windows: {}, sending: {}, inflight: {}, openGroupIds: {} })
  host = document.createElement('div')
  document.body.append(host)
  root = createRoot(host)
})

afterEach(() => {
  act(() => root.unmount())
  host.remove()
  vi.unstubAllGlobals()
})

async function mount(): Promise<void> {
  await act(async () => {
    root.render(<GroupChatScreen />)
  })
}

const text = () => host.textContent ?? ''

describe('the group transcript', () => {
  it('paints names, not ids, and the `→` chip off the row’s own address', async () => {
    await mount()
    expect(text()).toContain('项目群')
    expect(text()).toContain('小助手')
    expect(text()).toContain('Bob')
    // A raw id never reaches the transcript as text.
    expect(text()).not.toContain('ag-1')
    // The peer's row mentions the agent: the chip names it.
    const chip = host.querySelector('.chat-ref-chip')
    expect(chip?.textContent).toContain('小助手')
  })

  it('typing `@` opens the picker over the roster and picking splices `@Name `', async () => {
    await mount()
    const ta = host.querySelector<HTMLTextAreaElement>('[aria-label="消息输入框"]')!
    const setter = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value')!.set!
    act(() => {
      setter.call(ta, '看 @小')
      ta.selectionStart = ta.value.length
      ta.dispatchEvent(new Event('input', { bubbles: true }))
    })
    const rows = Array.from(host.querySelectorAll<HTMLButtonElement>('.group-mention-row'))
    expect(rows).toHaveLength(1)
    expect(rows[0]!.querySelector('.group-mention-name')!.textContent).toBe('小助手')
    act(() => rows[0]!.click())
    expect(ta.value).toBe('看 @小助手 ')
  })

  it('sending an @-ed line POSTs the mentions entry with the resolved target', async () => {
    await mount()
    const ta = host.querySelector<HTMLTextAreaElement>('[aria-label="消息输入框"]')!
    const setter = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value')!.set!
    act(() => {
      setter.call(ta, '看 @小助手')
      ta.dispatchEvent(new Event('input', { bubbles: true }))
    })
    const send = host.querySelector<HTMLButtonElement>('.composer-send')!
    expect(send.disabled).toBe(false)
    await act(async () => {
      send.click()
    })
    const mention = posts.find((p) => p.url.includes('/api/intents/mentions'))
    expect(mention?.body.targets).toEqual([AGENT])
    expect(mention?.body.chat_session_id).toBe(GROUP)
  })
})
