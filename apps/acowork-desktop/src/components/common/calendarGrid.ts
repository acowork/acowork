//! Calendar arithmetic for `DateTimePicker` — the data layer, no DOM.
//!
//! ## Why this is its own module
//!
//! The picker's value contract is *the same* contract the task form already
//! stores: a **local calendar day** written `YYYY-MM-DD` (`TaskFormValues.startAt`
//! / `dueAt`, `useTaskForm`). Rendering that grid, stepping a month, and
//! knowing which day is "today" are pure functions of `(year, month, clock)`,
//! so they live here — testable without a renderer, and impossible to get
//! subtly different in the second surface that will need a date field.
//!
//! ## The two rules this file pins
//!
//! 1. **No UTC anywhere.** `new Date("2026-11-06")` parses as UTC midnight and
//!    renders as the previous day west of UTC; the same trap `useTaskForm`
//!    documents for its save path. Every date here is built from local
//!    components (`new Date(y, m-1, d)`).
//! 2. **Weeks start on Monday.** The whole app already formats dates with one
//!    fixed, locale-independent grammar (`formatDateTime` → `YYYY-MM-DD HH:mm`),
//!    so the grid uses one fixed week order instead of guessing a locale's
//!    first weekday — which browsers disagree about (`Intl.Locale.weekInfo` is
//!    not available in every WebView2 build) and which would make the grid's
//!    layout untestable. Only the *weekday initials* are localized.

/** Rows in the month grid: 6 always, so the popup never changes height. */
export const GRID_ROWS = 6;

/** Columns: one per weekday. */
export const WEEK_LENGTH = 7;

/** Cells rendered per month: a fixed 6×7 box, padded by neighbouring days. */
export const GRID_SIZE = GRID_ROWS * WEEK_LENGTH;

/** A `YYYY-MM-DD` local calendar day, split into its parts. */
export interface CalendarDate {
  year: number;
  /** 1-12, not the `0-11` of `Date#getMonth`. */
  month: number;
  /** 1-31. */
  day: number;
}

/** One rendered cell. */
export interface CalendarCell extends CalendarDate {
  /** The cell's value: `YYYY-MM-DD`, i.e. exactly what `DateTimePicker` reports. */
  value: string;
  /** False for the leading/trailing days borrowed from the adjacent months. */
  inMonth: boolean;
}

const pad2 = (n: number) => String(n).padStart(2, "0");

/**
 * `YYYY-MM-DD` → parts, or `null` when the string is not a real calendar day.
 *
 * Rejects both shapes the form can hold outside a real date (`""`) and
 * impossible days (`2026-02-30`), because a picker that silently "corrects"
 * the value it was handed hides the bug that produced it.
 */
export function parseDateField(value: string): CalendarDate | null {
  const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(value);
  if (!m) return null;
  const year = Number(m[1]);
  const month = Number(m[2]);
  const day = Number(m[3]);
  return isValidDate(year, month, day) ? { year, month, day } : null;
}

/** Parts → `YYYY-MM-DD`. Callers pass already-valid parts (grid arithmetic). */
export function toDateField(year: number, month: number, day: number): string {
  return `${year}-${pad2(month)}-${pad2(day)}`;
}

function isValidDate(year: number, month: number, day: number): boolean {
  if (month < 1 || month > 12 || day < 1 || day > 31) return false;
  // Round-trip through a local Date: an overflowed day (`2026-02-30`) lands on
  // a different date, which is how it gets detected without a day-count table.
  const d = new Date(year, month - 1, day);
  return d.getFullYear() === year && d.getMonth() === month - 1 && d.getDate() === day;
}

/** Days in `month` (1-12) of `year` — `new Date(y, m, 0)` is the last day. */
export function daysInMonth(year: number, month: number): number {
  return new Date(year, month, 0).getDate();
}

/** Today as a local `YYYY-MM-DD` (never UTC — see the module note). */
export function todayDateField(now: Date = new Date()): string {
  return toDateField(now.getFullYear(), now.getMonth() + 1, now.getDate());
}

/** Local day of a `Date` — the projection `todayDateField` and stepping share. */
export function dateToField(d: Date): string {
  return toDateField(d.getFullYear(), d.getMonth() + 1, d.getDate());
}

/**
 * A local wall-clock timestamp: `YYYY-MM-DDTHH:mm`.
 *
 * No timezone suffix on purpose — the form edits *local* time, and the one
 * conversion to the stored UTC instant happens in `useTaskForm`. A `Z` here
 * would mean UTC and would shift the day for half the world.
 */
export function toDateTimeField(day: string, hour: number, minute: number): string {
  return `${day}T${pad2(hour)}:${pad2(minute)}`;
}

/** The current local minute — what a fresh task starts at. */
export function nowDateTimeField(now: Date = new Date()): string {
  return toDateTimeField(todayDateField(now), now.getHours(), now.getMinutes());
}

/** Step a `(year, month)` pair by `delta` months, rolling the year over. */
export function addMonths(year: number, month: number, delta: number): { year: number; month: number } {
  const zeroBased = year * 12 + (month - 1) + delta;
  return { year: Math.floor(zeroBased / 12), month: (zeroBased % 12 + 12) % 12 + 1 };
}

/** Step a `YYYY-MM-DD` value by `delta` days (arrow-key navigation). */
export function addDays(value: string, delta: number): string {
  const parts = parseDateField(value);
  if (!parts) return value;
  return dateToField(new Date(parts.year, parts.month - 1, parts.day + delta));
}

/**
 * Monday-first weekday index of a date: Monday `0` … Sunday `6`.
 *
 * `Date#getDay` is Sunday-based; the grid is Monday-based, hence the shift.
 */
export function mondayFirstWeekday(d: Date): number {
  return (d.getDay() + 6) % 7;
}

/**
 * The 42 cells of the month view for `(year, month)`: leading and trailing days
 * borrowed from the neighbouring months so the box is always 6×7.
 */
export function buildMonthGrid(year: number, month: number): CalendarCell[] {
  const firstWeekday = mondayFirstWeekday(new Date(year, month - 1, 1));
  const cells: CalendarCell[] = [];
  for (let i = 0; i < GRID_SIZE; i += 1) {
    // `new Date(y, m-1, 1 - offset + i)` normalizes negative and overflowed
    // day numbers into the previous / next month on its own.
    const d = new Date(year, month - 1, 1 - firstWeekday + i);
    cells.push({
      year: d.getFullYear(),
      month: d.getMonth() + 1,
      day: d.getDate(),
      value: dateToField(d),
      inMonth: d.getMonth() === month - 1 && d.getFullYear() === year,
    });
  }
  return cells;
}

/** `2026-11` — the popup's month title, in the same grammar as `formatDateTime`. */
export function formatMonthTitle(year: number, month: number): string {
  return `${year}-${pad2(month)}`;
}

/** One-letter weekday initials, Monday first, localized by `Intl` only. */
export function weekdayLabels(locale: string): string[] {
  try {
    const fmt = new Intl.DateTimeFormat(locale, { weekday: "narrow" });
    // 2024-01-01 was a Monday; stepping 0..6 days covers the whole week.
    return Array.from({ length: WEEK_LENGTH }, (_, i) =>
      fmt.format(new Date(2024, 0, 1 + i)),
    );
  } catch {
    return ["M", "T", "W", "T", "F", "S", "S"];
  }
}
