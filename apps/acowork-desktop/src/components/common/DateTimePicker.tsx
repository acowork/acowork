//! DateTimePicker — the app's date-time field, replacing `<input type="date">`.
//!
//! ## Why not the native control
//!
//! `<input type="date">` hands the calendar to WebView2/Chromium, which paints
//! its own popup (month arrows, weekday row, `清除 / 今天`). That popup is not
//! part of the page DOM, so no rule in `globals.css` can reach it — its arrow
//! buttons rendered oversized and its `今天` label sat in a misaligned box next
//! to a dialog styled entirely from our tokens. The fix is not a style but a
//! control: the popup has to be ours to be the design system's.
//!
//! ## What it reuses (and must keep reusing)
//!
//! - the field surface — `FIELD_SURFACE_CLASS`, the same constant `StyledInput`
//!   composes, so the trigger lines up with the text fields in the same row;
//! - the button vocabulary — `btn-solid` (清除) / `btn-accent` (今天) and
//!   `--ui-btn-py`. `styles/buttonVariants.test.ts` keeps that vocabulary
//!   closed, and a picker inventing its own tones is exactly the drift it
//!   exists to refuse;
//! - the popup material — `bg-modal-surface` / `border-border-outer` /
//!   `shadow-xl`, i.e. the same surface as every dialog.
//!
//! ## Layering
//!
//! The popup renders through a portal with `position: fixed`, anchored to the
//! trigger's rect. It has to: the task form's dialog panel is `overflow-hidden`
//! and its body scrolls inside `OverlayScrollbar`, so an absolutely positioned
//! popup would be clipped by whichever ancestor ran out of room first.
//!
//! ## The value contract
//!
//! `value` is a *local wall clock* written `YYYY-MM-DDTHH:mm` (`""` = no
//! value) — the exact shape `TaskFormValues.startAt` / `dueAt` already hold,
//! so the picker needs no knowledge of the ISO instants the API stores. All
//! arithmetic is in `calendarGrid.ts`; this file renders it.
//!
//! The clock is local on purpose: the form edits the wall clock the user
//! reads ("明早 8 点"), and the conversion to the stored UTC instant happens
//! once, in `useTaskForm`.
//!
//! ## Why the time is picked *before* the day
//!
//! The clock row (时 / 分) sits under the grid and above the footer buttons.
//! Setting the clock first and the day second is the order that always works:
//! a clock edit on an empty field has no day to attach to yet, so it waits in
//! `pendingTime` and lands on the day the user picks next.

import { useCallback, useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { CalendarDays, ChevronLeft, ChevronRight } from "lucide-react";
import { cn } from "../../lib/utils";
import { useTranslation } from "../../i18n/useTranslation";
import { FIELD_SURFACE_CLASS } from "./StyledInput";
import { Dropdown } from "./Dropdown";
import {
  addDays,
  addMonths,
  buildMonthGrid,
  daysInMonth,
  formatMonthTitle,
  parseDateField,
  toDateField,
  toDateTimeField,
  todayDateField,
  weekdayLabels,
} from "./calendarGrid";

/** Popup width: 7 × 32px cells + 2 × 8px padding. Also the width used while
 *  the popup has not been measured yet, so the anchor maths never divides by
 *  zero in a jsdom render (where every layout metric is 0). */
const POPUP_WIDTH = 240;

/** Space between the trigger and the popup. */
const POPUP_GAP = 4;

export interface DateTimePickerProps {
  /** Local wall clock `YYYY-MM-DDTHH:mm`, or `""` for "no value". */
  value: string;
  /** Receives the picked wall clock (`YYYY-MM-DDTHH:mm`) or `""` after 清除. */
  onChange: (value: string) => void;
  disabled?: boolean;
  /** Extra classes for the trigger, merged after the field surface. */
  className?: string;
  /** Accessible name for the trigger when it is not wrapped in a `<label>`. */
  ariaLabel?: string;
}

/** The `YYYY-MM-DD` day of a wall-clock value ("" when there is none). */
function dayOf(value: string): string {
  const day = value.slice(0, 10);
  return parseDateField(day) ? day : "";
}

/** The `HH:mm` clock of a wall-clock value (`""` when it carries no time). */
function clockOf(value: string): string {
  if (value.length < 16) return "";
  return `${value.slice(11, 13)}:${value.slice(14, 16)}`;
}

/** The current wall clock, to the minute — the default clock of a fresh field. */
function nowClock(now: Date = new Date()): string {
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(now.getHours())}:${pad(now.getMinutes())}`;
}

/** Hours on offer: every one of them, so any scheduled hour stays editable. */
const HOUR_OPTIONS = Array.from({ length: 24 }, (_, h) => {
  const hh = String(h).padStart(2, "0");
  return { value: hh, label: hh };
});

/** Minutes on offer: every one of them — a scheduled 09:37 must survive an edit. */
const MINUTE_OPTIONS = Array.from({ length: 60 }, (_, m) => {
  const mm = String(m).padStart(2, "0");
  return { value: mm, label: mm };
});

/** The month a value belongs to, falling back to today for an empty field. */
function monthOf(value: string): { year: number; month: number } {
  const parts = parseDateField(value);
  if (parts) return { year: parts.year, month: parts.month };
  const today = parseDateField(todayDateField());
  return today ? { year: today.year, month: today.month } : { year: 1970, month: 1 };
}

export function DateTimePicker({
  value,
  onChange,
  disabled,
  className,
  ariaLabel,
}: DateTimePickerProps) {
  const { t, i18n } = useTranslation();
  const [open, setOpen] = useState(false);
  const [pos, setPos] = useState<{ top: number; left: number } | null>(null);
  /**
   * The day that owns the keyboard. It is also what decides the visible month,
   * so browsing months and moving focus can never disagree about where the
   * grid is — one state, one grid.
   */
  const [focusedDay, setFocusedDay] = useState<string>(() => dayOf(value) || todayDateField());
  /**
   * The clock the user has dialled in. It is separate from `value` because a
   * clock edit on an empty field has no day to attach to yet — it waits here
   * and lands on the day picked next.
   */
  const [pendingTime, setPendingTime] = useState<string>(() => clockOf(value) || nowClock());

  const triggerRef = useRef<HTMLButtonElement>(null);
  const popupRef = useRef<HTMLDivElement>(null);
  const cellRefs = useRef(new Map<string, HTMLButtonElement>());
  const popupId = useId();

  const today = todayDateField();
  const day = dayOf(value);
  const view = monthOf(focusedDay);
  const cells = useMemo(() => buildMonthGrid(view.year, view.month), [view.year, view.month]);
  /**
   * Weekday initials are the only localized text here, and they are decoration:
   * the app's standard test seam mocks this hook as `{ t }` only, so a missing
   * language must degrade to English rather than take the field down with it.
   */
  const locale = i18n?.language ?? "en";
  const weekdays = useMemo(() => weekdayLabels(locale), [locale]);
  const selected = parseDateField(day);

  /**
   * The clock follows the stored value — a form reset or an edit from
   * elsewhere must not leave a stale clock in the popup.
   */
  useEffect(() => {
    setPendingTime(clockOf(value) || nowClock());
  }, [value]);

  /** Open on the value's month (or today's), with the keyboard on that day. */
  const show = useCallback(() => {
    setFocusedDay(dayOf(value) || todayDateField());
    setPos(null);
    setOpen(true);
  }, [value]);

  const hide = useCallback((refocus: boolean) => {
    setOpen(false);
    setPos(null);
    if (refocus) triggerRef.current?.focus();
  }, []);

  const commit = useCallback(
    (next: string) => {
      onChange(next);
      hide(true);
    },
    [hide, onChange],
  );

  /**
   * Pick a day. The clock comes along unchanged — the user dialled it in
   * above, and the day is what was missing.
   */
  const pickDay = useCallback(
    (next: string) => {
      commit(toDateTimeField(next, Number(pendingTime.slice(0, 2)), Number(pendingTime.slice(3, 5))));
    },
    [commit, pendingTime],
  );

  /**
   * Dial the clock. With a day in hand the new clock lands immediately; on an
   * empty field it waits in `pendingTime` for the day the user picks next.
   */
  const setClock = useCallback(
    (next: string) => {
      setPendingTime(next);
      if (day) onChange(`${day}T${next}`);
    },
    [day, onChange],
  );

  /**
   * Place the popup: below the trigger, flipped above when there is no room
   * and the trigger is not pinned to the top of the viewport. Measured on
   * mount and re-measured on resize / any scroll (capture, so the form body's
   * own scroller is included).
   */
  useLayoutEffect(() => {
    if (!open) return;
    const place = () => {
      const trigger = triggerRef.current;
      if (!trigger) return;
      const rect = trigger.getBoundingClientRect();
      const height = popupRef.current?.offsetHeight ?? 0;
      const width = popupRef.current?.offsetWidth || POPUP_WIDTH;
      const flipUp = rect.bottom + POPUP_GAP + height > window.innerHeight && rect.top > height;
      setPos({
        top: flipUp ? rect.top - POPUP_GAP - height : rect.bottom + POPUP_GAP,
        left: Math.max(POPUP_GAP, Math.min(rect.left, window.innerWidth - width - POPUP_GAP)),
      });
    };
    place();
    window.addEventListener("resize", place);
    window.addEventListener("scroll", place, true);
    return () => {
      window.removeEventListener("resize", place);
      window.removeEventListener("scroll", place, true);
    };
  }, [open]);

  /** A click anywhere outside the trigger or the popup dismisses it. */
  useEffect(() => {
    if (!open) return;
    const onPointerDown = (e: MouseEvent) => {
      const target = e.target as Node | null;
      if (!target) return;
      if (popupRef.current?.contains(target)) return;
      if (triggerRef.current?.contains(target)) return;
      // The clock dropdowns open their own portal, so their option list sits
      // *outside* this popup. A click on it must not dismiss the calendar
      // under it — the click would never reach the option.
      if (target instanceof Element && target.closest('[role="listbox"]')) return;
      hide(false);
    };
    document.addEventListener("mousedown", onPointerDown, true);
    return () => document.removeEventListener("mousedown", onPointerDown, true);
  }, [hide, open]);

  /**
   * Keyboard: arrows walk the grid, PageUp/PageDown walk months, Enter/Space
   * pick, Escape dismisses.
   *
   * Escape is stopped here on purpose. The task dialog closes itself on an
   * Escape that reaches `window`; letting the key through would dismiss the
   * whole form — and discard-guard it — because the user closed a calendar.
   */
  useEffect(() => {
    const popup = popupRef.current;
    if (!open || !popup) return;
    // Arrow / Enter belong to the day grid. The month arrows are ordinary
    // buttons: pressing Enter on 上个月 must step the month, not pick a day.
    const onDayCell = (target: EventTarget | null) =>
      target instanceof HTMLElement && target.dataset.day !== undefined;

    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        hide(true);
        return;
      }
      if (!onDayCell(e.target)) return;
      switch (e.key) {
        case "Enter":
        case " ":
          e.preventDefault();
          pickDay(focusedDay);
          break;
        case "ArrowLeft":
          e.preventDefault();
          setFocusedDay((current) => addDays(current, -1));
          break;
        case "ArrowRight":
          e.preventDefault();
          setFocusedDay((current) => addDays(current, 1));
          break;
        case "ArrowUp":
          e.preventDefault();
          setFocusedDay((current) => addDays(current, -7));
          break;
        case "ArrowDown":
          e.preventDefault();
          setFocusedDay((current) => addDays(current, 7));
          break;
        case "PageUp":
          e.preventDefault();
          setFocusedDay((current) => stepMonth(current, -1));
          break;
        case "PageDown":
          e.preventDefault();
          setFocusedDay((current) => stepMonth(current, 1));
          break;
        default:
          break;
      }
    };
    popup.addEventListener("keydown", onKeyDown);
    return () => popup.removeEventListener("keydown", onKeyDown);
  }, [focusedDay, hide, open, pickDay]);

  /** Follow the keyboard: the focused day is always visible, hence focused. */
  useEffect(() => {
    if (!open) return;
    cellRefs.current.get(focusedDay)?.focus();
  }, [focusedDay, open]);

  const isOpen = open && !!pos;

  return (
    <>
      <button
        ref={triggerRef}
        type="button"
        disabled={disabled}
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-controls={open ? popupId : undefined}
        aria-label={ariaLabel}
        data-testid="date-picker-trigger"
        onClick={() => (open ? hide(false) : show())}
        className={cn(
          FIELD_SURFACE_CLASS,
          // `max-w-full` keeps a caller's fixed width (e.g. `w-40`) from
          // overflowing a narrow grid cell.
          "flex max-w-full items-center gap-1.5 text-left disabled:cursor-not-allowed disabled:opacity-50",
          className,
        )}
      >
        <CalendarDays className="h-3.5 w-3.5 shrink-0 text-text-tertiary" aria-hidden />
        <span className={cn("truncate", !value && "text-text-tertiary")}>
          {value ? value.replace("T", " ") : t("datePicker.placeholder")}
        </span>
      </button>

      {open &&
        createPortal(
          <div
            ref={popupRef}
            id={popupId}
            role="dialog"
            aria-label={t("datePicker.placeholder")}
            data-testid="date-picker-popup"
            style={{
              position: "fixed",
              top: pos?.top ?? 0,
              left: pos?.left ?? 0,
              width: POPUP_WIDTH,
              visibility: isOpen ? "visible" : "hidden",
            }}
            className="z-[60] rounded-md border border-border-outer bg-modal-surface p-2 shadow-xl"
          >
            <div className="mb-1 flex items-center justify-between px-1">
              <span className="text-xs font-medium tabular-nums text-text">
                {formatMonthTitle(view.year, view.month)}
              </span>
              <div className="flex items-center gap-0.5">
                <button
                  type="button"
                  aria-label={t("datePicker.prevMonth")}
                  onClick={() => setFocusedDay((current) => stepMonth(current, -1))}
                  className="flex h-7 w-7 items-center justify-center rounded-md text-text-secondary transition-colors hover:bg-zinc-100 hover:text-text dark:hover:bg-zinc-700"
                >
                  <ChevronLeft className="h-3.5 w-3.5" aria-hidden />
                </button>
                <button
                  type="button"
                  aria-label={t("datePicker.nextMonth")}
                  onClick={() => setFocusedDay((current) => stepMonth(current, 1))}
                  className="flex h-7 w-7 items-center justify-center rounded-md text-text-secondary transition-colors hover:bg-zinc-100 hover:text-text dark:hover:bg-zinc-700"
                >
                  <ChevronRight className="h-3.5 w-3.5" aria-hidden />
                </button>
              </div>
            </div>

            <div className="grid grid-cols-7">
              {weekdays.map((label, i) => (
                <span
                  key={i}
                  aria-hidden
                  className="flex h-7 w-8 items-center justify-center text-10 text-text-tertiary"
                >
                  {label}
                </span>
              ))}
            </div>

            <div className="grid grid-cols-7 gap-y-0.5">
              {cells.map((cell) => {
                const isSelected = !!selected && cell.value === day;
                const isToday = cell.value === today;
                return (
                  <button
                    key={cell.value}
                    ref={(el) => {
                      if (el) cellRefs.current.set(cell.value, el);
                      else cellRefs.current.delete(cell.value);
                    }}
                    type="button"
                    aria-label={cell.value}
                    aria-pressed={isSelected}
                    aria-current={isToday ? "date" : undefined}
                    data-day={cell.value}
                    tabIndex={cell.value === focusedDay ? 0 : -1}
                    onClick={() => pickDay(cell.value)}
                    className={cn(
                      "flex h-8 w-8 items-center justify-center rounded-md text-xs tabular-nums transition-colors focus:outline-none focus-visible:ring-2 focus-visible:ring-[var(--color-accent)]",
                      isSelected
                        ? "btn-accent font-medium"
                        : cn(
                            "hover:bg-zinc-100 dark:hover:bg-zinc-700",
                            cell.inMonth ? "text-text" : "text-text-disabled",
                          ),
                      // 今天 stays readable while 选中, so it is marked with a
                      // ring rather than a second fill.
                      isToday && !isSelected && "ring-1 ring-inset ring-[var(--color-accent)]",
                    )}
                  >
                    {cell.day}
                  </button>
                );
              })}
            </div>

            {/* The clock. It sits above the buttons because it is edited
                *before* the day is settled: a clock with no day has nowhere to
                go yet, and the day picked next carries it. */}
            <div className="mt-1 flex items-center gap-1.5 border-t border-border-divider px-1 pt-2">
              <span className="text-10 text-text-tertiary">{t("datePicker.time")}</span>
              <Dropdown
                size="small"
                value={pendingTime.slice(0, 2)}
                onChange={(h) => setClock(`${h}:${pendingTime.slice(3, 5)}`)}
                options={HOUR_OPTIONS}
                aria-label={t("datePicker.hour")}
                className="w-14"
              />
              <span aria-hidden className="text-10 text-text-tertiary">
                :
              </span>
              <Dropdown
                size="small"
                value={pendingTime.slice(3, 5)}
                onChange={(m) => setClock(`${pendingTime.slice(0, 2)}:${m}`)}
                options={MINUTE_OPTIONS}
                aria-label={t("datePicker.minute")}
                className="w-14"
              />
            </div>

            <div className="mt-1 flex items-center justify-between gap-2 px-1">
              <button
                type="button"
                disabled={!value}
                onClick={() => commit("")}
                className="btn-solid inline-flex items-center justify-center rounded-md px-3 py-[var(--ui-btn-py)] text-xs font-medium disabled:cursor-not-allowed disabled:opacity-50"
              >
                {t("datePicker.clear")}
              </button>
              <button
                type="button"
                onClick={() => pickDay(today)}
                className="btn-accent inline-flex items-center justify-center rounded-md px-3 py-[var(--ui-btn-py)] text-xs font-medium"
              >
                {t("datePicker.today")}
              </button>
            </div>
          </div>,
          document.body,
        )}
    </>
  );
}

/** Step a day by whole months, clamping the day-of-month to the target month. */
function stepMonth(value: string, delta: number): string {
  const parts = parseDateField(value) ?? parseDateField(todayDateField());
  if (!parts) return value;
  const next = addMonths(parts.year, parts.month, delta);
  return toDateField(next.year, next.month, Math.min(parts.day, daysInMonth(next.year, next.month)));
}
