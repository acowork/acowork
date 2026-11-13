/**
 * UrlComboBox — text input that doubles as a history dropdown.
 *
 * Lightweight combo box purpose-built for the Gateway URL field:
 *   - Free-form typing in the input (handled exactly like a plain <input>).
 *   - Clicking the trailing chevron opens the list of known addresses.
 *     Selecting one populates the input via `onChange`; the caller decides
 *     whether to commit.
 *   - `renderOption` lets the caller decorate each row (reachability dot,
 *     "current" tag, ...). The CURRENT value stays in the list on purpose:
 *     the dropdown is the one place all known addresses live, so it must
 *     also say which one is in use — filtering it out was what forced the
 *     flat duplicate list below the field.
 *
 * Intentionally NOT a generic combobox — no filter / no async / no
 * keyboard nav. The history is short, the use is single-field, and the
 * 12-line browser <datalist> alternative doesn't render selected
 * markers. Keeping it local and small.
 */
import { useEffect, useRef, useState, type ReactNode } from "react";
import { ChevronDown } from "lucide-react";
import { cn } from "../../lib/utils";
import { OverlayScrollbar } from "../common/OverlayScrollbar";

export interface UrlComboBoxProps {
  value: string;
  onChange: (value: string) => void;
  /** Commit the current input. Called with no argument for Enter / blur;
   *  called with the picked URL when an option row is clicked — React has
   *  not applied the option's `onChange` yet at that moment, so the caller
   *  must trust the argument over its own draft. */
  onCommit?: (url?: string) => void;
  options: string[];
  placeholder?: string;
  inputClassName?: string;
  disabled?: boolean;
  ariaLabel?: string;
  /** Rich dropdown row. Receives the option URL; returns its content.
   *  Omit it to render the plain URL (legacy behaviour). */
  renderOption?: (url: string) => ReactNode;
}

export function UrlComboBox({
  value,
  onChange,
  onCommit,
  options,
  placeholder,
  inputClassName,
  disabled,
  ariaLabel,
  renderOption,
}: UrlComboBoxProps) {
  const [open, setOpen] = useState(false);
  const wrapRef = useRef<HTMLDivElement>(null);

  // Close on outside click so the dropdown doesn't trap pointer events.
  useEffect(() => {
    if (!open) return;
    const onDocDown = (e: MouseEvent) => {
      if (!wrapRef.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", onDocDown);
    return () => document.removeEventListener("mousedown", onDocDown);
  }, [open]);

  // The current value stays in the list: the dropdown is the single place
  // all known addresses (and their state) live. `renderOption` marks it.
  const items = options;

  return (
    <div ref={wrapRef} className="relative flex-1">
      <input
        type="text"
        value={value}
        aria-label={ariaLabel}
        placeholder={placeholder}
        disabled={disabled}
        onChange={(e) => onChange(e.target.value)}
        onBlur={() => {
          // Defer so a click on a dropdown item still registers as a
          // selection (mousedown fires before blur).
          requestAnimationFrame(() => {
            if (!wrapRef.current?.contains(document.activeElement)) {
              setOpen(false);
              onCommit?.();
            }
          });
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter") onCommit?.();
          if (e.key === "Escape") setOpen(false);
        }}
        className={cn("w-full rounded-md border border-input-border bg-input-bg px-3 py-[var(--ui-input-py)] pr-8 text-xs outline-none transition-colors focus:border-[var(--color-accent)]", inputClassName)}
      />
      <button
        type="button"
        aria-label="Show history"
        disabled={disabled || items.length === 0}
        onMouseDown={(e) => {
          // Prevent input blur before our click handler runs.
          e.preventDefault();
        }}
        onClick={() => {
          if (items.length === 0) return;
          setOpen((v) => !v);
        }}
        className="absolute right-1 top-1/2 -translate-y-1/2 rounded p-1 text-text-tertiary hover:text-text-primary disabled:opacity-40"
      >
        <ChevronDown className={cn("h-3.5 w-3.5 transition-transform", open && "rotate-180")} />
      </button>
      {open && items.length > 0 && (
        <OverlayScrollbar
          maxHeight={240}
          // NOT locked to the input's width: the wrapper may be squeezed
          // narrow (login card row), but the option rows must still show a
          // full URL. `w-max` grows to the longest URL, `min-w-full` keeps
          // at least the input's own width, and the viewport cap keeps a
          // pathological URL from running off-screen.
          containerClassName="absolute left-0 top-full z-20 mt-1 w-max min-w-full max-w-[calc(100vw-2rem)] overflow-hidden rounded-md border border-border-outer bg-page-bg shadow-lg"
        >
          <ul role="listbox">
            {items.map((url) => (
              <li
                key={url}
                role="option"
                aria-selected={url === value ? "true" : "false"}
                onMouseDown={(e) => {
                  // Use mousedown so we beat the input's blur-driven commit.
                  e.preventDefault();
                }}
                onClick={() => {
                  onChange(url);
                  setOpen(false);
                  onCommit?.(url);
                }}
                className="cursor-pointer px-2 py-1.5 hover:bg-panel-inset"
                title={url}
              >
                {renderOption ? (
                  renderOption(url)
                ) : (
                  <span className="block truncate font-mono text-xs">{url}</span>
                )}
              </li>
            ))}
          </ul>
        </OverlayScrollbar>
      )}
    </div>
  );
}
