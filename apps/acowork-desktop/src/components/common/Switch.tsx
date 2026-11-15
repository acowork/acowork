import { forwardRef, type InputHTMLAttributes, type ReactNode } from "react";
import { cn } from "../../lib/utils";

/**
 * Switch — iOS / Android style toggle with a circular thumb.
 *
 * A drop-in replacement for `<input type="checkbox">` that renders an
 * iOS/Android-style track + circular thumb. The native checkbox is
 * kept in the DOM (visually hidden via `peer sr-only`) so:
 *
 *   - Form submission / accessibility works unchanged (real
 *     `role="switch"`, keyboard space/enter, focus ring).
 *   - The browser / screen-reader still sees a checkbox; the visible
 *     track + thumb are pure CSS peers of the hidden input.
 *
 * Accent color is driven by `var(--color-accent)`, which is updated
 * dynamically by `settingsStore.applyAccentPreset()` whenever the user
 * changes the highlight color in Settings. This means every accent
 * preset (blue / indigo / violet / cyan / teal / green / rose / orange
 * / amber / pink) is supported without per-preset overrides.
 *
 * Sizing:
 *   - `sm` (28 × 16) — inline with 11–12px list-row text
 *   - `md` (36 × 20) — default iOS-style, used for standalone toggles
 *
 * Layout (`labelPosition`):
 *   - `"left"` (default, iOS-style) — label on the leading edge, switch
 *     anchored to the trailing edge. The label gets `flex-1` and the
 *     wrapper gets `w-full justify-between` so the toggle reaches the
 *     right side of the row.
 *   - `"right"` (Material-style) — switch on the leading edge, label
 *     sits inline to its right.
 *
 * Usage:
 *   <Switch checked={x} onChange={setX} />                              // bare toggle
 *   <Switch checked={x} onChange={setX} label="Enable compression" />   // iOS row layout
 *   <Switch checked={x} onChange={setX} label="..." labelPosition="right" />
 *
 * The component renders its own outer `<label>`, so the track + thumb
 * are clickable as a unit. Do NOT wrap a `<Switch>` inside another
 * `<label>` (HTML disallows nested labels) — use a `<div>` instead and
 * rely on the inner label association.
 */
export type SwitchSize = "sm" | "md";
export type SwitchLabelPosition = "left" | "right";

interface SwitchProps
  extends Omit<InputHTMLAttributes<HTMLInputElement>, "type" | "onChange" | "size"> {
  /** Whether the switch is on. */
  checked: boolean;
  /**
   * Change handler — receives the new boolean state. This shorthand
   * (vs. the standard `ChangeEvent`) is the more common shape for
   * toggle controls and plays well with `setState` / zustand setters:
   *
   *   <Switch checked={x} onChange={setX} />
   *   <Switch checked={x} onChange={(v) => save(v)} />
   *
   * Callers that need the underlying `ChangeEvent` can either read
   * `e.target.checked` themselves or wrap:
   *
   *   <Switch checked={x} onChange={(v) => onUpdate('field', v)} />
   */
  onChange: (checked: boolean) => void;
  /** Size preset. Default: `"md"`. */
  size?: SwitchSize;
  /**
   * Optional label rendered inside the component. Position is
   * controlled by `labelPosition`. When omitted, the Switch renders
   * as a bare toggle (callers handle their own layout — typical for
   * list rows where the label sits in the parent `<div>`).
   */
  label?: ReactNode;
  /**
   * Where to render the label relative to the track.
   * Default: `"left"` (iOS Settings style — text on the leading edge,
   * toggle anchored to the trailing edge).
   */
  labelPosition?: SwitchLabelPosition;
  /** Extra classes for the outer wrapper. */
  className?: string;
  /** Extra classes for the visible track element. */
  trackClassName?: string;
}

export const Switch = forwardRef<HTMLInputElement, SwitchProps>(function Switch(
  {
    checked,
    onChange,
    disabled,
    size = "md",
    label,
    labelPosition = "left",
    className,
    trackClassName,
    ...rest
  },
  ref,
) {
  const handleChange = (e: React.ChangeEvent<HTMLInputElement>) => {
    onChange(e.target.checked);
  };

  // Only stretch the wrapper when the label actually needs to push the
  // toggle to the trailing edge. A bare `<Switch />` (no label) inside
  // a flex row should stay at its intrinsic width — applying `w-full`
  // unconditionally would make it balloon to fill its parent.
  const hasLabel = label !== undefined && label !== null;
  const labelOnLeft = labelPosition === "left";

  return (
    <label
      className={cn(
        "inline-flex shrink-0 cursor-pointer items-center gap-2 select-none",
        // iOS-style "label left, toggle right" — the label gets flex-1
        // so it claims the middle space and the toggle anchors flush
        // to the right edge of the wrapper.
        labelOnLeft && hasLabel && "w-full justify-between",
        disabled && "cursor-not-allowed opacity-50",
        className,
      )}
    >
      {labelOnLeft && hasLabel && (
        <span className="flex-1 text-10 font-medium text-text-tertiary ">
          {label}
        </span>
      )}
      <span className="relative inline-flex items-center">
        <input
          ref={ref}
          type="checkbox"
          role="switch"
          checked={checked}
          disabled={disabled}
          onChange={handleChange}
          className="peer sr-only"
          {...rest}
        />
        {/* Track + thumb. The visual state is driven by `peer-checked` /
            `peer-disabled`, so the hidden input remains the source of truth. */}
        <span
          aria-hidden="true"
          className={cn(
            // Base track
            "block rounded-full transition-colors duration-150",
            "bg-surface-strong",
            "peer-checked:bg-[var(--color-accent)]",
            "peer-focus-visible:ring-2 peer-focus-visible:ring-[var(--color-accent)] peer-focus-visible:ring-offset-2 peer-focus-visible:ring-offset-white dark:peer-focus-visible:ring-offset-zinc-900",
            // Sizing
            size === "sm" ? "h-4 w-7" : "h-5 w-9",
            trackClassName,
          )}
        />
        <span
          aria-hidden="true"
          className={cn(
            // Thumb — positioned absolutely on the left (left-0.5 = 2px gap
            // on the leading edge of the track) and vertically centered
            // via top-1/2 + -translate-y-1/2.
            "pointer-events-none absolute left-0.5 top-1/2 -translate-y-1/2 rounded-full bg-white shadow",
            "transition-transform duration-150 ease-out",
            // Slide to the right edge when checked. Because the thumb
            // diameter is `track height - 4` (a 2px gap on each side),
            // translating by the thumb's own width (100%) puts it at
            // the trailing edge with the same 2px gap — the math works
            // for both sm and md without needing a per-size variant.
            "peer-checked:translate-x-full",
            // Sizing — thumb diameter = track height - 4px.
            size === "sm" ? "h-3 w-3" : "h-4 w-4",
          )}
        />
      </span>
      {!labelOnLeft && hasLabel && (
        <span className="text-10 font-medium text-text-tertiary ">
          {label}
        </span>
      )}
    </label>
  );
});
