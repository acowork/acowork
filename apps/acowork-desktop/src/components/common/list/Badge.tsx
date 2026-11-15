//! Badge — the unified inline status/label pill.
//!
//! Centralizes every hand-rolled 9px/10px badge that drifted across the
//! panels (zinc/amber/emerald/red literals with mixed padding). Tones are
//! token-driven: `success`/`warning` read the --color-ok/--color-warning
//! tokens (light + dark values in globals.css, no `dark:` prefix needed),
//! `danger` reuses --color-destructive, `accent` follows the established
//! `var(--color-accent)` wash used by memory type pills.
//!
//! NOT for semantic data-viz pills that need their own color coding (e.g.
//! the compression-level badge keeps amber/orange distinction).
import type { HTMLAttributes } from "react";
import { cn } from "../../../lib/utils";

export type BadgeTone = "neutral" | "accent" | "success" | "warning" | "danger";

export interface BadgeProps extends HTMLAttributes<HTMLSpanElement> {
  tone?: BadgeTone;
  /** All-caps with wide tracking (semantic status badges, memory types). */
  uppercase?: boolean;
  /** Monospace text (counts, ids). */
  mono?: boolean;
}

const toneClasses: Record<BadgeTone, string> = {
  neutral: "bg-surface text-text-tertiary ",
  accent:
    "bg-[var(--color-accent)]/10 text-[var(--color-accent)] dark:bg-[var(--color-accent)]/20",
  success: "bg-[var(--color-ok)]/12 text-[var(--color-ok)]",
  warning: "bg-[var(--color-warning)]/12 text-[var(--color-warning)]",
  danger: "bg-[var(--color-destructive)]/12 text-[var(--color-destructive)]",
};

export function Badge({
  tone = "neutral",
  uppercase,
  mono,
  className,
  children,
  ...rest
}: BadgeProps) {
  return (
    <span
      className={cn(
        "inline-flex items-center whitespace-nowrap rounded px-1.5 py-0.5 text-9 font-medium leading-none",
        toneClasses[tone],
        uppercase && "uppercase tracking-wider",
        mono && "font-mono",
        className,
      )}
      {...rest}
    >
      {children}
    </span>
  );
}
