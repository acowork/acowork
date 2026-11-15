/**
 * Settings tab (§10): root + 个人资料 / 通用 / 外观 / 网关.
 *
 * All five live in one file because four of them are a list of rows over a
 * store field. Splitting them into four modules would be four imports for
 * one shared shape.
 *
 * The rule that decides whether a row exists at all: a setting the mobile
 * shell cannot actually honour is shown disabled with the reason, never as a
 * control that appears to work (§2.1 诚实优于沉默). Gateway process control,
 * notifications and profile editing are still read-only; LANGUAGE is no
 * longer inert — it is driven by the account field and can be overridden
 * locally from 通用.
 */

import { useEffect, useState } from 'react'
import { useNavStore } from '../../stores/navStore'
import { useAuthStore } from '../../stores/authStore'
import { useSettingsStore } from '../../stores/settingsStore'
import { probeStatus } from '../../lib/api'
import { ACCENTS, resolveTheme, type ThemePref } from '../../lib/theme'
import { ListRow, Segmented, Banner } from '../../components/ui'
import { User, Cog, Palette, Server, Wrench, Puzzle } from 'lucide-react'
import { EdgeSwipe } from '../../components/EdgeSwipe'
import { useT } from '../../i18n/useT'
import {
  LANGS, LANG_LABEL, currentLang, languageOverride, normalizeLang,
  setLanguageOverride, t as tr, type Lang,
} from '../../i18n'

/** Uptime as "3 天 4 小时" — a raw second count is unreadable on a phone. */
function uptime(secs: number): string {
  const d = Math.floor(secs / 86400)
  const h = Math.floor((secs % 86400) / 3600)
  const m = Math.floor((secs % 3600) / 60)
  if (d) return tr('gateway.uptimeDays', { d, h })
  if (h) return tr('gateway.uptimeHours', { h, m })
  return tr('gateway.uptimeMinutes', { m })
}

function SubScreen({ title, children }: { title: string; children: React.ReactNode }) {
  const t = useT()
  const pop = useNavStore((s) => s.pop)
  return (
    <EdgeSwipe onBack={() => pop()}>
      <div className="screen">
        <header className="navbar">
          <button type="button" className="navbar-back" onClick={() => pop()} aria-label={t('settings.navBack')}>{t('settings.navBack')}</button>
          <span className="navbar-title">{title}</span>
          <span className="navbar-trail" />
        </header>
        <div className="scroll">{children}</div>
      </div>
    </EdgeSwipe>
  )
}

export function SettingsRootScreen() {
  const t = useT()
  const me = useAuthStore((s) => s.me)
  const push = useNavStore((s) => s.push)
  return (
    <div className="screen">
      <header className="navbar navbar-large">
        <h1 className="navbar-title">{t('settings.title')}</h1>
      </header>
      <div className="scroll">
        <div className="list-section list-section--settings">
          <ListRow arrow icon={<User size={22} />} iconColor="var(--color-icon-blue)" onClick={() => push('settings/profile')} label={t('settings.profile')} hint={t('settings.profileHint')} value={me?.display_name} />
          <ListRow arrow icon={<Cog size={22} />} iconColor="var(--color-icon-teal)" onClick={() => push('settings/general')} label={t('settings.general')} hint={t('settings.generalHint')} />
          <ListRow arrow icon={<Palette size={22} />} iconColor="var(--color-icon-orange)" onClick={() => push('settings/appearance')} label={t('settings.appearance')} hint={t('settings.appearanceHint')} />
          <ListRow arrow icon={<Server size={22} />} iconColor="var(--color-icon-green)" onClick={() => push('settings/gateway')} label={t('settings.gateway')} hint={t('settings.gatewayHint')} />
        </div>

        <div className="list-section-title">{t('settings.desktopOnlyTitle')}</div>
        <div className="list-section list-section--settings">
          <ListRow disabled icon={<Wrench size={22} />} label={t('settings.harness')} hint={t('settings.desktopOnlyHint')} />
          <ListRow disabled icon={<Puzzle size={22} />} label={t('settings.skills')} hint={t('settings.desktopOnlyHint')} />
        </div>
      </div>
    </div>
  )
}

export function ProfileScreen() {
  const t = useT()
  const me = useAuthStore((s) => s.me)
  if (!me) {
    return (
      <SubScreen title={t('settings.profile')}>
        <ListRow label={t('profile.notLoggedIn')} />
      </SubScreen>
    )
  }
  const city = profileField(me.city)
  const occupation = profileField(me.occupation)
  const lastLogin = profileField(me.last_login_at)
  return (
    <SubScreen title={t('settings.profile')}>
      <ListRow
        label={t('profile.avatar')}
        value={profileField(me.builtin_avatar) ? <span className="badge">{profileField(me.builtin_avatar)}</span> : profileField(me.avatar) ? t('profile.custom') : <span className="badge">{t('profile.default')}</span>}
      />
      <ListRow label={t('profile.nickname')} value={me.display_name} />
      <ListRow label={t('profile.username')} value={me.username} />
      <ListRow label={t('profile.role')} value={me.role === 'admin' ? t('profile.admin') : t('profile.user')} />
      {occupation ? <ListRow label={t('profile.occupation')} value={occupation} /> : null}
      {city ? <ListRow label={t('profile.city')} value={city} /> : null}
      <ListRow label={t('profile.accountId')} hint={me.user_id} />
      {lastLogin ? <ListRow label={t('profile.lastLogin')} value={new Date(lastLogin).toLocaleString(currentLang())} /> : null}
      <div style={{ padding: 'var(--space-3) var(--space-4)' }}>
        <Banner tone="info">{t('profile.readOnly')}</Banner>
      </div>
    </SubScreen>
  )
}

/** A profile field the user never filled arrives as `""`, not absent. */
function profileField(v: string | null | undefined): string | null {
  const text = (v ?? '').trim()
  return text ? text : null
}

/**
 * Language is real, live state now — not the read-only row it used to be.
 * "跟随账户" clears the local override so the account's `language` (written
 * by the Desktop profile dropdown) keeps driving the UI; picking one of the
 * five languages pins it to this device until the user follows the account
 * again.
 */
export function GeneralScreen() {
  const t = useT() // subscribes to language changes and re-renders this screen
  const override = languageOverride()
  const me = useAuthStore((s) => s.me)

  const accountLang = me?.language ? normalizeLang(me.language) : null
  const options: { value: Lang | 'follow'; label: string }[] = [
    { value: 'follow', label: t('general.followAccount') },
    ...LANGS.map((l) => ({ value: l, label: LANG_LABEL[l] })),
  ]

  return (
    <SubScreen title={t('settings.general')}>
      <div className="list-section-title" style={{ padding: 'var(--space-3) var(--space-4) var(--space-2)' }}>
        {t('general.languagePickerTitle')}
      </div>
      <div className="list-section list-section--settings">
        {options.map((o) => (
          <ListRow
            key={o.value}
            label={o.label}
            selected={o.value === 'follow' ? override === null : override === o.value}
            onClick={() => setLanguageOverride(o.value === 'follow' ? null : o.value)}
            value={o.value === 'follow'
              ? (accountLang ? LANG_LABEL[accountLang] : t('common.dash'))
              : undefined}
          />
        ))}
      </div>
      <div style={{ padding: 'var(--space-3) var(--space-4)' }}>
        <Banner tone="info">{t('general.languageHint')}</Banner>
      </div>
      <ListRow disabled label={t('general.timezone')} value={profileField(me?.timezone) ?? t('common.dash')} hint={t('general.editOnDesktop')} />
      <ListRow disabled label={t('general.notifications')} value={t('general.notificationsOff')} hint={t('general.notificationsHint')} />
    </SubScreen>
  )
}

export function AppearanceScreen() {
  const t = useT()
  const theme = useSettingsStore((s) => s.theme)
  const accent = useSettingsStore((s) => s.accent)
  const setTheme = useSettingsStore((s) => s.setTheme)
  const setAccent = useSettingsStore((s) => s.setAccent)
  // The swatch must show the shade the app will actually paint with: accents
  // carry a separate light/dark value, and a raw accent is what the highlight
  // colour is (the darkened --color-tint-strong is a derived surface colour).
  const resolved = resolveTheme(theme)
  return (
    <SubScreen title={t('settings.appearance')}>
      <div className="list-section-title" style={{ padding: 'var(--space-3) var(--space-4) var(--space-2)' }}>
        {t('appearance.theme')}
      </div>
      <div style={{ padding: '0 var(--space-4) var(--space-4)' }}>
        <Segmented<ThemePref>
          value={theme}
          onChange={setTheme}
          options={[
            { value: 'light', label: t('appearance.light') },
            { value: 'dark', label: t('appearance.dark') },
            { value: 'system', label: t('appearance.system') },
          ]}
        />
      </div>

      <div className="list-section-title" style={{ padding: '0 var(--space-4) var(--space-2)' }}>{t('appearance.accent')}</div>
      <div className="accent-row" role="radiogroup" aria-label={t('appearance.accent')}>
        {ACCENTS.map((a) => (
          <button
            key={a.id}
            type="button"
            role="radio"
            aria-checked={accent === a.id}
            aria-label={tr(a.label)}
            className={`accent-swatch${accent === a.id ? ' is-active' : ''}`}
            onClick={() => setAccent(a.id)}
          >
            <span className="accent-dot" style={{ background: resolved === 'dark' ? a.dark : a.light }} />
          </button>
        ))}
      </div>
    </SubScreen>
  )
}

export function GatewayScreen() {
  const t = useT()
  const baseUrl = useAuthStore((s) => s.baseUrl)
  const status = useAuthStore((s) => s.status)
  const changeAddress = useAuthStore((s) => s.changeAddress)
  const logout = useAuthStore((s) => s.logout)
  const [confirming, setConfirming] = useState(false)

  // Refresh gateway status on entry (uptime, agents etc) without touching
  // the auth phase — the old `probe()` call unconditionally set login.
  useEffect(() => {
    if (!baseUrl) return
    probeStatus(baseUrl)
      .then((st) => useAuthStore.setState({ status: st }))
      .catch(() => { /* keep stale status on transient failure */ })
  }, [baseUrl])

  return (
    <SubScreen title={t('settings.gateway')}>
      <ListRow label={t('gateway.server')} hint={baseUrl || t('gateway.notSet')} />
      <ListRow label={t('gateway.version')} value={status?.version ?? t('common.dash')} />
      <ListRow
        label={t('gateway.account')}
        value={
          status?.account_state === 'ready'
            ? t('gateway.accountReady')
            : status?.account_state === 'needs_setup'
              ? t('gateway.accountNeedsSetup')
              : status?.account_state === 'uninitialized'
                ? t('gateway.accountUninitialized')
                : t('common.dash')
        }
      />
      <ListRow label={t('gateway.uptime')} value={status ? uptime(status.uptime_secs) : t('common.dash')} />
      <ListRow label={t('gateway.agent')} value={status ? `${status.agents_running} / ${status.agents_installed}` : t('common.dash')} hint={t('gateway.agentHint')} />
      <ListRow label={t('gateway.mqttPort')} value={status ? String(status.mqtt_port) : t('common.dash')} />

      <div className="list-section-title" style={{ padding: 'var(--space-3) var(--space-4) 0' }}>{t('gateway.processControl')}</div>
      {/* The mobile shell has no Gateway process to control (§11.1). The
          rows stay visible so the difference from Desktop is legible. */}
      <ListRow disabled label={t('gateway.startStop')} value={<span className="badge">{t('common.desktopOnly')}</span>} />

      <ListRow
        destructive
        label={<span className="row-title">{t('gateway.changeAddress')}</span>}
        hint={t('gateway.changeAddressHint')}
        onClick={changeAddress}
      />

      <div style={{ padding: 'var(--space-4)' }}>
        {confirming ? (
          <div className="review-actions" style={{ padding: 0 }}>
            <button type="button" className="review-btn" onClick={() => setConfirming(false)}>{t('gateway.cancel')}</button>
            <button type="button" className="review-btn reject" onClick={() => void logout()}>{t('gateway.confirmLogout')}</button>
          </div>
        ) : (
          <button type="button" className="logout-btn" onClick={() => setConfirming(true)}>{t('gateway.logout')}</button>
        )}
      </div>
    </SubScreen>
  )
}
