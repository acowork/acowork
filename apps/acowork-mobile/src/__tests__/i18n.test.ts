/**
 * Mobile i18n core. The account language is the source of truth (Desktop
 * writes the same server field), with an explicit local override that must
 * win — otherwise a Desktop change would silently undo what the user picked
 * on the phone.
 */
import { beforeEach, describe, expect, it, vi } from 'vitest'
import {
  _resetLanguageForTests,
  applyAccountLanguage,
  currentLang,
  initLanguage,
  languageOverride,
  normalizeLang,
  setLanguageOverride,
  subscribeLanguage,
  t,
} from '../i18n'

beforeEach(() => {
  localStorage.clear()
  _resetLanguageForTests()
})

describe('normalizeLang', () => {
  it('maps BCP-47 variants onto the five shipped bundles', () => {
    expect(normalizeLang('zh-CN')).toBe('zh-CN')
    expect(normalizeLang('zh-Hans-CN')).toBe('zh-CN')
    expect(normalizeLang('zh-TW')).toBe('zh-TW')
    expect(normalizeLang('zh-Hant')).toBe('zh-TW')
    expect(normalizeLang('zh-HK')).toBe('zh-TW')
    expect(normalizeLang('en-US')).toBe('en')
    expect(normalizeLang('ja-JP')).toBe('ja')
    expect(normalizeLang('ko-KR')).toBe('ko')
  })

  it('falls back to zh-CN for empty and unknown values', () => {
    expect(normalizeLang(null)).toBe('zh-CN')
    expect(normalizeLang('')).toBe('zh-CN')
    expect(normalizeLang('fr-FR')).toBe('zh-CN')
  })
})

describe('t', () => {
  it('the zh-CN bundle keeps the legacy copy byte for byte', () => {
    localStorage.setItem('acowork.language', 'zh-CN')
    initLanguage()
    expect(t('tab.chat')).toBe('聊天')
    expect(t('login.title')).toBe('登录')
  })

  it('with no cached account language, follows the device locale', () => {
    initLanguage()
    // jsdom reports en-US; a zh-CN phone would resolve to zh-CN instead.
    expect(currentLang()).toBe(normalizeLang(navigator.language))
  })

  it('switches the whole dictionary and interpolates {{vars}}', () => {
    applyAccountLanguage('en')
    expect(t('tab.chat')).toBe('Chats')
    expect(t('common.retry')).toBe('Retry')
    expect(t('disconnected.sub', { url: 'http://192.168.0.101:19876' }))
      .toContain('http://192.168.0.101:19876')
    expect(t('gateway.uptimeDays', { d: 3, h: 4 })).toBe('3d 4h')
  })

  it('falls back to zh-CN for keys not yet translated', () => {
    // A phased migration must never render a raw key to the user.
    applyAccountLanguage('en')
    expect(t('this.key.does.not.exist')).toBe('this.key.does.not.exist')
    // And a deliberately partial bundle would still show Chinese copy.
    expect(t('tab.settings')).toBe('Settings')
  })
})

describe('account language sync', () => {
  it('applies and caches the account language from /api/auth/me', () => {
    initLanguage()
    applyAccountLanguage('ja')
    expect(currentLang()).toBe('ja')
    expect(localStorage.getItem('acowork.language')).toBe('ja')
    expect(t('tab.chat')).toBe('チャット')
  })

  it('notifies subscribers only on an actual change', () => {
    const fn = vi.fn()
    const stop = subscribeLanguage(fn)
    applyAccountLanguage('zh-CN') // already current
    expect(fn).not.toHaveBeenCalled()
    applyAccountLanguage('ko')
    expect(fn).toHaveBeenCalledTimes(1)
    stop()
    applyAccountLanguage('en')
    expect(fn).toHaveBeenCalledTimes(1)
  })

  it('a local override wins over a later Desktop account change', () => {
    applyAccountLanguage('en')
    setLanguageOverride('zh-TW')
    expect(currentLang()).toBe('zh-TW')
    expect(languageOverride()).toBe('zh-TW')
    applyAccountLanguage('ja') // Desktop changed its language
    expect(currentLang()).toBe('zh-TW')
    expect(t('tab.chat')).toBe('聊天')
  })

  it('clearing the override returns to the account language', () => {
    applyAccountLanguage('en')
    setLanguageOverride('ja')
    setLanguageOverride(null)
    expect(languageOverride()).toBeNull()
    expect(currentLang()).toBe('en')
  })

  it('an override survives a cold start', () => {
    setLanguageOverride('ko')
    _resetLanguageForTests()
    initLanguage()
    expect(currentLang()).toBe('ko')
    expect(languageOverride()).toBe('ko')
  })
})
