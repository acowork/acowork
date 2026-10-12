/**
 * Auth + boot state machine (v1.1 §7). The whole app gates on `phase`.
 *
 *   boot ──no url──▶ connect ──probe ok──▶ login ──login ok──▶ ready
 *     │                │                     ▲                   │
 *     ├─url+token─me ok──────────────────────┼──── 401 ladder ───┘
 *     ├─url+token─me fail──▶ login ──────────┘
 *     └─probe fail with saved url──▶ disconnected (retry / change address)
 *
 * `restricted` renders when `/api/status` says `requires_setup` — the
 * Gateway has no usable admin password yet and mobile never fixes that;
 * it points the user at the Desktop instead.
 *
 * Tokens live in the WebView's localStorage. The access token is 15-minute
 * lived and the refresh family is revocable server-side, so a stolen
 * device loses real value quickly; a native secure-store swap is a v1.1
 * item, not a v1 blocker.
 *
 * The 401→refresh→replay ladder is single-flight: parallel 401s share one
 * rotation, because two refreshes of the same token would trip the
 * Gateway's reuse detection and revoke the whole family.
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
import type { AccountMe, GatewayStatus, TokenPair } from '../lib/types'
import { clearUnread } from '../lib/unread'

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
  /** Username/password error text for the login screen. */
  loginError: string
  busy: boolean

  /** Cold start: decide the phase from persisted state. */
  boot(): Promise<void>
  /** Connect screen: probe a URL, branch on auth_mode/requires_setup. */
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
      console.log('[AUTH] boot() probeStatus ok, auth_mode=', st.auth_mode)
      set({ status: st })
      if (st.requires_setup) {
        console.log('[AUTH] boot() → restricted')
        set({ phase: 'restricted' })
        return
      }
      if (st.auth_mode === 'local') {
        // No account system: the Gateway's single bearer token is minted by
        // Desktop; mobile v1 only supports multi_user gateways in practice,
        // but a tokenless local gateway still gets a ready state.
        console.log('[AUTH] boot() →', access ? 'ready' : 'login', '(local mode)')
        set({ phase: access ? 'ready' : 'login' })
        return
      }
      if (access && refresh) {
        try {
          const me = await fetchMe()
          console.log('[AUTH] boot() fetchMe ok → ready')
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
      if (st.requires_setup) {
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
      console.log('[AUTH] login() → ready')
      set({ busy: false, phase: 'ready', me })
      return true
    } catch (e) {
      console.log('[AUTH] login() FAILED', e)
      const msg =
        e instanceof GatewayError && e.status === 401
          ? '用户名或密码错误'
          : e instanceof GatewayError && e.status === 422
            ? '账号未启用或需要首次登录，请在桌面端处理'
            : '无法连接网关，请检查网络或稍后重试'
      set({ busy: false, loginError: msg })
      return false
    }
  },

  pairLogin: async (baseUrl, pairingToken) => {
    const clean = baseUrl.trim().replace(/\/+$/, '')
    if (!/^https?:\/\//.test(clean)) {
      set({ loginError: '网关地址无效'
      })
      return false
    }
    set({ busy: true, loginError: '' })
    try {
      // 先落地址再兑换：redeemPairing 直接打 clean，后续 fetchMe 则走 bridge 的 baseUrl。
      lsSet(LS.url, clean)
      set({ baseUrl: clean })
      const pair = await redeemPairing(clean, pairingToken)
      get()._setTokens(pair)
      // 与 boot/probe 一致地拉 status；兑换成功即说明网关可用且已鉴权，requires_setup
      // 不应发生，但仍保守探测。me 失败不阻断进入 ready（与 login 同构）。
      const st = await probeStatus(clean).catch(() => null)
      const me = await fetchMe().catch((e) => { console.log('[AUTH] pairLogin() fetchMe error (non-fatal)', e); return null })
      set({ busy: false, status: st, me, phase: 'ready' })
      return true
    } catch (e) {
      const msg =
        e instanceof GatewayError && e.status === 401
          ? '二维码无效或已过期，请在桌面端重新生成'
          : '无法连接网关，请确认手机与电脑在同一网络'
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
    // The unread dot is per-device read state; a shared phone must not carry
    // it to the next account.
    clearUnread()
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
      if (st.requires_setup) set({ phase: 'restricted' })
      else if (get().accessToken && get().refreshToken) {
        try {
          await fetchMe()
          console.log('[AUTH] retryConnection() → ready')
          set({ phase: 'ready' })
        } catch (e) {
          console.log('[AUTH] retryConnection() fetchMe failed → login', e)
          set({ phase: 'login' })
        }
      } else { console.log('[AUTH] retryConnection() no token → login'); set({ phase: 'login' }) }
    } catch (e) {
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
        console.log('[AUTH] _refresh() FAILED → sessionExpired', e)
        get()._sessionExpired()
        return null
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
    set({ accessToken: null, refreshToken: null, phase: 'login', loginError: '登录已过期，请重新登录' })
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
