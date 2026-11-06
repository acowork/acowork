//! DatePicker — the app's date field, replacing `<input type="date">`.
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
//! `value` is a *local calendar day* written `YYYY-MM-DD` (`""` = no date) —
//! the exact shape `TaskFormValues.startAt` / `dueAt` already hold, so the
//! picker needs no knowledge of the ISO instants the API stores. All arithmetic
//! is in `calendarGrid.ts`; this file renders it.

import { useCallback, useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { CalendarDays, ChevronLeft, ChevronRight } from "lucide-react";
import { cn } from "../../lib/utils";
import { useTranslation } from "../../i18n/useTranslation";
import { FIELD_SURFACE_CLASS } from "./StyledInput";
import {
  addDays,
  addMonths,
  buildMonthGrid,
  daysInMonth,
  formatMonthTitle,
  parseDateField,
  toDateField,
  todayDateField,
  weekdayLabels,
} from "./calendarGrid";

/** Popup width: 7 × 32px cells + 2 × 8px padding. Also the width used while
 *  the popup has not been measured yet, so the anchor maths never divides by
 *  zero in a jsdom render (where every layout metric is 0). */
const POPUP_WIDTH = 240;

/** Space between the trigger and the popup. */
const POPUP_GAP = 4;

export interface DatePickerProps {
  /** Local calendar day `YYYY-MM-DD`, or `""` for "no date". */
  value: string;
  /** Receives the picked day (`YYYY-MM-DD`) or `""` after 清除. */
  onChange: (value: string) => void;
  disabled?: boolean;
  /** Extra classes for the trigger, merged after the field surface. */
  className?: string;
  /** Accessible name for the trigger when it is not wrapped in a `<label>`. */
  ariaLabel?: string;
}

/** The month a value belongs to, falling back to today for an empty field. */
function monthOf(value: string): { year: number; month: number } {
  const parts = parseDateField(value);
  if (parts) return { year: parts.year, month: parts.month };
  const today = parseDateField(todayDateField());
  return today ? { year: today.year, month: today.month } : { year: 1970, month: 1 };
}

export function DatePicker({ value, onChange, disabled, className, ariaLabel }: DatePickerProps) {
  const { t, i18n } = useTranslation();
  const [open, setOpen] = useState(false);
  const [pos, setPos] = useState<{ top: number; left: number } | null>(null);
  /**
   * The day that owns the keyboard. It is also what decides the visible month,
   * so browsing months and moving focus can never disagree about where the
   * grid is — one state, one grid.
   */
  const [focusedDay, setFocusedDay] = useState<string>(() => value || todayDateField());

  const triggerRef = useRef<HTMLButtonElement>(null);
  const popupRef = useRef<HTMLDivElement>(null);
  const cellRefs = useRef(new Map<string, HTMLButtonElement>());
  const popupId = useId();

  const today = todayDateField();
  const view = monthOf(focusedDay);
  const cells = useMemo(() => buildMonthGrid(view.year, view.month), [view.year, view.month]);
  /**
   * Weekday initials are the only localized text here, and they are decoration:
   * the app's standard test seam mocks this hook as `{ t }` only, so a missing
   * language must degrade to English rather than take the field down with it.
   */
  const locale = i18n?.language ?? "en";
  const weekdays = useMemo(() => weekdayLabels(locale), [locale]);
  const selected = parseDateField(value);

  /** Open on the value's month (or today's), with the keyboard on that day. */
  const show = useCallback(() => {
    setFocusedDay(value || todayDateField());
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
          commit(focusedDay);
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
  }, [commit, focusedDay, hide, open]);

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
          {value || t("datePicker.placeholder")}
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
                const isSelected = !!selected && cell.value === value;
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
                    onClick={() => commit(cell.value)}
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

            <div className="mt-1 flex items-center justify-between gap-2 border-t border-border-divider px-1 pt-2">
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
                onClick={() => commit(today)}
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
