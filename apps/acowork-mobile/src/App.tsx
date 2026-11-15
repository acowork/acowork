/**
 * The app shell: boot gate + TabBar + screen host.
 *
 * v1.1 §7: while `authStore.phase` is not `ready`, the shell is replaced by
 * the boot screens (connect / login / disconnected / restricted). The tab
 * stack machinery is untouched — the gate sits ABOVE navigation, not inside
 * it, so a logout cannot leave a half-populated stack behind.
 *
 * Screens are looked up in a registry keyed by route; an unknown route
 * renders an explicit "not implemented" placeholder rather than silently
 * rendering nothing, so a half-built v1 is visible in development instead
 * of looking like a crash.
 */

import { useEffect } from 'react'
import { useNavStore, TABS } from './stores/navStore'
import { useT, useLanguage } from './i18n/useT'
import { useAuthStore } from './stores/authStore'
import { ScreenHost } from './components/ScreenHost'
import { BootingScreen, ConnectScreen, DisconnectedScreen, LoginScreen, RestrictedScreen } from './screens/boot/Screens'
import type { ComponentType } from 'react'
import {
  OutlineChatIcon, FilledChatIcon,
  OutlineProjectsIcon, FilledProjectsIcon,
  OutlineDocsIcon, FilledDocsIcon,
  OutlineSettingsIcon, FilledSettingsIcon,
} from './components/TabIcons'

/**
 * Per-tab icon pair, copied from the Desktop NavBar: the outline variant is
 * shown when inactive, the filled variant when active — same silhouette, so
 * selecting only changes the fill (highlight-coloured), not the shape.
 */
type TabIcon = ComponentType<{ className?: string }>
const TAB_ICONS: Record<string, { outline: TabIcon; filled: TabIcon }> = {
  chat: { outline: OutlineChatIcon, filled: FilledChatIcon },
  projects: { outline: OutlineProjectsIcon, filled: FilledProjectsIcon },
  docs: { outline: OutlineDocsIcon, filled: FilledDocsIcon },
  settings: { outline: OutlineSettingsIcon, filled: FilledSettingsIcon },
}

export function App() {
  const phase = useAuthStore((s) => s.phase)

  // Screens migrated in phase 2 call the plain `t()` (not the reactive `useT`).
  // Subscribing at the root makes a language change re-render the whole tree,
  // so every screen re-evaluates its `tr()` calls in the new language without
  // each leaf having to opt into a hook. The subscription is cheap: the store
  // only notifies when the language value actually changes.
  useLanguage()

  // Android hardware/gesture back button. Without a handler Tauri's default
  // is to close the app on the first press, so every back press exited even
  // mid-navigation. Registering a handler takes over that behaviour: pop the
  // in-app nav stack, and only exit when the current tab is already at its
  // root. The app is a SPA that never touches webview history, so the
  // payload's `canGoBack` is irrelevant here — `pop()` is the source of truth.
  // Guarded so plain-browser dev / vitest (no Tauri runtime) skip it, and
  // loaded dynamically so the module never couples to the runtime at import.
  useEffect(() => {
    if (typeof window === 'undefined' || !('__TAURI_INTERNALS__' in window)) return
    let listener: { unregister: () => Promise<void> } | null = null
    let cancelled = false
    void import('@tauri-apps/api/app')
      .then(({ onBackButtonPress, exit }) =>
        onBackButtonPress(() => {
          if (!useNavStore.getState().pop()) void exit(0)
        }),
      )
      .then((l) => {
        if (cancelled) void l.unregister()
        else listener = l
      })
      .catch(() => {
        /* runtime without the back-button API: leave the default behaviour */
      })
    return () => {
      cancelled = true
      void listener?.unregister()
    }
  }, [])

  if (phase !== 'ready') {
    switch (phase) {
      case 'boot':
        return <BootingScreen />
      case 'connect':
        return <ConnectScreen />
      case 'login':
        return <LoginScreen />
      case 'disconnected':
        return <DisconnectedScreen />
      case 'restricted':
        return <RestrictedScreen />
    }
  }

  return <Shell />
}

function Shell() {
  const stack = useNavStore((s) => s.stacks[s.activeTab])
  const depth = (stack?.length ?? 1) - 1

  return (
    <div className="app">
      <OfflineBanner />
      <main id="screen-root" className="screen-root">
        <ScreenHost />
      </main>
      {/* Detail screens expect .screen-root to fill the full viewport
       * (composer pinned to bottom, etc.). Mounting TabBar inside a
       * reserved slot at every depth shrank it by ~90px and pushed the
       * chat detail upward. TabBar is therefore only mounted at the
       * root depth; transitions run on GPU-promoted layers (see
       * .screen-layer in app.css) which keeps the slide smooth without
       * restructuring layout. */}
      {depth === 0 ? <TabBar /> : null}
    </div>
  )
}

/**
 * Global offline banner (v1.1 §7.5 edge states). Driven by the poll loop's
 * consecutive-failure count: any network failure raises it, any success
 * clears it. It never blocks navigation — reads of cached lists still work.
 */
function OfflineBanner() {
  const t = useT()
  const phase = useAuthStore((s) => s.phase)
  if (phase === 'disconnected') {
    return (
      <div className="banner banner-offline" role="status">
        {t('banner.offline')}
        <button type="button" onClick={() => void useAuthStore.getState().retryConnection()}>
          {t('common.retry')}
        </button>
      </div>
    )
  }
  return null
}

function TabBar() {
  const t = useT()
  const activeTab = useNavStore((s) => s.activeTab)
  const switchTab = useNavStore((s) => s.switchTab)
  return (
    <nav className="tabbar" role="tablist" aria-label={t('tab.navAria')}>
      {TABS.map((tab) => (
        <button
          key={tab.key}
          type="button"
          role="tab"
          aria-selected={tab.key === activeTab}
          className={`tabbar-item${tab.key === activeTab ? ' is-active' : ''}`}
          onClick={() => switchTab(tab.key)}
        >
          {(() => {
            const set = TAB_ICONS[tab.key]
            if (!set) return <span className="tabbar-icon" aria-hidden />
            const Icon = tab.key === activeTab ? set.filled : set.outline
            return <Icon className="tabbar-svg" />
          })()}
          <span className="tabbar-label">{t(tab.labelKey)}</span>
        </button>
      ))}
    </nav>
  )
}
