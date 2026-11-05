/**
 * DeliveryAlertLine — the mobile twin of Desktop's `DeliveryAlertLine`: the
 * "this didn't go out" receipt painted **live** (ADR-105 D5, revision
 * 2026-11-06).
 *
 * It replaces the persisted `system_notice` row with
 * `notice_type = "delivery_failed"`. That row was never retracted, so a send
 * that failed and was retried successfully kept a yellow "The message wasn't
 * delivered" line in the transcript forever.
 *
 * Now the fact is a decoration of the row the send owed (`delivery.alert`),
 * served with its remaining life (`expires_in`), and this component drops it on
 * that same clock: a phone left on one chat must not keep a dead receipt on
 * screen, and dropping it must not wait for a re-read.
 *
 * The wording is the surface's, not the server's: `error_code` is a machine
 * token, and the sentence a human reads is localized copy.
 */

import { useEffect, useState } from 'react'
import type { ChatDeliveryAlert } from '../lib/types'

export interface DeliveryAlertLineProps {
  alert: ChatDeliveryAlert
  /** Agent display name, when the surface can resolve one for `alert.target`. */
  name?: string
  /** The column of the row it decorates. A failure always belongs to the
   *  message it owed, so a bystander in a group sees it under the sender's own
   *  bubble — never centered as a fact about the whole conversation. */
  align?: 'start' | 'end'
}

export function DeliveryAlertLine({ alert, name, align = 'end' }: DeliveryAlertLineProps) {
  // The service stopped counting when this page was read; `expires_in` is what
  // is left. Re-armed only when a genuinely new receipt arrives (different code
  // or life), so a re-read that still reports the same alert does not restart
  // the clock.
  const [expired, setExpired] = useState(alert.expires_in <= 0)

  useEffect(() => {
    setExpired(alert.expires_in <= 0)
    if (alert.expires_in <= 0) return
    const timer = setTimeout(() => setExpired(true), alert.expires_in * 1000)
    return () => clearTimeout(timer)
  }, [alert.expires_in, alert.error_code])

  if (expired) return null

  // Which agent failed matters in a group and not in a 1:1 — the same copy rule
  // the persisted receipt used, so nothing a reader sees changes except how
  // long it stays.
  const text = name
    ? `消息未能送达 ${name}，请重试或 @ 该 Agent`
    : '消息未能送达，请重试或 @ 该 Agent'

  return (
    <div
      className={align === 'end' ? 'chat-notice chat-notice-end' : 'chat-notice chat-notice-start'}
      data-testid="delivery-alert"
    >
      <span className="chat-notice-pill-failed">{text}</span>
    </div>
  )
}
