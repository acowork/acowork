/**
 * Layered screen host. Renders every entry of the active tab's stack as an
 * absolutely-positioned layer so we can slide-in / slide-out across push and
 * pop without unmounting the underlying screens.
 *
 * Why layered + keyed by route (not by stack position):
 *
 * 1. State preservation. ChatDetailScreen keeps drafts, sheet flags, and
 *    several `useState` slots that the user expects to survive push → pop.
 *    Keying by `${activeTab}:${route}` keeps the SAME React instance mounted
 *    as long as that route is reachable from the stack, so going from
 *    [list → detail → back to list] does not remount the list and lose its
 *    scroll position, drafts, etc.
 *
 * 2. Animation. A back gesture has to slide the popped screen off the right
 *    WHILE the previous screen is still visible underneath. That requires
 *    two layers overlapping, which means at least one extra layer is needed
 *    past the moment React would normally unmount the popped route. We park
 *    the popped route in `exits` long enough for the CSS animation, then
 *    drop the entry.
 *
 * 3. Tab switch is direction='none', so no slide — every layer unmounts and
 *    remounts instantly when `activeTab` changes (the key prefix flips).
 *
 * The exit-overlay list filters out any route that was re-pushed before its
 * exit animation finished — without that filter a rapid reverse nav would
 * render the same route as both an exiting and a top layer at the same key,
 * which React collapses to a single node but with a confusing class.
 */

import { useEffect, useLayoutEffect, useRef, useState, type ReactElement } from 'react'
import { useNavStore, type Route, type TabKey } from '../stores/navStore'
import { ROUTES } from '../routes'

const ANIM_MS = 240
const ANIM_CLEANUP_MS = ANIM_MS + 20

/** Inline placeholder so a half-built route stays visible in development
 *  rather than looking like a crash (matches App.tsx's prior behaviour). */
function Placeholder({ route }: { route: Route }) {
  return (
    <div className="empty-state">
      <div className="empty-title">未实现</div>
      <div className="empty-sub">{String(route)}</div>
    </div>
  )
}

/** One tab's worth of layers. Mounted keyed by activeTab so a tab switch
 *  tears the whole tree down — no cross-tab ghost layers. */
function TabLayerHost({ tab }: { tab: TabKey }) {
  const stack = useNavStore((s) => s.stacks[tab])
  const direction = useNavStore((s) => s.direction)

  const topRoute = stack[stack.length - 1] ?? ''

  // Routes that just left the stack via pop() and are still animating out.
  // Lives in component state (not the store) because it's purely visual —
  // if a re-render during the animation is dropped, losing the exit is fine.
  const [exits, setExits] = useState<Route[]>([])
  const prevTopRef = useRef<Route>(topRoute)

  // Capture the popped route BEFORE React paints, so we never see the
  // single-frame "popped screen gone, nothing animating" flicker.
  useLayoutEffect(() => {
    const prev = prevTopRef.current
    if (prev === topRoute) return
    if (direction === 'back') {
      setExits((es) => (es.includes(prev) ? es : [...es, prev]))
    } else {
      // Forward or 'none' (tab switch): no exit, drop any stale ones.
      setExits([])
    }
    prevTopRef.current = topRoute
  }, [topRoute, direction])

  // Drop the exit after the animation window — a small buffer past ANIM_MS
  // covers the slowest frame timing on cold JIT.
  useEffect(() => {
    if (exits.length === 0) return
    const t = setTimeout(() => setExits([]), ANIM_CLEANUP_MS)
    return () => clearTimeout(t)
  }, [exits])

  const layers: ReactElement[] = []

  // ponytail: dedupe stack entries at render time. A double-tap on a list
  // row that calls push() with the same route twice leaves [root, X, X] in
  // the stack — the two X layers would otherwise have the same React key
  // and a console warning would fire. Rendering the LAST occurrence is the
  // cheapest fix: the earlier copy is shadowed at translateX(0) anyway, so
  // dropping it changes nothing visually, and its state was overwritten by
  // the second mount anyway. ceiling: if we ever need to show both copies
  // side-by-side (e.g. side-by-side compare), revisit.
  const seen = new Set<Route>()
  const uniqueStack = stack.filter((r) => (seen.has(r) ? false : (seen.add(r), true)))

  // Render every stack entry as its own layer. The top-most one animates in
  // on push; everything below is static. z-index keeps the stack visually
  // ordered so the top covers the layers beneath.
  uniqueStack.forEach((route, i) => {
    const Screen = ROUTES[route]
    const isTop = i === uniqueStack.length - 1
    const cls =
      isTop && direction === 'forward'
        ? 'screen-layer screen-layer-forward'
        : 'screen-layer screen-layer-static'
    layers.push(
      <div
        key={`${tab}:${route}`}
        className={cls}
        style={{ zIndex: i }}
      >
        {Screen ? <Screen /> : <Placeholder route={route} />}
      </div>,
    )
  })

  // Render the popped route(s) on top of everything with the exit anim.
  // Exclude any that are already in the stack — a re-push mid-exit cancels
  // the animation by simply not double-mounting the route.
  for (const route of exits) {
    if (stack.includes(route)) continue
    const Screen = ROUTES[route]
    if (!Screen) continue
    layers.push(
      <div
        key={`${tab}:exit:${route}`}
        className="screen-layer screen-layer-exit"
        style={{ zIndex: 100 }}
      >
        <Screen />
      </div>,
    )
  }

  return <>{layers}</>
}

export function ScreenHost() {
  const activeTab = useNavStore((s) => s.activeTab)
  // Re-mounting per tab is the simplest way to guarantee no cross-tab layers
  // leak through, and it costs nothing — they were about to be torn down
  // anyway on most tab switches.
  return <TabLayerHost key={activeTab} tab={activeTab} />
}
