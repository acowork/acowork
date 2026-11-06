/**
 * `DeliveryAlertLine` — the live "this didn't go out" receipt (ADR-105 D5,
 * revision 2026-11-06).
 *
 * The contract has two halves, and both matter:
 *   1. it paints the failure while it is TRUE — a bounced send must not be
 *      silent (that was the original bug);
 *   2. it stops painting on its own clock — the receipt is news, not history, so
 *      nothing has to retract it and no reader sees it a week later.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { DeliveryAlertLine } from '../components/DeliveryAlertLine'
import type { ChatDeliveryAlert } from '../lib/types'

;(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true

const alert = (expiresIn = 600): ChatDeliveryAlert => ({
  target: 'agent-a',
  error_code: 'target_not_installed',
  expires_in: expiresIn,
})

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
  vi.useRealTimers()
})

const line = () => container.querySelector('[data-testid="delivery-alert"]')

function mount(a: ChatDeliveryAlert, align?: 'start' | 'end', name?: string): void {
  act(() => root.render(<DeliveryAlertLine alert={a} align={align} name={name} />))
}

describe('DeliveryAlertLine', () => {
  it('paints the surface-owned wording in the sender’s own column', () => {
    mount(alert())
    expect(line()!.textContent).toContain('消息未能送达')
    // A fact about ONE message sits at that message's edge, never centered.
    expect(line()!.className).toContain('chat-notice-end')
    // The quiet warning token — noticed once, not an alarm.
    expect(line()!.querySelector('.chat-notice-pill-failed')).toBeTruthy()
  })

  it('moves to the other edge for a bystander looking at someone else’s send', () => {
    // In a group the alert is served to everyone reading that row: it belongs to
    // the sender's column, not to the reader's.
    mount(alert(), 'start')
    expect(line()!.className).toContain('chat-notice-start')
  })

  it('names the target when the surface can resolve one', () => {
    mount(alert(), 'end', '小助手')
    expect(line()!.textContent).toContain('小助手')
  })

  it('removes itself when its life runs out, with no re-read to trigger it', () => {
    // The whole point of not storing it: the window that stays open must not keep
    // a dead receipt on screen, and nothing arrives to take it away.
    vi.useFakeTimers()
    mount(alert(5))
    expect(line()).toBeTruthy()
    act(() => {
      vi.advanceTimersByTime(4999)
    })
    expect(line()).toBeTruthy()
    act(() => {
      vi.advanceTimersByTime(1)
    })
    expect(line()).toBeNull()
  })

  it('paints nothing at all when it arrives already expired', () => {
    mount(alert(0))
    expect(container.innerHTML).toBe('')
  })
})
