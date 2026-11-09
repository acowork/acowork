/**
 * Who owns the wording on a chat row (design 27 §4.2, the two human-authored
 * surfaces).
 *
 * Human input is literal text; markdown is agent speech. Routing a
 * `user_message` through the markdown renderer would make `**bold**` render bold
 * in one surface while reading literally in another, and would pick up the md
 * sub-styles (`.md-quote`'s secondary gray) that are unreadable on the accent-
 * filled bubble's white text. The execution / DM surfaces share `Bubble`, so the
 * CALLER says which side of that line a message is on rather than letting the
 * role decide — both sides of a human DM are human.
 *
 * Carried over from the retired card-closure file: the card rules moved into the
 * round's block (ADR-106 D8) and are pinned in `chat-block.test.tsx`, but this
 * grammar did not change with them.
 */
import { describe, expect, it } from 'vitest'
import { renderToStaticMarkup } from 'react-dom/server'
import { Bubble } from '../components/Bubble'
import { RowView, type RowProps } from '../screens/chat/AgentChatScreen'
import type { ChatMessageRow } from '../lib/types'

let seq = 0
function row(kind: string, meta: ChatMessageRow['meta'] = {}, body = ''): ChatMessageRow {
  seq += 1
  return { seq, ts: seq, from: kind === 'user_message' ? 'u1' : 'ag', kind, body, to: 'a1', meta } as ChatMessageRow
}

function renderRow(r: ChatMessageRow): string {
  const props: RowProps = {
    row: r,
    chatSessionId: 'cs-1',
    isSelf: r.kind === 'user_message',
    selfAvatar: 'me',
    selfName: 'me',
    agentAvatar: 'ag',
    agentName: 'agent',
    onJumpToExec: () => {},
    onReplyToRound: () => {},
    onOpenAttachment: () => {},
  }
  return renderToStaticMarkup(<RowView {...props} />)
}

describe('human input is literal text, markdown is agent speech', () => {
  it('a user message renders literally — no markdown parsing', () => {
    const html = renderRow(row('user_message', { message_id: 'm1' }, '**不是粗体**'))
    expect(html).toContain('**不是粗体**')
    expect(html).not.toContain('<strong>')
    expect(html).toContain('bubble-raw')
  })

  it('an agent message still renders through the markdown shell', () => {
    const html = renderRow(row('agent_message', { message_id: 'm2' }, '**是粗体**'))
    expect(html).toContain('<strong>')
    expect(html).not.toContain('**是粗体**')
  })

  it('a row of an unknown kind degrades to a notice line, never a blank hole', () => {
    const html = renderRow(row('some_future_kind', { message_id: 'm3' }, '还没认识这一类'))
    expect(html).toContain('chat-notice-pill')
    expect(html).toContain('还没认识这一类')
  })
})

describe('Bubble: human input is literal text', () => {
  const msg = (role: 'user' | 'assistant', content: string) => ({
    id: 'm1',
    role,
    content,
    created_at: '2026-10-24T00:00:00.000Z',
  }) as never

  it('renders markdown as prose when the caller says agent speech', () => {
    const html = renderToStaticMarkup(<Bubble msg={msg('assistant', '**粗体**')} />)
    expect(html).toContain('<strong>')
  })

  it('keeps the raw source when the caller says literal (both DM sides)', () => {
    const html = renderToStaticMarkup(<Bubble msg={msg('user', '**不是粗体**')} literal />)
    expect(html).toContain('**不是粗体**')
    expect(html).not.toContain('<strong>')
    expect(html).toContain('bubble-raw')
  })
})

/**
 * The exception to that grammar: an agent-origin intent turn whose row role is
 * `user` (design 27 §5.4.5).
 *
 * An execution session writes every inbound turn as a user row, and for an intent
 * the row's `content` is the machine envelope the model reads. The Runtime returns
 * the human projection beside it as `display`; the bubble paints that instead, as
 * markdown, and folds the envelope. `literal` must not win here — the words are an
 * agent's even though the role says otherwise.
 */
describe('Bubble: a display projection overrides both the role and `literal`', () => {
  const ENVELOPE = '[intent:inst-a:create_dev_tasks] {"project_id":"p-1"}'
  const intentMsg = (display?: {
    origin: string
    action: string
    summary: string
    body: string
    truncated?: boolean
  }) =>
    ({
      id: 'm1',
      role: 'user',
      content: ENVELOPE,
      created_at: '2026-10-24T00:00:00.000Z',
      display,
    }) as never

  const DISPLAY = {
    origin: 'request',
    action: 'create_dev_tasks',
    summary: '拆解 v0.2 任务',
    body: '### 拆解 v0.2 任务\n\n- **project_id**: p-1',
  }

  it('paints the projection as markdown, not the envelope', () => {
    const html = renderToStaticMarkup(<Bubble msg={intentMsg(DISPLAY)} literal />)
    // `###` becomes a heading (the renderer maps `#` to h3 — a bubble has no h1).
    expect(html).toContain('<h5 class="md-h">拆解 v0.2 任务</h5>')
    expect(html).toContain('<strong>project_id</strong>')
    expect(html).not.toContain('### 拆解')
    // The envelope is present only inside the folded <details>.
    expect(html).toContain('chat-run-raw')
    expect(html.indexOf('[intent:inst-a:create_dev_tasks]')).toBeGreaterThan(
      html.indexOf('<details'),
    )
  })

  it('states a truncated body and keeps the envelope reachable', () => {
    const html = renderToStaticMarkup(
      <Bubble msg={intentMsg({ ...DISPLAY, truncated: true })} literal />,
    )
    expect(html).toContain('正文已截断')
    expect(html).toContain('<details class="chat-run-raw">')
    expect(html).toContain('<summary>')
  })

  it('falls back to `literal` for a row with no projection', () => {
    const html = renderToStaticMarkup(<Bubble msg={intentMsg(undefined)} literal />)
    expect(html).toContain('[intent:inst-a:create_dev_tasks]')
    expect(html).toContain('bubble-raw')
    expect(html).not.toContain('<details')
  })
})
