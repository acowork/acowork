/**
 * Phase-2 i18n regression: the gettext-style msgid catalogue, interpolation,
 * and the root subscription that makes plain `t()` screens re-render on a
 * language change (phase-2 screens import the non-reactive core `t`).
 */
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import { act } from 'react'
import { createRoot, type Root } from 'react-dom/client'
import { App } from '../App'
import { useAuthStore } from '../stores/authStore'
import { MSGIDS } from '../i18n/msgids'
import {
  _resetLanguageForTests,
  initLanguage,
  setLanguageOverride,
  t,
} from '../i18n'

;(globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true

// A screen that deliberately uses the NON-reactive core `t()`. If App did not
// subscribe at the root, switching language would leave this text in Chinese.
vi.mock('../components/ScreenHost', async () => {
  const React = await import('react')
  const { t: coreT } = await import('../i18n')
  return {
    ScreenHost: () =>
      React.createElement('div', { 'data-testid': 'probe' }, coreT('全部')),
  }
})

const LANGS = ['zh-TW', 'en', 'ja', 'ko'] as const

beforeEach(() => {
  localStorage.clear()
  localStorage.setItem('acowork.language', 'zh-CN')
  _resetLanguageForTests()
  initLanguage()
  useAuthStore.setState({ phase: 'ready' })
})

afterEach(() => {
  localStorage.clear()
  _resetLanguageForTests()
  useAuthStore.setState({ phase: 'connect' })
})

describe('phase-2 msgid catalogue', () => {
  it('has the same 350 keys in every non-zh-CN bundle', () => {
    const keys = Object.keys(MSGIDS.en)
    expect(keys.length).toBe(350)
    for (const lang of LANGS) {
      expect(Object.keys(MSGIDS[lang]).sort()).toEqual(keys.sort())
    }
  })

  it('never leaves an empty translation', () => {
    for (const lang of LANGS) {
      for (const [key, value] of Object.entries(MSGIDS[lang])) {
        expect(value.trim(), `${lang}: ${key}`).not.toBe('')
      }
    }
  })

  it('keeps {{var}} placeholders identical across all languages', () => {
    const vars = (s: string) => (s.match(/\{\{(\w+)\}\}/g) ?? []).sort()
    for (const key of Object.keys(MSGIDS.en)) {
      const want = vars(key)
      for (const lang of LANGS) {
        expect(vars(MSGIDS[lang][key]!), `${lang}: ${key}`).toEqual(want)
      }
    }
  })

  it('translates a msgid and interpolates vars in every language', () => {
    expect(t('全部')).toBe('全部')
    expect(t('回复 {{quote}}', { quote: 'hi' })).toBe('回复 hi')

    setLanguageOverride('en')
    expect(t('全部')).toBe('All')
    expect(t('回复 {{quote}}', { quote: 'hi' })).toBe('Replying to hi')

    setLanguageOverride('ja')
    expect(t('全部')).toBe('すべて')
    expect(t('回复 {{quote}}', { quote: 'hi' })).toBe('hi に返信')

    setLanguageOverride('ko')
    expect(t('全部')).toBe('전체')

    setLanguageOverride('zh-TW')
    expect(t('全部')).toBe('全部')
    expect(t('回复 {{quote}}', { quote: 'hi' })).toBe('回覆 hi')
  })

  it('falls back to the key itself for an unknown msgid', () => {
    setLanguageOverride('en')
    expect(t('一条服务端返回的原始错误消息')).toBe('一条服务端返回的原始错误消息')
  })

  it('still resolves phase-1 named keys', () => {
    setLanguageOverride('en')
    expect(t('tab.chat')).toBe('Chats')
  })
})

describe('root language subscription', () => {
  let host: HTMLDivElement
  let root: Root

  afterEach(async () => {
    await act(async () => {
      setLanguageOverride(null)
      root.unmount()
    })
    host.remove()
  })

  it('re-renders a screen using plain t() when the language changes', async () => {
    host = document.createElement('div')
    document.body.appendChild(host)
    root = createRoot(host)

    await act(async () => { root.render(<App />) })
    expect(host.textContent).toContain('全部')

    await act(async () => { setLanguageOverride('en') })
    expect(host.textContent).toContain('All')
    expect(host.textContent).not.toContain('全部')

    await act(async () => { setLanguageOverride('ja') })
    expect(host.textContent).toContain('すべて')
  })
})
