/**
 * Boot gate screens (v1.1 §7). The App renders one of these instead of the
 * tab shell until `authStore.phase === 'ready'`.
 *
 * They are dumb: every decision (probe, login, refresh, expiry) lives in
 * `authStore` / `lib/api`, because Mobile — like Desktop — carries no
 * business logic in the view layer. Copy comes from `i18n/useT` so a Desktop
 * profile language change reaches the very first screen, before sign-in.
 */

import { useState } from 'react'
import { useAuthStore } from '../../stores/authStore'
import { ScanModal } from './ScanModal'
import { useT } from '../../i18n/useT'
// Shared brand wordmark (repo-root assets) — same file the Desktop splash uses.
import brandMark from '../../../../../assets/brand-mark.svg'

function Shell({ children }: { children: React.ReactNode }) {
  return <div className="app"><div className="boot-screen">{children}</div></div>
}

export function ConnectScreen() {
  const t = useT()
  const probe = useAuthStore((s) => s.probe)
  const busy = useAuthStore((s) => s.busy)
  const [url, setUrl] = useState('')
  const [err, setErr] = useState('')
  const [scanning, setScanning] = useState(false)

  const submit = async () => {
    setErr('')
    if (!/^https?:\/\/.+/.test(url.trim())) {
      setErr(t('connect.invalidUrl'))
      return
    }
    const ok = await probe(url)
    // No "allow LAN" toggle exists on Desktop — the Gateway binds 0.0.0.0 by
    // default. A failed probe is almost always: not on the same network, the
    // Gateway is down, or the phone's Wi-Fi power-save dropped the LAN link
    // (an adb-reverse USB tunnel sidesteps it). Point at those, not a phantom switch.
    if (!ok) setErr(t('connect.failed'))
  }

  return (
    <Shell>
      <img className="boot-logo" src={brandMark} alt="ACowork" />
      <div className="boot-title">{t('connect.title')}</div>
      <div className="boot-sub">{t('connect.sub')}</div>
      <div className="boot-form">
        <button type="button" className="boot-primary" onClick={() => setScanning(true)}>
          {t('connect.scan')}
        </button>
        <input
          className={`boot-input${err ? ' is-error' : ''}`}
          placeholder="http://192.168.1.23:19876"
          value={url}
          onChange={(e) => setUrl(e.target.value)}
          onKeyDown={(e) => e.key === 'Enter' && void submit()}
          autoComplete="off"
          spellCheck={false}
        />
        {err ? <div className="boot-error">{err}</div> : null}
        <button type="button" className="boot-secondary" onClick={() => void submit()} disabled={busy}>
          {busy ? t('common.checking') : t('connect.manual')}
        </button>
        <div className="boot-hint">{t('connect.hint')}</div>
      </div>
      {scanning ? <ScanModal onClose={() => setScanning(false)} /> : null}
    </Shell>
  )
}

export function LoginScreen() {
  const t = useT()
  const login = useAuthStore((s) => s.login)
  const busy = useAuthStore((s) => s.busy)
  const loginError = useAuthStore((s) => s.loginError)
  const baseUrl = useAuthStore((s) => s.baseUrl)
  const changeAddress = useAuthStore((s) => s.changeAddress)
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [scanning, setScanning] = useState(false)

  const submit = () => {
    if (!username || !password) return
    void login(username, password)
  }

  return (
    <Shell>
      <div className="boot-title">{t('login.title')}</div>
      <div className="boot-sub">{baseUrl}</div>
      <div className="boot-form">
        <input
          className="boot-input"
          placeholder={t('login.username')}
          value={username}
          onChange={(e) => setUsername(e.target.value)}
          autoComplete="username"
        />
        <input
          className={`boot-input${loginError ? ' is-error' : ''}`}
          type="password"
          placeholder={t('login.password')}
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          onKeyDown={(e) => e.key === 'Enter' && submit()}
          autoComplete="current-password"
        />
        {loginError ? <div className="boot-error">{t(loginError)}</div> : null}
        <button type="button" className="boot-primary" onClick={submit} disabled={busy || !username || !password}>
          {busy ? t('login.submitting') : t('login.submit')}
        </button>
        <button type="button" className="boot-secondary" onClick={() => setScanning(true)}>
          {t('login.scan')}
        </button>
        <button type="button" className="boot-secondary" onClick={changeAddress}>
          {t('login.changeAddress')}
        </button>
        <div className="boot-hint">{t('login.hint')}</div>
      </div>
      {scanning ? <ScanModal onClose={() => setScanning(false)} /> : null}
    </Shell>
  )
}

export function DisconnectedScreen() {
  const t = useT()
  const baseUrl = useAuthStore((s) => s.baseUrl)
  const retry = useAuthStore((s) => s.retryConnection)
  const change = useAuthStore((s) => s.changeAddress)
  const busy = useAuthStore((s) => s.busy)
  const [scanning, setScanning] = useState(false)

  return (
    <Shell>
      <div className="boot-title">{t('disconnected.title')}</div>
      <div className="boot-sub">
        {t('disconnected.sub', { url: baseUrl || t('disconnected.gatewayFallback') })}
      </div>
      <div className="boot-form">
        {/* `retryConnection` always re-probes the SAME saved URL, so once that
         *  address is stale the old screen was a dead end: retry could never
         *  succeed and there was no login/scan route out. Pairing replaces the
         *  URL and the token pair in one shot, so offer it as the primary
         *  action instead of leaving the user stuck. */}
        <button type="button" className="boot-primary" onClick={() => setScanning(true)}>
          {t('disconnected.scan')}
        </button>
        <button type="button" className="boot-secondary" onClick={() => void retry()} disabled={busy}>
          {busy ? t('disconnected.retrying') : t('common.retry')}
        </button>
        <button type="button" className="boot-secondary" onClick={change}>
          {t('disconnected.changeAddress')}
        </button>
      </div>
      {scanning ? <ScanModal onClose={() => setScanning(false)} /> : null}
    </Shell>
  )
}

export function RestrictedScreen() {
  const t = useT()
  const retry = useAuthStore((s) => s.retryConnection)
  const busy = useAuthStore((s) => s.busy)
  return (
    <Shell>
      <div className="boot-title">{t('restricted.title')}</div>
      <div className="boot-sub">{t('restricted.sub')}</div>
      <div className="boot-form">
        <button type="button" className="boot-primary" onClick={() => void retry()} disabled={busy}>
          {busy ? t('common.checking') : t('restricted.retry')}
        </button>
      </div>
    </Shell>
  )
}

export function BootingScreen() {
  const t = useT()
  return (
    <Shell>
      <img className="boot-logo" src={brandMark} alt="ACowork" />
      <div className="boot-sub">{t('boot.loading')}</div>
    </Shell>
  )
}
