/**
 * Mount smoke test: every route in the registry renders against a stubbed
 * Gateway without throwing. This is the cheap substitute for clicking through
 * the app in a browser, and it targets the failure class the live contract
 * probe kept finding — a screen that type-checks, passes its unit tests, and
 * then dies (or renders nothing) on a field the Gateway does not send.
 *
 * Shapes here are copied from the live 19876 responses, not invented.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { App } from '../App'
import { ROUTES } from '../routes'
import { useAuthStore } from '../stores/authStore'
import { useNavStore } from '../stores/navStore'
import { useChatStore } from '../stores/chatStore'
import { usePmStore } from '../stores/pmStore'
import { useDocStore } from '../stores/docStore'
import { useUserChatStore } from '../stores/userChatStore'

;(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true

const AGENT = 'a1-deadbeef'
const SESSION = 's-1'

const ROUTES_MAP: [RegExp, () => unknown][] = [
  [/\/api\/agents$/, () => [{ agent_id: 'com.acowork.ponytail', instance_id: AGENT, name: 'Ponytail', display_name: 'Ponytail', status: 'running', session_count: 1 }]],
  [/\/api\/users\/directory$/, () => ({ users: [{ user_id: 'u1', username: 'nancy', display_name: 'Nancy' }] })],
  [/\/api\/auth\/me$/, () => ({ user_id: 'u1', username: 'nancy', display_name: 'Nancy', role: 'user', language: 'zh-CN', timezone: 'Asia/Shanghai', avatar: '', builtin_avatar: '', last_login_at: '2026-10-04T10:00:00Z' })],
  [/\/api\/status$/, () => ({ version: '0.1.0', agents_installed: 2, agents_running: 1, uptime_secs: 3600, mqtt_port: 19875, auth_mode: 'multi_user', registration_open: false, requires_setup: false })],
  [/\/sessions\?page=/, () => ({ sessions: [{ session_id: SESSION, title: 'Smoke session', created_at: '2026-10-01T00:00:00Z', last_active_at: '2026-10-04T00:00:00Z', message_count: 2, can_write: true, visibility: 'private' }], total_count: 1, total_pages: 1, page: 1, size: 20 })],
  [/\/sessions$/, () => ({ sessions: [], total_count: 0, total_pages: 1, page: 1, size: 20 })],
  [/\/sessions\/[^/]+\/messages/, () => ({ messages: [{ id: 'm1', ts: '2026-10-04T10:00:00Z', role: 'user', content: 'hello' }, { id: 'm2', ts: '2026-10-04T10:00:01Z', role: 'assistant', content: '**hi** there' }] })],
  [/\/sessions\/[^/?]+$/, () => ({ meta: { session_id: SESSION, created_at: '2026-10-01T00:00:00Z', last_active_at: '2026-10-04T00:00:00Z', message_count: 2 }, live_state: { status: { status: 'idle' } } })],
  [/\/status$/, () => ({ agent_id: AGENT, matches: true, work_dir: 'D:/x', pid: 4242, latest_session: SESSION, embed_dim: 512 })],
  [/\/workspaces$/, () => ({ agent_id: 'com.acowork.ponytail', workspaces: [{ id: 'ws-1', alias: 'ACoworkDev', path: 'D:/projects/tranxon/ACoworkDev', access: 'read-write' }, { id: 'ws-2', path: 'D:/home', access: 'read-only' }] })],
  [/\/memory\/stats$/, () => ({ total_nodes: 12, by_type: { fact: 12 }, by_status: { active: 12 }, index_health: 'ok', stored_dim: 512 })],
  [/\/builtin-tools$/, () => ({ agent_id: AGENT, tools: [{ name: 'bash', enabled: true }, { name: 'file_edit', enabled: false }] })],
  [/\/config$/, () => ({ agent_id: AGENT, matches: true, config: { model: 'gpt-5', provider: 'openai' } })],
  [/\/api\/chat-sessions\/default/, () => ({ session_id: 'cs-1', kind: 'user_agent', title: 'Ponytail', created_at: 1, last_active_at: 2, message_count: 1 })],
  [/\/api\/chat-sessions\/[^/]+\/messages/, () => ({ messages: [{ seq: 1, ts: 1728000000, from: 'u1', kind: 'user_message', body: 'ping', to: 'a1', meta: { message_id: 'm1' } }], total: 1, offset: 0, limit: 50 })],
  [/\/api\/users\/[^/]+\/chats$/, () => ({ chats: [{ chat_id: 'u1__u2', peer_user_id: 'u2', peer_display_name: 'Bob', unread_count: 0, last_active_at: '2026-10-04T09:00:00Z', last_message_preview: 'ping' }] })],
  [/\/api\/pm\/projects$/, () => [{ id: 'p1', title: 'acowork', description: '', status: 'active', created_by: 'u1', created_at: '2026-09-01T00:00:00Z', updated_at: '2026-10-01T00:00:00Z', metadata: {}, members: [] }]],
  [/\/api\/pm\/projects\/p1\/tasks/, () => [{ id: 't1', project_id: 'p1', title: 'Ship the APK', description: '', type: 'task', status: 'in_progress', review_status: 'not_required', priority: 'high', assignee: null, due_at: null, created_by: 'u1', created_at: '2026-10-01T00:00:00Z', updated_at: '2026-10-04T00:00:00Z', depends_on: [], attachments: [], depth: 0, is_blocked: false, blocked_by: [] }]],
  [/\/api\/pm\/tasks\/t1\/children/, () => []],
  [/\/api\/pm\/tasks\/t1$/, () => ({ id: 't1', project_id: 'p1', title: 'Ship the APK', description: '', type: 'task', status: 'in_progress', review_status: 'not_required', priority: 'high', assignee: null, due_at: null, created_by: 'u1', created_at: '2026-10-01T00:00:00Z', updated_at: '2026-10-04T00:00:00Z', depends_on: [], attachments: [] })],
  [/\/api\/doc\/tree/, () => ({ dir_id: 'root', name: 'root', path: '/', files: [{ doc_id: 'd1', name: '25-mobile-app.md', version: 3, created_at: '2026-09-01T00:00:00Z', updated_at: '2026-10-01T00:00:00Z', deleted: false }], dirs: [{ dir_id: 'adr', name: 'adr', updated_at: '2026-10-01T00:00:00Z', deleted: false }] })],
  [/\/api\/doc\/requests/, () => [{ request_id: 'r1', doc_id: 'd1', path: '/25-mobile-app.md', base_version: 3, content: 'x', submitted_by: 'agent-1', status: 'pending', created_at: '2026-10-04T00:00:00Z' }]],
  [/\/api\/doc\/docs\/d1/, () => ({ meta: { doc_id: 'd1', name: '25-mobile-app.md', version: 3, created_at: '2026-09-01T00:00:00Z', updated_at: '2026-10-01T00:00:00Z', deleted: false }, content: '# Mobile App\n\nbody text\n', path: '/25-mobile-app.md' })],
  [/\/api\/doc\/search/, () => ({ hits: [] })],
]

function stubFetch(): void {
  const impl = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input)
    const method = (init?.method ?? 'GET').toUpperCase()
    const hit = ROUTES_MAP.find(([re]) => re.test(url) && (method === 'GET' || re.source.startsWith('\/api\/agents\$') === false))
    const body = hit ? hit[1]() : {}
    return {
      ok: true,
      status: 200,
      json: async () => body,
      text: async () => JSON.stringify(body),
    }
  })
  vi.stubGlobal('fetch', impl)
}

let host: HTMLDivElement
let root: Root

async function mount(): Promise<void> {
  await act(async () => { root.render(<App />) })
}
async function go(route: string): Promise<void> {
  useNavStore.getState().switchTab(route.split('/')[0] as never)
  await act(async () => { useNavStore.getState().push(route) })
  await act(async () => { await new Promise((r) => setTimeout(r, 0)) })
}
const text = (): string => host.textContent ?? ''

describe('every route mounts', () => {
  beforeEach(() => {
    stubFetch()
    host = document.createElement('div')
    document.body.append(host)
    root = createRoot(host)
    useAuthStore.setState({ phase: 'ready', baseUrl: 'http://gw.test:19876', accessToken: 'tok', refreshToken: 'rt' })
    useNavStore.getState().popToRoot()
  })
  afterEach(() => {
    act(() => { root.unmount() })
    host.remove()
    vi.unstubAllGlobals()
  })

  it.each(Object.keys(ROUTES))('%s renders without throwing', async (route) => {
    // Seed whatever a detail screen needs to have been navigated into.
    // Only the selection is seeded: the screen's own openSession() builds the
    // per-agent view, so the test exercises the real mount path instead of a
    // hand-written partial state no store action can produce.
    useChatStore.setState({ selectedAgentId: AGENT })
    usePmStore.setState({ activeProjectId: 'p1', activeTask: { id: 't1', project_id: 'p1', title: 'Ship the APK', type: 'task', status: 'in_progress', review_status: 'not_required', priority: 'high', created_by: 'u1', created_at: '', updated_at: '', depth: 0 } as never })
    useDocStore.setState({ activeDoc: { meta: { doc_id: 'd1', name: 'x.md', path: '/x.md', version: 1, updated_at: '', size: 1 } as never, content: '# t\n', path: '/x.md' } })
    useUserChatStore.setState({ activePeerId: 'u2', activePeerName: 'Bob' })
    await mount()
    await go(route)
    expect(text().trim().length, `${route} rendered nothing`).toBeGreaterThan(0)
  })
})
