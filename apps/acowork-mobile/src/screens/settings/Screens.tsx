/**
 * Settings tab (§10): root + 个人资料 / 通用 / 外观 / 网关.
 *
 * All five live in one file because four of them are a list of rows over a
 * store field. Splitting them into four modules would be four imports for
 * one shared shape.
 *
 * The rule that decides whether a row exists at all: a setting the mobile
 * shell cannot actually honour is shown disabled with the reason, never as a
 * control that appears to work (§2.1 诚实优于沉默). That is why 通知, 语言 and
 * the Gateway start/stop buttons are present-but-inert.
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

/** Uptime as "3 天 4 小时" — a raw second count is unreadable on a phone. */
function uptime(secs: number): string {
  const d = Math.floor(secs / 86400)
  const h = Math.floor((secs % 86400) / 3600)
  const m = Math.floor((secs % 3600) / 60)
  if (d) return `${d} 天 ${h} 小时`
  if (h) return `${h} 小时 ${m} 分`
  return `${m} 分`
}

function SubScreen({ title, children }: { title: string; children: React.ReactNode }) {
  const pop = useNavStore((s) => s.pop)
  return (
    <EdgeSwipe onBack={() => pop()}>
      <div className="screen">
        <header className="navbar">
          <button type="button" className="navbar-back" onClick={() => pop()} aria-label="返回">返回</button>
          <span className="navbar-title">{title}</span>
          <span className="navbar-trail" />
        </header>
        <div className="scroll">{children}</div>
      </div>
    </EdgeSwipe>
  )
}

export function SettingsRootScreen() {
  const me = useAuthStore((s) => s.me)
  const push = useNavStore((s) => s.push)
  return (
    <div className="screen">
      <header className="navbar navbar-large">
        <h1 className="navbar-title">设置</h1>
      </header>
      <div className="scroll">
        <div className="list-section list-section--settings">
          <ListRow arrow icon={<User size={22} />} iconColor="var(--color-icon-blue)" onClick={() => push('settings/profile')} label="个人资料" hint="账号、头像、角色" value={me?.display_name} />
          <ListRow arrow icon={<Cog size={22} />} iconColor="var(--color-icon-teal)" onClick={() => push('settings/general')} label="通用" hint="语言、通知、默认行为" />
          <ListRow arrow icon={<Palette size={22} />} iconColor="var(--color-icon-orange)" onClick={() => push('settings/appearance')} label="外观" hint="主题、高亮色" />
          <ListRow arrow icon={<Server size={22} />} iconColor="var(--color-icon-green)" onClick={() => push('settings/gateway')} label="网关" hint="连接地址、状态" />
        </div>

        <div className="list-section-title">仅桌面端</div>
        <div className="list-section list-section--settings">
          <ListRow disabled icon={<Wrench size={22} />} label="Harness" hint="移动端不提供，请在桌面端使用" />
          <ListRow disabled icon={<Puzzle size={22} />} label="扩展 / Skills" hint="移动端不提供，请在桌面端使用" />
        </div>
      </div>
    </div>
  )
}

export function ProfileScreen() {
  const me = useAuthStore((s) => s.me)
  if (!me) {
    return (
      <SubScreen title="个人资料">
        <ListRow label="未登录" />
      </SubScreen>
    )
  }
  const city = profileField(me.city)
  const occupation = profileField(me.occupation)
  const lastLogin = profileField(me.last_login_at)
  return (
    <SubScreen title="个人资料">
      <ListRow
        label="头像"
        value={profileField(me.builtin_avatar) ? <span className="badge">{profileField(me.builtin_avatar)}</span> : profileField(me.avatar) ? '自定义' : <span className="badge">默认</span>}
      />
      <ListRow label="昵称" value={me.display_name} />
      <ListRow label="用户名" value={me.username} />
      <ListRow label="角色" value={me.role === 'admin' ? '管理员' : '用户'} />
      {occupation ? <ListRow label="职业" value={occupation} /> : null}
      {city ? <ListRow label="城市" value={city} /> : null}
      <ListRow label="账号 ID" hint={me.user_id} />
      {lastLogin ? <ListRow label="最近登录" value={new Date(lastLogin).toLocaleString('zh-CN')} /> : null}
      <div style={{ padding: 'var(--space-3) var(--space-4)' }}>
        <Banner tone="info">头像、昵称与资料的修改在桌面端进行（v1 移动端只读）</Banner>
      </div>
    </SubScreen>
  )
}

/** A profile field the user never filled arrives as `""`, not absent. */
function profileField(v: string | null | undefined): string | null {
  const t = (v ?? '').trim()
  return t ? t : null
}

const LANGUAGE_LABEL: Record<string, string> = { 'zh-CN': '简体中文', en: 'English' }

export function GeneralScreen() {
  const me = useAuthStore((s) => s.me)
  return (
    <SubScreen title="通用">
      {/* The account's language and timezone are real server state, shown
          read-only; the interface strings themselves are zh-CN only in v1. */}
      <ListRow disabled label="语言" value={LANGUAGE_LABEL[me?.language ?? ''] ?? me?.language ?? '—'} hint="在桌面端个人资料中修改" />
      <ListRow disabled label="时区" value={profileField(me?.timezone) ?? '—'} hint="在桌面端个人资料中修改" />
      <ListRow disabled label="通知" value="关闭" hint="v1 不做后台推送，仅应用内实时" />
    </SubScreen>
  )
}

export function AppearanceScreen() {
  const theme = useSettingsStore((s) => s.theme)
  const accent = useSettingsStore((s) => s.accent)
  const setTheme = useSettingsStore((s) => s.setTheme)
  const setAccent = useSettingsStore((s) => s.setAccent)
  // The swatch must show the shade the app will actually paint with: accents
  // carry a separate light/dark value, and a raw accent is what the highlight
  // colour is (the darkened --color-tint-strong is a derived surface colour).
  const resolved = resolveTheme(theme)
  return (
    <SubScreen title="外观">
      <div className="list-section-title" style={{ padding: 'var(--space-3) var(--space-4) var(--space-2)' }}>
        主题
      </div>
      <div style={{ padding: '0 var(--space-4) var(--space-4)' }}>
        <Segmented<ThemePref>
          value={theme}
          onChange={setTheme}
          options={[
            { value: 'light', label: '浅色' },
            { value: 'dark', label: '深色' },
            { value: 'system', label: '跟随系统' },
          ]}
        />
      </div>

      <div className="list-section-title" style={{ padding: '0 var(--space-4) var(--space-2)' }}>高亮色</div>
      <div className="accent-row" role="radiogroup" aria-label="高亮色">
        {ACCENTS.map((a) => (
          <button
            key={a.id}
            type="button"
            role="radio"
            aria-checked={accent === a.id}
            aria-label={a.label}
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
    <SubScreen title="网关">
      <ListRow label="服务器地址" hint={baseUrl || '未设置'} />
      <ListRow label="版本" value={status?.version ?? '—'} />
      <ListRow label="模式" value={status?.auth_mode === 'multi_user' ? '多用户' : '本地'} />
      <ListRow label="运行时长" value={status ? uptime(status.uptime_secs) : '—'} />
      <ListRow label="Agent" value={status ? `${status.agents_running} / ${status.agents_installed}` : '—'} hint="运行中 / 已安装" />
      <ListRow label="MQTT 端口" value={status ? String(status.mqtt_port) : '—'} />

      <div className="list-section-title" style={{ padding: 'var(--space-3) var(--space-4) 0' }}>进程控制</div>
      {/* The mobile shell has no Gateway process to control (§11.1). The
          rows stay visible so the difference from Desktop is legible. */}
      <ListRow disabled label="启动 / 停止 / 重启 Gateway" value={<span className="badge">仅桌面端</span>} />

      <ListRow
        destructive
        label={<span className="row-title">更换服务器地址</span>}
        hint="清除本机保存的地址与登录状态"
        onClick={changeAddress}
      />

      <div style={{ padding: 'var(--space-4)' }}>
        {confirming ? (
          <div className="review-actions" style={{ padding: 0 }}>
            <button type="button" className="review-btn" onClick={() => setConfirming(false)}>取消</button>
            <button type="button" className="review-btn reject" onClick={() => void logout()}>确认退出</button>
          </div>
        ) : (
          <button type="button" className="logout-btn" onClick={() => setConfirming(true)}>退出登录</button>
        )}
      </div>
    </SubScreen>
  )
}
