/**
 * Mobile chat attachments (P3): capsule semantics, the viewer's three shapes,
 * and the row wiring that connects them.
 *
 * The bug these exist for is a parity bug: the chat's uploaded files rendered
 * as `📎 name` body text — no size, no icon, no tap — while the same file on
 * the desktop surface was a capsule that opened. So the assertions are about
 * what the pixels promise: a `<button>` (not a span), a name, a size, and a
 * tap that reaches the parent's viewer.
 */
import { describe, expect, it, vi, beforeEach, afterEach } from 'vitest'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { renderToStaticMarkup } from 'react-dom/server'
import type { ChatMessageRow } from '../lib/types'
import type { ChatAttachment } from '../lib/chatAttachment'
import {
  attachmentKind,
  humanSize,
  isTextual,
  resetAttachmentObjectUrls,
} from '../lib/chatAttachment'
import { ChatAttachmentCapsule } from '../components/ChatAttachmentCapsule'
import { AttachmentViewer } from '../components/AttachmentViewer'
import { RowView, type RowProps } from '../screens/chat/AgentChatScreen'

// Partial mock: only the bytes fetch is stubbed. `setAuthBridge` (installed by
// authStore at module load) must stay real, or importing the chat screen blows
// up before a single assertion runs.
vi.mock('../lib/api', async (importOriginal) => ({
  ...(await importOriginal<typeof import('../lib/api')>()),
  fetchChatAttachmentBlob: vi.fn(),
}))

const api = await import('../lib/api')

const PNG: ChatAttachment = { id: 'att-1', name: 'shot.png', mime: 'image/png', size: 348_160 }
const LOG: ChatAttachment = { id: 'att-2', name: 'build.log', mime: 'text/plain', size: 2_048 }
const PDF: ChatAttachment = { id: 'att-3', name: 'report.pdf', mime: 'application/pdf', size: 12_288 }

describe('attachment classification', () => {
  it('classifies by mime, never by filename', () => {
    expect(attachmentKind('image/png')).toBe('image')
    expect(attachmentKind('text/plain')).toBe('text')
    expect(attachmentKind('application/json')).toBe('text')
    expect(attachmentKind('application/pdf')).toBe('other')
    // A user-named "shot.png" carrying a PDF mime is a PDF: the store's mime
    // is what was uploaded, the name is what the user typed.
    expect(attachmentKind('application/octet-stream')).toBe('other')
  })

  it('treats json/xml/js as text and binary as not', () => {
    expect(isTextual('application/json')).toBe(true)
    expect(isTextual('application/problem+json')).toBe(true)
    expect(isTextual('image/svg+xml')).toBe(true)
    expect(isTextual('application/javascript')).toBe(true)
    expect(isTextual('application/zip')).toBe(false)
    expect(isTextual('image/png')).toBe(false)
  })

  it('formats sizes the way the desktop chip and the router do', () => {
    expect(humanSize(0)).toBe('0B')
    expect(humanSize(900)).toBe('900B')
    expect(humanSize(12_288)).toBe('12KB')
    expect(humanSize(348_160)).toBe('340KB')
    expect(humanSize(3_565_158)).toBe('3.4MB')
    // A tombstoned row can carry nonsense; the chip must still paint.
    expect(humanSize(Number.NaN)).toBe('0B')
  })
})

describe('attachment capsule', () => {
  it('is a real button carrying name and size', () => {
    const html = renderToStaticMarkup(
      <ChatAttachmentCapsule attachment={PNG} onOpen={() => {}} />,
    )
    expect(html).toContain('<button')
    expect(html).toContain('chat-attachment-tappable')
    expect(html).toContain('shot.png')
    expect(html).toContain('340KB')
    // The old rendering: `📎 shot.png` as body text. Gone.
    expect(html).not.toContain('📎')
    expect(html).toContain('aria-label="shot.png，图片，340KB"')
  })

  it('spells the file type out for a screen reader', () => {
    expect(
      renderToStaticMarkup(<ChatAttachmentCapsule attachment={LOG} onOpen={() => {}} />),
    ).toContain('，文本，')
    expect(
      renderToStaticMarkup(<ChatAttachmentCapsule attachment={PDF} onOpen={() => {}} />),
    ).toContain('，文件，')
  })
})

describe('chat row wiring', () => {
  let container: HTMLDivElement
  let root: Root

  beforeEach(() => {
    container = document.createElement('div')
    document.body.appendChild(container)
    root = createRoot(container)
  })

  afterEach(() => {
    act(() => root.unmount())
    container.remove()
  })

  function rowWith(attachments: ChatAttachment[]): ChatMessageRow {
    return {
      seq: 1,
      ts: 1,
      from: 'u1',
      kind: 'user_message',
      body: '看看这个',
      to: 'a1',
      meta: {},
      attachments,
    } as unknown as ChatMessageRow
  }

  function propsWith(row: ChatMessageRow, onOpenAttachment: (a: ChatAttachment) => void): RowProps {
    return {
      row,
      isSelf: true,
      selfAvatar: 'me',
      selfName: 'me',
      agentAvatar: 'ag',
      agentName: 'agent',
      closed: false,
      answered: false,
      answeredCards: new Set<string>(),
      chatAnswered: new Set<string>(),
      onOpenExec: () => {},
      onAnswerCard: () => {},
      onOpenAttachment,
    }
  }

  it('renders one tappable capsule per uploaded file and reports the tap', () => {
    const opened: ChatAttachment[] = []
    act(() => {
      root.render(<RowView {...propsWith(rowWith([PNG, LOG]), (a) => opened.push(a))} />)
    })

    const capsules = container.querySelectorAll<HTMLButtonElement>('.chat-attachment-tappable')
    expect(capsules).toHaveLength(2)
    expect(capsules[0]!.textContent).toContain('shot.png')
    expect(capsules[1]!.textContent).toContain('build.log')

    act(() => capsules[1]!.click())
    expect(opened).toHaveLength(1)
    expect(opened[0]!.name).toBe('build.log')
  })

  it('keeps an execution surface label inert (ADR-100 D12)', () => {
    const row = {
      ...rowWith([]),
      meta: { exec_attachments: [{ filename: 'report.docx', size: 10 }] },
    } as unknown as ChatMessageRow
    act(() => {
      root.render(<RowView {...propsWith(row, () => {})} />)
    })
    expect(container.querySelector('.chat-attachment-label')).not.toBeNull()
    expect(container.querySelectorAll('.chat-attachment-tappable')).toHaveLength(0)
  })
})

describe('attachment viewer', () => {
  let container: HTMLDivElement
  let root: Root

  beforeEach(() => {
    resetAttachmentObjectUrls()
    // jsdom implements neither: the viewer mint a `blob:` URL from fetched
    // bytes, and without a stub every image path would land in its failure
    // branch instead of the one under test.
    Object.defineProperty(URL, 'createObjectURL', {
      configurable: true,
      value: () => 'blob:test-url',
    })
    Object.defineProperty(URL, 'revokeObjectURL', { configurable: true, value: () => {} })
    container = document.createElement('div')
    document.body.appendChild(container)
    root = createRoot(container)
  })

  afterEach(() => {
    act(() => root.unmount())
    container.remove()
    vi.clearAllMocks()
  })

  it('renders nothing when closed', () => {
    act(() => {
      root.render(<AttachmentViewer chatSessionId="cs-1" attachment={null} onClose={() => {}} />)
    })
    expect(container.innerHTML).toBe('')
  })

  it('opens an image full-screen and closes on a tap anywhere', async () => {
    vi.mocked(api.fetchChatAttachmentBlob).mockResolvedValue(new Blob(['png']))
    let closed = 0
    await act(async () => {
      root.render(
        <AttachmentViewer chatSessionId="cs-1" attachment={PNG} onClose={() => (closed += 1)} />,
      )
    })

    const img = container.querySelector<HTMLImageElement>('.attachment-lightbox-img')
    expect(img).not.toBeNull()
    expect(img!.getAttribute('src')).toContain('blob:')
    expect(container.textContent).toContain('shot.png')
    expect(container.textContent).toContain('340KB')

    act(() => container.querySelector<HTMLButtonElement>('.attachment-lightbox-surface')!.click())
    expect(closed).toBe(1)
  })

  it('says so when the blob is gone instead of showing a broken image', async () => {
    const { GatewayError } = await import('../lib/api')
    vi.mocked(api.fetchChatAttachmentBlob).mockRejectedValue(new GatewayError(404, 'gone'))
    await act(async () => {
      root.render(<AttachmentViewer chatSessionId="cs-1" attachment={PNG} onClose={() => {}} />)
    })
    expect(container.querySelector('.attachment-lightbox-img')).toBeNull()
    expect(container.textContent).toContain('图片已不可用')
  })

  it('previews a text attachment in a sheet', async () => {
    // jsdom's Blob has no `text()` (real WebViews do), so the reader is stubbed
    // rather than the module: what is under test is the cap/trim decision, not
    // the platform's blob decoder.
    vi.mocked(api.fetchChatAttachmentBlob).mockResolvedValue({
      text: async () => 'line one\nline two',
    } as unknown as Blob)
    await act(async () => {
      root.render(<AttachmentViewer chatSessionId="cs-1" attachment={LOG} onClose={() => {}} />)
    })
    // One extra microtask turn: the sheet starts in `loading` and settles on
    // the promise chain of the fetch, which act() alone does not drain.
    await act(async () => {})
    const pre = container.querySelector('.attachment-text')
    expect(pre).not.toBeNull()
    expect(pre!.textContent).toContain('line one')
    // The sheet names the file and its size, so a reader who scrolled past the
    // capsule still knows what they are looking at.
    expect(container.textContent).toContain('build.log · 2KB')
  })

  it('tells the truth about a type this build cannot paint', async () => {
    await act(async () => {
      root.render(<AttachmentViewer chatSessionId="cs-1" attachment={PDF} onClose={() => {}} />)
    })
    expect(container.querySelector('.attachment-text')).toBeNull()
    expect(container.textContent).toContain('无法在应用内预览')
    // A binary never triggers a download of bytes it cannot show.
    expect(api.fetchChatAttachmentBlob).not.toHaveBeenCalled()
  })
})