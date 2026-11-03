/**
 * The second level of the `@` on mobile (ADR-106 D7) — the twin of Desktop's
 * `ChatAddressPicker` cases, and the reason there is no candidate endpoint.
 *
 * What is pinned:
 *   1. the list is `GET …/sessions` carrying `chat_session_id` / `surface`: the
 *      narrowing is the Runtime's permission rule, so a sheet that asked for the
 *      plain listing would offer runs this chat may not address;
 *   2. paging stays inside that scope and appends without repeating a row — the
 *      paging IS the window now, there is no 20-row cap and no freshness cut;
 *   3. a failed read says so and can be retried rather than reading as "nothing
 *      to continue";
 *   4. the two fixed answers survive: "开新任务" first, "不指定" reachable.
 */
import { describe, expect, it, vi, beforeEach, afterEach } from 'vitest'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { ChatAddressSheet } from '../components/ChatAddressSheet'
import { fetchChatSessions, setAuthBridge } from '../lib/api'
import { formatListTime } from '../lib/time'
import type { SessionRow } from '../lib/types'

;(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true

const AGENT = 'a1-deadbeef'
const CHAT = 'cs-1'

function row(sessionId: string, activeAt: string): SessionRow {
  return {
    session_id: sessionId,
    title: `任务 ${sessionId}`,
    created_at: '2026-10-01T00:00:00Z',
    last_active_at: activeAt,
    message_count: 2,
    can_write: true,
    visibility: 'private',
  }
}

/** Page 1: s-1, s-2. Page 2: s-2 again (created between the reads — the server
 *  pages a moving list) plus s-3. */
function pageFor(url: string): { sessions: SessionRow[]; total_count: number; total_pages: number; page: number; size: number } {
  const page = Number(new URL(url).searchParams.get('page') ?? 1)
  const sessions = page === 1 ? [row('s-1', '2026-10-04T10:00:00Z'), row('s-2', '2026-10-04T09:00:00Z')] : [row('s-2', '2026-10-04T09:00:00Z'), row('s-3', '2026-10-02T09:00:00Z')]
  return { sessions, total_count: 3, total_pages: 2, page, size: 20 }
}

let urls: string[] = []
let failNext = false

beforeEach(() => {
  urls = []
  failNext = false
  setAuthBridge({
    baseUrl: () => 'http://gw.test',
    accessToken: () => 'tok',
    refresh: async () => null,
    sessionExpired: () => {},
  })
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input)
      urls.push(url)
      if (failNext) return { ok: false, status: 500, json: async () => ({}), text: async () => 'boom' }
      const body = pageFor(url)
      return { ok: true, status: 200, json: async () => body, text: async () => JSON.stringify(body) }
    }),
  )
})

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('fetchChatSessions (the shared listing, scoped)', () => {
  it('asks GET …/sessions with this chat scope and reports the next page', async () => {
    const res = await fetchChatSessions(AGENT, CHAT, 'direct', 1)

    const url = new URL(urls[0]!)
    expect(url.pathname).toBe(`/api/agents/${AGENT}/sessions`)
    // Drop either parameter and the Runtime answers with the agent's whole
    // history — the offer would include runs this chat cannot deliver to.
    expect(url.searchParams.get('chat_session_id')).toBe(CHAT)
    expect(url.searchParams.get('surface')).toBe('direct')
    expect(url.searchParams.get('page')).toBe('1')
    expect(res.items.map((s) => s.session_id)).toEqual(['s-1', 's-2'])
    expect(res.hasMore).toBe(true)
  })

  it('asks nothing when there is no chat to scope to', async () => {
    const res = await fetchChatSessions(AGENT, '', 'direct', 1)
    expect(res).toEqual({ items: [], hasMore: false })
    expect(urls).toEqual([])
  })
})

describe('ChatAddressSheet', () => {
  let host: HTMLDivElement
  let root: Root

  beforeEach(() => {
    host = document.createElement('div')
    document.body.append(host)
    root = createRoot(host)
  })

  afterEach(() => {
    act(() => root.unmount())
    host.remove()
  })

  async function open(pick = vi.fn()): Promise<typeof pick> {
    await act(async () => {
      root.render(<ChatAddressSheet instanceId={AGENT} chatSessionId={CHAT} onPick={pick} onClose={() => {}} />)
    })
    await act(async () => {})
    return pick
  }

  const buttons = (testId: string) =>
    Array.from(host.querySelectorAll<HTMLButtonElement>(`[data-testid="${testId}"]`))

  it('lists the addressable runs of this chat, newest activity first', async () => {
    await open()
    expect(urls[0]).toContain(`chat_session_id=${CHAT}`)
    expect(
      buttons('address-session').map((b) => b.querySelector('.chat-sheet-option-label')!.textContent),
    ).toEqual(['任务 s-1', '任务 s-2'])
    // The listing carries no run status, so the trailing slot is when the work
    // last moved — not an invented "进行中".
    expect(buttons('address-session')[0]!.querySelector('.chat-sheet-option-value')!.textContent).toBe(
      formatListTime(Date.parse('2026-10-04T10:00:00Z')),
    )
  })

  it("puts the cheap escape first and names a run on tap", async () => {
    const pick = await open()
    expect(host.querySelector('.chat-sheet-option')!.getAttribute('data-testid')).toBe('address-new')
    act(() => buttons('address-session')[1]!.click())
    expect(pick).toHaveBeenLastCalledWith({ sessionId: 's-2', label: '任务 s-2' })
    act(() => buttons('address-none')[0]!.click())
    expect(pick).toHaveBeenLastCalledWith({})
  })

  it('pages back inside the same scope without repeating a row', async () => {
    await open()
    expect(buttons('address-more')).toHaveLength(1)

    await act(async () => {
      buttons('address-more')[0]!.click()
    })
    await act(async () => {})

    expect(urls[1]).toContain('page=2')
    expect(urls[1]).toContain(`chat_session_id=${CHAT}`)
    // s-2 was on both pages; two rows for one run would be two taps for one
    // address and a duplicate key.
    expect(buttons('address-session').map((b) => b.getAttribute('title'))).toEqual(['s-1', 's-2', 's-3'])
    // The last page offers no further tap.
    expect(buttons('address-more')).toHaveLength(0)
  })

  it('says so when the list cannot be read, and retries on tap', async () => {
    failNext = true
    await open()
    expect(host.textContent).toContain('读不到本会话的任务列表')
    expect(buttons('address-session')).toHaveLength(0)

    failNext = false
    await act(async () => {
      Array.from(host.querySelectorAll<HTMLButtonElement>('.chat-sheet-option'))
        .find((b) => b.textContent?.includes('重试'))!
        .click()
    })
    await act(async () => {})

    expect(buttons('address-session').map((b) => b.getAttribute('title'))).toEqual(['s-1', 's-2'])
  })
})
