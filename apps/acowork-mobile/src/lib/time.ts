/**
 * Timestamp formatting for the inbox and message rows (design §2.3).
 *
 * iOS Messages' convention, which is what a Chinese-market IM user expects:
 * today shows a clock, yesterday says 昨天, anything inside a week says the
 * weekday, and older shows the date. Everything is in the device's own zone
 * — there is no server timezone in this app's contract, and inventing one
 * would put every timestamp an hour off for half the users.
 */

const DAY = 86_400_000

function startOfDay(ms: number): number {
  const d = new Date(ms)
  d.setHours(0, 0, 0, 0)
  return d.getTime()
}

function clock(ms: number): string {
  const d = new Date(ms)
  const hh = d.getHours().toString().padStart(2, '0')
  const mm = d.getMinutes().toString().padStart(2, '0')
  return `${hh}:${mm}`
}

/** Compact label for a list row's trailing slot. */
export function formatListTime(ms: number | undefined | null): string {
  if (!ms || !Number.isFinite(ms)) return ''
  const days = Math.round((startOfDay(Date.now()) - startOfDay(ms)) / DAY)
  const d = new Date(ms)
  if (days <= 0) return clock(ms)
  if (days === 1) return '昨天'
  if (days < 7) return ['周日', '周一', '周二', '周三', '周四', '周五', '周六'][d.getDay()]!
  const y = d.getFullYear()
  const md = `${d.getMonth() + 1}/${d.getDate()}`
  return y === new Date().getFullYear() ? md : `${y}/${md}`
}

/** Full label for a message row: date plus clock, always unambiguous. */
export function formatMessageTime(ms: number | undefined | null): string {
  if (!ms || !Number.isFinite(ms)) return ''
  const d = new Date(ms)
  const days = Math.round((startOfDay(Date.now()) - startOfDay(d.getTime())) / DAY)
  if (days <= 0) return clock(d.getTime())
  if (days === 1) return `昨天 ${clock(d.getTime())}`
  return `${d.getMonth() + 1}月${d.getDate()}日 ${clock(d.getTime())}`
}

/**
 * A scheduled moment — a task's start / due time.
 *
 * Unlike a message row, a schedule is never relative: "昨天" for a deadline
 * reads as a past moment, and the clock alone would hide which day it is.
 * The day word plus the clock is what the user set.
 */
export function formatScheduleTime(ms: number | undefined | null): string {
  if (!ms || !Number.isFinite(ms)) return ''
  const d = new Date(ms)
  const days = Math.round((startOfDay(Date.now()) - startOfDay(d.getTime())) / DAY)
  const c = clock(d.getTime())
  if (days <= 0) return `今天 ${c}`
  if (days === 1) return `昨天 ${c}`
  return `${d.getMonth() + 1}月${d.getDate()}日 ${c}`
}

/**
 * A day separator between messages, the IM convention that keeps a long
 * thread scannable. Empty string when the previous message is the same day.
 */
export function daySeparator(prevMs: number | null, ms: number): string | null {
  const cur = new Date(ms)
  if (prevMs !== null && startOfDay(prevMs) === startOfDay(ms)) return null
  const days = Math.round((startOfDay(Date.now()) - startOfDay(ms)) / DAY)
  const time = formatClock(cur)
  if (days <= 0) return `今天 ${time}`
  if (days === 1) return `昨天 ${time}`
  return `${cur.getMonth() + 1}\u6708${cur.getDate()}\u65e5 ${time}`
}

/** 5-minute gap time separator (WeChat style). */
const GAP_MS = 5 * 60_000

export function timeGapSeparator(prevMs: number | null, ms: number): string | null {
  if (prevMs === null) return null
  // Only show if gap > 5 min AND not already showing a day separator
  if (startOfDay(prevMs) !== startOfDay(ms)) return null // day sep handles it
  if (ms - prevMs < GAP_MS) return null
  return formatClock(new Date(ms))
}

function formatClock(d: Date): string {
  const h = d.getHours()
  const m = d.getMinutes().toString().padStart(2, '0')
  if (h < 6) return `凌晨 ${h}:${m}`
  if (h < 12) return `上午 ${h}:${m}`
  if (h < 14) return `中午 ${h === 12 ? 12 : h - 12}:${m}`
  if (h < 18) return `下午 ${h - 12}:${m}`
  return `晚上 ${h - 12}:${m}`
}
