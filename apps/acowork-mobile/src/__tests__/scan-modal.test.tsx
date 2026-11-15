/**
 * ScanModal — a failed scan must be escapable.
 *
 * Regression: the full-bleed <video> is `position:absolute`, so it paints
 * AFTER the in-flow button bar and still hit-tests at `opacity:0`; OEM
 * Android WebViews may even composite it into a separate surface above the
 * DOM. Users saw 返回 on the camera-error screen but tapping it did nothing,
 * leaving a dead end on top of an already-stale Gateway session.
 *
 * Mobile has no @testing-library dependency, so this uses the same createRoot
 * + act harness as the fold/block tests. jsdom cannot open a camera, which is
 * exactly the failure path under test.
 */
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { ScanModal, parsePairPayload } from '../screens/boot/ScanModal'

let host: HTMLDivElement
let root: Root
const mediaDescriptor = Object.getOwnPropertyDescriptor(navigator, 'mediaDevices')

beforeEach(() => {
  ;(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true
  Object.defineProperty(navigator, 'mediaDevices', {
    configurable: true,
    value: { getUserMedia: vi.fn(async () => { throw new Error('camera denied') }) },
  })
  host = document.createElement('div')
  document.body.append(host)
  root = createRoot(host)
})

afterEach(() => {
  act(() => root.unmount())
  host.remove()
  if (mediaDescriptor) Object.defineProperty(navigator, 'mediaDevices', mediaDescriptor)
  else Reflect.deleteProperty(navigator, 'mediaDevices')
})

async function mountFailingScan(onClose: () => void): Promise<void> {
  await act(async () => {
    root.render(<ScanModal onClose={onClose} />)
    // Let the async getUserMedia rejection settle and commit the failed state.
    await new Promise((r) => setTimeout(r, 0))
  })
}

describe('ScanModal failure screen', () => {
  it('shows 返回 with a usable hint when the camera cannot start', async () => {
    await mountFailingScan(vi.fn())
    expect(host.querySelector('button')?.textContent).toBe('返回')
    expect(host.textContent).toContain('无法访问相机')
  })

  it('the video never hit-tests over the button bar', async () => {
    await mountFailingScan(vi.fn())
    const video = host.querySelector('video')!
    const bar = host.querySelector('button')!.parentElement!
    expect(video.style.pointerEvents).toBe('none')
    expect(bar.style.position).toBe('relative')
  })

  it('tapping 返回 actually closes the modal', async () => {
    const onClose = vi.fn()
    await mountFailingScan(onClose)
    const btn = host.querySelector('button')!
    await act(async () => {
      btn.dispatchEvent(new MouseEvent('click', { bubbles: true }))
    })
    expect(onClose).toHaveBeenCalledTimes(1)
  })
})

describe('parsePairPayload', () => {
  it('accepts only schema v1 payloads', () => {
    expect(parsePairPayload('{"v":1,"gw":"http://g:19876","pt":"t"}')).toEqual({
      gw: 'http://g:19876', pt: 't',
    })
    expect(parsePairPayload('{"v":2,"gw":"http://g","pt":"t"}')).toBeNull()
    expect(parsePairPayload('{"v":1,"gw":"http://g"}')).toBeNull()
    expect(parsePairPayload('not json')).toBeNull()
  })
})
