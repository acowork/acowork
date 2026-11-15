/**
 * View-level proof that language changes actually repaint the screens —
 * the exact regression behind "设置多语言，对 mobile 没起作用". The core i18n
 * tests cover the dictionary; this covers the React subscription, which is
 * where Desktop's react-i18next-based hook broke under React 19.
 */
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import {
  _resetLanguageForTests, applyAccountLanguage, currentLang, initLanguage,
  languageOverride, setLanguageOverride,
} from '../i18n'
import { ConnectScreen } from '../screens/boot/Screens'
import { GeneralScreen } from '../screens/settings/Screens'

let host: HTMLDivElement
let root: Root

beforeEach(() => {
  ;(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true
  localStorage.clear()
  _resetLanguageForTests()
  localStorage.setItem('acowork.language', 'zh-CN')
  initLanguage()
  host = document.createElement('div')
  document.body.append(host)
  root = createRoot(host)
})

afterEach(() => {
  act(() => root.unmount())
  host.remove()
})

describe('boot screens follow the active language', () => {
  it('starts with the zh-CN legacy copy', async () => {
    await act(async () => { root.render(<ConnectScreen />) })
    expect(host.textContent).toContain('连接 ACowork')
    expect(host.textContent).toContain('扫码连接并登录')
  })

  it('repaints to English when the account language changes', async () => {
    await act(async () => { root.render(<ConnectScreen />) })
    expect(host.textContent).toContain('连接 ACowork')

    await act(async () => { setLanguageOverride('en') })
    expect(host.textContent).toContain('Connect to ACowork')
    expect(host.textContent).toContain('Scan QR & sign in')
    expect(host.textContent).not.toContain('连接 ACowork')
  })

  it('a local override survives an account-language change', async () => {
    await act(async () => { root.render(<ConnectScreen />) })
    await act(async () => { setLanguageOverride('ja') })
    expect(host.textContent).toContain('ACowork に接続')
    // Desktop pushes a different account language — the device override wins.
    await act(async () => { applyAccountLanguage('en') })
    expect(host.textContent).toContain('ACowork に接続')
  })
})

describe('General → 界面语言 picker', () => {
  it('picking a language applies it immediately and repaints the screen', async () => {
    await act(async () => { root.render(<GeneralScreen />) })
    expect(host.textContent).toContain('界面语言')

    const english = [...host.querySelectorAll('button.list-row')]
      .find((b) => b.textContent?.includes('English'))
    expect(english).toBeTruthy()

    await act(async () => {
      english!.dispatchEvent(new MouseEvent('click', { bubbles: true }))
    })
    expect(currentLang()).toBe('en')
    expect(languageOverride()).toBe('en')
    expect(host.textContent).toContain('Interface language')
  })

  it('跟随账户 clears the override and returns to the account language', async () => {
    await act(async () => { root.render(<GeneralScreen />) })
    await act(async () => { setLanguageOverride('ko') })
    expect(languageOverride()).toBe('ko')

    const follow = [...host.querySelectorAll('button.list-row')]
      .find((b) => b.textContent?.includes('跟随账户') || b.textContent?.includes('계정'))
    expect(follow).toBeTruthy()
    await act(async () => {
      follow!.dispatchEvent(new MouseEvent('click', { bubbles: true }))
    })
    expect(languageOverride()).toBeNull()
    expect(currentLang()).toBe('zh-CN')
  })
})
