/**
 * Dropdown — the app's themed select control.
 *
 * ## Why the native <select> is gone
 *
 * `<select>` hands its option list to WebView2/Chromium. That popup is not
 * part of the page DOM, so no rule in `globals.css` can reach it: the list
 * stayed light grey/white while the fields around it used the global accent.
 * The fix is not a style but a control — the same argument that produced
 * the app's own date/time field for `<input type="date">`.
 *
 * ## The hidden native <select>
 *
 * A visually hidden, non-tabbable `<select>` is still rendered as the value /
 * event / accessibility carrier. It keeps the native form semantics, the
 * `role="combobox"` + accessible name, the `<option>` list, and the existing
 * `fireEvent.change`-style contracts. It never paints anything and never
 * opens a native popup: the trigger and the option list are ours.
 *
 *   IMPORTANT: the <select> itself must stay in the accessibility tree. Do
 *   NOT replace `SR_ONLY_STYLE` with `display: none` or put `aria-hidden` on
 *   the select — both would remove the combobox and break its consumers.
 *   Its <option>s DO carry `aria-hidden`: the visible listbox owns the option
 *   roles, and two copies of every choice would make the tree (and every
 *   `getByRole("option")` query) ambiguous. DOM `option` nodes stay intact.
 *
 *   `id` / `data-testid` / `name` stay on the carrier to preserve the form
 *   and test seams. `style` and `title` do NOT: they belong to the visible
 *   trigger, exactly as they did when the <select> was visible. One known
 *   trade-off: a `<label htmlFor>` focuses the carrier rather than opening
 *   our popup (no call site relies on clicking a label).
 *
 * ## Theming
 *
 * All colours come from `dropdownTheme.ts`, which composes the two existing
 * sources of truth: `FIELD_SURFACE_CLASS` (accent border on focus) for the
 * trigger and `listRowSelected` / `listRowHover` (accent wash) for the rows.
 * There is no third colour vocabulary here and no `bg-zinc-*` selection.
 *
 * ## Layering
 *
 * The popup renders through a portal with `position: fixed`, because dialogs
 * and side panes are `overflow-hidden` and scroll their own bodies — exactly
 * the constraint the date/time picker documents. Escape is stopped at the
 * popup so closing a dropdown does not close the dialog that hosts it.
 */
import * as React from "react";
import { useCallback, useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Check, ChevronDown } from "lucide-react";
import { cn } from "../../lib/utils";
import { OverlayScrollbar } from "./OverlayScrollbar";
import {
  DROPDOWN_ARROW_CLASS,
  DROPDOWN_CHECK_CLASS,
  DROPDOWN_PANEL_CLASS,
  DROPDOWN_TRIGGER_CLASS,
  DROPDOWN_TRIGGER_OPEN_CLASS,
  DROPDOWN_TRIGGER_SMALL_CLASS,
  dropdownItemClass,
} from "./dropdownTheme";

export interface DropdownOption {
  value: string;
  label: React.ReactNode;
}

export interface DropdownPlaceholder {
  value: string;
  label: React.ReactNode;
  /** Whether the placeholder option can be re-picked by the user. */
  selectable?: boolean;
}

export type DropdownSize = "standard" | "small";

export interface DropdownProps
  extends Omit<React.SelectHTMLAttributes<HTMLSelectElement>, "size" | "onChange"> {
  value: string;
  onChange: (value: string) => void;
  options: DropdownOption[];
  placeholder?: DropdownPlaceholder;
  size?: DropdownSize;
  /** Extra className layered on top of the trigger's base classes. */
  className?: string;
}

/** Keeps the native carrier accessible while removing it from the layout.
 *  Never merge a caller's `style` into this: opacity/clip are load-bearing. */
const SR_ONLY_STYLE: React.CSSProperties = {
  position: "absolute",
  width: 1,
  height: 1,
  padding: 0,
  margin: -1,
  overflow: "hidden",
  clip: "rect(0, 0, 0, 0)",
  whiteSpace: "nowrap",
  borderWidth: 0,
  opacity: 0,
  pointerEvents: "none",
};

const MENU_MIN_WIDTH = 160;
const MENU_MAX_HEIGHT = 240;
const MENU_MIN_HEIGHT = 24;
const MENU_GAP = 4;

interface Placement {
  top: number;
  left: number;
  width: number;
  maxHeight: number;
}

/** `<option>` may only hold text; a label can be any ReactNode. */
function optionText(node: React.ReactNode): string {
  if (node === null || node === undefined || typeof node === "boolean") return "";
  if (typeof node === "string" || typeof node === "number") return String(node);
  if (Array.isArray(node)) return node.map(optionText).join("");
  if (React.isValidElement(node)) {
    return optionText((node.props as { children?: React.ReactNode }).children);
  }
  return "";
}

export function Dropdown({
  value,
  onChange,
  options,
  placeholder,
  size = "standard",
  className,
  disabled,
  style,
  title,
  "aria-label": ariaLabel,
  ...rest
}: DropdownProps) {
  const [open, setOpen] = useState(false);
  const [pos, setPos] = useState<Placement | null>(null);
  const [activeIndex, setActiveIndex] = useState(0);

  const triggerRef = useRef<HTMLButtonElement>(null);
  const popupRef = useRef<HTMLDivElement>(null);
  const itemRefs = useRef<Array<HTMLButtonElement | null>>([]);
  const popupId = useId();

  /** A non-selectable placeholder is display-only, never a menu row. */
  const menuOptions = useMemo<DropdownOption[]>(() => {
    if (placeholder?.selectable) {
      return [{ value: placeholder.value, label: placeholder.label }, ...options];
    }
    return options;
  }, [placeholder, options]);

  const currentLabel =
    options.find((o) => o.value === value)?.label ??
    (placeholder && value === placeholder.value ? placeholder.label : value);

  const hide = useCallback((refocus: boolean) => {
    setOpen(false);
    setPos(null);
    if (refocus) triggerRef.current?.focus();
  }, []);

  const show = useCallback(() => {
    const current = menuOptions.findIndex((o) => o.value === value);
    setActiveIndex(current >= 0 ? current : 0);
    setPos(null);
    setOpen(true);
  }, [menuOptions, value]);

  const commit = useCallback(
    (next: string) => {
      onChange(next);
      hide(true);
    },
    [onChange, hide],
  );

  /** Place below the trigger, flipping above when there is no room. Re-run
   *  on resize and on any scroll (capture includes the dialog's own body). */
  useLayoutEffect(() => {
    if (!open) return;
    const place = () => {
      const trigger = triggerRef.current;
      if (!trigger) return;
      const rect = trigger.getBoundingClientRect();
      const width = Math.max(rect.width, MENU_MIN_WIDTH);
      const height = popupRef.current?.offsetHeight ?? 0;
      const spaceBelow = window.innerHeight - rect.bottom - MENU_GAP;
      const spaceAbove = rect.top - MENU_GAP;
      const wanted = height || MENU_MAX_HEIGHT;
      const flipUp = spaceBelow < wanted && spaceAbove > spaceBelow;
      // Do NOT raise this floor to a "comfortable" height: when the trigger
      // sits near a viewport edge the available space is the real cap, and an
      // 80px floor would push the menu off-screen. The small floor only keeps
      // a degenerate 0-available case from producing an unusable 0px box.
      const maxHeight = Math.max(
        MENU_MIN_HEIGHT,
        Math.min(MENU_MAX_HEIGHT, flipUp ? spaceAbove : spaceBelow),
      );
      const drawn = Math.min(wanted, maxHeight);
      setPos({
        top: flipUp
          ? Math.max(MENU_GAP, rect.top - MENU_GAP - drawn)
          : rect.bottom + MENU_GAP,
        left: Math.max(MENU_GAP, Math.min(rect.left, window.innerWidth - width - MENU_GAP)),
        width,
        maxHeight,
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

  const isOpen = open && !!pos;

  /** The keyboard cursor owns focus. Mouse hover uses the row's CSS hover
   *  tint and does NOT move DOM focus — a cursor brushing the list must not
   *  steal focus from an in-progress keyboard navigation. */
  useEffect(() => {
    if (!isOpen) return;
    itemRefs.current[activeIndex]?.focus();
  }, [isOpen, activeIndex]);

  const onPopupKeyDown = (e: React.KeyboardEvent<HTMLDivElement>) => {
    if (e.key === "Escape") {
      // Do not let the dialog's window-level Escape handler see this key:
      // the user closed a dropdown, not the form.
      e.preventDefault();
      e.stopPropagation();
      hide(true);
      return;
    }
    if (e.key === "Tab") {
      hide(false);
      return;
    }
    if (!menuOptions.length) return;
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      e.stopPropagation();
      const step = e.key === "ArrowDown" ? 1 : -1;
      setActiveIndex((i) => (i + step + menuOptions.length) % menuOptions.length);
      return;
    }
    if (e.key === "Home" || e.key === "End") {
      e.preventDefault();
      e.stopPropagation();
      setActiveIndex(e.key === "Home" ? 0 : menuOptions.length - 1);
      return;
    }
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      e.stopPropagation();
      const item = menuOptions[activeIndex];
      if (item) commit(item.value);
    }
  };

  return (
    <>
      {/*
        The native carrier: value, <option> list, form/change events and the
        combobox role. Visually hidden but still in the accessibility tree.
      */}
      <select
        {...rest}
        aria-label={ariaLabel}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        disabled={disabled}
        tabIndex={-1}
        style={SR_ONLY_STYLE}
      >
        {placeholder && (
          <option
            value={placeholder.value}
            disabled={!placeholder.selectable}
            hidden={!placeholder.selectable}
            aria-hidden="true"
          >
            {optionText(placeholder.label)}
          </option>
        )}
        {options.map((o) => (
          <option key={o.value} value={o.value} aria-hidden="true">
            {optionText(o.label)}
          </option>
        ))}
      </select>

      <button
        ref={triggerRef}
        type="button"
        disabled={disabled}
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={open ? popupId : undefined}
        title={title}
        data-testid="dropdown-trigger"
        onClick={() => (open ? hide(false) : show())}
        onKeyDown={(e) => {
          if (e.key === "ArrowDown" || e.key === "ArrowUp") {
            e.preventDefault();
            if (!open) show();
          }
          if (e.key === "Escape" && open) {
            e.preventDefault();
            e.stopPropagation();
            hide(true);
          }
        }}
        className={cn(
          DROPDOWN_TRIGGER_CLASS,
          size === "small" && DROPDOWN_TRIGGER_SMALL_CLASS,
          open && DROPDOWN_TRIGGER_OPEN_CLASS,
          className,
        )}
        // `style` / `title` belong to the visible control. Inline borderColor
        // beats a caller's semantic border override (WorkspaceManager tints
        // read/write orange): an open dropdown is always accent-bordered.
        style={open ? { ...style, borderColor: "var(--color-accent)" } : style}
      >
        <span className="min-w-0 truncate">{currentLabel}</span>
        <ChevronDown
          className={cn(DROPDOWN_ARROW_CLASS, open && "rotate-180")}
          aria-hidden
        />
      </button>

      {open &&
        createPortal(
          <div
            ref={popupRef}
            id={popupId}
            role="listbox"
            aria-label={ariaLabel}
            data-testid="dropdown-menu"
            onKeyDown={onPopupKeyDown}
            style={{
              position: "fixed",
              top: pos?.top ?? 0,
              left: pos?.left ?? 0,
              width: pos?.width ?? MENU_MIN_WIDTH,
              visibility: isOpen ? "visible" : "hidden",
            }}
            className={DROPDOWN_PANEL_CLASS}
          >
            {/* The app's scrollbar, not an OS one — every scroll container in
                the app is bound to `OverlayScrollbar` (see the coverage test). */}
            <OverlayScrollbar maxHeight={pos?.maxHeight ?? MENU_MAX_HEIGHT}>
              {menuOptions.map((o, i) => {
                const selected = o.value === value;
                return (
                  <button
                    key={o.value}
                    ref={(el) => {
                      itemRefs.current[i] = el;
                    }}
                    type="button"
                    role="option"
                    aria-selected={selected}
                    tabIndex={-1}
                    onClick={() => commit(o.value)}
                    className={dropdownItemClass(selected, i === activeIndex)}
                  >
                    <span className="min-w-0 truncate">{o.label}</span>
                    {selected && <Check className={DROPDOWN_CHECK_CLASS} aria-hidden />}
                  </button>
                );
              })}
            </OverlayScrollbar>
          </div>,
          document.body,
        )}
    </>
  );
}
