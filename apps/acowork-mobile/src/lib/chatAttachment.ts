/**
 * Chat attachments on the mobile surfaces — the twin of Desktop's
 * `openChatAttachment`, split the same way: everything that can be decided
 * without a DOM lives here and is unit-tested, the components only paint.
 *
 * The rule this module exists to enforce: a chat attachment is a *file the
 * user uploaded*, so it is tappable and it says what it is (name + size) —
 * not `📎 name` in body text. The execution surface's `exec_attachments` stay
 * inert (ADR-100): those bytes belong to that run, not to this chat.
 */

import { fetchChatAttachmentBlob } from './api'

/** One row attachment as the chat store receives it (user service vocabulary). */
export interface ChatAttachment {
  id: string
  name: string
  mime: string
  size: number
}

export type AttachmentKind = 'image' | 'text' | 'other'

/** Text previews are capped: a 20 MB log is not a preview, it is a hang. */
export const TEXT_PREVIEW_MAX_BYTES = 2 * 1024 * 1024

/**
 * How tapping this attachment behaves. Derived from the mime the store
 * recorded at upload time — never from the filename, which the user controls.
 */
export function attachmentKind(mime: string): AttachmentKind {
  if (mime.startsWith('image/')) return 'image'
  if (isTextual(mime)) return 'text'
  return 'other'
}

/**
 * Textual enough to show in-app. `application/json` / `+xml` / `+json` are
 * text with extra syllables; a `text/*` type the platform cannot decode
 * (e.g. `text/csv` with an odd charset) still renders acceptably as UTF-8.
 */
export function isTextual(mime: string): boolean {
  const m = mime.toLowerCase()
  return (
    m.startsWith('text/') ||
    m === 'application/json' ||
    m.endsWith('+json') ||
    m.endsWith('+xml') ||
    m === 'application/xml' ||
    m === 'application/javascript' ||
    m === 'application/yaml' ||
    m === 'application/x-yaml'
  )
}

/** `900B` / `12KB` / `3.4MB` — the same shape the router's own `human_size`
 *  writes into `[attachments]` lines, so a file weighs the same here as in the
 *  context the router decides from. (Desktop's capsule writes `12.0 KB` for the
 *  same file; the two are cosmetic siblings, not one function — see
 *  `AttachmentCapsule.formatAttachmentSize`.) */
export function humanSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0B'
  if (bytes < 1024) return `${Math.round(bytes)}B`
  const kb = bytes / 1024
  if (kb < 1024) return `${Math.round(kb)}KB`
  return `${(kb / 1024).toFixed(1)}MB`
}

/**
 * Object URLs for attachment bytes, one per attachment.
 *
 * Memoized because a thread re-renders on every poll tick and each miss would
 * otherwise re-download the same blob. Failures are evicted so a flaky fetch
 * is not cached as permanent. Never revoked: a mobile session sees a bounded
 * number of attachments and each URL dies with the WebView.
 */
const blobUrls = new Map<string, Promise<string>>()

export function attachmentObjectUrl(
  chatSessionId: string,
  attachmentId: string,
): Promise<string> {
  const key = `${chatSessionId}/${attachmentId}`
  let url = blobUrls.get(key)
  if (!url) {
    url = fetchChatAttachmentBlob(chatSessionId, attachmentId).then((blob) =>
      URL.createObjectURL(blob),
    )
    url.catch(() => blobUrls.delete(key))
    blobUrls.set(key, url)
  }
  return url
}

/** Test seam: drop the cache so one test cannot leak a blob URL into the next. */
export function resetAttachmentObjectUrls(): void {
  blobUrls.clear()
}

/**
 * The attachment's opening lines, or `null` when there is nothing to show
 * (binary, oversized, gone). Callers treat `null` as "fall back to the
 * external-open path", never as an error to surface.
 */
export async function fetchAttachmentText(
  chatSessionId: string,
  attachment: ChatAttachment,
): Promise<string | null> {
  if (attachment.size > TEXT_PREVIEW_MAX_BYTES) return null
  try {
    const blob = await fetchChatAttachmentBlob(chatSessionId, attachment.id)
    const text = await blob.text()
    return text.trim() ? text.slice(0, TEXT_PREVIEW_MAX_BYTES) : null
  } catch {
    return null
  }
}