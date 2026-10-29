/**
 * The chat attachment capsule — one tappable pill per uploaded file, shared by
 * every mobile surface that renders a chat row.
 *
 * Why a component and not a class on a span: the desktop twin
 * (`AttachmentCapsule`) settled the same question, and the answer is that a
 * tappable thing must be a `<button>` so the platform gives it focus, an
 * accessible name, and the right press feedback. A `span` with an onClick is
 * not tappable for a screen reader, and iOS/Android keep the 300ms tap delay.
 */
import type { ChatAttachment } from '../lib/chatAttachment'
import { attachmentKind, humanSize } from '../lib/chatAttachment'

const ICON: Record<'image' | 'text' | 'other', string> = {
  // 12px stroke icons, drawn at the capsule's own text size so the glyph and
  // the label share a baseline — the same visual weight the desktop chip has.
  image:
    'M2 3.5h12v9H2zM2 10l3.5-3 3 2.5L12 6l2 1.8M4.5 6.2a.9.9 0 1 0 0-1.8.9.9 0 0 0 0 1.8Z',
  text: 'M4 2.5h5l3 3v8H4zM9 2.5v3h3M5.8 8.5h4.4M5.8 10.8h4.4',
  other: 'M4 2.5h5l3 3v8H4zM9 2.5v3h3',
}

export function ChatAttachmentCapsule({
  attachment,
  onOpen,
}: {
  attachment: ChatAttachment
  onOpen: (attachment: ChatAttachment) => void
}) {
  const kind = attachmentKind(attachment.mime)
  return (
    <button
      type="button"
      className="chat-attachment-chip chat-attachment-tappable"
      // The accessible name carries what the pixels carry: an icon, a name and
      // a size are decoration to a screen reader unless they are spelled out.
      aria-label={`${attachment.name}，${kindLabel(kind)}，${humanSize(attachment.size)}`}
      onClick={() => onOpen(attachment)}
    >
      <svg width="12" height="12" viewBox="0 0 16 16" aria-hidden="true">
        <path
          d={ICON[kind]}
          fill="none"
          stroke="currentColor"
          strokeWidth="1.2"
          strokeLinejoin="round"
          strokeLinecap="round"
        />
      </svg>
      <span className="chat-attachment-name">{attachment.name}</span>
      <span className="chat-attachment-size">{humanSize(attachment.size)}</span>
    </button>
  )
}

function kindLabel(kind: 'image' | 'text' | 'other'): string {
  return kind === 'image' ? '图片' : kind === 'text' ? '文本' : '文件'
}