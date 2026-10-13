/**
 * Status-bar / navigation-bar clearance.
 *
 * The shell already reserves the safe area — `.navbar` pads its top by
 * `--safe-top`, the TabBar and composer pad their bottom by `--safe-bottom`
 * (styles/tokens.css). The problem is where that value comes from: on
 * Android the WebView derives `env(safe-area-inset-*)` from the *display
 * cutout* alone, so a notch-less phone reports `0px` in every direction
 * while the activity is edge-to-edge and the webview really does paint under
 * the status bar. The chat back button then lands under the clock — measured
 * on a HUAWEI Mate 10 (BLA-AL00): all four `env()` values 0, `.navbar`
 * top 0, status bar 24 CSS px.
 *
 * So the value is resolved here instead: ask the platform for the real
 * window insets, keep `env()` as an equal input (iOS reports a correct
 * non-zero value there and the native command answers zeros), take the
 * larger, and write the result as an inline custom property on `:root` —
 * which outranks the `:root` rule in tokens.css, so no stylesheet has to
 * know about this.
 *
 * Nothing here invents a floor: in a plain browser there is no status bar,
 * both inputs are 0, and `0px` is the correct answer.
 */
import { invoke } from '@tauri-apps/api/core'

/** A four-side clearance, CSS pixels. */
export interface Insets {
  top: number
  bottom: number
  left: number
  right: number
}

const ZERO: Insets = { top: 0, bottom: 0, left: 0, right: 0 }

/**
 * What the platform command answers: PHYSICAL pixels, because that is what
 * Android's `WindowInsets` and dimen resources are in.
 */
interface NativeInsets {
  top: number
  bottom: number
  left: number
  right: number
}

/**
 * One side's final value in CSS pixels.
 *
 * `dpr` is clamped to >= 1 rather than trusted: a broken scale factor must
 * not turn a real 72px bar into a 72px-tall blank strip, and it must not
 * divide by zero.
 */
export function resolveInset(envPx: number, nativePx: number, dpr: number): number {
  const env = Number.isFinite(envPx) && envPx > 0 ? envPx : 0
  const native = Number.isFinite(nativePx) && nativePx > 0 ? nativePx / Math.max(1, dpr) : 0
  return Math.max(env, native)
}

/**
 * Read `env(safe-area-inset-*)` by letting the engine lay out a probe.
 *
 * `getComputedStyle(documentElement).getPropertyValue(...)` cannot be used:
 * the var is declared with an `env()` fallback, so it would return the
 * unresolved `env(...)` string on every engine that does support it.
 */
export function measureEnvInsets(): Insets {
  if (typeof document === 'undefined') return { ...ZERO }
  const probe = document.createElement('div')
  probe.style.cssText =
    'position:fixed;top:0;left:0;width:0;height:0;visibility:hidden;pointer-events:none;' +
    'padding-top:env(safe-area-inset-top,0px);' +
    'padding-bottom:env(safe-area-inset-bottom,0px);' +
    'padding-left:env(safe-area-inset-left,0px);' +
    'padding-right:env(safe-area-inset-right,0px)'
  document.body.appendChild(probe)
  const cs = getComputedStyle(probe)
  const px = (v: string): number => Number.parseFloat(v) || 0
  const out: Insets = {
    top: px(cs.paddingTop),
    bottom: px(cs.paddingBottom),
    left: px(cs.paddingLeft),
    right: px(cs.paddingRight),
  }
  probe.remove()
  return out
}

/** Publish the resolved clearance. Inline on `:root` outranks tokens.css. */
export function publishInsets(insets: Insets): void {
  if (typeof document === 'undefined') return
  const s = document.documentElement.style
  s.setProperty('--safe-top', `${insets.top}px`)
  s.setProperty('--safe-bottom', `${insets.bottom}px`)
}

/**
 * Native clearance, physical pixels. Fails closed to zeros: outside Tauri
 * (a browser tab, vitest) there is no command host, and zeros leave `env()`
 * in charge — which is correct there.
 */
async function readNativeInsets(): Promise<NativeInsets> {
  try {
    return await invoke<NativeInsets>('system_bar_insets')
  } catch {
    return { ...ZERO }
  }
}

/** Resolve once and publish. Idempotent; safe to call on every resize. */
export async function syncSafeArea(): Promise<Insets> {
  const env = measureEnvInsets()
  const native = await readNativeInsets()
  const dpr = typeof window === 'undefined' ? 1 : window.devicePixelRatio || 1
  const resolved: Insets = {
    top: resolveInset(env.top, native.top, dpr),
    bottom: resolveInset(env.bottom, native.bottom, dpr),
    left: resolveInset(env.left, native.left, dpr),
    right: resolveInset(env.right, native.right, dpr),
  }
  publishInsets(resolved)
  return resolved
}

/**
 * Keep the clearance in sync for the life of the page; returns the teardown.
 *
 * Rotation and window resize are the cases that change a bar's size (and the
 * side it lands on), and the keyboard is not one of them — the activity runs
 * `adjust=resize`, so the webview itself shrinks.
 */
export function startSafeAreaSync(): () => void {
  void syncSafeArea()
  const onViewportChange = (): void => {
    void syncSafeArea()
  }
  window.addEventListener('resize', onViewportChange)
  window.addEventListener('orientationchange', onViewportChange)
  return () => {
    window.removeEventListener('resize', onViewportChange)
    window.removeEventListener('orientationchange', onViewportChange)
  }
}
