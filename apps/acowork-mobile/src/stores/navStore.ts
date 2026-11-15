/**
 * Navigation: four tabs, each with an INDEPENDENT stack (iOS push/pop).
 *
 * Why per-tab stacks rather than one global stack: a user who drills into
 * `设置 > Gateway`, jumps to `文档`, and comes back must land on the
 * Gateway page, not the settings root. One shared stack cannot express
 * that, and retrofitting it later means re-deriving every screen's path.
 *
 * The TabBar is hidden on any depth>0 screen — two navigation bars at
 * once is the classic mobile anti-pattern, and it costs ~49pt of height
 * on a device that has none to spare.
 */

import { create } from 'zustand'

export type TabKey = 'chat' | 'projects' | 'docs' | 'settings'

/** `labelKey` is an i18n key — TabBar resolves it through `useT()` so tab
 *  labels follow the account language like every other screen. */
export const TABS: { key: TabKey; labelKey: string; icon: string }[] = [
  { key: 'chat', labelKey: 'tab.chat', icon: 'message-circle' },
  { key: 'projects', labelKey: 'tab.projects', icon: 'folder-kanban' },
  { key: 'docs', labelKey: 'tab.docs', icon: 'file-text' },
  { key: 'settings', labelKey: 'tab.settings', icon: 'settings' },
]

/** A screen is identified by a route string so deep links stay possible. */
export type Route = string

interface NavStore {
  activeTab: TabKey
  /** One stack per tab; index 0 is the tab root. */
  stacks: Record<TabKey, Route[]>
  /** iOS transition direction for the next render. */
  direction: 'forward' | 'back' | 'none'

  switchTab: (tab: TabKey) => void
  push: (route: Route) => void
  pop: () => boolean
  /** Pop back to the tab root. */
  popToRoot: () => void
  current: () => Route
  depth: () => number
}

const ROOT: Record<TabKey, Route> = {
  chat: 'chat/list',
  projects: 'projects/list',
  docs: 'docs/list',
  settings: 'settings/root',
}

const initialStacks = (): NavStore['stacks'] => ({
  chat: [ROOT.chat],
  projects: [ROOT.projects],
  docs: [ROOT.docs],
  settings: [ROOT.settings],
})

export const useNavStore = create<NavStore>((set, get) => ({
  activeTab: 'chat',
  stacks: initialStacks(),
  direction: 'none',

  switchTab: (tab) => {
    const { activeTab, stacks } = get()
    if (tab === activeTab) {
      // Re-tapping the active tab pops to root — the standard iOS behavior.
      const stack = stacks[tab]
      if (stack.length > 1) set({ stacks: { ...stacks, [tab]: [stack[0]!] }, direction: 'back' })
      return
    }
    // Switching tabs does NOT reset the other tab's stack.
    set({ activeTab: tab, direction: 'none' })
  },

  push: (route) =>
    set((s) => ({
      stacks: { ...s.stacks, [s.activeTab]: [...(s.stacks[s.activeTab] ?? [ROOT[s.activeTab]]), route] },
      direction: 'forward',
    })),

  pop: () => {
    const s = get()
    const stack = s.stacks[s.activeTab] ?? []
    if (stack.length <= 1) return false
    set({ stacks: { ...s.stacks, [s.activeTab]: stack.slice(0, -1) }, direction: 'back' })
    return true
  },

  popToRoot: () =>
    set((s) => {
      const stack = s.stacks[s.activeTab] ?? []
      if (stack.length <= 1) return s
      return { stacks: { ...s.stacks, [s.activeTab]: [stack[0]!] }, direction: 'back' }
    }),

  current: () => {
    const s = get()
    const stack = s.stacks[s.activeTab] ?? []
    return stack[stack.length - 1] ?? ROOT[s.activeTab]
  },

  depth: () => {
    const s = get()
    return (s.stacks[s.activeTab] ?? []).length - 1
  },
}))
