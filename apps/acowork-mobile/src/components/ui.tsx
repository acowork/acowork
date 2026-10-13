/**
 * Layout primitives. Deliberately tiny: these are the mobile analogues of
 * Desktop's shadcn/ui set, reduced to the components the IM shell needs.
 * A component earns its place only when at least two screens use it —
 * no speculative variants (ADR-086 §被否决方案 C).
 */

import type { CSSProperties, ReactNode } from 'react'

/** The grouped-list container — the single most-used iOS surface.
 *  `title` is a ReactNode so a section header can carry markup (the inbox's
 *  group titles render a leading glyph beside the name, plus a status dot
 *  for a node). */
export function ListSection({ title, children }: { title?: ReactNode; children: ReactNode }) {
  return (
    <>
      {title ? <div className="list-section-title">{title}</div> : null}
      <div className="list-section">{children}</div>
    </>
  )
}

/** One row. `value` renders right-aligned secondary text; `arrow` the chevron.
 *  `avatar` shows a 48px left image/initial block (WeChat chat-list style).
 *  `icon` shows a 24px leading icon (WeChat settings style). */
export function ListRow({
  label,
  value,
  arrow,
  onClick,
  disabled,
  destructive,
  hint,
  badge,
  avatar,
  avatarClass,
  avatarUnread,
  icon,
  iconColor,
}: {
  label: ReactNode
  value?: ReactNode
  arrow?: boolean
  onClick?: () => void
  disabled?: boolean
  destructive?: boolean
  hint?: string
  /** Count bubble. Only ever a NUMBER the server actually reported. */
  badge?: number
  /** Avatar element (image or initials block) rendered 48px left.
   *  Unread always lands on this avatar's top-right corner — the position
   *  every IM trains people to look at: a server `badge` renders as a red
   *  number bubble, the client-inferred "there is something new" for an
   *  agent session as a plain red dot (`avatarUnread`, §8.5 — never a number
   *  there, because the count is inferred). A row WITHOUT an avatar has no
   *  corner to sit on and keeps the bubble in the trail. */
  avatar?: ReactNode
  avatarClass?: string
  avatarUnread?: boolean
  /** Leading icon element (24px) for settings-style rows. */
  icon?: ReactNode
  /** Optional glyph color (a `--color-icon-*` token) — WeChat Discover-style
   *  multicolor. Falls back to the dim gray when omitted (e.g. disabled rows). */
  iconColor?: string
}) {
  const Tag = onClick ? 'button' : 'div'
  return (
    <Tag
      className={`list-row${disabled ? ' is-disabled' : ''}${destructive ? ' is-destructive' : ''}`}
      onClick={disabled ? undefined : onClick}
      type={onClick ? 'button' : undefined}
      role={onClick ? 'button' : undefined}
    >
      {avatar ? (
        <div className="list-row-avatar-wrap">
          <div className={`list-row-avatar${avatarClass ? ' ' + avatarClass : ''}`}>{avatar}</div>
          {/* The count outranks the dot: a row that knows how many is not
              served a vaguer signal. They are mutually exclusive in practice
              (agent rows dot, contact rows count) — the order is the fallback,
              not a stack. */}
          {badge && badge > 0 ? (
            <span className="avatar-unread-count">{badge > 99 ? '99+' : badge}</span>
          ) : avatarUnread ? (
            <span className="avatar-unread" aria-label="有新消息" />
          ) : null}
        </div>
      ) : null}
      {icon && !avatar ? (
        <div
          className="list-row-icon"
          style={iconColor ? ({ '--row-icon-color': iconColor } as CSSProperties) : undefined}
        >
          {icon}
        </div>
      ) : null}
      <div className="list-row-main">
        <div className="list-row-label">{label}</div>
        {hint ? <div className="list-row-hint">{hint}</div> : null}
      </div>
      <div className="list-row-trail">
        {badge && badge > 0 && !avatar ? (
          <span className="badge-count">{badge > 99 ? '99+' : badge}</span>
        ) : null}
        {value !== undefined ? <div className="list-row-value">{value}</div> : null}
        {arrow ? <span className="chevron" aria-hidden /> : null}
      </div>
    </Tag>
  )
}

/** iOS Switch. */
export function Switch({
  checked,
  onChange,
  disabled,
  label,
}: {
  checked: boolean
  onChange: (v: boolean) => void
  disabled?: boolean
  label: string
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      disabled={disabled}
      className={`switch${checked ? ' is-on' : ''}${disabled ? ' is-disabled' : ''}`}
      onClick={() => !disabled && onChange(!checked)}
    >
      <span className="switch-knob" />
    </button>
  )
}

/** iOS Segmented control. */
export function Segmented<T extends string>({
  options,
  value,
  onChange,
}: {
  options: { value: T; label: string }[]
  value: T
  onChange: (v: T) => void
}) {
  return (
    <div className="segmented" role="tablist">
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          role="tab"
          aria-selected={o.value === value}
          className={`segmented-item${o.value === value ? ' is-active' : ''}`}
          onClick={() => onChange(o.value)}
        >
          {o.label}
        </button>
      ))}
    </div>
  )
}

/** iOS Stepper. */
export function Stepper({
  value,
  onChange,
  min = 0,
  max = 100,
  label,
}: {
  value: number
  onChange: (v: number) => void
  min?: number
  max?: number
  label: string
}) {
  return (
    <div className="stepper" role="group" aria-label={label}>
      <button
        type="button"
        aria-label="减少"
        disabled={value <= min}
        onClick={() => onChange(Math.max(min, value - 1))}
      >
        −
      </button>
      <span className="stepper-value">{value}</span>
      <button
        type="button"
        aria-label="增加"
        disabled={value >= max}
        onClick={() => onChange(Math.min(max, value + 1))}
      >
        +
      </button>
    </div>
  )
}

/** Full-screen modal layer (session switcher, pickers). */
export function Sheet({ open, onClose, children, title }: { open: boolean; onClose: () => void; children: ReactNode; title?: string }) {
  if (!open) return null
  return (
    <div className="sheet-backdrop" onClick={onClose} role="presentation">
      <div className="sheet" onClick={(e) => e.stopPropagation()} role="dialog" aria-modal="true" aria-label={title}>
        <div className="sheet-grabber" />
        {title ? <div className="sheet-title">{title}</div> : null}
        <div className="sheet-body">{children}</div>
      </div>
    </div>
  )
}

/** Inline banner for read-only sessions and connection problems. */
export function Banner({ tone, children }: { tone: 'info' | 'warning' | 'error'; children: ReactNode }) {
  return <div className={`banner banner-${tone}`} role="status">{children}</div>
}
