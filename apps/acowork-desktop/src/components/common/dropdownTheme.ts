/**
 * Dropdown theme — the single source of truth for every select-like surface
 * in the desktop app.
 *
 * Why this file exists:
 *   The old `Dropdown` was a native `<select>`. Its focus ring came from
 *   `zinc-400` (and only in the `standard` variant), while the option list
 *   was painted by WebView2 — a light grey/white surface no rule in
 *   `globals.css` can reach. Inputs, by contrast, already used the global
 *   accent (`FIELD_SURFACE_CLASS`), and menu rows already used the accent
 *   tint family (`listRowSelected` / `listRowHover`). A dropdown therefore
 *   read as a different widget from the field beside it and from the menu
 *   it opened.
 *
 *   These tokens compose those two *existing* sources rather than defining
 *   a third colour vocabulary:
 *     trigger = the input field surface (accent border on focus/open)
 *     item    = the list-row tint family (accent wash + accent check)
 *
 *   `Dropdown.tsx` is the control that consumes them; popup menus that are
 *   not `<select>` replacements (model picker, PM filters, combo boxes …)
 *   consume the same tokens so "selected" reads identically everywhere.
 *   Do NOT reintroduce `bg-zinc-*` / `hover:bg-panel-inset` for a selected
 *   or expanded dropdown state.
 */
import { cn } from "../../lib/utils";
import { listRowHover, listRowSelected } from "../../lib/ui-styles";
import { FIELD_SURFACE_CLASS } from "./StyledInput";

/**
 * The trigger is a field, not a button: it inherits the exact surface
 * `StyledInput` / `StyledTextarea` / the date-time picker use, including
 * the thin `focus:border-[var(--color-accent)]`. No ring, no outline, no grey.
 */
export const DROPDOWN_TRIGGER_CLASS = cn(
  FIELD_SURFACE_CLASS,
  "flex max-w-full items-center justify-between gap-2 text-left",
  "disabled:cursor-not-allowed disabled:opacity-50",
);

/** Compact variant — geometry (height / width / padding / font size) only.
 *  NO colour tokens here: a small dropdown must not read dimmer than a
 *  standard one or than the text field beside it. */
export const DROPDOWN_TRIGGER_SMALL_CLASS =
  "h-7 w-auto rounded-md px-1.5 py-0 text-11";

/** Open state keeps the accent border even when the trigger loses focus. */
export const DROPDOWN_TRIGGER_OPEN_CLASS = "border-[var(--color-accent)]";

/**
 * Expanded / pressed toolbar-style trigger (model picker, workspace picker,
 * title-bar menus). A borderless trigger cannot take an accent *border*, so
 * it takes the same accent wash a selected menu row wears instead. This is
 * the ONE source for that recipe — `ui-styles.ts` deliberately does not
 * export a second copy.
 */
export const DROPDOWN_TRIGGER_ACTIVE_CLASS = cn(
  listRowSelected,
  "text-[var(--color-accent)]",
);

/** The popup surface: same material as every dialog / context menu. */
export const DROPDOWN_PANEL_CLASS =
  "z-[100] rounded-md border border-border-outer bg-modal-surface p-1 shadow-xl";

/** One menu row. Background colour is supplied by the state tokens below. */
export const DROPDOWN_ITEM_CLASS =
  "flex w-full cursor-pointer items-center justify-between gap-2 rounded px-2.5 py-1.5 text-left text-xs transition-colors outline-none";

/** Cursor over an unselected row / keyboard-active unselected row. */
export const DROPDOWN_ITEM_HOVER_CLASS = listRowHover;

/** The current selection — accent wash, label keeps its neutral foreground. */
export const DROPDOWN_ITEM_SELECTED_CLASS = listRowSelected;

/** Keyboard cursor, drawn as a hairline ring so it cannot wash out the
 *  selected row's stronger tint. */
export const DROPDOWN_ITEM_ACTIVE_CLASS =
  "ring-1 ring-inset ring-[var(--color-accent)]/40";

/** The check mark is the only accent-coloured *foreground* in a menu row. */
export const DROPDOWN_CHECK_CLASS =
  "h-3.5 w-3.5 shrink-0 text-[var(--color-accent)]";

export const DROPDOWN_ARROW_CLASS =
  "h-3.5 w-3.5 shrink-0 text-text-tertiary transition-transform";

/** Assemble a row's classes. Kept here so the state precedence has one home. */
export function dropdownItemClass(
  selected: boolean,
  active = false,
  extra?: string,
): string {
  return cn(
    DROPDOWN_ITEM_CLASS,
    selected ? DROPDOWN_ITEM_SELECTED_CLASS : DROPDOWN_ITEM_HOVER_CLASS,
    active && DROPDOWN_ITEM_ACTIVE_CLASS,
    extra,
  );
}
