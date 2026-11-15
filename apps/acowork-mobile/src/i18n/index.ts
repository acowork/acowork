/**
 * Zero-dependency i18n core for Mobile. Kept free of React so stores and plain
 * lib modules can call `t()` without pulling a view dependency in.
 *
 * Language precedence:
 *   1. an explicit local override (General → 界面语言 picker),
 *   2. the account's `language` from `/api/auth/me` (Desktop profile writes
 *      the same field, so a Desktop change propagates to the phone),
 *   3. the device locale,
 *   4. zh-CN.
 *
 * The account value is applied on boot / login / pairing / foreground return,
 * so Desktop changes show up without reinstalling or clearing app data.
 */
import { DICTS, LANGS, type Lang } from './locales'
import { MSGIDS } from './msgids'

export type { Lang } from './locales'
export { LANGS, LANG_LABEL } from './locales'

const CACHE_KEY = 'acowork.language'
const OVERRIDE_KEY = 'acowork.languageOverride'

let current: Lang = 'zh-CN'
let override: Lang | null = null
const listeners = new Set<() => void>()

function lsGet(k: string): string | null {
  try { return localStorage.getItem(k) } catch { return null }
}
function lsSet(k: string, v: string | null): void {
  try {
    if (v === null) localStorage.removeItem(k)
    else localStorage.setItem(k, v)
  } catch { /* storage is a convenience, not a boundary */ }
}

/** Map a BCP-47 tag (or a server-side value) onto one of our five bundles. */
export function normalizeLang(raw: string | null | undefined): Lang {
  const s = (raw ?? '').trim().toLowerCase()
  if (!s) return 'zh-CN'
  if (s.startsWith('zh')) return /hant|tw|hk|mo/.test(s) ? 'zh-TW' : 'zh-CN'
  if (s.startsWith('ja')) return 'ja'
  if (s.startsWith('ko')) return 'ko'
  if (s.startsWith('en')) return 'en'
  return 'zh-CN'
}

function storedOverride(): Lang | null {
  const v = lsGet(OVERRIDE_KEY)
  return v && LANGS.includes(v as Lang) ? (v as Lang) : null
}

function deviceLang(): string {
  return typeof navigator !== 'undefined' ? navigator.language : ''
}

export function currentLang(): Lang { return current }
export function languageOverride(): Lang | null { return override }
export function subscribeLanguage(fn: () => void): () => void {
  listeners.add(fn)
  return () => { listeners.delete(fn) }
}

function setCurrent(next: Lang): void {
  if (next === current) return
  current = next
  for (const fn of listeners) fn()
}

/** Call once at cold start, before the first render. */
export function initLanguage(): void {
  override = storedOverride()
  const seed = override ?? lsGet(CACHE_KEY) ?? deviceLang()
  setCurrent(normalizeLang(seed))
}

/** Account language from `/api/auth/me`. An explicit local override wins. */
export function applyAccountLanguage(raw: string | null | undefined): void {
  if (!raw || override) return
  const next = normalizeLang(raw)
  lsSet(CACHE_KEY, next)
  setCurrent(next)
}

/** Manual picker: `null` clears the override and follows the account again. */
export function setLanguageOverride(next: Lang | null): void {
  override = next
  if (next) {
    lsSet(OVERRIDE_KEY, next)
    setCurrent(next)
  } else {
    lsSet(OVERRIDE_KEY, null)
    setCurrent(normalizeLang(lsGet(CACHE_KEY) ?? deviceLang()))
  }
}

/**
 * Translate a key. Two catalogues are consulted:
 *   1. `DICTS` — named keys for boot / connect / settings / tab bar,
 *   2. `MSGIDS` — gettext-style msgids, where the zh-CN source copy IS the key.
 * A miss falls back to zh-CN and finally to the key itself, so an unmigrated
 * string degrades to readable Chinese rather than a raw identifier.
 */
export function t(key: string, vars?: Record<string, string | number>): string {
  const msgids = current === 'zh-CN' ? undefined : MSGIDS[current]
  const raw = DICTS[current][key] ?? msgids?.[key] ?? DICTS['zh-CN'][key] ?? key
  if (!vars) return raw
  return raw.replace(/\{\{(\w+)\}\}/g, (match, name: string) =>
    name in vars ? String(vars[name]) : match,
  )
}

/** Test-only: drop module-level state between cases. */
export function _resetLanguageForTests(): void {
  current = 'zh-CN'
  override = null
  listeners.clear()
}
