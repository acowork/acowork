/**
 * React binding for the i18n core.
 *
 * `useSyncExternalStore` is used instead of a hand-rolled subscription:
 * Desktop's `useTranslation.ts` had to work around a broken react-i18next v17
 * + React 19 subscription, and Mobile would inherit the same failure mode if
 * it borrowed that stack. The store snapshot is a plain string, so React can
 * cheaply detect a change and re-render every screen using `useT()`.
 */
import { useSyncExternalStore } from 'react'
import { currentLang, subscribeLanguage, t } from './index'
import type { Lang } from './locales'

/** Returns the stable `t` function and re-renders the caller on language change. */
export function useT(): typeof t {
  useSyncExternalStore(subscribeLanguage, currentLang, currentLang)
  return t
}

/** Current language, for controls that display a selected value. */
export function useLanguage(): Lang {
  return useSyncExternalStore(subscribeLanguage, currentLang, currentLang)
}

export { t }
