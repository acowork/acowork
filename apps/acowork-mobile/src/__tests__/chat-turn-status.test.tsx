/**
 * The waiting line and the composer lock on the mobile agent chat (ADR-106 D9).
 *
 * Both are driven by ONE fact: a send THIS device made, keyed by the id its own
 * response carried, that nothing in the transcript has answered yet. The two
 * ways that claim goes wrong are the two bugs this screen has already had —
 * deriving it from the transcript's shape (so a dead node looked busy forever),
 * and dropping it when the POST returned (so the seconds before the turn's first
 * row showed nothing at all, with a send button still live and no Chat-side
 * queue behind it).
 *
 * Stores are real here, so the release path is exercised too: the screen hands
 * the claim back to `settleTurn` once a row answers it, and the composer's own
 * `disabled` state is what the human sees of that.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { AgentChatScreen } from '../screens/chat/AgentChatScreen'
import { useAuthStore } from '../stores/authStore'
import { useAgentStore } from '../stores/agentStore'
import { useChatStore } from '../stores/chatStore'
import { useChatSessionStore } from '../stores/chatSessionStore'
import type { InflightTurn } from '../lib/chatTurnInflight'
import type { ChatMessageRow } from '../lib/types'

;(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true

const AGENT = 'ag-1'
const SESSION = 'cs-1'

function row(over: Partial<ChatMessageRow>): ChatMessageRow {
  return {
    seq: 1,
    ts: 1,
    from: 'u1',
    kind: 'user_message',
    body: '把分页修一下',
    to: AGENT,
    meta: { message_id: 'm-1' },
    ...over,
  } as ChatMessageRow
}

const HUMAN = row({})
const ROUND = row({
  seq: 2,
  from: AGENT,
  kind: 'execution_round',
  body: '',
  to: 'u1',
  meta: { message_id: 'round:exec|r1', round_id: 'r1', agent_instance_id: AGENT, agent_session_id: 'exec', in_reply_to: 'm-1' },
})
const RECEIPT = row({
  seq: 2,
  from: AGENT,
  kind: 'system_notice',
  body: '',
  to: 'u1',
  meta: { message_id: 'dlv-failed:m-1:ag', notice_type: 'delivery_failed', in_reply_to: 'm-1' },
})

let container: HTMLDivElement
let root: Root

/** Seed the surface with a window and (optionally) the claim of a send. */
function seed(tail: ChatMessageRow[], inflight: Record<string, InflightTurn | undefined>): void {
  useChatSessionStore.setState({
    byAgent: { [AGENT]: { sessionId: SESSION, resolved: true } },
    windows: {
      [SESSION]: { older: [], tail, tailLoaded: tail.length, total: tail.length, loadingOlder: false },
    },
    sending: {},
    inflight,
  })
}

const statusLine = () => container.querySelector('[role="status"]')
const composer = () => container.querySelector<HTMLTextAreaElement>('.composer-input')!
const sendButton = () => container.querySelector<HTMLButtonElement>('.composer-send')!

function mount(): void {
  act(() => root.render(<AgentChatScreen />))
}

describe('AgentChatScreen in-flight turn claim', () => {
  beforeEach(() => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => ({ ok: true, status: 200, json: async () => ({}), text: async () => '' })),
    )
    useAuthStore.setState({ me: { user_id: 'u1', username: 'nancy', display_name: 'Nancy' } as never })
    useChatStore.setState({ selectedAgentId: AGENT })
    useAgentStore.setState({
      agents: { [AGENT]: { info: { id: AGENT, name: '小助手', node_id: 'n1' } as never, sessions: [] } },
    })
    container = document.createElement('div')
    document.body.appendChild(container)
    root = createRoot(container)
  })

  afterEach(() => {
    act(() => root.unmount())
    container.remove()
    useChatSessionStore.setState({ byAgent: {}, windows: {}, sending: {}, inflight: {} })
    vi.unstubAllGlobals()
  })

  it('the claim past the POST holds the line up and locks the composer', () => {
    // The window this exists for: the send has returned, the router is still
    // deciding, the transcript holds only the human's own row. Nothing here
    // says "busy" but this device's own send — and a second send would race it.
    seed([HUMAN], { [SESSION]: { messageId: 'm-1', at: 1 } })
    mount()
    expect(statusLine()?.textContent).toContain('小助手')
    expect(composer().disabled).toBe(true)
    expect(sendButton().disabled).toBe(true)
  })

  it('the round’s reference row settles the claim: line gone, composer live', () => {
    // From here the block narrates the run (D3/D9), so a follow-up is allowed —
    // the execution side has a queue for it; the Chat does not need one. And the
    // screen hands the claim back rather than carrying an id the transcript has
    // already answered.
    seed([HUMAN, ROUND], { [SESSION]: { messageId: 'm-1', at: 1 } })
    mount()
    expect(statusLine()).toBeNull()
    expect(composer().disabled).toBe(false)
    expect(useChatSessionStore.getState().inflight[SESSION]).toBeUndefined()
  })

  it('a receipt settles it too — the human has an answer on screen', () => {
    seed([HUMAN, RECEIPT], { [SESSION]: { messageId: 'm-1', at: 1 } })
    mount()
    expect(statusLine()).toBeNull()
    expect(composer().disabled).toBe(false)
  })

  it('an unanswered row is not liveness: no claim of our own, no line (D9)', () => {
    // The reload case. Claiming busy here is what made a dead node look alive
    // forever, and it would lock a composer with nothing behind it.
    seed([HUMAN], {})
    mount()
    expect(statusLine()).toBeNull()
    expect(composer().disabled).toBe(false)
  })

  it('another session’s claim does not lock this one', () => {
    // The claim is keyed per Chat session; a stale flag from a session that is
    // not on screen must not freeze this composer.
    seed([HUMAN], { 'cs-other': { messageId: 'm-9', at: 1 } })
    mount()
    expect(statusLine()).toBeNull()
    expect(composer().disabled).toBe(false)
  })
})
