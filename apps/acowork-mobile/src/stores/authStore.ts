/**
 * Auth + boot state machine (v1.1 §7). The whole app gates on `phase`.
 *
 *   boot ──no url──▶ connect ──probe ok──▶ login ──login ok──▶ ready
 *     │                │                     ▲                   │
 *     ├─url+token─me ok──────────────────────┼──── 401 ladder ───┘
 *     ├─url+token─me fail──▶ login ──────────┘
 *     └─probe fail with saved url──▶ disconnected (retry / change address)
 *
 * `restricted` renders when `/api/status` reports `uninitialized` or
 * `needs_setup` (ADR-114) — the Gateway has no usable admin password yet
 * and mobile never fixes that; it points the user at the Desktop instead.
 * An absent/unknown state is treated as `ready`, matching the Desktop and
 * the Gateway's own startup-window default: a broken status payload must
 * not lock the client into a setup screen it cannot complete.
 *
 * Tokens live in the WebView's localStorage. The access token is 15-minute
 * lived and the refresh family is revocable server-side, so a stolen
 * device loses real value quickly; a native secure-store swap is a v1.1
 * item, not a v1 blocker.
 *
 * The 401→refresh→replay ladder is single-flight: parallel 401s share one
 * rotation, because two refreshes of the same token would trip the
 * Gateway's reuse detection and revoke the whole family.
 *
 * Every successful `/auth/me` also applies the account's `language` (see
 * `i18n`) — the Desktop profile dropdown writes the same server field, so
 * a language change made there reaches the phone without a reinstall.
 */

import { create } from 'zustand'
import {
  GatewayError,
  loginRequest,
  logoutRequest,
  probeStatus,
  refreshRequest,
  redeemPairing,
  setAuthBridge,
  fetchMe,
} from '../lib/api'
import { applyAccountLanguage } from '../i18n'
import type { AccountMe, GatewayStatus, TokenPair } from '../lib/types'

export type BootPhase =
  | 'boot'
  | 'connect'
  | 'login'
  | 'restricted'
  | 'disconnected'
  | 'ready'

const LS = {
  url: 'acowork.gatewayUrl',
  access: 'acowork.accessToken',
  refresh: 'acowork.refreshToken',
  me: 'acowork.me',
}

/**
 * ADR-114: `requires_setup` is not a wire field — it is derived from state.
 * Only the two explicit restricted states gate the client. `undefined` /
 * unrecognised values fail open to `ready`, the same rule as the Desktop's
 * `fetchAuthPolicy()`; `st.account_state !== 'ready'` used to fail *closed*
 * and stranded Mobile on a restricted screen whenever `/api/status` came
 * back without the field.
 */
function needsSetup(st: GatewayStatus): boolean {
  return st.account_state === 'uninitialized' || st.account_state === 'needs_setup'
}

function lsGet(k: string): string | null {
  try {
    return localStorage.getItem(k)
  } catch {
    return null
  }
}
function lsSet(k: string, v: string | null): void {
  try {
    if (v === null) localStorage.removeItem(k)
    else localStorage.setItem(k, v)
  } catch {
    /* storage is a convenience here, not a safety boundary */
  }
}

interface AuthStore {
  phase: BootPhase
  baseUrl: string
  status: GatewayStatus | null
  accessToken: string | null
  refreshToken: string | null
  /**
   * The resolved account, or null until `/auth/me` succeeds. User-to-user
   * chat addresses every route as `/api/users/{me.user_id}/…`, so this is
   * not a convenience: without it a 联系人 thread cannot be named or opened.
   */
  me: AccountMe | null
  /**
   * Login error as an i18n key (or a raw server message). Render with `t()`:
   * a key resolves to the active language, an unknown string falls back to
   * itself. Storing the key — not a translated snapshot — is what makes the
   * error re-translate when the language changes.
   */
  loginError: string
  busy: boolean

  /** Cold start: decide the phase from persisted state. */
  boot(): Promise<void>
  /** Connect screen: probe a URL, branch on account_state. */
  probe(url: string): Promise<boolean>
  login(username: string, password: string): Promise<boolean>
  /** 方案③：扫码兑换配对票据 -> 落 token -> ready（含地址）。 */
  pairLogin(baseUrl: string, pairingToken: string): Promise<boolean>
  logout(): Promise<void>
  /** Retry after `disconnected` — re-probes the saved URL. */
  retryConnection(): Promise<void>
  /** Back to the connect screen (change address). */
  changeAddress(): void

  // — bridge callbacks used by lib/api —
  _setTokens(p: TokenPair): void
  _refresh(): Promise<string | null>
  _sessionExpired(): void
}

let inflightRefresh: Promise<string | null> | null = null

/** Apply the account language wherever a fresh `/auth/me` lands. */
function syncLanguage(me: AccountMe | null): void {
  if (me) applyAccountLanguage(me.language)
}

export const useAuthStore = create<AuthStore>((set, get) => ({
  phase: 'boot',
  baseUrl: '',
  status: null,
  accessToken: null,
  refreshToken: null,
  me: null,
  loginError: '',
  busy: false,

  boot: async () => {
    console.log('[AUTH] boot() start')
    const url = lsGet(LS.url)
    const access = lsGet(LS.access)
    const refresh = lsGet(LS.refresh)
    console.log('[AUTH] boot() url=', url, 'access?', !!access, 'refresh?', !!refresh)
    if (!url) {
      console.log('[AUTH] boot() → connect (no url)')
      set({ phase: 'connect', baseUrl: '' })
      return
    }
    set({ baseUrl: url, accessToken: access, refreshToken: refresh })
    try {
      const st = await probeStatus(url)
      console.log('[AUTH] boot() probeStatus ok, account_state=', st.account_state)
      set({ status: st })
      if (needsSetup(st)) {
        console.log('[AUTH] boot() → restricted')
        set({ phase: 'restricted' })
        return
      }
      if (access && refresh) {
        try {
          const me = await fetchMe()
          console.log('[AUTH] boot() fetchMe ok → ready')
          syncLanguage(me)
          set({ me, phase: 'ready' })
          return
        } catch (e) {
          console.log('[AUTH] boot() fetchMe FAILED', e instanceof GatewayError ? `status=${e.status}` : e)
          if (!(e instanceof GatewayError) || e.status !== 401) {
            console.log('[AUTH] boot() → disconnected (non-401)')
            set({ phase: 'disconnected' })
            return
          }
        }
      }
      console.log('[AUTH] boot() → login (no token or 401)')
      set({ phase: 'login' })
    } catch (e) {
      console.log('[AUTH] boot() probeStatus threw → disconnected', e)
      set({ phase: 'disconnected' })
    }
  },

  probe: async (url) => {
    const clean = url.trim().replace(/\/+$/, '')
    if (!/^https?:\/\//.test(clean)) {
      set({ status: null })
      return false
    }
    set({ busy: true })
    try {
      const st = await probeStatus(clean)
      lsSet(LS.url, clean)
      set({ baseUrl: clean, status: st, busy: false })
      if (needsSetup(st)) {
        set({ phase: 'restricted' })
      } else {
        set({ phase: 'login' })
      }
      return true
    } catch {
      set({ busy: false, status: null })
      return false
    }
  },

  login: async (username, password) => {
    console.log('[AUTH] login() start user=', username)
    set({ busy: true, loginError: '' })
    try {
      const pair = await loginRequest(get().baseUrl, username, password)
      console.log('[AUTH] login() loginRequest ok, setting tokens')
      get()._setTokens(pair)
      // Best-effort: reaching `ready` must not hinge on the profile call. A
      // failure leaves `me` null and the screens that need it say so, rather
      // than rendering a thread that silently appears to have no messages.
      const me = await fetchMe().catch((e) => { console.log('[AUTH] login() fetchMe error (non-fatal)', e); return null })
      syncLanguage(me)
      console.log('[AUTH] login() → ready')
      set({ busy: false, phase: 'ready', me })
      return true
    } catch (e) {
      console.log('[AUTH] login() FAILED', e)
      const msg =
        e instanceof GatewayError && e.status === 401
          ? 'auth.wrongCredentials'
          : e instanceof GatewayError && e.status === 422
            ? 'auth.accountDisabled'
            : 'auth.unreachable'
      set({ busy: false, loginError: msg })
      return false
    }
  },

  pairLogin: async (baseUrl, pairingToken) => {
    const clean = baseUrl.trim().replace(/\/+$/, '')
    if (!/^https?:\/\//.test(clean)) {
      set({ loginError: 'auth.invalidGateway' })
      return false
    }
    set({ busy: true, loginError: '' })
    try {
      // 先落地址再兑换：redeemPairing 直接打 clean，后续 fetchMe 则走 bridge 的 baseUrl。
      lsSet(LS.url, clean)
      set({ baseUrl: clean })
      const pair = await redeemPairing(clean, pairingToken)
      get()._setTokens(pair)
      // 与 boot/probe 一致地拉 status；兑换成功即说明网关可用且已鉴权，
      // account_state 应为 ready，但仍保守探测。me 失败不阻断 ready（同 login）。
      const st = await probeStatus(clean).catch(() => null)
      const me = await fetchMe().catch((e) => { console.log('[AUTH] pairLogin() fetchMe error (non-fatal)', e); return null })
      syncLanguage(me)
      set({ busy: false, status: st, me, phase: 'ready' })
      return true
    } catch (e) {
      const msg =
        e instanceof GatewayError && e.status === 401
          ? 'auth.pairInvalid'
          : 'auth.pairUnreachable'
      set({ busy: false, loginError: msg })
      return false
    }
  },

  logout: async () => {
    const { baseUrl, refreshToken } = get()
    if (refreshToken) await logoutRequest(baseUrl, refreshToken)
    lsSet(LS.access, null)
    lsSet(LS.refresh, null)
    lsSet(LS.me, null)
    set({ accessToken: null, refreshToken: null, me: null, phase: 'login', status: null })
    // Keep the saved URL: re-login should not require retyping it.
    get().boot()
  },

  retryConnection: async () => {
    console.log('[AUTH] retryConnection() start')
    const url = get().baseUrl
    set({ busy: true })
    try {
      const st = await probeStatus(url)
      console.log('[AUTH] retryConnection() probeStatus ok')
      set({ status: st, busy: false })
      if (needsSetup(st)) set({ phase: 'restricted' })
      else if (get().accessToken && get().refreshToken) {
        try {
          const me = await fetchMe()
          syncLanguage(me)
          console.log('[AUTH] retryConnection() → ready')
          set({ me, phase: 'ready' })
        } catch (e) {
          console.log('[AUTH] retryConnection() fetchMe failed → login', e)
          set({ phase: 'login' })
        }
      } else { console.log('[AUTH] retryConnection() no token → login'); set({ phase: 'login' }) }
    } catch (e) {
      // Deliberately keep the saved tokens: a dead URL is a connectivity
      // failure, not an expired session (see `_refresh`). The scan action on
      // DisconnectedScreen is the escape hatch that replaces BOTH the stale
      // URL and the token pair when retrying the same address can never work.
      console.log('[AUTH] retryConnection() probeStatus failed', e)
      set({ busy: false })
    }
  },

  changeAddress: () => {
    lsSet(LS.url, null)
    lsSet(LS.access, null)
    lsSet(LS.refresh, null)
    set({ baseUrl: '', accessToken: null, refreshToken: null, status: null, phase: 'connect' })
  },

  _setTokens: (p) => {
    lsSet(LS.access, p.access_token)
    lsSet(LS.refresh, p.refresh_token)
    set({ accessToken: p.access_token, refreshToken: p.refresh_token })
  },

  _refresh: async () => {
    // Single-flight: concurrent 401s must share one rotation.
    if (inflightRefresh) return inflightRefresh
    const rt = get().refreshToken
    const url = get().baseUrl
    if (!rt) { console.log('[AUTH] _refresh() no refreshToken → null'); return null }
    console.log('[AUTH] _refresh() starting rotation')
    inflightRefresh = (async () => {
      try {
        const pair = await refreshRequest(url, rt)
        console.log('[AUTH] _refresh() rotation ok')
        get()._setTokens(pair)
        return pair.access_token
      } catch (e) {
        // Only an explicit auth rejection means the refresh family is dead.
        // A network failure — stale LAN IP after sleep/wake, Gateway asleep,
        // phone Wi-Fi powersave — is NOT a token problem. Rethrow it so the
        // caller sees a connectivity error instead of a fabricated 401, and
        // KEEP the tokens so a later reconnect can still reuse them.
        // `authedFetch` owns the single `sessionExpired()` call: doing it
        // here too fired it twice and mislabelled network outages as
        // "登录已过期" (the token error users kept seeing).
        if (e instanceof GatewayError && (e.status === 401 || e.status === 403)) {
          console.log('[AUTH] _refresh() rejected → session expired', e)
          return null
        }
        console.log('[AUTH] _refresh() network failure, keeping tokens', e)
        throw e
      } finally {
        inflightRefresh = null
      }
    })()
    return inflightRefresh
  },

  _sessionExpired: () => {
    console.log('[AUTH] *** _sessionExpired() CALLED — clearing tokens, → login ***')
    lsSet(LS.access, null)
    lsSet(LS.refresh, null)
    set({ accessToken: null, refreshToken: null, phase: 'login', loginError: 'auth.sessionExpired' })
  },
}))

// Install the transport bridge at module load: `lib/api` stays store-free
// and the cycle never forms (api → nothing, authStore → api).
setAuthBridge({
  baseUrl: () => useAuthStore.getState().baseUrl,
  accessToken: () => useAuthStore.getState().accessToken,
  refresh: () => useAuthStore.getState()._refresh(),
  sessionExpired: () => useAuthStore.getState()._sessionExpired(),
})
