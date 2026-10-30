/**
 * The attachment viewer — what a tap on a capsule opens.
 *
 * Three shapes, one component, because they share one lifecycle (open, load,
 * close) and differ only in how the bytes are painted:
 *
 * - `image`: full-screen on a black backdrop, the way every phone photo viewer
 *   does it. A chat screenshot thumbnailed to 56pt is a picture of a picture.
 * - `text`:  a sheet with the opening text, monospaced and capped.
 * - `other`: a sheet that states what the file is and that this build cannot
 *   paint it. Android has no `<a download>` for a `blob:` URL and the Gateway's
 *   attachment route is bearer-guarded, so an external "open with" needs a
 *   FileProvider intent from the native shell (see the note in the sheet).
 *   Saying so is the honest outcome; a dead capsule would just read as broken.
 */
import { useEffect, useState } from 'react'

import { Sheet } from './ui'
import type { ChatAttachment } from '../lib/chatAttachment'
import {
  attachmentKind,
  attachmentObjectUrl,
  fetchAttachmentText,
  humanSize,
} from '../lib/chatAttachment'

export function AttachmentViewer({
  chatSessionId,
  attachment,
  onClose,
}: {
  chatSessionId: string
  /** `null` = closed. One prop drives open/closed so the load state can never
   *  outlive the file it belongs to. */
  attachment: ChatAttachment | null
  onClose: () => void
}) {
  if (!attachment) return null
  const kind = attachmentKind(attachment.mime)
  if (kind === 'image') {
    return (
      <ImageAttachmentViewer
        chatSessionId={chatSessionId}
        attachment={attachment}
        onClose={onClose}
      />
    )
  }
  return <SheetAttachmentViewer chatSessionId={chatSessionId} attachment={attachment} kind={kind} onClose={onClose} />
}

function ImageAttachmentViewer({
  chatSessionId,
  attachment,
  onClose,
}: {
  chatSessionId: string
  attachment: ChatAttachment
  onClose: () => void
}) {
  const [url, setUrl] = useState<string | null>(null)
  const [failed, setFailed] = useState(false)

  useEffect(() => {
    let cancelled = false
    setUrl(null)
    setFailed(false)
    attachmentObjectUrl(chatSessionId, attachment.id)
      .then((u) => {
        if (!cancelled) setUrl(u)
      })
      .catch(() => {
        if (!cancelled) setFailed(true)
      })
    return () => {
      cancelled = true
    }
  }, [chatSessionId, attachment.id])

  return (
    <div className="attachment-lightbox" role="dialog" aria-modal="true" aria-label={attachment.name}>
      {/* Tapping anywhere closes: the whole surface is the dismiss target, so a
          full-bleed photo never traps the user behind a tiny × in a corner. */}
      <button type="button" className="attachment-lightbox-surface" onClick={onClose}>
        {url ? (
          <img className="attachment-lightbox-img" src={url} alt={attachment.name} />
        ) : (
          <span className="attachment-lightbox-status">
            {failed ? '图片已不可用' : '加载中…'}
          </span>
        )}
      </button>
      <div className="attachment-lightbox-bar">
        <span className="attachment-lightbox-name">{attachment.name}</span>
        <span className="attachment-lightbox-size">{humanSize(attachment.size)}</span>
      </div>
    </div>
  )
}

function SheetAttachmentViewer({
  chatSessionId,
  attachment,
  kind,
  onClose,
}: {
  chatSessionId: string
  attachment: ChatAttachment
  kind: 'text' | 'other'
  onClose: () => void
}) {
  const [text, setText] = useState<string | null>(null)
  const [state, setState] = useState<'loading' | 'ready' | 'empty'>('loading')

  useEffect(() => {
    if (kind !== 'text') return
    let cancelled = false
    setState('loading')
    fetchAttachmentText(chatSessionId, attachment).then((t) => {
      if (cancelled) return
      setText(t)
      setState(t ? 'ready' : 'empty')
    })
    return () => {
      cancelled = true
    }
  }, [chatSessionId, attachment.id, kind])

  return (
    <Sheet open onClose={onClose} title={`${attachment.name} · ${humanSize(attachment.size)}`}>
      {kind === 'text' ? (
        state === 'ready' ? (
          <pre className="attachment-text">{text}</pre>
        ) : (
          <div className="attachment-void">{state === 'loading' ? '加载中…' : '没有可显示的内容'}</div>
        )
      ) : (
        <div className="attachment-void">
          这类文件无法在应用内预览。请在桌面端聊天中打开，或让助手直接把内容读出来。
        </div>
      )}
    </Sheet>
  )
}