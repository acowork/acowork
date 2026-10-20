/**
 * UrlComboBox — text input that doubles as a history dropdown.
 *
 * Lightweight combo box purpose-built for the Gateway URL field:
 *   - Free-form typing in the input (handled exactly like a plain <input>).
 *   - Clicking the trailing chevron opens a list of recently-saved URLs
 *     from `options` (most-recent first). Selecting one populates the
 *     input via `onChange`; the caller decides whether to commit.
 *
 * Intentionally NOT a generic combobox — no filter / no async / no
 * keyboard nav. The history is short, the use is single-field, and the
 * 12-line browser <datalist> alternative doesn't render selected
 * markers. Keeping it local and small.
 */
import { useEffect, useRef, useState } from "react";
import { ChevronDown } from "lucide-react";
import { cn } from "../../lib/utils";
import { OverlayScrollbar } from "../common/OverlayScrollbar";

export interface UrlComboBoxProps {
  value: string;
  onChange: (value: string) => void;
  onCommit?: () => void;
  options: string[];
  placeholder?: string;
  inputClassName?: string;
  disabled?: boolean;
  ariaLabel?: string;
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

  // De-dupe options vs current value so the dropdown doesn't echo the
  // value the user is currently editing.
  const items = options.filter((o) => o !== value);

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
          containerClassName="absolute left-0 right-0 top-full z-20 mt-1 overflow-hidden rounded-md border border-border-outer bg-page-bg shadow-lg"
        >
          <ul role="listbox">
            {items.map((url) => (
              <li
                key={url}
                role="option"
                aria-selected="false"
                onMouseDown={(e) => {
                  // Use mousedown so we beat the input's blur-driven commit.
                  e.preventDefault();
                }}
                onClick={() => {
                  onChange(url);
                  setOpen(false);
                  onCommit?.();
                }}
                className="cursor-pointer truncate px-3 py-1.5 font-mono text-xs hover:bg-panel-inset"
                title={url}
              >
                {url}
              </li>
            ))}
          </ul>
        </OverlayScrollbar>
      )}
    </div>
  );
}