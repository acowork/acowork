/**
 * The inbox red dot, rendered.
 *
 * The pure rule is covered by `sessionAttention.test.ts`; what this file pins is
 * the wiring — that the dot the rule asks for actually reaches the row, that it
 * carries the reason as its label, and that it goes away by itself when the
 * status moves on. A badge that only appears in a unit test is the failure mode
 * this feature exists to prevent, one level up.
 *
 * Stores are seeded directly rather than through the network: the screen reads
 * `agentList` / `nodes` / `sessionStatuses`, and those are exactly the three
 * inputs whose interaction is under test.
 */
import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { ChatListScreen } from '../screens/chat/ChatListScreen'
import { useAgentStore } from '../stores/agentStore'
import { useAuthStore } from '../stores/authStore'
import { useNavStore } from '../stores/navStore'

;(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true

const AGENT = 'i-1'
const SESSION = 's-1'

let host: HTMLDivElement
let root: Root

function seed(): void {
  useAuthStore.setState({ phase: 'ready', baseUrl: 'http://gw.test:19876', accessToken: 'tok', refreshToken: 'rt' })
  useNavStore.getState().popToRoot()
  useAgentStore.setState({
    nodes: [{ node_id: 'n-1', online: true, node_name: 'Dev' }],
    agentList: [{ id: AGENT, name: 'Senior', node_id: 'n-1', status: 'idle' }],
    agents: {
      [AGENT]: {
        info: { id: AGENT, name: 'Senior', node_id: 'n-1', status: 'idle' },
        sessions: [{ session_id: SESSION, title: 'Red dot', updated_at: 2 }],
      },
    },
    sessionStatuses: {},
    directoryLoading: false,
    startingAgentIds: new Set<string>(),
  })
}

const dot = (): Element | null => host.querySelector('.avatar-attention-dot')
/** The AGENT row's status dot. Not `host.querySelector('.dot-sm')`: the node
 *  group header carries one too, and reading that one makes every assertion
 *  below pass or fail for the wrong reason. */
const statusDot = (): Element | null => host.querySelector('.row-title > .dot-sm')

/** Push a session status the way the presence socket would, then settle. */
async function pushStatus(fact: Parameters<ReturnType<typeof useAgentStore.getState>['applySessionState']>[2]) {
  await act(async () => {
    useAgentStore.getState().applySessionState(AGENT, SESSION, fact)
  })
}

describe('inbox agent row attention dot', () => {
  beforeEach(async () => {
    // The screen's entry effects re-pull directory/chats/groups; no directory
    // source is installed in this test, so all three are no-ops, and the stub
    // only has to keep a stray request from reaching the network.
    vi.stubGlobal('fetch', vi.fn(async () => ({ ok: true, status: 200, json: async () => ({}), text: async () => '{}' })))
    seed()
    host = document.createElement('div')
    document.body.append(host)
    root = createRoot(host)
    await act(async () => { root.render(<ChatListScreen />) })
  })
  afterEach(() => {
    act(() => { root.unmount() })
    host.remove()
    vi.unstubAllGlobals()
  })

  it('is absent while the agent is simply online, and the green dot says so', async () => {
    expect(dot()).toBeNull()
    expect(statusDot()?.className).toContain('dot-online')
  })

  it('appears when a session is parked on an approval card, labelled with the reason', async () => {
    await pushStatus({ status: 'waiting_approval' })
    expect(dot()).not.toBeNull()
    expect(dot()?.getAttribute('aria-label')).toBe('等待你确认')
    // Parked is ALSO working: the turn is still open, so both dots are true.
    expect(statusDot()?.className).toContain('dot-busy')
  })

  it('appears for a paused-at-iteration-limit turn', async () => {
    await pushStatus({ status: 'paused', reason: 'iteration_limit' })
    expect(dot()?.getAttribute('aria-label')).toBe('已到迭代上限，等待你决定')
  })

  it('stays away for a 429 retry wait — the Runtime resumes that by itself', async () => {
    await pushStatus({ status: 'paused', retry: true })
    expect(dot()).toBeNull()
    expect(statusDot()?.className).toContain('dot-busy')
  })

  it('clears itself once the status moves on (answered or timed out)', async () => {
    await pushStatus({ status: 'waiting_approval' })
    expect(dot()).not.toBeNull()
    // Continuing after the answer — no dot, still working.
    await pushStatus({ status: 'tool_executing' })
    expect(dot()).toBeNull()
    expect(statusDot()?.className).toContain('dot-busy')
    // Turn over.
    await pushStatus({ status: 'idle' })
    expect(dot()).toBeNull()
    expect(statusDot()?.className).toContain('dot-online')
  })

  it('does not outlive the agent going offline', async () => {
    await pushStatus({ status: 'waiting_approval' })
    expect(dot()).not.toBeNull()
    // An offline agent holds no obligation — the Runtime that was holding the
    // card is gone.
    await act(async () => { useAgentStore.getState().applyPresence(AGENT, 'offline') })
    expect(dot()).toBeNull()
  })
})
